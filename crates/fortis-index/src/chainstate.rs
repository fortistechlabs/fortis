//! The in-memory block table and consistent reader views. The writer commits a
//! batch first and publishes the new table second; a reader pairs a table with
//! a DB snapshot only when both name the same tip, so no request ever mixes two
//! chain states (see "Consistency for readers" in the v2 design).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use arc_swap::ArcSwap;

use crate::db::{Db, Reader};
use crate::keys::{BlockRec, TxNum};

#[derive(Clone, Default)]
pub struct BlockTable {
    start: u32,
    recs: Vec<BlockRec>,
}

impl BlockTable {
    /// Build from `Db::load_blocks` rows; heights must be contiguous.
    pub fn from_db(rows: Vec<(u32, BlockRec)>) -> Result<Self> {
        let Some(&(start, _)) = rows.first() else {
            return Ok(Self::default());
        };
        for (i, (h, _)) in rows.iter().enumerate() {
            if *h != start + i as u32 {
                bail!(
                    "blocks table has a gap: expected height {}, found {h}",
                    start + i as u32
                );
            }
        }
        Ok(Self {
            start,
            recs: rows.into_iter().map(|(_, r)| r).collect(),
        })
    }

    pub fn tip(&self) -> Option<(u32, &BlockRec)> {
        self.recs
            .last()
            .map(|r| (self.start + self.recs.len() as u32 - 1, r))
    }

    pub fn get(&self, h: u32) -> Option<&BlockRec> {
        h.checked_sub(self.start)
            .and_then(|i| self.recs.get(i as usize))
    }

    /// The txnum the next applied block starts at.
    pub fn next_txnum(&self) -> TxNum {
        self.recs
            .last()
            .map_or(0, |r| r.first_txnum + r.n_txs as TxNum)
    }

    /// Height of the block containing txnum `n`.
    pub fn height_of(&self, n: TxNum) -> Option<u32> {
        // First block whose range ends after n; blocks with no indexed txs
        // have an empty range and are skipped naturally.
        let i = self
            .recs
            .partition_point(|r| r.first_txnum + r.n_txs as TxNum <= n);
        let r = self.recs.get(i)?;
        (r.first_txnum <= n).then_some(self.start + i as u32)
    }

    /// Append block `h`, which must be the tip + 1 (or any height when empty).
    pub fn push(&mut self, h: u32, rec: BlockRec) {
        match self.tip() {
            None => self.start = h,
            Some((t, _)) => assert_eq!(h, t + 1, "BlockTable::push out of order"),
        }
        self.recs.push(rec);
    }

    pub fn pop(&mut self) -> Option<(u32, BlockRec)> {
        let h = self.tip()?.0;
        self.recs.pop().map(|r| (h, r))
    }
}

pub type SharedChain = Arc<ArcSwap<BlockTable>>;

pub struct View<'a> {
    pub chain: Arc<BlockTable>,
    pub reader: Reader<'a>,
}

const VIEW_TRIES: u32 = 1000;

/// A block table and DB snapshot that agree on the tip. The writer publishes
/// right after committing, so a mismatch lasts microseconds; after
/// `VIEW_TRIES` attempts (tens of milliseconds) this gives up.
pub fn view<'a>(db: &'a Db, chain: &SharedChain) -> Result<View<'a>> {
    for i in 0..VIEW_TRIES {
        let table = chain.load_full();
        let reader = db.reader();
        let db_tip = reader.tip()?;
        if db_tip == table.tip().map(|(h, r)| (h, r.hash)) {
            return Ok(View {
                chain: table,
                reader,
            });
        }
        if i < 100 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_micros(50));
        }
    }
    bail!("index is between states (writer has not published); try again")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbConfig, Durability};
    use crate::extract::{Funded, ParsedBlock, TxRows};
    use bitcoin::hashes::Hash;
    use bitcoin::{BlockHash, Txid};

    fn rec(first_txnum: TxNum, n_txs: u32) -> BlockRec {
        BlockRec {
            hash: BlockHash::all_zeros(),
            time: 0,
            first_txnum,
            n_txs,
        }
    }

    #[test]
    fn height_of_skips_blocks_with_no_indexed_txs() {
        let t =
            BlockTable::from_db(vec![(10, rec(0, 2)), (11, rec(2, 0)), (12, rec(2, 3))]).unwrap();
        assert_eq!(t.height_of(0), Some(10));
        assert_eq!(t.height_of(1), Some(10));
        assert_eq!(t.height_of(2), Some(12));
        assert_eq!(t.height_of(4), Some(12));
        assert_eq!(t.height_of(5), None);
        assert_eq!(t.next_txnum(), 5);
        assert_eq!(t.tip().unwrap().0, 12);
        assert_eq!(t.get(11).unwrap().n_txs, 0);
        assert!(t.get(9).is_none() && t.get(13).is_none());
    }

    #[test]
    fn from_db_rejects_a_gap() {
        assert!(BlockTable::from_db(vec![(10, rec(0, 1)), (12, rec(1, 1))]).is_err());
        assert!(BlockTable::from_db(vec![]).unwrap().tip().is_none());
    }

    #[test]
    fn push_and_pop_track_the_tip() {
        let mut t = BlockTable::default();
        assert_eq!(t.next_txnum(), 0);
        t.push(100, rec(0, 2));
        t.push(101, rec(2, 1));
        assert_eq!(t.next_txnum(), 3);
        assert_eq!(t.pop().unwrap().0, 101);
        assert_eq!(t.tip().unwrap().0, 100);
        assert_eq!(t.pop().unwrap().0, 100);
        assert!(t.pop().is_none());
    }

    fn blk(height: u32) -> ParsedBlock {
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(&height.to_be_bytes());
        ParsedBlock {
            height,
            hash: BlockHash::from_byte_array(h),
            prev: BlockHash::all_zeros(),
            time: 0,
            txs: vec![TxRows {
                txid: Txid::from_byte_array([height as u8; 32]),
                funded: vec![Funded {
                    program: [1; 20],
                    vout: 0,
                    value: 1,
                }],
                spent: vec![],
            }],
        }
    }

    #[test]
    fn view_never_pairs_a_table_with_a_newer_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        let recs = db
            .apply(&[blk(100), blk(101)], 0, Durability::Durable, false)
            .unwrap();
        let mut at100 = BlockTable::default();
        at100.push(100, recs[0]);
        let chain: SharedChain = Arc::new(ArcSwap::from_pointee(at100.clone()));
        assert!(view(&db, &chain).is_err());

        let mut at101 = at100;
        at101.push(101, recs[1]);
        let publisher = chain.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            publisher.store(Arc::new(at101));
        });
        let v = view(&db, &chain).unwrap();
        assert_eq!(v.chain.tip().unwrap().0, 101);
        assert_eq!(v.reader.tip().unwrap().unwrap().0, 101);
        t.join().unwrap();
    }
}
