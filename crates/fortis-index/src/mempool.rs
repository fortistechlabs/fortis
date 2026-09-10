//! An in-memory overlay of the node's mempool, refreshed on the same cadence as
//! the block sync. It carries unconfirmed outputs (funds received but not yet
//! mined) and unconfirmed spends (so a coin the wallet already spent in a pending
//! tx is not offered again for selection).
//!
//! Nothing here is written to SQLite — the confirmed index stays clean and the
//! overlay is just rebuilt each poll from `getrawmempool` + `getrawtransaction
//! <txid> 2` (verbosity 2, which includes `prevout` for every input).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use fortis_node::Rpc;

fn outpoint(txid: &str, vout: u32) -> String {
    format!("{txid}:{vout}")
}

fn to_sat(v: &Value) -> u64 {
    (v.as_f64().unwrap_or(0.0) * 1e8).round().max(0.0) as u64
}

#[derive(Default, Clone)]
pub struct Mempool {
    /// txid → its `getrawtransaction <txid> 2` JSON.
    txs: HashMap<String, Value>,
    /// `"txid:vout"` outpoints spent by some mempool tx.
    spent: HashSet<String>,
    /// spk hex → outputs the mempool creates for it: `(txid, vout, value_sat)`.
    outputs: HashMap<String, Vec<(String, u32, u64)>>,
    /// `"txid:vout"` → `(spk hex, value_sat)` for every output the mempool creates
    /// — backfills the prevout of a mempool-to-mempool spend.
    by_outpoint: HashMap<String, (String, u64)>,
    /// spk hex → txids that fund or spend it (for `/address/:a/txs`).
    txids: HashMap<String, Vec<String>>,
}

impl Mempool {
    pub fn len(&self) -> usize {
        self.txs.len()
    }

    /// Pull the current mempool, keeping already-decoded txs and fetching only
    /// new ones, then rebuild the derived lookups.
    pub fn refresh(&mut self, rpc: &Rpc) -> Result<()> {
        let ids: Vec<String> = rpc
            .call("getrawmempool", json!([]))?
            .as_array()
            .context("getrawmempool")?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let live: HashSet<&str> = ids.iter().map(String::as_str).collect();

        self.txs.retain(|k, _| live.contains(k.as_str()));
        for id in &ids {
            if self.txs.contains_key(id) {
                continue;
            }
            // A tx can be mined or replaced between the list and the fetch — skip it.
            if let Ok(t) = rpc.call("getrawtransaction", json!([id, 2])) {
                if t.is_object() {
                    self.txs.insert(id.clone(), t);
                }
            }
        }
        self.reindex();
        Ok(())
    }

    fn reindex(&mut self) {
        self.spent.clear();
        self.outputs.clear();
        self.by_outpoint.clear();
        self.txids.clear();

        for (txid, t) in &self.txs {
            for i in t["vin"].as_array().into_iter().flatten() {
                if let (Some(pt), Some(pv)) = (i["txid"].as_str(), i["vout"].as_u64()) {
                    self.spent.insert(outpoint(pt, pv as u32));
                }
                if let Some(spk) = i["prevout"]["scriptPubKey"]["hex"].as_str() {
                    self.txids.entry(spk.to_string()).or_default().push(txid.clone());
                }
            }
            for (n, o) in t["vout"].as_array().into_iter().flatten().enumerate() {
                let Some(spk) = o["scriptPubKey"]["hex"].as_str() else { continue };
                let value = to_sat(&o["value"]);
                self.outputs
                    .entry(spk.to_string())
                    .or_default()
                    .push((txid.clone(), n as u32, value));
                self.by_outpoint
                    .insert(outpoint(txid, n as u32), (spk.to_string(), value));
                self.txids.entry(spk.to_string()).or_default().push(txid.clone());
            }
        }
        for v in self.txids.values_mut() {
            v.sort();
            v.dedup();
        }
    }

    pub fn is_spent(&self, txid: &str, vout: u32) -> bool {
        self.spent.contains(&outpoint(txid, vout))
    }

    /// `(spk hex, value_sat)` for an output some mempool tx created.
    pub fn output_at(&self, txid: &str, vout: u32) -> Option<(String, u64)> {
        self.by_outpoint.get(&outpoint(txid, vout)).cloned()
    }

    /// Unconfirmed outputs for `spk` that aren't themselves already spent by
    /// another mempool tx: `(txid, vout, value_sat)`.
    pub fn utxos_for<'a>(&'a self, spk: &str) -> impl Iterator<Item = &'a (String, u32, u64)> {
        let spent = &self.spent;
        self.outputs
            .get(spk)
            .into_iter()
            .flatten()
            .filter(move |(t, v, _)| !spent.contains(&outpoint(t, *v)))
    }

    /// The `getrawtransaction <txid> 2` JSON for every mempool tx touching `spk`.
    pub fn txs_for(&self, spk: &str) -> Vec<&Value> {
        self.txids
            .get(spk)
            .into_iter()
            .flatten()
            .filter_map(|id| self.txs.get(id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `getrawtransaction <txid> 2`-shaped object.
    fn raw(txid: &str, vin: Value, vout: Value) -> Value {
        json!({ "txid": txid, "vin": vin, "vout": vout, "fee": 0.0 })
    }
    fn vout_to(spk: &str, btc: f64) -> Value {
        json!({ "value": btc, "scriptPubKey": { "hex": spk } })
    }
    fn vin_from(txid: &str, vout: u32, prev_spk: &str) -> Value {
        json!({ "txid": txid, "vout": vout, "prevout": { "scriptPubKey": { "hex": prev_spk } } })
    }

    fn indexed(txs: &[(&str, Value)]) -> Mempool {
        let mut m = Mempool::default();
        for (id, t) in txs {
            m.txs.insert((*id).into(), t.clone());
        }
        m.reindex();
        m
    }

    #[test]
    fn exposes_unconfirmed_outputs() {
        let m = indexed(&[("t1", raw("t1", json!([]), json!([vout_to("spkA", 1.0)])))]);
        let us: Vec<_> = m.utxos_for("spkA").collect();
        assert_eq!(us.len(), 1);
        assert_eq!(us[0], &("t1".to_string(), 0, 100_000_000));
        assert!(m.txs_for("spkA").len() == 1);
    }

    #[test]
    fn marks_a_confirmed_coin_as_spent() {
        // t2 spends confirmed output cc:0 (which pays spkA)
        let m = indexed(&[(
            "t2",
            raw("t2", json!([vin_from("cc", 0, "spkA")]), json!([vout_to("spkB", 0.9)])),
        )]);
        assert!(m.is_spent("cc", 0));
        // spkA's mempool history includes the spend
        assert_eq!(m.txs_for("spkA").len(), 1);
    }

    #[test]
    fn hides_a_mempool_output_already_spent_by_another_mempool_tx() {
        let m = indexed(&[
            ("t1", raw("t1", json!([]), json!([vout_to("spkA", 1.0)]))),
            ("t2", raw("t2", json!([vin_from("t1", 0, "spkA")]), json!([vout_to("spkC", 0.9)]))),
        ]);
        assert!(m.utxos_for("spkA").next().is_none()); // t1:0 is spent by t2
        assert_eq!(m.utxos_for("spkC").count(), 1);
    }

    #[test]
    fn output_at_resolves_an_outpoint_to_its_spk_and_value() {
        let m = indexed(&[("t1", raw("t1", json!([]), json!([vout_to("spkA", 1.0), vout_to("spkB", 0.5)])))]);
        assert_eq!(m.output_at("t1", 0), Some(("spkA".to_string(), 100_000_000)));
        assert_eq!(m.output_at("t1", 1), Some(("spkB".to_string(), 50_000_000)));
        assert_eq!(m.output_at("t1", 2), None);
        assert_eq!(m.output_at("nope", 0), None);
    }

    #[test]
    fn refresh_drops_txs_that_left_the_mempool() {
        let mut m = indexed(&[("t1", raw("t1", json!([]), json!([vout_to("spkA", 1.0)])))]);
        assert_eq!(m.len(), 1);
        m.txs.retain(|k, _| k == "nope"); // simulate `refresh` seeing an empty mempool
        m.reindex();
        assert!(m.utxos_for("spkA").next().is_none());
    }
}
