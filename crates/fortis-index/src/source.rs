//! Where blocks come from: the node over JSON-RPC (`RpcSource`), or an
//! in-memory chain in tests (`mem::MemSource`).

use std::str::FromStr;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use bitcoin::BlockHash;
use serde_json::json;

use fortis_node::Rpc;

pub trait BlockSource: Send + Sync {
    /// The node's best height.
    fn tip(&self) -> Result<u32>;
    /// Hashes of heights `from .. from + count` (one batch RPC, `count` ≤ 1000).
    fn hashes(&self, from: u32, count: u32) -> Result<Vec<BlockHash>>;
    /// The serialized block (`getblock <hash> 0`).
    fn raw_block(&self, hash: &BlockHash) -> Result<Vec<u8>>;
    /// Return when a new block arrives or after `timeout_ms`.
    fn wait_for_block(&self, timeout_ms: u64) -> Result<()>;
}

/// The node over JSON-RPC. Each call borrows an `Rpc` with its own HTTP agent
/// from a pool (created with `fresh()` on demand), so parallel fetch workers
/// never share a connection.
pub struct RpcSource {
    base: Rpc,
    pool: Mutex<Vec<Rpc>>,
}

impl RpcSource {
    pub fn new(rpc: Rpc) -> Self {
        RpcSource {
            base: rpc,
            pool: Mutex::new(Vec::new()),
        }
    }

    /// One RPC call on a pooled agent.
    pub fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.with(|r| r.call(method, params))
    }

    /// One JSON-RPC batch on a pooled agent.
    pub fn batch(
        &self,
        calls: &[(&str, serde_json::Value)],
    ) -> Result<Vec<Result<serde_json::Value>>> {
        self.with(|r| r.call_batch(calls))
    }

    fn with<T>(&self, f: impl FnOnce(&Rpc) -> Result<T>) -> Result<T> {
        let rpc = self
            .pool
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| self.base.fresh());
        let out = f(&rpc);
        self.pool.lock().unwrap().push(rpc);
        out
    }
}

impl BlockSource for RpcSource {
    fn tip(&self) -> Result<u32> {
        let v = self.with(|r| r.call("getblockcount", json!([])))?;
        v.as_u64()
            .and_then(|h| u32::try_from(h).ok())
            .ok_or_else(|| anyhow!("getblockcount: {v}"))
    }

    fn hashes(&self, from: u32, count: u32) -> Result<Vec<BlockHash>> {
        let calls: Vec<(&str, serde_json::Value)> = (from..from + count)
            .map(|h| ("getblockhash", json!([h])))
            .collect();
        let out = self.with(|r| r.call_batch(&calls))?;
        out.into_iter()
            .zip(from..)
            .map(|(r, h)| {
                let v = r.with_context(|| format!("getblockhash {h}"))?;
                let s = v.as_str().ok_or_else(|| anyhow!("getblockhash {h}: {v}"))?;
                BlockHash::from_str(s).with_context(|| format!("getblockhash {h}: {s}"))
            })
            .collect()
    }

    fn raw_block(&self, hash: &BlockHash) -> Result<Vec<u8>> {
        let v = self.with(|r| r.call("getblock", json!([hash.to_string(), 0])))?;
        let s = v
            .as_str()
            .ok_or_else(|| anyhow!("getblock {hash} 0: not a string"))?;
        hex::decode(s).with_context(|| format!("getblock {hash} 0: bad hex"))
    }

    fn wait_for_block(&self, timeout_ms: u64) -> Result<()> {
        self.with(|r| r.call("waitfornewblock", json!([timeout_ms])))?;
        Ok(())
    }
}

#[cfg(test)]
pub mod mem {
    //! An in-memory chain of synthetic 80-byte-header blocks. Block `h` holds
    //! one transaction that spends block `h-1`'s output (a P2WPKH witness
    //! spend) and pays a P2WPKH output to `program(h, salt)`.

    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::time::Duration;

    use bitcoin::absolute::LockTime;
    use bitcoin::block::{Header, Version};
    use bitcoin::hashes::{hash160, Hash};
    use bitcoin::{
        Amount, Block, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
        TxMerkleNode, TxOut, Txid, WPubkeyHash, Witness,
    };

    use crate::keys::Program;

    fn pubkey(height: u32, salt: u8) -> [u8; 33] {
        let mut k = [0u8; 33];
        k[0] = 2;
        k[1..5].copy_from_slice(&height.to_be_bytes());
        k[5] = salt;
        k
    }

    /// The program block `height` (built with `salt`) pays to.
    pub fn program(height: u32, salt: u8) -> Program {
        hash160::Hash::hash(&pubkey(height, salt)).to_byte_array()
    }

    fn tx(height: u32, salt: u8, prev: Option<(Txid, u32, u8)>) -> Transaction {
        let input = match prev {
            Some((txid, ph, psalt)) => TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[vec![0x30; 72], pubkey(ph, psalt).to_vec()]),
            },
            // Distinct non-null prevout so it isn't a coinbase.
            None => TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([0xee; 32]),
                    vout: height,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            },
        };
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![input],
            output: vec![TxOut {
                value: Amount::from_sat(1_000_000 - height as u64),
                script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(program(
                    height, salt,
                ))),
            }],
        }
    }

    #[derive(Default)]
    struct Chain {
        blocks: Vec<Block>,
        salts: Vec<u8>,
        by_hash: HashMap<BlockHash, usize>,
    }

    impl Chain {
        fn build_from(&mut self, from: u32, to_inclusive: u32, salt: u8) {
            self.blocks.truncate(from as usize);
            self.salts.truncate(from as usize);
            for h in from..=to_inclusive {
                let (prev_hash, prev_tx) = match h.checked_sub(1) {
                    Some(p) => {
                        let b = &self.blocks[p as usize];
                        (
                            b.block_hash(),
                            Some((b.txdata[0].compute_txid(), p, self.salts[p as usize])),
                        )
                    }
                    None => (BlockHash::all_zeros(), None),
                };
                let header = Header {
                    version: Version::TWO,
                    prev_blockhash: prev_hash,
                    merkle_root: TxMerkleNode::all_zeros(),
                    time: 1_600_000_000 + h,
                    bits: CompactTarget::from_consensus(0x207fffff),
                    nonce: salt as u32,
                };
                self.blocks.push(Block {
                    header,
                    txdata: vec![tx(h, salt, prev_tx)],
                });
                self.salts.push(salt);
            }
            self.by_hash = self
                .blocks
                .iter()
                .enumerate()
                .map(|(i, b)| (b.block_hash(), i))
                .collect();
        }
    }

    type Delay = Box<dyn Fn(u32) -> Duration + Send + Sync>;

    pub struct MemSource {
        chain: Mutex<Chain>,
        delay: Mutex<Option<Delay>>,
        /// `raw_block` of this height fails.
        pub fail_at: Mutex<Option<u32>>,
        /// The next this-many `tip()` calls fail.
        pub tip_failures: AtomicU32,
        pub in_flight: AtomicUsize,
        /// `raw_block` calls started (the consumer compares with blocks received).
        pub started: AtomicUsize,
    }

    impl MemSource {
        /// Blocks 0..=tip, all built with salt 0.
        pub fn new(tip: u32) -> Self {
            let mut chain = Chain::default();
            chain.build_from(0, tip, 0);
            MemSource {
                chain: Mutex::new(chain),
                delay: Mutex::new(None),
                fail_at: Mutex::new(None),
                tip_failures: AtomicU32::new(0),
                in_flight: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
            }
        }

        /// Replace blocks `from..` with a fork built with `salt`, up to `new_tip`.
        pub fn reorg(&self, from: u32, new_tip: u32, salt: u8) {
            self.chain.lock().unwrap().build_from(from, new_tip, salt);
        }

        pub fn set_delay(&self, f: impl Fn(u32) -> Duration + Send + Sync + 'static) {
            *self.delay.lock().unwrap() = Some(Box::new(f));
        }

        pub fn hash(&self, h: u32) -> BlockHash {
            self.chain.lock().unwrap().blocks[h as usize].block_hash()
        }
    }

    impl BlockSource for MemSource {
        fn tip(&self) -> Result<u32> {
            if self.tip_failures.load(Ordering::SeqCst) > 0 {
                self.tip_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow!("node unreachable (test)"));
            }
            Ok(self.chain.lock().unwrap().blocks.len() as u32 - 1)
        }

        fn hashes(&self, from: u32, count: u32) -> Result<Vec<BlockHash>> {
            let c = self.chain.lock().unwrap();
            (from..from + count)
                .map(|h| {
                    c.blocks
                        .get(h as usize)
                        .map(Block::block_hash)
                        .ok_or_else(|| anyhow!("no block {h}"))
                })
                .collect()
        }

        fn raw_block(&self, hash: &BlockHash) -> Result<Vec<u8>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            let found = {
                let c = self.chain.lock().unwrap();
                c.by_hash
                    .get(hash)
                    .map(|&i| (i as u32, bitcoin::consensus::serialize(&c.blocks[i])))
            };
            let out = match found {
                None => Err(anyhow!("block {hash} not found")),
                Some((h, bytes)) => {
                    if let Some(d) = self.delay.lock().unwrap().as_ref() {
                        std::thread::sleep(d(h));
                    }
                    if *self.fail_at.lock().unwrap() == Some(h) {
                        Err(anyhow!("raw_block failed at {h} (test)"))
                    } else {
                        Ok(bytes)
                    }
                }
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            out
        }

        fn wait_for_block(&self, timeout_ms: u64) -> Result<()> {
            std::thread::sleep(Duration::from_millis(timeout_ms.min(1)));
            Ok(())
        }
    }
}
