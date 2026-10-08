//! Esplora-shaped transaction JSON. Confirmed transactions are rendered once
//! from the node and cached in the DB (`render` CF); pending ones are cached
//! in memory while they stay in the mempool.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::hashes::{hash160, Hash};
use bitcoin::{Address, BlockHash, Network, OutPoint, ScriptBuf, Txid, WPubkeyHash};
use serde_json::{json, Value};

use crate::chainstate::View;
use crate::db::Db;
use crate::keys::{Program, TxNum};
use crate::mempool::MempoolView;
use crate::source::RpcSource;

pub trait TxSource: Send + Sync {
    /// `getrawtransaction <txid> 2 [blockhash]`.
    fn tx_verbose(&self, txid: &Txid, block: Option<&BlockHash>) -> Result<Value>;
}

impl TxSource for RpcSource {
    fn tx_verbose(&self, txid: &Txid, block: Option<&BlockHash>) -> Result<Value> {
        let params = match block {
            Some(b) => json!([txid.to_string(), 2, b.to_string()]),
            None => json!([txid.to_string(), 2]),
        };
        self.call("getrawtransaction", params)
            .with_context(|| format!("getrawtransaction {txid}"))
    }
}

const PENDING_CACHE_MAX: usize = 20_000;
/// Concurrent node RPCs across all requests — well under the node's default
/// RPC work queue so the syncer's own calls are never starved.
const RPC_SLOTS: usize = 8;

struct Slots {
    free: Mutex<usize>,
    cv: Condvar,
}

/// How long a request waits for a free node-RPC slot before giving up.
const SLOT_WAIT: Duration = Duration::from_secs(30);

impl Slots {
    /// Run `f` holding one slot; an error if none frees up within `wait`.
    fn run<T>(&self, wait: Duration, f: impl FnOnce() -> T) -> Result<T> {
        let deadline = Instant::now() + wait;
        let mut n = self.free.lock().unwrap();
        while *n == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("node RPC busy: no free slot within {wait:?}");
            }
            n = self.cv.wait_timeout(n, left).unwrap().0;
        }
        *n -= 1;
        drop(n);
        let out = f();
        *self.free.lock().unwrap() += 1;
        self.cv.notify_one();
        Ok(out)
    }
}

/// `f(i)` for every `i < n`, on at most `RPC_SLOTS` threads, each call
/// holding a node-RPC slot. Results in index order.
fn fan_out(
    slots: &Slots,
    n: usize,
    f: impl Fn(usize) -> Result<Value> + Sync,
) -> Vec<Result<Value>> {
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<Result<Value>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..n.min(RPC_SLOTS) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= n {
                    return;
                }
                let r = slots.run(SLOT_WAIT, || f(i)).and_then(|r| r);
                *out[i].lock().unwrap() = Some(r);
            });
        }
    });
    out.into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap()
                .unwrap_or_else(|| Err(anyhow!("render worker panicked")))
        })
        .collect()
}

pub struct Renderer {
    src: Arc<dyn TxSource>,
    db: Db,
    network: Network,
    slots: Slots,
    pending: Mutex<HashMap<Txid, Value>>,
}

impl Renderer {
    pub fn new(src: Arc<dyn TxSource>, db: Db, network: Network) -> Self {
        Renderer {
            src,
            db,
            network,
            slots: Slots {
                free: Mutex::new(RPC_SLOTS),
                cv: Condvar::new(),
            },
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Esplora JSON for confirmed txnums, in order. Cached renders come from
    /// the DB; misses are fetched in parallel (bounded by the shared RPC
    /// slots) and cached. Any failure fails the whole call.
    pub fn confirmed(&self, v: &View, ns: &[TxNum]) -> Result<Vec<Value>> {
        let mut rows = Vec::with_capacity(ns.len());
        for &n in ns {
            let txid = v
                .reader
                .txid(n)?
                .ok_or_else(|| anyhow!("txnum {n} has no txid"))?;
            let h = v
                .chain
                .height_of(n)
                .ok_or_else(|| anyhow!("txnum {n} is in no block"))?;
            let rec = *v.chain.get(h).expect("height_of returns a known height");
            let status = json!({ "confirmed": true, "block_height": h, "block_time": rec.time });
            // The cached value starts with its txid, so a row left by a
            // rolled-back chain can never be served for a reassigned txnum.
            let cached = v.reader.render_get(n)?.and_then(|b| {
                let (id, body) = b.split_at_checked(32)?;
                (id == txid.to_byte_array())
                    .then(|| serde_json::from_slice::<Value>(body).ok())
                    .flatten()
            });
            rows.push((n, txid, rec.hash, status, cached));
        }
        let misses: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].4.is_none()).collect();
        let fetched = fan_out(&self.slots, misses.len(), |k| {
            let (_, txid, block, _, _) = &rows[misses[k]];
            self.src.tx_verbose(txid, Some(block))
        });
        for (&i, r) in misses.iter().zip(fetched) {
            let (n, txid, ..) = rows[i];
            let tx = esplora_tx(&r?, Value::Null);
            let mut buf = txid.to_byte_array().to_vec();
            buf.extend_from_slice(&serde_json::to_vec(&tx)?);
            self.db.render_put(n, &buf)?;
            rows[i].4 = Some(tx);
        }
        Ok(rows
            .into_iter()
            .map(|(_, _, _, status, tx)| {
                let mut tx = tx.expect("every row filled or an error returned");
                tx["status"] = status;
                tx
            })
            .collect())
    }

    /// Esplora JSON for mempool txs, with missing input prevouts back-filled
    /// from the mempool and confirmed UTXOs. A tx that left the mempool (or
    /// was mined) since the snapshot is skipped.
    pub fn pending(&self, v: &View, mp: &MempoolView, ids: &[Txid]) -> Result<Vec<Value>> {
        let mut out: Vec<Option<Value>> = {
            let cache = self.pending.lock().unwrap();
            ids.iter().map(|id| cache.get(id).cloned()).collect()
        };
        let misses: Vec<usize> = (0..ids.len()).filter(|&i| out[i].is_none()).collect();
        let fetched = fan_out(&self.slots, misses.len(), |k| {
            self.src.tx_verbose(&ids[misses[k]], None)
        });
        for (&i, r) in misses.iter().zip(fetched) {
            match r {
                Ok(t) => {
                    let mut t = backfill_prevouts(&t, v, mp, self.network);
                    fill_fee(&mut t);
                    let tx = esplora_tx(&t, json!({ "confirmed": false }));
                    let mut cache = self.pending.lock().unwrap();
                    if cache.len() >= PENDING_CACHE_MAX {
                        cache.clear();
                    }
                    cache.insert(ids[i], tx.clone());
                    out[i] = Some(tx);
                }
                Err(e) => eprintln!("api: pending tx {} skipped: {e:#}", ids[i]),
            }
        }
        Ok(out.into_iter().flatten().collect())
    }

    /// Drop cached pending renders for txs no longer in the mempool.
    pub fn forget_pending_not_in(&self, mp: &MempoolView) {
        self.pending.lock().unwrap().retain(|id, _| mp.contains(id));
    }
}

/// The program a verbose-JSON input spends if it looks like a P2WPKH spend
/// (empty scriptSig, two witness items): HASH160 of the second item.
fn spend_program_json(vin: &Value) -> Option<Program> {
    let sig_empty = vin["scriptSig"]["hex"].as_str().is_none_or(str::is_empty);
    let w = vin["txinwitness"].as_array()?;
    if !sig_empty || w.len() != 2 {
        return None;
    }
    let pk = hex::decode(w[1].as_str()?).ok()?;
    Some(hash160::Hash::hash(&pk).to_byte_array())
}

fn program_address(p: &Program, network: Network) -> String {
    let spk = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(*p));
    Address::from_script(&spk, network)
        .expect("p2wpkh is a standard script")
        .to_string()
}

/// Some Knots/BLAKE2b nodes omit `vin[].prevout` for a mempool transaction's
/// inputs (`getrawtransaction <txid> 2` only fills it once mined). Without it
/// the client can't tell that a pending tx spends the wallet's own coins, so
/// an outgoing payment shows as an incoming receive of its change. Fill any
/// missing prevout from the mempool overlay or the confirmed UTXO set.
pub(crate) fn backfill_prevouts(t: &Value, v: &View, mp: &MempoolView, network: Network) -> Value {
    let mut t = t.clone();
    let Some(vins) = t.get_mut("vin").and_then(Value::as_array_mut) else {
        return t;
    };
    for i in vins {
        if i.get("prevout").is_some_and(|p| !p.is_null()) {
            continue;
        }
        let (Some(ptxid), Some(pvout)) = (
            i["txid"].as_str().and_then(|s| s.parse::<Txid>().ok()),
            i["vout"].as_u64(),
        ) else {
            continue;
        };
        let op = OutPoint {
            txid: ptxid,
            vout: pvout as u32,
        };
        let found = mp.output_at(&op).or_else(|| {
            let p = spend_program_json(i)?;
            v.reader.utxo(&p, &op).ok().flatten().map(|u| (p, u.value))
        });
        let Some((p, value_sat)) = found else {
            continue;
        };
        i["prevout"] = json!({
            "scriptPubKey": { "address": program_address(&p, network) },
            "value": value_sat as f64 / 1e8,
        });
    }
    t
}

/// Mempool txs come from the node without `fee` (it is only known once the
/// prevouts are). When every input's prevout is present — after
/// [`backfill_prevouts`] — derive it; otherwise leave it unknown.
pub(crate) fn fill_fee(t: &mut Value) {
    if t.get("fee").is_some_and(|f| !f.is_null()) {
        return;
    }
    let ins: Option<u64> = t["vin"].as_array().and_then(|vin| {
        vin.iter()
            .map(|i| {
                i.get("prevout")
                    .filter(|p| !p.is_null())
                    .map(|p| btc_to_sat(&p["value"]))
            })
            .sum()
    });
    let outs: u64 = t["vout"]
        .as_array()
        .map_or(0, |v| v.iter().map(|o| btc_to_sat(&o["value"])).sum());
    if let Some(fee) = ins.and_then(|i| i.checked_sub(outs)) {
        t["fee"] = json!(fee as f64 / 1e8);
    }
}

/// The Esplora shape of a verbose node transaction, with `status` as given.
pub fn esplora_tx(t: &Value, status: Value) -> Value {
    let vin: Vec<Value> = t["vin"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| match i.get("prevout") {
                    Some(p) if !p.is_null() => json!({
                        "prevout": {
                            "scriptpubkey_address": p["scriptPubKey"]["address"],
                            "value": btc_to_sat(&p["value"]),
                        }
                    }),
                    _ => json!({ "prevout": Value::Null }),
                })
                .collect()
        })
        .unwrap_or_default();
    let vout: Vec<Value> = t["vout"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|o| {
                    json!({
                        "scriptpubkey_address": o["scriptPubKey"]["address"],
                        "value": btc_to_sat(&o["value"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "txid": t["txid"],
        "vin": vin,
        "vout": vout,
        "fee": t.get("fee").map(btc_to_sat).unwrap_or(0),
        "status": status,
    })
}

fn btc_to_sat(v: &Value) -> u64 {
    (v.as_f64().unwrap_or(0.0) * 1e8).round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::{view, BlockTable, SharedChain};
    use crate::db::{DbConfig, Durability};
    use crate::extract::{Funded, ParsedBlock, TxRows};
    use arc_swap::ArcSwap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // BIP-173 P2WPKH example program.
    const P2WPKH_SPK: &str = "0014751e76e8199196d454941c45d1b3a323f1433bd6";
    const P2WPKH_ADDR: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    fn spk_hex_to_address(spk_hex: &str, network: Network) -> Option<String> {
        let spk = ScriptBuf::from(hex::decode(spk_hex).ok()?);
        Address::from_script(&spk, network)
            .ok()
            .map(|a| a.to_string())
    }

    #[derive(Default)]
    struct Fake {
        calls: AtomicUsize,
        jitter: bool,
        fail: bool,
    }

    impl TxSource for Fake {
        fn tx_verbose(&self, txid: &Txid, block: Option<&BlockHash>) -> Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.jitter {
                std::thread::sleep(Duration::from_micros(
                    txid.to_byte_array()[0] as u64 * 12 % 3000,
                ));
            }
            if self.fail {
                return Err(anyhow!("node down"));
            }
            Ok(json!({
                "txid": txid.to_string(),
                "blockhash": block.map(|b| b.to_string()),
                "blocktime": 1,
                "vin": [],
                "vout": [{ "value": 0.0001, "scriptPubKey": { "address": "bc1qdest" } }],
            }))
        }
    }

    fn txid(n: u8) -> Txid {
        Txid::from_byte_array([n; 32])
    }

    struct Rig {
        _dir: tempfile::TempDir,
        db: Db,
        chain: SharedChain,
    }

    /// Block 100 (time 7100) holds `n` txs, each funding program [1;20].
    fn rig(n: u8) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        let txs = (0..n)
            .map(|i| TxRows {
                txid: txid(i + 1),
                funded: vec![Funded {
                    program: [1; 20],
                    vout: 0,
                    value: 500_000,
                }],
                spent: vec![],
            })
            .collect();
        db.apply(
            &[ParsedBlock {
                height: 100,
                hash: BlockHash::all_zeros(),
                prev: BlockHash::all_zeros(),
                time: 7100,
                txs,
            }],
            0,
            Durability::Durable,
            false,
        )
        .unwrap();
        let chain = Arc::new(ArcSwap::from_pointee(
            BlockTable::from_db(db.load_blocks().unwrap()).unwrap(),
        ));
        Rig {
            _dir: dir,
            db,
            chain,
        }
    }

    /// Records which threads called it.
    #[derive(Default)]
    struct ThreadCounter(Mutex<std::collections::HashSet<std::thread::ThreadId>>);

    impl TxSource for ThreadCounter {
        fn tx_verbose(&self, txid: &Txid, _: Option<&BlockHash>) -> Result<Value> {
            self.0.lock().unwrap().insert(std::thread::current().id());
            std::thread::sleep(Duration::from_millis(2));
            Ok(json!({ "txid": txid.to_string(), "vin": [], "vout": [] }))
        }
    }

    #[test]
    fn many_misses_use_at_most_rpc_slots_threads() {
        let r = rig(60);
        let src = Arc::new(ThreadCounter::default());
        let renderer = Renderer::new(src.clone(), r.db.clone(), Network::Bitcoin);
        let v = view(&r.db, &r.chain).unwrap();
        let ns: Vec<TxNum> = (0..60).collect();
        assert_eq!(renderer.confirmed(&v, &ns).unwrap().len(), 60);
        assert!(src.0.lock().unwrap().len() <= RPC_SLOTS);
        src.0.lock().unwrap().clear();
        let ids: Vec<Txid> = (100..160u8).map(txid).collect();
        assert_eq!(
            renderer
                .pending(&v, &MempoolView::default(), &ids)
                .unwrap()
                .len(),
            60
        );
        assert!(
            src.0.lock().unwrap().len() <= RPC_SLOTS,
            "{} threads",
            src.0.lock().unwrap().len()
        );
    }

    #[test]
    fn a_saturated_node_is_an_error_after_the_slot_wait_not_a_hang() {
        let slots = Slots {
            free: Mutex::new(0),
            cv: Condvar::new(),
        };
        let t = std::time::Instant::now();
        assert!(slots.run(Duration::from_millis(50), || ()).is_err());
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_pending_fee_is_derived_from_complete_prevouts() {
        let mut t = json!({
            "vin": [
                { "prevout": { "scriptPubKey": { "address": "bc1qa" }, "value": 0.01 } },
                { "prevout": { "scriptPubKey": { "address": "bc1qb" }, "value": 0.00500001 } },
            ],
            "vout": [{ "value": 0.0149, "scriptPubKey": { "address": "bc1qc" } }],
        });
        fill_fee(&mut t);
        assert_eq!(esplora_tx(&t, Value::Null)["fee"], 10_001);
        // An unknown prevout leaves the fee unknown (0), never a wrong number.
        let mut partial = json!({
            "vin": [{ "prevout": { "value": 0.01 } }, { "prevout": null }],
            "vout": [{ "value": 0.005 }],
        });
        fill_fee(&mut partial);
        assert_eq!(esplora_tx(&partial, Value::Null)["fee"], 0);
        // A fee the node gave is kept.
        let mut given =
            json!({ "fee": 0.0002, "vin": [{ "prevout": { "value": 1.0 } }], "vout": [] });
        fill_fee(&mut given);
        assert_eq!(esplora_tx(&given, Value::Null)["fee"], 20_000);
    }

    #[test]
    fn spk_hex_to_address_round_trips_a_p2wpkh() {
        assert_eq!(
            spk_hex_to_address(P2WPKH_SPK, Network::Bitcoin).as_deref(),
            Some(P2WPKH_ADDR)
        );
        assert_eq!(spk_hex_to_address("not-hex", Network::Bitcoin), None);
        assert_eq!(spk_hex_to_address("00", Network::Bitcoin), None); // not a known template
        let p: Program = hex::decode(&P2WPKH_SPK[4..]).unwrap().try_into().unwrap();
        assert_eq!(program_address(&p, Network::Bitcoin), P2WPKH_ADDR);
    }

    #[test]
    fn backfill_fills_a_null_mempool_prevout_from_the_confirmed_index() {
        // Confirmed UTXO txid(1):0 belongs to HASH160(pubkey); a mempool tx
        // spends it with a P2WPKH witness and the node gave no prevout.
        let pubkey = [3u8; 33];
        let program: Program = hash160::Hash::hash(&pubkey).to_byte_array();
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        db.apply(
            &[ParsedBlock {
                height: 100,
                hash: BlockHash::all_zeros(),
                prev: BlockHash::all_zeros(),
                time: 0,
                txs: vec![TxRows {
                    txid: txid(0xaa),
                    funded: vec![Funded {
                        program,
                        vout: 0,
                        value: 500_000,
                    }],
                    spent: vec![],
                }],
            }],
            0,
            Durability::Durable,
            false,
        )
        .unwrap();
        let chain: SharedChain = Arc::new(ArcSwap::from_pointee(
            BlockTable::from_db(db.load_blocks().unwrap()).unwrap(),
        ));
        let v = view(&db, &chain).unwrap();
        let pending = json!({
            "txid": txid(0xbb).to_string(),
            "vin": [{
                "txid": txid(0xaa).to_string(), "vout": 0,
                "scriptSig": { "asm": "", "hex": "" },
                "txinwitness": [ "30".repeat(72), hex::encode(pubkey) ],
            }],
            "vout": [{ "value": 0.004, "scriptPubKey": { "address": "bc1qdest" } }],
        });
        let filled = backfill_prevouts(&pending, &v, &MempoolView::default(), Network::Bitcoin);
        let e = esplora_tx(&filled, json!({ "confirmed": false }));
        assert_eq!(
            e["vin"][0]["prevout"]["scriptpubkey_address"],
            program_address(&program, Network::Bitcoin)
        );
        assert_eq!(e["vin"][0]["prevout"]["value"], 500_000);
        assert_eq!(e["status"]["confirmed"], false);
    }

    #[test]
    fn backfill_leaves_a_prevout_the_node_already_gave_us_untouched() {
        let r = rig(0);
        let v = view(&r.db, &r.chain).unwrap();
        let tx = json!({
            "txid": txid(0xbb).to_string(),
            "vin": [{
                "txid": txid(0xaa).to_string(), "vout": 0,
                "prevout": { "scriptPubKey": { "address": "bc1qkeep" }, "value": 0.001 }
            }],
            "vout": [],
        });
        let filled = backfill_prevouts(&tx, &v, &MempoolView::default(), Network::Bitcoin);
        assert_eq!(
            filled["vin"][0]["prevout"]["scriptPubKey"]["address"],
            "bc1qkeep"
        );
    }

    #[test]
    fn a_failed_fetch_fails_the_whole_call_and_caches_nothing_for_the_failed_row() {
        let r = rig(2);
        let src = Arc::new(Fake {
            fail: true,
            ..Default::default()
        });
        let renderer = Renderer::new(src, r.db.clone(), Network::Bitcoin);
        let v = view(&r.db, &r.chain).unwrap();
        assert!(renderer.confirmed(&v, &[0, 1]).is_err());
        assert_eq!(r.db.reader().render_get(0).unwrap(), None);
    }

    #[test]
    fn confirmed_render_is_cached_across_restarts() {
        let r = rig(1);
        let path = r._dir.path().to_path_buf();
        let src = Arc::new(Fake::default());
        let first = {
            let renderer = Renderer::new(src.clone(), r.db.clone(), Network::Bitcoin);
            let v = view(&r.db, &r.chain).unwrap();
            renderer.confirmed(&v, &[0]).unwrap()
        };
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
        let Rig { _dir, db, chain } = r;
        drop(db);
        let db = Db::open(&path, &DbConfig { cache_mb: 8 }, "btc").unwrap();
        let renderer = Renderer::new(src.clone(), db.clone(), Network::Bitcoin);
        let v = view(&db, &chain).unwrap();
        assert_eq!(renderer.confirmed(&v, &[0]).unwrap(), first);
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
        drop(_dir);
    }

    #[test]
    fn confirmed_keeps_row_order_with_parallel_misses() {
        let r = rig(20);
        let src = Arc::new(Fake {
            jitter: true,
            ..Default::default()
        });
        let renderer = Renderer::new(src.clone(), r.db.clone(), Network::Bitcoin);
        let v = view(&r.db, &r.chain).unwrap();
        let ns: Vec<TxNum> = (0..20).rev().collect();
        let out = renderer.confirmed(&v, &ns).unwrap();
        let got: Vec<String> = out
            .iter()
            .map(|t| t["txid"].as_str().unwrap().to_string())
            .collect();
        let want: Vec<String> = ns.iter().map(|&n| txid(n as u8 + 1).to_string()).collect();
        assert_eq!(got, want);
        assert_eq!(src.calls.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn status_block_time_comes_from_the_block_table() {
        let r = rig(1);
        let renderer = Renderer::new(Arc::new(Fake::default()), r.db.clone(), Network::Bitcoin);
        let v = view(&r.db, &r.chain).unwrap();
        let out = renderer.confirmed(&v, &[0]).unwrap();
        assert_eq!(
            out[0]["status"],
            json!({ "confirmed": true, "block_height": 100, "block_time": 7100 })
        );
    }

    #[test]
    fn a_stale_render_row_for_a_reassigned_txnum_is_a_miss() {
        let r = rig(1);
        let mut stale = txid(0x99).to_byte_array().to_vec();
        stale.extend_from_slice(br#"{"txid":"stale"}"#);
        r.db.render_put(0, &stale).unwrap();
        let src = Arc::new(Fake::default());
        let renderer = Renderer::new(src.clone(), r.db.clone(), Network::Bitcoin);
        let v = view(&r.db, &r.chain).unwrap();
        let out = renderer.confirmed(&v, &[0]).unwrap();
        assert_eq!(out[0]["txid"], txid(1).to_string());
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pending_skips_a_tx_that_left_the_mempool_and_caches_the_rest() {
        let r = rig(0);
        let v = view(&r.db, &r.chain).unwrap();
        let src = Arc::new(Fake::default());
        let renderer = Renderer::new(src.clone(), r.db.clone(), Network::Bitcoin);
        let mp = MempoolView::default();
        let out = renderer.pending(&v, &mp, &[txid(5)]).unwrap();
        assert_eq!(out[0]["status"], json!({ "confirmed": false }));
        renderer.pending(&v, &mp, &[txid(5)]).unwrap();
        assert_eq!(src.calls.load(Ordering::SeqCst), 1);
        renderer.forget_pending_not_in(&mp);
        renderer.pending(&v, &mp, &[txid(5)]).unwrap();
        assert_eq!(src.calls.load(Ordering::SeqCst), 2);

        let failing = Renderer::new(
            Arc::new(Fake {
                fail: true,
                ..Default::default()
            }),
            r.db.clone(),
            Network::Bitcoin,
        );
        assert!(failing.pending(&v, &mp, &[txid(5)]).unwrap().is_empty());
    }
}
