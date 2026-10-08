//! The mempool overlay. Each refresh diffs the node's mempool (txids plus its
//! sequence number) against what we already hold, fetches only new txs (raw,
//! batched), and publishes an immutable `MempoolView` that requests read
//! without locks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use bitcoin::consensus::deserialize;
use bitcoin::{OutPoint, Transaction, Txid};
use serde_json::json;

use crate::extract::{tx_rows, TxRows};
use crate::keys::Program;
use crate::source::RpcSource;

const RAW_BATCH: usize = 500;

pub trait MempoolSource: Send + Sync {
    /// `getrawmempool false true`: the mempool sequence and every txid.
    fn snapshot(&self) -> Result<(u64, Vec<Txid>)>;
    /// Raw transactions (`getrawtransaction <id> 0`); `None` for one that
    /// left the mempool. At most `RAW_BATCH` ids per call.
    fn raw_txs(&self, ids: &[Txid]) -> Result<Vec<Option<Vec<u8>>>>;
}

impl MempoolSource for RpcSource {
    fn snapshot(&self) -> Result<(u64, Vec<Txid>)> {
        let v = self.call("getrawmempool", json!([false, true]))?;
        let seq = v["mempool_sequence"]
            .as_u64()
            .ok_or_else(|| anyhow!("getrawmempool: no mempool_sequence"))?;
        let ids = v["txids"]
            .as_array()
            .ok_or_else(|| anyhow!("getrawmempool: no txids"))?
            .iter()
            .map(|t| {
                t.as_str()
                    .ok_or_else(|| anyhow!("getrawmempool: bad txid {t}"))?
                    .parse()
                    .context("txid")
            })
            .collect::<Result<_>>()?;
        Ok((seq, ids))
    }

    fn raw_txs(&self, ids: &[Txid]) -> Result<Vec<Option<Vec<u8>>>> {
        let calls: Vec<(&str, serde_json::Value)> = ids
            .iter()
            .map(|id| ("getrawtransaction", json!([id.to_string(), 0])))
            .collect();
        let out = self.batch(&calls)?;
        Ok(out
            .into_iter()
            .map(|r| {
                r.ok()
                    .and_then(|v| v.as_str().and_then(|s| hex::decode(s).ok()))
            })
            .collect())
    }
}

#[derive(Default)]
pub struct MempoolView {
    txs: HashMap<Txid, Arc<TxRows>>,
    by_program: HashMap<Program, Vec<Txid>>,
    spent: HashSet<OutPoint>,
    outputs: HashMap<OutPoint, (Program, u64)>,
}

impl MempoolView {
    pub(crate) fn build(txs: HashMap<Txid, Arc<TxRows>>) -> Self {
        let mut v = MempoolView::default();
        for (id, rows) in &txs {
            let mut touched = HashSet::new();
            for f in &rows.funded {
                touched.insert(f.program);
                v.outputs.insert(
                    OutPoint {
                        txid: *id,
                        vout: f.vout,
                    },
                    (f.program, f.value),
                );
            }
            for s in &rows.spent {
                touched.insert(s.program);
                v.spent.insert(s.prevout);
            }
            for p in touched {
                v.by_program.entry(p).or_default().push(*id);
            }
        }
        for ids in v.by_program.values_mut() {
            ids.sort();
        }
        v.txs = txs;
        v
    }

    /// Number of mempool transactions touching any P2WPKH program.
    pub fn len(&self) -> usize {
        self.txs.len()
    }

    #[allow(dead_code)] // pairs with `len` (clippy::len_without_is_empty)
    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn txids_for(&self, p: &Program) -> Vec<Txid> {
        self.by_program.get(p).cloned().unwrap_or_default()
    }

    /// Outputs to `p` created in the mempool and not spent by another mempool tx.
    pub fn utxos_for(&self, p: &Program) -> Vec<(OutPoint, u64)> {
        let mut out: Vec<(OutPoint, u64)> = self
            .txids_for(p)
            .iter()
            .flat_map(|id| {
                self.txs[id]
                    .funded
                    .iter()
                    .filter(|f| &f.program == p)
                    .map(|f| {
                        (
                            OutPoint {
                                txid: *id,
                                vout: f.vout,
                            },
                            f.value,
                        )
                    })
            })
            .filter(|(op, _)| !self.spent.contains(op))
            .collect();
        out.sort();
        out
    }

    pub fn is_spent(&self, op: &OutPoint) -> bool {
        self.spent.contains(op)
    }

    pub fn output_at(&self, op: &OutPoint) -> Option<(Program, u64)> {
        self.outputs.get(op).copied()
    }

    pub fn contains(&self, id: &Txid) -> bool {
        self.txs.contains_key(id)
    }
}

pub type SharedMempool = Arc<arc_swap::ArcSwap<MempoolView>>;

#[derive(Default)]
pub struct MempoolTracker {
    rows: HashMap<Txid, Arc<TxRows>>,
    /// Txids already fetched that touch no P2WPKH program.
    nothing: HashSet<Txid>,
    seq: Option<u64>,
}

impl MempoolTracker {
    /// Refresh from the node; `None` if its mempool sequence is unchanged.
    pub fn refresh(&mut self, src: &dyn MempoolSource) -> Result<Option<MempoolView>> {
        let (seq, ids) = src.snapshot()?;
        if self.seq == Some(seq) {
            return Ok(None);
        }
        let current: HashSet<Txid> = ids.iter().copied().collect();
        self.rows.retain(|id, _| current.contains(id));
        self.nothing.retain(|id| current.contains(id));
        let new: Vec<Txid> = ids
            .into_iter()
            .filter(|id| !self.rows.contains_key(id) && !self.nothing.contains(id))
            .collect();
        for chunk in new.chunks(RAW_BATCH) {
            let raws = src.raw_txs(chunk)?;
            for (id, raw) in chunk.iter().zip(raws) {
                // Gone before we fetched it: the next snapshot won't list it.
                let Some(raw) = raw else { continue };
                let tx: Transaction = match deserialize(&raw) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("mempool: undecodable tx {id}: {e}");
                        continue;
                    }
                };
                match tx_rows(&tx) {
                    Some(r) => {
                        self.rows.insert(*id, Arc::new(r));
                    }
                    None => {
                        self.nothing.insert(*id);
                    }
                }
            }
        }
        self.seq = Some(seq);
        Ok(Some(MempoolView::build(self.rows.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::hashes::{hash160, Hash};
    use bitcoin::{Amount, ScriptBuf, Sequence, TxIn, TxOut, WPubkeyHash, Witness};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        seq: Mutex<u64>,
        txs: Mutex<Vec<Transaction>>,
        /// Listed in the snapshot but gone by fetch time.
        vanished: Mutex<HashSet<Txid>>,
        raw_calls: AtomicUsize,
        fetched: AtomicUsize,
    }

    impl Fake {
        fn set(&self, seq: u64, txs: Vec<Transaction>) {
            *self.seq.lock().unwrap() = seq;
            *self.txs.lock().unwrap() = txs;
        }
    }

    impl MempoolSource for Fake {
        fn snapshot(&self) -> Result<(u64, Vec<Txid>)> {
            let ids = self
                .txs
                .lock()
                .unwrap()
                .iter()
                .map(|t| t.compute_txid())
                .collect();
            Ok((*self.seq.lock().unwrap(), ids))
        }

        fn raw_txs(&self, ids: &[Txid]) -> Result<Vec<Option<Vec<u8>>>> {
            self.raw_calls.fetch_add(1, Ordering::SeqCst);
            self.fetched.fetch_add(ids.len(), Ordering::SeqCst);
            let txs = self.txs.lock().unwrap();
            let gone = self.vanished.lock().unwrap();
            Ok(ids
                .iter()
                .map(|id| {
                    if gone.contains(id) {
                        return None;
                    }
                    txs.iter()
                        .find(|t| &t.compute_txid() == id)
                        .map(bitcoin::consensus::serialize)
                })
                .collect())
        }
    }

    fn key(n: u8) -> Vec<u8> {
        let mut k = vec![2u8; 33];
        k[1] = n;
        k
    }

    fn prog(n: u8) -> Program {
        hash160::Hash::hash(&key(n)).to_byte_array()
    }

    /// Spends `prev` (owned by key `from`, if any) and pays `value` to key `to`.
    fn tx(prev: OutPoint, from: Option<u8>, to: u8, value: u64) -> Transaction {
        let witness = match from {
            Some(f) => Witness::from_slice(&[vec![0x30; 72], key(f)]),
            None => Witness::new(),
        };
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: vec![TxOut {
                value: Amount::from_sat(value),
                script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(prog(to))),
            }],
        }
    }

    fn op(n: u8) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([n; 32]),
            vout: 0,
        }
    }

    fn no_p2wpkh(n: u8) -> Transaction {
        let mut t = tx(op(n), None, 0, 1);
        t.output[0].script_pubkey = ScriptBuf::new_op_return([n]);
        t
    }

    #[test]
    fn a_new_tx_funding_a_program_is_a_pending_utxo() {
        let src = Fake::default();
        let t = tx(op(1), None, 7, 900);
        src.set(1, vec![t.clone()]);
        let v = MempoolTracker::default().refresh(&src).unwrap().unwrap();
        let out = OutPoint {
            txid: t.compute_txid(),
            vout: 0,
        };
        assert_eq!(v.utxos_for(&prog(7)), vec![(out, 900)]);
        assert_eq!(v.txids_for(&prog(7)), vec![t.compute_txid()]);
        assert_eq!(v.output_at(&out), Some((prog(7), 900)));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn a_chained_mempool_spend_hides_the_parent_output() {
        let src = Fake::default();
        let parent = tx(op(1), None, 7, 900);
        let child = tx(
            OutPoint {
                txid: parent.compute_txid(),
                vout: 0,
            },
            Some(7),
            8,
            800,
        );
        src.set(1, vec![parent.clone(), child.clone()]);
        let v = MempoolTracker::default().refresh(&src).unwrap().unwrap();
        assert!(v.utxos_for(&prog(7)).is_empty());
        let mut both = vec![parent.compute_txid(), child.compute_txid()];
        both.sort();
        assert_eq!(v.txids_for(&prog(7)), both);
        assert_eq!(v.utxos_for(&prog(8)).len(), 1);
    }

    #[test]
    fn replaced_tx_releases_its_spent_coin() {
        let src = Fake::default();
        let confirmed = op(5);
        let tx1 = tx(confirmed, Some(3), 4, 100);
        let tx2 = tx(op(6), None, 9, 50);
        let mut tracker = MempoolTracker::default();
        src.set(1, vec![tx1.clone()]);
        let v = tracker.refresh(&src).unwrap().unwrap();
        assert!(v.is_spent(&confirmed));
        src.set(2, vec![tx2]);
        let v = tracker.refresh(&src).unwrap().unwrap();
        assert!(!v.is_spent(&confirmed));
        assert!(v.utxos_for(&prog(4)).is_empty());
        assert!(v.txids_for(&prog(3)).is_empty());
        assert_eq!(
            v.output_at(&OutPoint {
                txid: tx1.compute_txid(),
                vout: 0
            }),
            None
        );
    }

    #[test]
    fn unchanged_sequence_fetches_nothing() {
        let src = Fake::default();
        src.set(1, vec![tx(op(1), None, 7, 900)]);
        let mut tracker = MempoolTracker::default();
        assert!(tracker.refresh(&src).unwrap().is_some());
        assert!(tracker.refresh(&src).unwrap().is_none());
        assert_eq!(src.raw_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn txs_touching_nothing_are_fetched_once() {
        let src = Fake::default();
        let mut tracker = MempoolTracker::default();
        src.set(1, vec![no_p2wpkh(1), no_p2wpkh(2)]);
        assert_eq!(tracker.refresh(&src).unwrap().unwrap().len(), 0);
        src.set(2, vec![no_p2wpkh(1), no_p2wpkh(2), tx(op(3), None, 7, 1)]);
        assert_eq!(tracker.refresh(&src).unwrap().unwrap().len(), 1);
        assert_eq!(src.fetched.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_tx_that_vanished_before_fetch_is_skipped() {
        let src = Fake::default();
        let t = tx(op(1), None, 7, 900);
        src.vanished.lock().unwrap().insert(t.compute_txid());
        src.set(1, vec![t]);
        let v = MempoolTracker::default().refresh(&src).unwrap().unwrap();
        assert!(v.is_empty());
        assert!(v.utxos_for(&prog(7)).is_empty());
    }

    #[test]
    fn large_mempools_are_fetched_in_batches_of_500() {
        let src = Fake::default();
        src.set(
            1,
            (0..1001u32)
                .map(|i| {
                    tx(
                        OutPoint {
                            txid: Txid::all_zeros(),
                            vout: i,
                        },
                        None,
                        7,
                        1,
                    )
                })
                .collect(),
        );
        let v = MempoolTracker::default().refresh(&src).unwrap().unwrap();
        assert_eq!(v.len(), 1001);
        assert_eq!(src.raw_calls.load(Ordering::SeqCst), 3);
    }
}
