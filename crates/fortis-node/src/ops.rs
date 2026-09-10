//! Wallet operations on top of [`Rpc`]: chain status, the watch-only descriptor
//! wallet, UTXO reads, fee estimation, history, broadcast.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use wallet_core::bitcoin::{Amount, OutPoint, ScriptBuf, Txid};
use wallet_core::wallet::Utxo;

use crate::rpc::Rpc;

#[derive(Debug, Clone, Serialize)]
pub struct ChainStatus {
    pub chain: String,
    pub blocks: u64,
    pub headers: u64,
    pub progress: f64,
    pub ibd: bool,
    pub pruned: bool,
    pub subversion: String,
    /// `Some(true/false)` when the node reports a `blake2b` deployment, else `None`.
    pub blake2b_active: Option<bool>,
    pub blake2b_height: Option<u64>,
}

pub fn chain_status(rpc: &Rpc) -> Result<ChainStatus> {
    let bci = rpc.call("getblockchaininfo", json!([]))?;
    let net = rpc.call("getnetworkinfo", json!([]))?;

    let (mut b2b_active, mut b2b_height) = (None, None);
    if let Ok(dep) = rpc.call("getdeploymentinfo", json!([])) {
        // Knots exposes it either at the top level or under `deployments`.
        let node = dep
            .get("blake2b")
            .or_else(|| dep.pointer("/deployments/blake2b"));
        if let Some(b) = node {
            b2b_active = b.get("active").and_then(Value::as_bool);
            b2b_height = b.get("height").and_then(Value::as_u64);
        }
    }

    Ok(ChainStatus {
        chain: get_str(&bci, "chain"),
        blocks: get_u64(&bci, "blocks"),
        headers: get_u64(&bci, "headers"),
        progress: bci.get("verificationprogress").and_then(Value::as_f64).unwrap_or(0.0),
        ibd: bci.get("initialblockdownload").and_then(Value::as_bool).unwrap_or(false),
        pruned: bci.get("pruned").and_then(Value::as_bool).unwrap_or(false),
        subversion: get_str(&net, "subversion"),
        blake2b_active: b2b_active,
        blake2b_height: b2b_height,
    })
}

pub fn wallet_loaded(rpc: &Rpc, name: &str) -> Result<bool> {
    let list = rpc.call("listwallets", json!([]))?;
    Ok(list
        .as_array()
        .map(|a| a.iter().any(|w| w.as_str() == Some(name)))
        .unwrap_or(false))
}

fn wallet_on_disk(rpc: &Rpc, name: &str) -> bool {
    rpc.call("listwalletdir", json!([]))
        .ok()
        .and_then(|d| {
            d.pointer("/wallets").and_then(Value::as_array).map(|a| {
                a.iter()
                    .any(|w| w.get("name").and_then(Value::as_str) == Some(name))
            })
        })
        .unwrap_or(false)
}

/// Ensure a descriptor, watch-only (private keys disabled) wallet named `name` is
/// loaded on the node, creating it if necessary.
pub fn ensure_watch_wallet(rpc: &Rpc, name: &str) -> Result<&'static str> {
    if wallet_loaded(rpc, name)? {
        return Ok("already loaded");
    }
    if wallet_on_disk(rpc, name) {
        rpc.call("loadwallet", json!([name]))?;
        return Ok("loaded");
    }
    // createwallet: name, disable_private_keys, blank, passphrase, avoid_reuse,
    //               descriptors, load_on_startup
    rpc.call(
        "createwallet",
        json!([name, true, true, "", false, true, true]),
    )?;
    Ok("created")
}

pub fn descriptor_checksum(rpc: &Rpc, descriptor: &str) -> Result<String> {
    let info = rpc.call("getdescriptorinfo", json!([descriptor]))?;
    info.get("checksum")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("getdescriptorinfo returned no checksum for `{descriptor}`"))
}

/// One entry for `importdescriptors`. Ranged descriptors may not carry a label.
pub fn import_request(descriptor: &str, internal: bool, range: u32, rescan: bool) -> Value {
    json!({
        "desc": descriptor,
        "active": true,
        "internal": internal,
        "timestamp": if rescan { json!(0) } else { json!("now") },
        "range": [0, range],
    })
}

pub fn import_descriptors(rpc: &Rpc, wallet: &str, requests: Vec<Value>) -> Result<Value> {
    rpc.wallet_call(wallet, "importdescriptors", json!([requests]))
}

/// The BIP-84 range descriptor for one branch: `wpkh([fp/84h/<coin>h/<account>h]xpub/<branch>/*)`.
pub fn wpkh_branch_descriptor(
    fingerprint: &str,
    coin_type: u32,
    account: u32,
    account_xpub: &str,
    branch: u32,
) -> String {
    format!("wpkh([{fingerprint}/84h/{coin_type}h/{account}h]{account_xpub}/{branch}/*)")
}

/// Import the receive (0) and change (1) branches of a BIP-84 account into `wallet`
/// as active, watch-only descriptors. Returns `(receive_ok, change_ok)`.
#[allow(clippy::too_many_arguments)]
pub fn import_account(
    rpc: &Rpc,
    wallet: &str,
    fingerprint: &str,
    coin_type: u32,
    account: u32,
    account_xpub: &str,
    range: u32,
    rescan: bool,
) -> Result<(bool, bool)> {
    let mut requests = Vec::with_capacity(2);
    for (branch, internal) in [(0u32, false), (1u32, true)] {
        let bare = wpkh_branch_descriptor(fingerprint, coin_type, account, account_xpub, branch);
        let checksum = descriptor_checksum(rpc, &bare)?;
        requests.push(import_request(
            &format!("{bare}#{checksum}"),
            internal,
            range,
            rescan,
        ));
    }
    let res = import_descriptors(rpc, wallet, requests)?;
    let ok = |i: usize| {
        res.get(i)
            .and_then(|r| r.get("success"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Ok((ok(0), ok(1)))
}

/// `Some((progress_0_to_1, duration_secs))` while a rescan is running.
pub fn scanning(rpc: &Rpc, wallet: &str) -> Option<(f64, u64)> {
    let info = rpc.wallet_call(wallet, "getwalletinfo", json!([])).ok()?;
    let s = info.get("scanning")?;
    if s.is_boolean() {
        return None;
    }
    Some((
        s.get("progress").and_then(Value::as_f64).unwrap_or(0.0),
        s.get("duration").and_then(Value::as_u64).unwrap_or(0),
    ))
}

/// A fee rate in sat/vB: `estimatesmartfee` for `conf_target`, floored at the
/// node's `minrelaytxfee` / `mempoolminfee` and never below 1.
pub fn estimate_feerate(rpc: &Rpc, conf_target: u16) -> Result<u64> {
    let btc_kvb_to_sat_vb = |btc_per_kvb: f64| (btc_per_kvb * 1e8 / 1000.0).ceil() as u64;

    let smart = rpc
        .call("estimatesmartfee", json!([conf_target]))
        .ok()
        .and_then(|v| v.get("feerate").and_then(Value::as_f64))
        .map(btc_kvb_to_sat_vb)
        .unwrap_or(0);

    let mp = rpc.call("getmempoolinfo", json!([]))?;
    let floor = |k: &str| {
        mp.get(k)
            .and_then(Value::as_f64)
            .map(btc_kvb_to_sat_vb)
            .unwrap_or(0)
    };

    Ok(smart
        .max(floor("minrelaytxfee"))
        .max(floor("mempoolminfee"))
        .max(1))
}

/// Read this wallet's spendable coins, mapping each back to its BIP-84 branch and
/// index via the descriptor string Core attaches to every `listunspent` row.
pub fn collect_utxos(rpc: &Rpc, wallet: &str, min_conf: u32) -> Result<Vec<Utxo>> {
    let rows = rpc.wallet_call(wallet, "listunspent", json!([min_conf, 9_999_999]))?;
    let rows = rows.as_array().cloned().unwrap_or_default();

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        if !r.get("spendable").and_then(Value::as_bool).unwrap_or(true) {
            continue;
        }
        let txid: Txid = r
            .get("txid")
            .and_then(Value::as_str)
            .context("listunspent row without txid")?
            .parse()
            .context("listunspent txid")?;
        let vout = r.get("vout").and_then(Value::as_u64).context("listunspent vout")? as u32;
        let amount_btc = r.get("amount").and_then(Value::as_f64).context("listunspent amount")?;
        let value = Amount::from_sat((amount_btc * 1e8).round() as u64);
        let spk_hex = r
            .get("scriptPubKey")
            .and_then(Value::as_str)
            .context("listunspent scriptPubKey")?;
        let script_pubkey = ScriptBuf::from_hex(spk_hex).context("listunspent scriptPubKey hex")?;
        let confirmations = r.get("confirmations").and_then(Value::as_u64).unwrap_or(0) as u32;

        let desc = r.get("desc").and_then(Value::as_str).ok_or_else(|| {
            anyhow!(
                "UTXO {txid}:{vout} has no descriptor — cannot determine its key path. \
                 Re-run the descriptor import."
            )
        })?;
        let (is_change, derivation_index) = parse_desc_path(desc).ok_or_else(|| {
            anyhow!("UTXO {txid}:{vout}: unrecognised descriptor path in `{desc}`")
        })?;

        out.push(Utxo {
            outpoint: OutPoint::new(txid, vout),
            value,
            script_pubkey,
            confirmations,
            derivation_index,
            is_change,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Balances {
    pub confirmed_sat: u64,
    pub pending_sat: u64,
    pub immature_sat: u64,
}

pub fn wallet_balances(rpc: &Rpc, wallet: &str) -> Result<Balances> {
    let b = rpc.wallet_call(wallet, "getbalances", json!([]))?;
    let mine = b.get("mine").cloned().unwrap_or(Value::Null);
    let sat = |k: &str| {
        mine.get(k)
            .and_then(Value::as_f64)
            .map(|v| (v * 1e8).round() as u64)
            .unwrap_or(0)
    };
    Ok(Balances {
        confirmed_sat: sat("trusted"),
        pending_sat: sat("untrusted_pending"),
        immature_sat: sat("immature"),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub txid: String,
    /// `"send"` | `"receive"` (`immature`/`generate`/`orphan` are folded into these).
    pub direction: String,
    /// Signed: negative for sends (incl. fee), positive for receives.
    pub amount_sat: i64,
    pub fee_sat: i64,
    pub confirmations: i64,
    pub time: i64,
    pub address: Option<String>,
}

/// Recent wallet transactions, newest first, de-duplicated by txid (a self-send
/// shows once as a send).
pub fn history(rpc: &Rpc, wallet: &str, count: u32) -> Result<Vec<HistoryEntry>> {
    let rows = rpc.wallet_call(
        wallet,
        "listtransactions",
        json!(["*", count.min(1000), 0, true]),
    )?;
    let rows = rows.as_array().cloned().unwrap_or_default();

    let mut out: Vec<HistoryEntry> = Vec::new();
    for r in rows.iter().rev() {
        let txid = r.get("txid").and_then(Value::as_str).unwrap_or_default().to_string();
        if txid.is_empty() {
            continue;
        }
        let btc = r.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
        let fee = r.get("fee").and_then(Value::as_f64).unwrap_or(0.0);
        let cat = r.get("category").and_then(Value::as_str).unwrap_or("");
        let direction = match cat {
            "send" => "send",
            _ => "receive",
        }
        .to_string();
        let entry = HistoryEntry {
            direction,
            amount_sat: (btc * 1e8).round() as i64,
            fee_sat: (fee * 1e8).round() as i64,
            confirmations: r.get("confirmations").and_then(Value::as_i64).unwrap_or(0),
            time: r.get("time").and_then(Value::as_i64).unwrap_or(0),
            address: r.get("address").and_then(Value::as_str).map(str::to_string),
            txid: txid.clone(),
        };
        match out.iter_mut().find(|e| e.txid == txid) {
            Some(existing) => {
                // combine the send/receive legs of a self-transfer
                existing.amount_sat += entry.amount_sat;
                if entry.fee_sat != 0 {
                    existing.fee_sat = entry.fee_sat;
                }
                if entry.direction == "send" {
                    existing.direction = "send".into();
                }
            }
            None => out.push(entry),
        }
    }
    Ok(out)
}

/// `testmempoolaccept` for one raw transaction; `Err` with the reject reason if the
/// node would not accept it.
pub fn test_accept(rpc: &Rpc, raw_hex: &str) -> Result<()> {
    let res = rpc.call("testmempoolaccept", json!([[raw_hex]]))?;
    let first = res.get(0).cloned().unwrap_or(Value::Null);
    if first.get("allowed").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(());
    }
    let reason = first
        .get("reject-reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    bail!("node would reject the transaction: {reason}")
}

/// `testmempoolaccept` then `sendrawtransaction`. Returns the txid.
pub fn broadcast(rpc: &Rpc, raw_hex: &str) -> Result<Txid> {
    test_accept(rpc, raw_hex)?;
    let sent = rpc.call("sendrawtransaction", json!([raw_hex]))?;
    sent.as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("sendrawtransaction returned {sent}"))
}

/// Pull `(is_change, index)` out of a descriptor string. Handles both the ranged
/// form we import (`wpkh([fp/84h/0h/0h]xpub…/<b>/<i>)#cs`) and the expanded form
/// Core returns in `listunspent` (`wpkh([fp/84h/1h/0h/<b>/<i>]<pubkey>)#cs`).
fn parse_desc_path(desc: &str) -> Option<(bool, u32)> {
    let body = desc.split(')').next()?; // strip `)#checksum`
    let after_origin = body.rsplit(']').next()?; // after the [origin], or the whole body
    let path = if after_origin.contains('/') {
        after_origin // ranged: the /<b>/<i> trails the xpub
    } else {
        body.split('[').nth(1)?.split(']').next()? // expanded: /<b>/<i> is inside [origin]
    };
    let mut parts = path.rsplit('/');
    let index: u32 = parts.next()?.trim_end_matches('h').parse().ok()?;
    let branch: u32 = parts.next()?.trim_end_matches('h').parse().ok()?;
    Some((branch == 1, index))
}

fn get_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("?").to_string()
}

fn get_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::parse_desc_path;

    #[test]
    fn descriptor_path_extraction() {
        // ranged form (what the descriptor import uses)
        assert_eq!(
            parse_desc_path("wpkh([093338f9/84h/0h/0h]xpub6CTfb.../0/5)#abcdefgh"),
            Some((false, 5))
        );
        assert_eq!(
            parse_desc_path("wpkh([093338f9/84h/0h/0h]xpub6CTfb.../1/0)#abcdefgh"),
            Some((true, 0))
        );
        // expanded form (what `listunspent` returns) — indices inside the origin
        assert_eq!(
            parse_desc_path(
                "wpkh([73c5da0a/84h/1h/0h/0/0]02e7ab2537b5d49e970309aae06e9e49f36ce1c9febbd44ec8e0d1cca0b4f9c319)#6lpdnkpv"
            ),
            Some((false, 0))
        );
        assert_eq!(
            parse_desc_path("wpkh([73c5da0a/84h/1h/0h/1/7]03aabb)#cs"),
            Some((true, 7))
        );
        assert_eq!(parse_desc_path("wpkh(xpub.../0/42)"), Some((false, 42)));
        assert_eq!(parse_desc_path("raw(00)#xx"), None);
    }
}
