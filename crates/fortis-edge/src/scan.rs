//! `POST /{chain}/scan` — a whole wallet's balance and recent history in one
//! request, instead of two per address.
//!
//! A wallet derives its addresses locally and knows the whole set it wants
//! checked; making it ask about them one at a time (a gap-limit walk of
//! `/address/:a/txs` and `/address/:a/utxo`, hundreds of round trips for a deep
//! wallet) puts the cost — latency on the phone, rate-limit budget here — in
//! the wrong place. The backend already holds (XBT: `fortis-index`) or can
//! batch-fetch (BTC: Haskoin) all of it.
//!
//! Request: `{"addresses": ["bc1q…", …], "history": 50}` — up to
//! [`MAX_ADDRESSES`] addresses, at most [`MAX_HISTORY`] confirmed txs back.
//!
//! Response (identical for both chains):
//! ```text
//! { "tip":    972801,
//!   "used":   ["bc1q…", …],        // appeared in any tx, pending included
//!   "utxos":  [{ "address", "txid", "vout", "value", "status" }, …],
//!   "txs":    [ <Esplora tx>, … ], // pending first, then newest confirmed; each tx once
//!   "failed": [] }                 // addresses that couldn't be checked
//! ```
//! `failed` non-empty means the answer is incomplete; a client must not show
//! a balance built from it as if it were the whole wallet.

use std::collections::HashSet;

use serde_json::{json, Value};

pub const MAX_ADDRESSES: usize = 1000;
pub const DEFAULT_HISTORY: usize = 50;
pub const MAX_HISTORY: usize = 100;

pub struct ScanRequest {
    pub addresses: Vec<String>,
    pub history: usize,
}

impl ScanRequest {
    /// Validate and normalise (drop duplicate addresses, clamp `history`).
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| format!("body must be JSON: {e}"))?;
        let list = v["addresses"].as_array().ok_or("expected {\"addresses\": [...]}")?;
        if list.is_empty() || list.len() > MAX_ADDRESSES {
            return Err(format!("addresses: expected 1 to {MAX_ADDRESSES}"));
        }
        let mut seen = HashSet::new();
        let mut addresses = Vec::with_capacity(list.len());
        for a in list {
            let a = a.as_str().ok_or("addresses must be strings")?;
            if a.is_empty() || a.len() >= 128 {
                return Err("address has an implausible length".into());
            }
            if seen.insert(a) {
                addresses.push(a.to_string());
            }
        }
        let history = v["history"].as_u64().map_or(DEFAULT_HISTORY, |n| n as usize).clamp(1, MAX_HISTORY);
        Ok(Self { addresses, history })
    }

    /// The normalised request, as forwarded to an upstream that serves `/scan` itself.
    pub fn to_body(&self) -> Vec<u8> {
        serde_json::to_vec(&json!({ "addresses": self.addresses, "history": self.history })).unwrap_or_default()
    }
}

pub fn response(tip: u64, used: Vec<String>, utxos: Vec<Value>, txs: Vec<Value>) -> Value {
    json!({ "tip": tip, "used": used, "utxos": utxos, "txs": txs, "failed": Vec::<String>::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dedupes_and_clamps() {
        let r = ScanRequest::parse(br#"{"addresses":["a","b","a"],"history":9999}"#).unwrap();
        assert_eq!(r.addresses, vec!["a", "b"]);
        assert_eq!(r.history, MAX_HISTORY);
        assert_eq!(ScanRequest::parse(br#"{"addresses":["a"]}"#).unwrap().history, DEFAULT_HISTORY);
        assert_eq!(ScanRequest::parse(br#"{"addresses":["a"],"history":0}"#).unwrap().history, 1);
    }

    #[test]
    fn rejects_malformed_or_oversized_requests() {
        assert!(ScanRequest::parse(b"nope").is_err());
        assert!(ScanRequest::parse(br#"{}"#).is_err());
        assert!(ScanRequest::parse(br#"{"addresses":[]}"#).is_err());
        assert!(ScanRequest::parse(br#"{"addresses":[1]}"#).is_err());
        assert!(ScanRequest::parse(br#"{"addresses":[""]}"#).is_err());
        let many: Vec<String> = (0..=MAX_ADDRESSES).map(|i| format!("a{i}")).collect();
        let body = serde_json::to_vec(&json!({ "addresses": many })).unwrap();
        assert!(ScanRequest::parse(&body).is_err());
    }

    #[test]
    fn to_body_round_trips() {
        let r = ScanRequest::parse(br#"{"addresses":["a","b"],"history":7}"#).unwrap();
        let again = ScanRequest::parse(&r.to_body()).unwrap();
        assert_eq!((again.addresses, again.history), (vec!["a".to_string(), "b".to_string()], 7));
    }
}
