//! Blockchain.com's Haskoin Store as a *batch* source for BTC address data.
//!
//! `GET /address/{unspent,transactions/full}?addresses=a,b,c,…` answer a whole
//! wallet's gap-limit scan in two requests. `POST /btc/prewarm` (in `main`) feeds
//! the edge a wallet's address set, we fetch it here, reshape each address's
//! slice into the Esplora `/address/{a}/utxo` and `/address/{a}/txs` bodies the
//! client already parses, and the caller drops them in the response cache — so
//! the per-address scan that follows is served entirely from memory.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::{json, Value};

pub struct HaskoinStore {
    base: String,
    key: Option<String>,
    agent: ureq::Agent,
}

/// One address's Esplora-shaped bodies, ready to cache.
pub struct Warmed {
    pub utxo: Vec<u8>,
    pub txs: Vec<u8>,
}

impl HaskoinStore {
    pub fn new(base: &str, key: Option<String>) -> Self {
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(15));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        Self { base: base.trim_end_matches('/').to_string(), key, agent: b.build() }
    }

    fn get(&self, path: &str) -> Result<Value> {
        let mut req = self.agent.get(&format!("{}/{}", self.base, path));
        if let Some(k) = &self.key {
            req = req.set("X-API-Key", k);
        }
        Ok(serde_json::from_str(&req.call()?.into_string()?)?)
    }

    /// Two batch calls → each address's `(utxo, txs)` Esplora JSON.
    pub fn warm(&self, addresses: &[String]) -> Result<HashMap<String, Warmed>> {
        if addresses.is_empty() {
            return Ok(HashMap::new());
        }
        let csv = addresses.join(",");
        let unspent = self.get(&format!("address/unspent?addresses={csv}&limit=1000"))?;
        let history = self.get(&format!("address/transactions/full?addresses={csv}&limit=100"))?;

        let want: HashSet<&str> = addresses.iter().map(String::as_str).collect();
        let mut utxo_by: HashMap<&str, Vec<Value>> = HashMap::new();
        let mut txs_by: HashMap<&str, Vec<Value>> = HashMap::new();

        for u in unspent.as_array().into_iter().flatten() {
            if let Some(a) = u.get("address").and_then(Value::as_str).filter(|a| want.contains(a)) {
                utxo_by.entry(a).or_default().push(esplora_utxo(u));
            }
        }
        for t in history.as_array().into_iter().flatten() {
            let touched: HashSet<&str> = ["inputs", "outputs"]
                .iter()
                .flat_map(|side| t.get(side).and_then(Value::as_array).into_iter().flatten())
                .filter_map(|io| io.get("address").and_then(Value::as_str))
                .filter(|a| want.contains(a))
                .collect();
            if touched.is_empty() {
                continue;
            }
            let e = esplora_tx(t);
            for a in touched {
                txs_by.entry(a).or_default().push(e.clone());
            }
        }

        Ok(addresses
            .iter()
            .map(|a| {
                let utxo = utxo_by.remove(a.as_str()).unwrap_or_default();
                let txs = txs_by.remove(a.as_str()).unwrap_or_default();
                (
                    a.clone(),
                    Warmed {
                        utxo: serde_json::to_vec(&utxo).unwrap_or_else(|_| b"[]".to_vec()),
                        txs: serde_json::to_vec(&txs).unwrap_or_else(|_| b"[]".to_vec()),
                    },
                )
            })
            .collect())
    }
}

/// Confirmed block height, or `None` for a mempool entry (`height` absent or -1).
fn height(v: &Value) -> Option<i64> {
    v.get("block")
        .and_then(|b| b.get("height"))
        .and_then(Value::as_i64)
        .filter(|h| *h >= 0)
}

fn esplora_utxo(u: &Value) -> Value {
    let h = height(u);
    json!({
        "txid": u.get("txid"),
        "vout": u.get("index"),
        "value": u.get("value"),
        "status": { "confirmed": h.is_some(), "block_height": h.unwrap_or(0) },
    })
}

fn esplora_tx(t: &Value) -> Value {
    let h = height(t);
    let map_io = |side: &str, f: fn(&Value) -> Value| -> Vec<Value> {
        t.get(side).and_then(Value::as_array).map(|a| a.iter().map(f).collect()).unwrap_or_default()
    };
    json!({
        "txid": t.get("txid"),
        "fee": t.get("fee"),
        "vin": map_io("inputs", |i| json!({
            "prevout": { "scriptpubkey_address": i.get("address"), "value": i.get("value") }
        })),
        "vout": map_io("outputs", |o| json!({
            "scriptpubkey_address": o.get("address"), "value": o.get("value")
        })),
        "status": { "confirmed": h.is_some(), "block_height": h.unwrap_or(0), "block_time": t.get("time") },
    })
}

#[cfg(test)]
mod tests {
    use super::{esplora_tx, esplora_utxo};
    use serde_json::{json, Value};

    #[test]
    fn utxo_confirmed_and_mempool() {
        let c = esplora_utxo(&json!({ "txid": "a", "index": 2, "value": 500, "block": { "height": 900 } }));
        assert_eq!(c["vout"], 2);
        assert_eq!(c["status"]["confirmed"], true);
        assert_eq!(c["status"]["block_height"], 900);

        let m = esplora_utxo(&json!({ "txid": "b", "index": 0, "value": 1, "block": { "height": -1 } }));
        assert_eq!(m["status"]["confirmed"], false);
        assert_eq!(m["status"]["block_height"], 0);
    }

    #[test]
    fn tx_maps_inputs_outputs_fee_status() {
        let e = esplora_tx(&json!({
            "txid": "t1", "fee": 250, "time": 1788,
            "inputs": [{ "address": "in1", "value": 1000, "coinbase": false }, { "coinbase": true }],
            "outputs": [{ "address": "out1", "value": 700 }, { "address": "out2", "value": 50 }],
            "block": { "height": 42 },
        }));
        assert_eq!(e["fee"], 250);
        assert_eq!(e["vin"][0]["prevout"]["scriptpubkey_address"], "in1");
        assert_eq!(e["vin"][0]["prevout"]["value"], 1000);
        assert_eq!(e["vin"][1]["prevout"]["scriptpubkey_address"], Value::Null);
        assert_eq!(e["vout"][1]["scriptpubkey_address"], "out2");
        assert_eq!(e["status"]["confirmed"], true);
        assert_eq!(e["status"]["block_time"], 1788);
    }
}
