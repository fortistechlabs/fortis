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

            let hash = self
                .rpc
                .call("getblockhash", json!([next]))?
                .as_str()
                .context("getblockhash")?
                .to_string();
            let block = self.rpc.call("getblock", json!([hash, 2]))?;
            let prev = block["previousblockhash"].as_str().unwrap_or("");

            // Reorg: our current tip must be this block's parent. If not, unwind
            // one block and retry — deep reorgs unwind across iterations, and a
            // reorg back past `start_height` clears the index and re-syncs.
            if let Some((our_h, our_hash)) = &our {
                if our_hash != prev {
                    self.store.rollback_from((*our_h).max(self.start_height))?;
                    continue;
                }
            }

            let txs = block_txs(&block)?;
            self.store.apply_block(next, &hash, &txs)?;
            applied += 1;
        }
        Ok((node_tip, applied))
    }
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
        let outputs = tx
            .output
            .iter()
            .map(|o| TxOut {
                spk_hex: hex::encode(o.script_pubkey.as_bytes()),
                value_sat: o.value.to_sat(),
            })
            .collect();
        out.push(IndexedTx { txid, inputs, outputs });
    }
    Ok(out)
}
