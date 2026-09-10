//! Route handlers. Each returns a JSON body or an `anyhow` error (rendered as a
//! 400 with `{"error": ...}`).

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use fortis_node::Rpc;
use wallet_core::bitcoin::address::NetworkUnchecked;
use wallet_core::bitcoin::{consensus, Address, Network, Transaction};
use wallet_core::{Chain, ChainParams};

use crate::state::{Connected, Pricing, State};

fn conn(state: &State) -> Result<&Connected> {
    state
        .connected
        .as_ref()
        .ok_or_else(|| anyhow!("gateway is not connected — POST /v1/connect first"))
}

pub fn status(rpc: &Rpc, state: &State, pricing: Option<&Pricing>) -> Result<Value> {
    let node = fortis_node::chain_status(rpc)?;
    let scanning = state
        .connected
        .as_ref()
        .and_then(|c| fortis_node::scanning(rpc, &c.watch_wallet))
        .map(|(progress, duration)| json!({ "progress": progress, "duration": duration }));
    Ok(json!({
        "node": node,
        "connected": state.connected,
        "scanning": scanning,
        "pricing": pricing,
    }))
}

#[derive(Deserialize)]
pub struct ConnectReq {
    pub chain: String,
    pub network: String,
    pub account_xpub: String,
    pub master_fingerprint: String,
    #[serde(default)]
    pub rescan: bool,
    #[serde(default = "default_range")]
    pub range: u32,
}
fn default_range() -> u32 {
    1000
}

pub fn connect(rpc: &Rpc, state: &mut State, home: &Path, req: ConnectReq) -> Result<Value> {
    let chain = match req.chain.as_str() {
        "xbt" => Chain::Xbt,
        "btc" => Chain::Btc,
        other => bail!("chain must be \"xbt\" or \"btc\", got {other:?}"),
    };
    let params = ChainParams::resolve(chain, &req.network)
        .ok_or_else(|| anyhow!("unknown chain/network {}/{}", req.chain, req.network))?;
    let watch_wallet = format!("fortis-{}", req.chain);

    fortis_node::ensure_watch_wallet(rpc, &watch_wallet)?;
    let (recv_ok, change_ok) = fortis_node::import_account(
        rpc,
        &watch_wallet,
        &req.master_fingerprint,
        params.bip44_coin_type,
        0,
        &req.account_xpub,
        req.range,
        req.rescan,
    )?;
    if !(recv_ok && change_ok) {
        bail!("descriptor import failed on the node");
    }

    state.connected = Some(Connected {
        chain: req.chain,
        network: req.network,
        account_xpub: req.account_xpub,
        master_fingerprint: req.master_fingerprint,
        watch_wallet: watch_wallet.clone(),
    });
    state.save(home)?;

    let scanning = fortis_node::scanning(rpc, &watch_wallet)
        .map(|(progress, duration)| json!({ "progress": progress, "duration": duration }));
    Ok(json!({ "watch_wallet": watch_wallet, "imported": true, "scanning": scanning }))
}

pub fn balances(rpc: &Rpc, state: &State) -> Result<Value> {
    let b = fortis_node::wallet_balances(rpc, &conn(state)?.watch_wallet)?;
    Ok(serde_json::to_value(b)?)
}

pub fn utxos(rpc: &Rpc, state: &State, min_conf: u32) -> Result<Value> {
    let list = fortis_node::collect_utxos(rpc, &conn(state)?.watch_wallet, min_conf)?;
    let rows: Vec<Value> = list
        .iter()
        .map(|u| {
            json!({
                "txid": u.outpoint.txid.to_string(),
                "vout": u.outpoint.vout,
                "value_sat": u.value.to_sat(),
                "script_pubkey_hex": hex::encode(u.script_pubkey.as_bytes()),
                "confirmations": u.confirmations,
                "is_change": u.is_change,
                "derivation_index": u.derivation_index,
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

pub fn feerate(rpc: &Rpc, conf_target: u16) -> Result<Value> {
    Ok(json!({ "sat_vb": fortis_node::estimate_feerate(rpc, conf_target.max(1))? }))
}

pub fn history(rpc: &Rpc, state: &State, count: u32) -> Result<Value> {
    let h = fortis_node::history(rpc, &conn(state)?.watch_wallet, count.clamp(1, 1000))?;
    Ok(serde_json::to_value(h)?)
}

#[derive(Deserialize)]
pub struct BroadcastReq {
    pub hex: String,
}

/// The `bitcoin` crate's network for address parsing. Matches `ChainParams`:
/// both chains use `Bitcoin`-prefixed addresses on mainnet.
fn address_network(settings_network: &str) -> Network {
    if settings_network.starts_with("regtest") { Network::Regtest } else { Network::Bitcoin }
}

/// Reject a broadcast that doesn't pay at least `floor_sat` to the configured
/// service fee address. A floor-only check: it can't tell payment from change,
/// so it doesn't verify the full percentage — that's cooperative, this is the
/// minimum integrity check against a buggy or stripped client.
fn check_service_fee(tx: &Transaction, p: &Pricing, network: Network) -> Result<()> {
    let fee_spk = p
        .address
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|e| anyhow!("bad service fee address {}: {e}", p.address))?
        .require_network(network)
        .map_err(|_| anyhow!("service fee address {} is not valid on this network", p.address))?
        .script_pubkey();
    let paid: u64 = tx.output.iter().filter(|o| o.script_pubkey == fee_spk).map(|o| o.value.to_sat()).sum();
    if paid < p.floor_sat {
        bail!("this transaction must pay the service fee (>= {} sat to {})", p.floor_sat, p.address);
    }
    Ok(())
}

pub fn broadcast(rpc: &Rpc, req: BroadcastReq, pricing: Option<&Pricing>, settings_network: &str) -> Result<Value> {
    if let Some(p) = pricing {
        let raw = hex::decode(req.hex.trim()).map_err(|e| anyhow!("bad tx hex: {e}"))?;
        let tx: Transaction = consensus::deserialize(&raw).map_err(|e| anyhow!("bad tx: {e}"))?;
        check_service_fee(&tx, p, address_network(settings_network))?;
    }
    let txid = fortis_node::broadcast(rpc, req.hex.trim())?;
    Ok(json!({ "txid": txid.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wallet_core::bitcoin::hashes::Hash;
    use wallet_core::bitcoin::{transaction, Amount, ScriptBuf, TxOut, WPubkeyHash};

    fn tx_paying(spk: ScriptBuf, sat: u64) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: wallet_core::bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut { value: Amount::from_sat(sat), script_pubkey: spk }],
        }
    }

    fn fee_spk() -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([7u8; 20]))
    }

    fn pricing() -> Pricing {
        let address = Address::from_script(&fee_spk(), Network::Regtest).unwrap().to_string();
        Pricing { address, bps: 25, floor_sat: 200, cap_sat: 5_000 }
    }

    #[test]
    fn accepts_a_tx_paying_at_least_the_floor() {
        let tx = tx_paying(fee_spk(), 200);
        assert!(check_service_fee(&tx, &pricing(), Network::Regtest).is_ok());
    }

    #[test]
    fn rejects_a_tx_paying_below_the_floor() {
        let tx = tx_paying(fee_spk(), 199);
        assert!(check_service_fee(&tx, &pricing(), Network::Regtest).is_err());
    }

    #[test]
    fn rejects_a_tx_that_never_pays_the_fee_address() {
        let other = ScriptBuf::from(vec![0u8; 22]);
        let tx = tx_paying(other, 10_000);
        assert!(check_service_fee(&tx, &pricing(), Network::Regtest).is_err());
    }

    #[test]
    fn address_network_matches_settings() {
        assert_eq!(address_network("regtest"), Network::Regtest);
        assert_eq!(address_network("mainnet"), Network::Bitcoin);
    }
}
