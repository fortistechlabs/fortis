//! JSON DTOs for the JS boundary. Explicit shapes so the TS contract is stable and
//! doesn't depend on rust-bitcoin's serde representations.

use serde::{Deserialize, Serialize};
use wallet_core::bitcoin::{Address, Amount, Network, OutPoint, ScriptBuf};
use wallet_core::{FundingPlan, PsbtReview, Utxo};

#[derive(Deserialize)]
pub struct JsUtxo {
    pub txid: String,
    pub vout: u32,
    pub value_sat: u64,
    pub script_pubkey_hex: String,
    pub confirmations: u32,
    #[serde(default)]
    pub derivation_index: u32,
    #[serde(default)]
    pub is_change: bool,
}

impl JsUtxo {
    pub fn to_core(&self) -> Result<Utxo, String> {
        Ok(Utxo {
            outpoint: OutPoint {
                txid: self.txid.parse().map_err(|_| format!("bad txid {}", self.txid))?,
                vout: self.vout,
            },
            value: Amount::from_sat(self.value_sat),
            script_pubkey: ScriptBuf::from(
                hex::decode(&self.script_pubkey_hex).map_err(|e| e.to_string())?,
            ),
            confirmations: self.confirmations,
            derivation_index: self.derivation_index,
            is_change: self.is_change,
        })
    }
}

#[derive(Serialize)]
pub struct JsSelectedInput {
    pub txid: String,
    pub vout: u32,
    pub value_sat: u64,
    pub script_pubkey_hex: String,
    pub derivation_index: u32,
    pub is_change: bool,
}

#[derive(Serialize)]
pub struct JsFundingPlan {
    pub tx_hex: String,
    pub fee_sat: u64,
    pub change_sat: Option<u64>,
    pub service_fee_sat: Option<u64>,
    pub selected: Vec<JsSelectedInput>,
    /// This plan as a PSBT, base64 — set by the caller (`lib.rs`'s
    /// `plan_payment`/`plan_sweep`) only when a master fingerprint is on file
    /// for this session, never by `from_core` itself. `None` either way must
    /// never fail the surrounding call — see the field's doc on
    /// `wallet_core::FundingPlan`.
    pub psbt_base64: Option<String>,
}

impl JsFundingPlan {
    pub fn from_core(p: &FundingPlan) -> Self {
        Self {
            tx_hex: hex::encode(wallet_core::bitcoin::consensus::serialize(&p.tx)),
            fee_sat: p.fee.to_sat(),
            change_sat: p.change.map(|c| c.to_sat()),
            service_fee_sat: p.service_fee.map(|c| c.to_sat()),
            selected: p
                .selected
                .iter()
                .map(|u| JsSelectedInput {
                    txid: u.outpoint.txid.to_string(),
                    vout: u.outpoint.vout,
                    value_sat: u.value.to_sat(),
                    script_pubkey_hex: hex::encode(u.script_pubkey.as_bytes()),
                    derivation_index: u.derivation_index,
                    is_change: u.is_change,
                })
                .collect(),
            psbt_base64: None,
        }
    }
}

#[derive(Serialize)]
pub struct JsPsbtDestination {
    pub script_pubkey_hex: String,
    /// `None` only for a script this wallet doesn't know how to render as an
    /// address (never expected in practice — this wallet only ever builds
    /// standard P2WPKH/OP_RETURN outputs) — the client falls back to the hex.
    pub address: Option<String>,
    pub amount_sat: u64,
}

#[derive(Serialize)]
pub struct JsPsbtReview {
    pub fee_sat: u64,
    pub change_sat: Option<u64>,
    pub destinations: Vec<JsPsbtDestination>,
    pub op_return_hex: Option<String>,
    pub selected: Vec<JsSelectedInput>,
}

impl JsPsbtReview {
    pub fn from_core(r: &PsbtReview, network: Network) -> Self {
        Self {
            fee_sat: r.fee.to_sat(),
            change_sat: r.change.map(|c| c.to_sat()),
            destinations: r
                .destinations
                .iter()
                .map(|d| JsPsbtDestination {
                    script_pubkey_hex: hex::encode(d.script_pubkey.as_bytes()),
                    address: Address::from_script(&d.script_pubkey, network).ok().map(|a| a.to_string()),
                    amount_sat: d.value.to_sat(),
                })
                .collect(),
            op_return_hex: r.op_return.as_ref().map(hex::encode),
            selected: r
                .selected
                .iter()
                .map(|u| JsSelectedInput {
                    txid: u.outpoint.txid.to_string(),
                    vout: u.outpoint.vout,
                    value_sat: u.value.to_sat(),
                    script_pubkey_hex: hex::encode(u.script_pubkey.as_bytes()),
                    derivation_index: u.derivation_index,
                    is_change: u.is_change,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
pub struct JsPayTo {
    pub address: String,
    pub amount_sat: u64,
}

/// A hosted backend's pricing, as reported in `/v1/status`. Self-hosted backends
/// don't send one, and no fee is charged.
#[derive(Deserialize)]
pub struct JsServiceFee {
    pub address: String,
    pub bps: u32,
    #[serde(default)]
    pub floor_sat: u64,
    #[serde(default)]
    pub cap_sat: u64,
}

#[derive(Deserialize)]
pub struct JsSpentInput {
    pub value_sat: u64,
    pub script_pubkey_hex: String,
    pub derivation_index: u32,
    #[serde(default)]
    pub is_change: bool,
}

#[derive(Serialize)]
pub struct JsFoundOutput {
    pub vout: u32,
    pub value_sat: u64,
}

#[derive(Deserialize)]
pub struct JsSwapParams {
    pub swap_id_hex: String,
    /// `"mainnet"` or `"regtest"`; empty keeps the current `setNetwork` value.
    #[serde(default)]
    pub network: String,
    /// `"initiator"` or `"participant"`.
    pub role: String,
    /// `"btc"` or `"xbt"` — the chain we send on.
    pub send_chain: String,
    /// The chain we receive on.
    pub recv_chain: String,
    pub send_amount_sat: u64,
    pub recv_amount_sat: u64,
    pub hashlock_hex: String,
    pub our_pubkey_send_hex: String,
    pub their_pubkey_send_hex: String,
    pub our_pubkey_recv_hex: String,
    pub their_pubkey_recv_hex: String,
    /// Absolute CLTV (unix seconds) on our contract.
    pub our_contract_locktime: u32,
    /// Absolute CLTV we require on the counterparty's contract.
    pub their_contract_locktime: u32,
    pub required_incoming_confs: u32,
}

#[derive(Serialize)]
pub struct JsAddress {
    pub address: String,
    pub script_pubkey_hex: String,
}

#[derive(Serialize)]
pub struct JsIndices {
    pub next_receive: u32,
    pub next_change: u32,
}

#[derive(Serialize)]
pub struct JsOutPoint {
    pub txid: String,
    pub vout: u32,
}

#[derive(Serialize)]
pub struct JsUnsignedSpend {
    pub tx_hex: String,
    pub sighash_hex: String,
}
