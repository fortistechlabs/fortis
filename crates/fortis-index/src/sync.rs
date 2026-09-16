//! The block follower: catch the index up to the node's tip, unwinding on reorg.
//!
//! Blocks are read as `getblock <hash> 2` (JSON) — the node decodes the fork's
//! 164-byte header and modified PoW, so this never parses a raw block. Each
//! transaction's `hex` *is* parsed with the `bitcoin` crate (tx format is
//! unchanged by the fork) for exact amounts and scripts.

use anyhow::{anyhow, Context, Result};
use bitcoin::consensus::deserialize;
use bitcoin::Transaction;
use serde_json::{json, Value};

use fortis_node::Rpc;

use crate::store::{IndexedTx, Store, TxIn, TxOut};

/// Blocks fetched per round and applied together as one transaction (see
/// `Store::apply_blocks`) — one commit (one WAL flush) per batch instead of
/// per block is most of what makes a from-SegWit-activation sync fast.
const FETCH_BATCH: usize = 32;
/// Concurrent RPC connections used to fill one batch. `getblockhash` +
/// `getblock` per height is a network round trip even on localhost, and the
/// node services RPC calls concurrently (its own worker thread pool), so
/// overlapping requests hides that latency instead of paying it serially —
/// confirmed live: the sync rate held steady across the SegWit boundary
/// (much bigger blocks than the pre-SegWit era) instead of dropping with
/// block size, meaning RPC/write overhead, not raw block-fetch time, was
/// the bottleneck this actually targets.
const FETCH_WORKERS: usize = 8;

pub struct Syncer<'a> {
    pub rpc: &'a Rpc,
    pub store: &'a mut Store,
    pub start_height: u64,
}

impl Syncer<'_> {
    /// Advance the index to the node's current tip. Returns `(node_tip,
    /// blocks_applied)`.
    pub fn sync_to_tip(&mut self) -> Result<(u64, u64)> {
        let node_tip = self
            .rpc
            .call("getblockcount", json!([]))?
            .as_u64()
            .context("getblockcount")?;

        let mut applied = 0u64;
        loop {
            let our = self.store.tip()?;
            let next = match &our {
                Some((h, _)) => h + 1,
                None => self.start_height,
            };
            if next > node_tip {
                break;
            }

            let end = (next + FETCH_BATCH as u64 - 1).min(node_tip);
            let heights: Vec<u64> = (next..=end).collect();
            let fetched = fetch_blocks(self.rpc, &heights)?;
            let prev_hash = our.as_ref().map(|(_, h)| h.clone());
            let (owned, parse_err) = validate_batch(&heights, fetched, prev_hash);

            if !owned.is_empty() {
                let refs: Vec<(u64, &str, &[IndexedTx])> =
                    owned.iter().map(|(h, hash, txs)| (*h, hash.as_str(), txs.as_slice())).collect();
                self.store.apply_blocks(&refs)?;
                applied += owned.len() as u64;
            }

            // Apply whatever validated before the failure *first* (above) so
            // a parse error doesn't waste already-good work — but still
            // propagate it now, rather than looping straight to the next
            // batch: this height will fail the exact same way on every
            // retry until it's fixed in code, and propagating lets the
            // caller's normal poll-interval sleep throttle those retries.
            // Without this, an empty `owned` here would fall into the
            // reorg-rollback branch below with nothing to actually unwind,
            // spinning at full speed with no backoff at all.
            if let Some(e) = parse_err {
                return Err(e);
            }

            if owned.is_empty() {
                // The very first block in the batch didn't chain from our
                // stored tip — a reorg. Unwind one block and retry; deep
                // reorgs unwind across loop iterations, same as before
                // batching. `our` is always `Some` here: an empty store's
                // first-ever fetched block has nothing to mismatch against.
                let (our_h, _) = our.expect("reorg check only fails against an existing tip");
                self.store.rollback_from(our_h.max(self.start_height))?;
            }
        }
        Ok((node_tip, applied))
    }
}

/// Validate a freshly-fetched batch against the running chain state (`our`'s
/// hash, or the hash of whichever block in the batch was accepted just
/// before), returning the longest connected-in-order prefix, plus a hard
/// error if a block failed to *parse* (distinct from a reorg — see below).
/// Fetching is unordered/parallel, but applying must stay strictly
/// sequential — this walks the batch in height order, checking each block's
/// parent hash against the one before it, the same reorg-safety check
/// `sync_to_tip` did per-block before batching existed.
///
/// Two different reasons the walk can stop early, and the caller must tell
/// them apart:
/// - **Chain break** (parent hash mismatch): a reorg mid-fetch invalidates
///   only the blocks after it — an empty prefix here is the normal,
///   self-correcting "roll back one block and retry immediately" case.
/// - **Parse failure** (a transaction the `bitcoin` crate's deserializer
///   rejects): retrying changes nothing — the same block fails the same way
///   every time until it's fixed in code. Returned as `Some(error)`
///   alongside whatever prefix parsed fine before it, so the caller can
///   apply that real progress *and* still propagate the error for its
///   normal poll-interval backoff, rather than either discarding the good
///   prefix (the batch equivalent of the BIP30 stall fixed earlier this
///   session) or retrying with no backoff at all (which an empty prefix
///   funnelled into the reorg-rollback path would do, since there's nothing
///   to actually unwind).
fn validate_batch(
    heights: &[u64],
    fetched: Vec<(String, Value)>,
    mut prev_hash: Option<String>,
) -> (Vec<(u64, String, Vec<IndexedTx>)>, Option<anyhow::Error>) {
    let mut owned = Vec::with_capacity(heights.len());
    for (height, (hash, block)) in heights.iter().zip(fetched) {
        let prev = block["previousblockhash"].as_str().unwrap_or("").to_string();
        if let Some(ph) = &prev_hash {
            if *ph != prev {
                break;
            }
        }
        let txs = match block_txs(&block) {
            Ok(txs) => txs,
            Err(e) => return (owned, Some(e.context(format!("block {height} ({hash})")))),
        };
        prev_hash = Some(hash.clone());
        owned.push((*height, hash, txs));
    }
    (owned, None)
}

/// Fetch `getblockhash` + `getblock <hash> 2` for every height in `heights`,
/// spread across `FETCH_WORKERS` threads sharing the same `Rpc` (its
/// underlying `ureq::Agent` pools connections and is safe to call
/// concurrently). Results come back in the same order as `heights`
/// regardless of which worker finished first.
fn fetch_blocks(rpc: &Rpc, heights: &[u64]) -> Result<Vec<(String, Value)>> {
    let chunk_size = heights.len().div_ceil(FETCH_WORKERS).max(1);
    let mut results: Vec<Option<Result<(String, Value)>>> = (0..heights.len()).map(|_| None).collect();
    std::thread::scope(|scope| -> Result<()> {
        let handles: Vec<_> = heights
            .chunks(chunk_size)
            .enumerate()
            .map(|(ci, chunk)| {
                let base = ci * chunk_size;
                (base, scope.spawn(move || chunk.iter().map(|&h| fetch_one(rpc, h)).collect::<Vec<_>>()))
            })
            .collect();
        for (base, handle) in handles {
            let chunk_results = handle.join().map_err(|_| anyhow!("block-fetch worker panicked"))?;
            for (i, r) in chunk_results.into_iter().enumerate() {
                results[base + i] = Some(r);
            }
        }
        Ok(())
    })?;
    results.into_iter().map(|o| o.expect("every height was assigned to a worker")).collect()
}

fn fetch_one(rpc: &Rpc, height: u64) -> Result<(String, Value)> {
    let hash = rpc
        .call("getblockhash", json!([height]))?
        .as_str()
        .context("getblockhash")?
        .to_string();
    let block = rpc.call("getblock", json!([hash, 2]))?;
    Ok((hash, block))
}

fn block_txs(block: &Value) -> Result<Vec<IndexedTx>> {
    let mut out = Vec::new();
    for t in block["tx"].as_array().context("block.tx")? {
        let txid = t["txid"].as_str().context("tx.txid")?.to_string();
        let raw = hex::decode(t["hex"].as_str().context("tx.hex")?)
            .with_context(|| format!("decoding tx {txid}"))?;
        let tx: Transaction =
            deserialize(&raw).map_err(|e| anyhow!("parsing tx {txid}: {e}"))?;

        let inputs = if tx.is_coinbase() {
            Vec::new()
        } else {
            tx.input
                .iter()
                .map(|i| TxIn {
                    txid: i.previous_output.txid.to_string(),
                    vout: i.previous_output.vout,
                })
                .collect()
        };
        // Only ever store P2WPKH outputs: `wallet-core` derives exclusively
        // BIP-84 P2WPKH addresses (`Address::p2wpkh`, `wallet.rs`) — no other
        // script type can ever be a fortis wallet address. Everything else
        // (OP_RETURN, legacy P2PKH, P2SH, Taproot/inscriptions, bare
        // multisig) can never be looked up by any address this app can
        // derive, so storing it is pure waste — this is most of what made
        // the genesis backfill's database balloon. `vout` is each kept
        // output's *true* position (captured before filtering), not its
        // position in this filtered list — see `TxOut`'s doc comment.
        let outputs = tx
            .output
            .iter()
            .enumerate()
            .filter(|(_, o)| o.script_pubkey.is_p2wpkh())
            .map(|(vout, o)| TxOut {
                vout: vout as u32,
                spk_hex: hex::encode(o.script_pubkey.as_bytes()),
                value_sat: o.value.to_sat(),
            })
            .collect();
        out.push(IndexedTx { txid, inputs, outputs });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::script::PushBytesBuf;
    use bitcoin::{absolute, transaction, Amount, PubkeyHash, ScriptBuf, WPubkeyHash};

    #[test]
    fn block_txs_keeps_only_p2wpkh_outputs_at_their_true_vout() {
        let op_return = ScriptBuf::new_op_return(PushBytesBuf::try_from(vec![0xde, 0xad]).unwrap());
        let p2wpkh = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([1u8; 20]));
        let p2pkh = ScriptBuf::new_p2pkh(&PubkeyHash::from_byte_array([2u8; 20]));

        // P2WPKH deliberately isn't first, to prove `vout` tracks true
        // position, not position in the filtered/kept list.
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![],
            output: vec![
                bitcoin::TxOut { value: Amount::from_sat(1_000), script_pubkey: op_return },
                bitcoin::TxOut { value: Amount::from_sat(2_000), script_pubkey: p2wpkh },
                bitcoin::TxOut { value: Amount::from_sat(3_000), script_pubkey: p2pkh },
            ],
        };
        let txid = tx.compute_txid().to_string();
        let hex = hex::encode(bitcoin::consensus::serialize(&tx));
        let block = json!({ "tx": [{ "txid": txid, "hex": hex }] });

        let indexed = block_txs(&block).unwrap();
        assert_eq!(indexed.len(), 1);
        // OP_RETURN and P2PKH both dropped — only the P2WPKH output survives.
        assert_eq!(indexed[0].outputs.len(), 1);
        assert_eq!(indexed[0].outputs[0].vout, 1);
        assert_eq!(indexed[0].outputs[0].value_sat, 2_000);
    }

    #[test]
    fn block_txs_drops_a_tx_with_no_p2wpkh_outputs_entirely() {
        let op_return = ScriptBuf::new_op_return(PushBytesBuf::try_from(vec![0xff]).unwrap());
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![],
            output: vec![bitcoin::TxOut { value: Amount::ZERO, script_pubkey: op_return }],
        };
        let txid = tx.compute_txid().to_string();
        let hex = hex::encode(bitcoin::consensus::serialize(&tx));
        let block = json!({ "tx": [{ "txid": txid, "hex": hex }] });

        let indexed = block_txs(&block).unwrap();
        assert_eq!(indexed.len(), 1);
        assert!(indexed[0].outputs.is_empty());
    }

    fn fake_block(prev_hash: &str) -> Value {
        json!({ "previousblockhash": prev_hash, "tx": [] })
    }

    #[test]
    fn validate_batch_keeps_a_cleanly_connected_chain() {
        let heights = vec![100, 101, 102];
        let fetched = vec![
            ("h100".to_string(), fake_block("h99")),
            ("h101".to_string(), fake_block("h100")),
            ("h102".to_string(), fake_block("h101")),
        ];
        let (owned, err) = validate_batch(&heights, fetched, Some("h99".to_string()));
        assert!(err.is_none());
        assert_eq!(owned.len(), 3);
        assert_eq!((owned[2].0, owned[2].1.as_str()), (102, "h102"));
    }

    #[test]
    fn validate_batch_stops_at_a_reorg_mid_batch() {
        // 101 doesn't actually chain from 100 (simulates a reorg discovered
        // mid-fetch) — the walk stops there, so 102 (which would chain from
        // the now-rejected 101) is never even considered, even though it's
        // internally consistent with what 101 claims.
        let heights = vec![100, 101, 102];
        let fetched = vec![
            ("h100".to_string(), fake_block("h99")),
            ("h101-orphan".to_string(), fake_block("wrong-parent")),
            ("h102".to_string(), fake_block("h101-orphan")),
        ];
        let (owned, err) = validate_batch(&heights, fetched, Some("h99".to_string()));
        assert!(err.is_none());
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].0, 100);
    }

    #[test]
    fn validate_batch_is_empty_when_even_the_first_block_does_not_connect() {
        let heights = vec![100];
        let fetched = vec![("h100".to_string(), fake_block("not-our-tip"))];
        let (owned, err) = validate_batch(&heights, fetched, Some("h99".to_string()));
        assert!(err.is_none()); // a hash mismatch, not a parse failure
        assert!(owned.is_empty());
    }

    #[test]
    fn validate_batch_accepts_any_first_block_when_the_store_is_empty() {
        // `our` is None (fresh store, nothing to mismatch against yet) — the
        // standard case for the very first block ever indexed.
        let heights = vec![481_824];
        let fetched = vec![("h481824".to_string(), fake_block("h481823"))];
        let (owned, err) = validate_batch(&heights, fetched, None);
        assert!(err.is_none());
        assert_eq!(owned.len(), 1);
    }

    #[test]
    fn validate_batch_keeps_the_valid_prefix_and_reports_a_parse_failure() {
        // Unlike a chain break, a parse failure is a hard error the caller
        // must propagate (for its poll-interval backoff) — but the already-
        // valid block 100 in front of it must still survive, not be
        // discarded along with the failure.
        let heights = vec![100, 101, 102];
        let bad_block = json!({ "previousblockhash": "h100", "tx": [{ "txid": "bad" }] }); // no "hex"
        let fetched = vec![
            ("h100".to_string(), fake_block("h99")),
            ("h101".to_string(), bad_block),
            ("h102".to_string(), fake_block("h101")), // never reached
        ];
        let (owned, err) = validate_batch(&heights, fetched, Some("h99".to_string()));
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].0, 100);
        let err = err.expect("a missing tx.hex must surface as an error, not be silently dropped");
        assert!(err.to_string().contains("101"), "error should name the failing height: {err}");
    }
}
