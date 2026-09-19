//! Blockchain.com's Haskoin Store as a *batch* source for BTC address data.
//!
//! `GET /address/{unspent,transactions/full}?addresses=a,b,c,…` answer a whole
//! wallet's gap-limit scan in two requests. `POST /btc/prewarm` (in `main`) feeds
//! the edge a wallet's address set, we fetch it here, reshape each address's
//! slice into the Esplora `/address/{a}/utxo` and `/address/{a}/txs` bodies the
//! client already parses, and the caller drops them in the response cache — so
//! the per-address scan that follows is served entirely from memory.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Addresses per Haskoin call — a comma-joined query param, so an unbounded
/// list would trip a URL-length limit (`414`) somewhere on the way.
const ADDRS_PER_CALL: usize = 150;
/// Haskoin's `limit` on `address/transactions/full` for [`HaskoinStore::warm`]
/// is *set-wide*, not per address: a response this size may have been cut
/// off, so it can't be trusted as any single address's full history.
const WARM_TXS_LIMIT: usize = 100;
const WARM_UNSPENT_LIMIT: usize = 1000;
const UNSPENT_PAGE: usize = 1000;

pub struct HaskoinStore {
    base: String,
    key: Option<String>,
    agent: ureq::Agent,
}

/// One address's Esplora-shaped bodies, ready to cache. `None` where the batch
/// response may have been truncated — caching a cut-off list as if it were the
/// address's whole history is worse than a slower per-address fetch.
pub struct Warmed {
    pub utxo: Option<Vec<u8>>,
    pub txs: Option<Vec<u8>>,
}

/// A whole address set's balance and history, Esplora-shaped — the BTC side of
/// `POST /btc/scan` (see `scan.rs`).
pub struct Scanned {
    pub used: Vec<String>,
    pub utxos: Vec<Value>,
    pub txs: Vec<Value>,
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

    /// Two batch calls per chunk → each address's `(utxo, txs)` Esplora JSON.
    /// Chunked at [ADDRS_PER_CALL] addresses per HTTP call (a comma-joined
    /// query param, not a request body — an unbounded single call risks
    /// tripping a URL-length limit somewhere between here and Haskoin once a
    /// caller's guess window gets genuinely wide) rather than one call for
    /// the whole list, so `POST /btc/prewarm` can accept a wallet's *real*
    /// full depth (hundreds of addresses for an actively-used wallet, not
    /// just a guessed-small window) without that risk.
    pub fn warm(&self, addresses: &[String]) -> Result<HashMap<String, Warmed>> {
        if addresses.is_empty() {
            return Ok(HashMap::new());
        }
        let want: HashSet<&str> = addresses.iter().map(String::as_str).collect();
        let mut utxo_by: HashMap<String, Vec<Value>> = HashMap::new();
        let mut txs_by: HashMap<String, Vec<Value>> = HashMap::new();
        let mut truncated_utxo: HashSet<&str> = HashSet::new();
        let mut truncated_txs: HashSet<&str> = HashSet::new();

        for chunk in addresses.chunks(ADDRS_PER_CALL) {
            let csv = chunk.join(",");
            let unspent = self.get(&format!("address/unspent?addresses={csv}&limit={WARM_UNSPENT_LIMIT}"))?;
            let history =
                self.get(&format!("address/transactions/full?addresses={csv}&limit={WARM_TXS_LIMIT}"))?;
            if unspent.as_array().is_some_and(|a| a.len() >= WARM_UNSPENT_LIMIT) {
                truncated_utxo.extend(chunk.iter().map(String::as_str));
            }
            if history.as_array().is_some_and(|a| a.len() >= WARM_TXS_LIMIT) {
                truncated_txs.extend(chunk.iter().map(String::as_str));
            }

            for u in unspent.as_array().into_iter().flatten() {
                if let Some(a) = u.get("address").and_then(Value::as_str).filter(|a| want.contains(a)) {
                    utxo_by.entry(a.to_string()).or_default().push(esplora_utxo(u));
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
                    txs_by.entry(a.to_string()).or_default().push(e.clone());
                }
            }
        }

        Ok(addresses
            .iter()
            .map(|a| {
                let utxo = utxo_by.remove(a.as_str()).unwrap_or_default();
                let txs = txs_by.remove(a.as_str()).unwrap_or_default();
                let body = |v: &Vec<Value>| serde_json::to_vec(v).unwrap_or_else(|_| b"[]".to_vec());
                (
                    a.clone(),
                    Warmed {
                        utxo: (!truncated_utxo.contains(a.as_str())).then(|| body(&utxo)),
                        txs: (!truncated_txs.contains(a.as_str())).then(|| body(&txs)),
                    },
                )
            })
            .collect())
    }

    /// Balance and recent history for a whole address set: `address/balances`
    /// says which addresses are used and which hold coins (one call per
    /// [`ADDRS_PER_CALL`]), `address/unspent` is then asked only about the ones
    /// that do, and `address/transactions/full` for the newest `history` txs
    /// across the used ones. Nothing here is per-address, so a several-hundred
    /// address wallet is a handful of calls, and nothing is silently cut off:
    /// a missing balance row or a malformed reply is an error, never an
    /// address quietly treated as empty.
    pub fn scan(&self, addresses: &[String], history: usize) -> Result<Scanned> {
        let mut used = Vec::new();
        let mut funded = Vec::new();
        for chunk in addresses.chunks(ADDRS_PER_CALL) {
            let reply = self.get(&format!("address/balances?addresses={}", chunk.join(",")))?;
            let rows = reply.as_array().ok_or_else(|| anyhow!("haskoin balances: expected an array"))?;
            let (u, f) = classify_balances(chunk, rows)?;
            used.extend(u);
            funded.extend(f);
        }

        let mut utxos = Vec::new();
        for chunk in funded.chunks(ADDRS_PER_CALL) {
            let csv = chunk.join(",");
            let mut offset = 0;
            loop {
                let reply =
                    self.get(&format!("address/unspent?addresses={csv}&limit={UNSPENT_PAGE}&offset={offset}"))?;
                let rows = reply.as_array().ok_or_else(|| anyhow!("haskoin unspent: expected an array"))?;
                utxos.extend(rows.iter().map(esplora_utxo_for_address));
                if rows.len() < UNSPENT_PAGE {
                    break;
                }
                offset += UNSPENT_PAGE;
            }
        }

        let mut txs = Vec::new();
        for chunk in used.chunks(ADDRS_PER_CALL) {
            let reply = self
                .get(&format!("address/transactions/full?addresses={}&limit={history}", chunk.join(",")))?;
            let rows = reply.as_array().ok_or_else(|| anyhow!("haskoin transactions: expected an array"))?;
            txs.extend(rows.iter().map(esplora_tx));
        }
        Ok(Scanned { used, utxos, txs: merge_history(txs, history) })
    }
}

/// `(used, funded)` from an `address/balances` reply, in `requested` order.
/// Errors if any requested address has no row: treating a missing row as
/// "unused" would understate a wallet with no sign anything was wrong.
fn classify_balances(requested: &[String], rows: &[Value]) -> Result<(Vec<String>, Vec<String>)> {
    let by_addr: HashMap<&str, &Value> =
        rows.iter().filter_map(|r| Some((r.get("address")?.as_str()?, r))).collect();
    let (mut used, mut funded) = (Vec::new(), Vec::new());
    for a in requested {
        let r = by_addr.get(a.as_str()).ok_or_else(|| anyhow!("haskoin returned no balance row for {a}"))?;
        let n = |k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
        let has_coins = n("utxo") > 0 || n("unconfirmed") != 0;
        if n("txs") > 0 || has_coins {
            used.push(a.clone());
        }
        if has_coins {
            funded.push(a.clone());
        }
    }
    Ok((used, funded))
}

/// Each tx once; unconfirmed first, then confirmed newest-first, the confirmed
/// list cut to `history`. Each `address/transactions/full` call returns its
/// own newest `history` for one chunk of addresses, so the newest overall are
/// among the union — nothing needed is lost by capping per call first.
fn merge_history(txs: Vec<Value>, history: usize) -> Vec<Value> {
    let mut seen = HashSet::new();
    let (mut pending, mut confirmed): (Vec<Value>, Vec<Value>) = txs
        .into_iter()
        .filter(|t| seen.insert(t["txid"].as_str().unwrap_or_default().to_string()))
        .partition(|t| t["status"]["confirmed"] != true);
    confirmed.sort_by(|a, b| {
        b["status"]["block_height"]
            .as_i64()
            .cmp(&a["status"]["block_height"].as_i64())
            .then_with(|| a["txid"].as_str().cmp(&b["txid"].as_str()))
    });
    confirmed.truncate(history);
    pending.extend(confirmed);
    pending
}

fn esplora_utxo_for_address(u: &Value) -> Value {
    let mut e = esplora_utxo(u);
    e["address"] = u.get("address").cloned().unwrap_or(Value::Null);
    e
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
    use super::{classify_balances, esplora_tx, esplora_utxo, esplora_utxo_for_address, merge_history};
    use serde_json::{json, Value};

    fn bal(addr: &str, txs: i64, utxo: i64, unconfirmed: i64) -> Value {
        json!({ "address": addr, "txs": txs, "utxo": utxo, "unconfirmed": unconfirmed, "confirmed": 0, "received": 0 })
    }

    #[test]
    fn balances_classify_used_and_funded() {
        let asked: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let rows = [bal("a", 0, 0, 0), bal("b", 3, 0, 0), bal("c", 2, 1, 0), bal("d", 0, 0, 500)];
        let (used, funded) = classify_balances(&asked, &rows).unwrap();
        assert_eq!(used, vec!["b", "c", "d"]); // a: never seen; d: only a pending deposit
        assert_eq!(funded, vec!["c", "d"]);
    }

    #[test]
    fn a_missing_balance_row_is_an_error_not_an_unused_address() {
        let asked = vec!["a".to_string(), "b".to_string()];
        assert!(classify_balances(&asked, &[bal("a", 1, 0, 0)]).is_err());
    }

    fn etx(txid: &str, confirmed: bool, height: i64) -> Value {
        json!({ "txid": txid, "status": { "confirmed": confirmed, "block_height": height } })
    }

    #[test]
    fn history_merges_pending_first_then_newest_confirmed_each_once() {
        let merged = merge_history(
            vec![
                etx("old", true, 100),
                etx("new", true, 300),
                etx("pend", false, 0),
                etx("mid", true, 200),
                etx("new", true, 300), // the same tx seen via a second chunk
            ],
            2,
        );
        let ids: Vec<&str> = merged.iter().map(|t| t["txid"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["pend", "new", "mid"]);
    }

    #[test]
    fn utxo_for_address_carries_the_owning_address() {
        let e = esplora_utxo_for_address(
            &json!({ "address": "bc1qa", "txid": "t", "index": 1, "value": 9, "block": { "height": 5 } }),
        );
        assert_eq!((e["address"].as_str(), e["vout"].as_u64(), e["value"].as_u64()), (Some("bc1qa"), Some(1), Some(9)));
    }

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
