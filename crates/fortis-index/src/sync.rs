//! The syncer: catches the index up to the node in bulk mode (no WAL, 64
//! blocks per batch, no undo), then follows it durably one block at a time
//! with undo records, rolling back on reorgs. Always commit, then publish.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::chain::HeaderFormat;
use crate::chainstate::{BlockTable, SharedChain};
use crate::db::{Db, Durability, NoUndo, OpenError, MAX_REORG_DEPTH};
use crate::extract::ParsedBlock;
use crate::fetch::{fetch_range, FetchConfig};
use crate::source::BlockSource;

const BULK_BATCH: usize = 64;

pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            min: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

#[derive(Default)]
pub struct SyncStatus {
    pub node_tip: AtomicU32,
    pub follow: AtomicBool,
    /// Transient failures retried since start (each one is a backoff sleep).
    pub retries: AtomicU32,
}

pub struct Syncer {
    pub db: Db,
    pub src: Arc<dyn BlockSource>,
    pub fmt: HeaderFormat,
    pub chain: SharedChain,
    pub start_height: u32,
    pub fetch: FetchConfig,
    pub stop_height: Option<u32>,
    pub backoff: Backoff,
    pub status: Arc<SyncStatus>,
    /// Bulk (WAL-less) writes not yet flushed: must be flushed before any
    /// durable write, or a crash could replay the WAL on top of a hole.
    bulk_dirty: bool,
}

/// Errors that must stop the process (exit 2) rather than be retried.
pub fn is_fatal(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<NoUndo>() || c.is::<OpenError>())
}

impl Syncer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        src: Arc<dyn BlockSource>,
        fmt: HeaderFormat,
        chain: SharedChain,
        start_height: u32,
        fetch: FetchConfig,
        stop_height: Option<u32>,
        backoff: Backoff,
        status: Arc<SyncStatus>,
    ) -> Self {
        Syncer {
            db,
            src,
            fmt,
            chain,
            start_height,
            fetch,
            stop_height,
            backoff,
            status,
            bulk_dirty: false,
        }
    }

    fn table(&self) -> BlockTable {
        (**self.chain.load()).clone()
    }

    fn publish(&self, t: &BlockTable) {
        self.chain.store(Arc::new(t.clone()));
    }

    /// Roll back while our tip is not on the node's chain.
    fn unwind(&mut self, node_tip: u32, t: &mut BlockTable) -> Result<()> {
        while let Some((h, rec)) = t.tip().map(|(h, r)| (h, *r)) {
            if h <= node_tip && self.src.hashes(h, 1)?[0] == rec.hash {
                return Ok(());
            }
            if self.bulk_dirty {
                self.db.flush()?;
                self.bulk_dirty = false;
            }
            let new_tip = h.checked_sub(1).and_then(|p| t.get(p).map(|r| (p, r.hash)));
            self.db.rollback(h, &rec, new_tip)?;
            t.pop();
            self.publish(t);
            eprintln!("index: reorg — rolled back block {h} ({})", rec.hash);
        }
        Ok(())
    }

    fn apply(&mut self, t: &mut BlockTable, blocks: &[ParsedBlock], d: Durability) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        if d == Durability::Durable && self.bulk_dirty {
            self.db.flush()?;
            self.bulk_dirty = false;
        }
        let keep_undo = d == Durability::Durable;
        let recs = self.db.apply(blocks, t.next_txnum(), d, keep_undo)?;
        if d == Durability::Bulk {
            self.bulk_dirty = true;
        }
        for (b, r) in blocks.iter().zip(recs) {
            t.push(b.height, r);
        }
        self.publish(t);
        Ok(())
    }

    fn enter_follow(&mut self, h: u32) -> Result<()> {
        if self.status.follow.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.db.flush()?;
        self.bulk_dirty = false;
        self.db.compact_background();
        eprintln!("index: caught up at {h}, following");
        Ok(())
    }

    /// One pass: reorg check, then catch up to the node (or `stop_height`).
    /// Returns the number of blocks applied.
    pub fn step(&mut self, stop: &AtomicBool) -> Result<u32> {
        let node_tip = self.src.tip().context("node tip")?;
        self.status.node_tip.store(node_tip, Ordering::SeqCst);
        let mut t = self.table();
        self.unwind(node_tip, &mut t)?;

        let from = t.tip().map_or(self.start_height, |(h, _)| h + 1);
        let to = self.stop_height.map_or(node_tip, |s| s.min(node_tip));
        // Blocks this close to the node tip may still be reorged: keep undo.
        let durable_from = node_tip.saturating_sub(MAX_REORG_DEPTH);
        if from > to {
            if from >= durable_from {
                self.enter_follow(from.saturating_sub(1))?;
            }
            return Ok(0);
        }

        let fetch_stop = Arc::new(AtomicBool::new(false));
        let rx = fetch_range(
            self.src.clone(),
            self.fmt,
            from,
            to,
            &self.fetch,
            fetch_stop.clone(),
        );
        let mut applied = 0u32;
        let mut bulk: Vec<ParsedBlock> = Vec::with_capacity(BULK_BATCH);
        let result = (|| -> Result<()> {
            for b in rx.iter() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let b = b?;
                let prev = bulk
                    .last()
                    .map(|p| p.hash)
                    .or_else(|| t.tip().map(|(_, r)| r.hash));
                if prev.is_some_and(|p| p != b.prev) {
                    // The node reorged under us; the next pass unwinds.
                    break;
                }
                if b.height < durable_from {
                    bulk.push(b);
                    if bulk.len() == BULK_BATCH {
                        applied += bulk.len() as u32;
                        self.apply(&mut t, &bulk, Durability::Bulk)?;
                        bulk.clear();
                    }
                } else {
                    applied += bulk.len() as u32;
                    self.apply(&mut t, &bulk, Durability::Bulk)?;
                    bulk.clear();
                    self.enter_follow(b.height.saturating_sub(1))?;
                    let h = b.height;
                    self.apply(&mut t, std::slice::from_ref(&b), Durability::Durable)?;
                    applied += 1;
                    self.db.prune_undo(h.saturating_sub(MAX_REORG_DEPTH))?;
                }
            }
            applied += bulk.len() as u32;
            self.apply(&mut t, &bulk, Durability::Bulk)?;
            Ok(())
        })();
        fetch_stop.store(true, Ordering::SeqCst);
        result?;
        Ok(applied)
    }

    fn reached_stop_height(&self) -> bool {
        match (self.stop_height, self.chain.load().tip()) {
            (Some(s), Some((h, _))) => h >= s,
            _ => false,
        }
    }

    /// Sync until `stop` is set or `stop_height` is reached, calling
    /// `after_pass` after every successful pass. Transient errors back off
    /// exponentially; only fatal errors (see [`is_fatal`]) are returned.
    pub fn run(&mut self, stop: &AtomicBool, after_pass: &mut dyn FnMut()) -> Result<()> {
        let mut delay = self.backoff.min;
        while !stop.load(Ordering::SeqCst) {
            match self.step(stop) {
                Ok(n) => {
                    delay = self.backoff.min;
                    after_pass();
                    if self.reached_stop_height() {
                        if self.bulk_dirty {
                            self.db.flush()?;
                        }
                        eprintln!("index: reached --stop-height");
                        return Ok(());
                    }
                    if n == 0 {
                        if let Err(e) = self.src.wait_for_block(1000) {
                            eprintln!("index: waitfornewblock: {e:#}");
                            sleep_unless_stopped(delay, stop);
                        }
                    }
                }
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => {
                    self.status.retries.fetch_add(1, Ordering::SeqCst);
                    eprintln!("index: {e:#} — retrying in {delay:?}");
                    sleep_unless_stopped(delay, stop);
                    delay = (delay * 2).min(self.backoff.max);
                }
            }
        }
        if self.bulk_dirty {
            self.db.flush()?;
            self.bulk_dirty = false;
        }
        Ok(())
    }
}

fn sleep_unless_stopped(d: Duration, stop: &AtomicBool) {
    let end = std::time::Instant::now() + d;
    while !stop.load(Ordering::SeqCst) {
        let now = std::time::Instant::now();
        if now >= end {
            return;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainstate::BlockTable;
    use crate::db::DbConfig;
    use crate::source::mem::{program, MemSource};
    use arc_swap::ArcSwap;
    use bitcoin::Txid;

    struct Rig {
        _dir: tempfile::TempDir,
        src: Arc<MemSource>,
        syncer: Syncer,
    }

    fn rig(src: Arc<MemSource>, start: u32, stop_height: Option<u32>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        let chain: SharedChain = Arc::new(ArcSwap::from_pointee(BlockTable::default()));
        let syncer = Syncer::new(
            db,
            src.clone(),
            HeaderFormat::BTC,
            chain,
            start,
            FetchConfig { workers: 4 },
            stop_height,
            Backoff {
                min: Duration::from_millis(1),
                max: Duration::from_millis(5),
            },
            Arc::new(SyncStatus::default()),
        );
        Rig {
            _dir: dir,
            src,
            syncer,
        }
    }

    fn tip(s: &Syncer) -> u32 {
        s.chain.load().tip().unwrap().0
    }

    type Snapshot = Vec<(Vec<Txid>, Vec<(bitcoin::OutPoint, crate::keys::UtxoVal)>)>;

    /// Every program's history (as txids) and UTXOs, for both fork salts.
    fn state(s: &Syncer, max_h: u32) -> Snapshot {
        let r = s.db.reader();
        let mut out = Vec::new();
        for h in 0..=max_h {
            for salt in [0u8, 1] {
                let p = program(h, salt);
                let hist = r
                    .history(&p, 1000)
                    .unwrap()
                    .into_iter()
                    .map(|n| r.txid(n).unwrap().unwrap())
                    .collect();
                out.push((hist, r.utxos(&p, 1000).unwrap()));
            }
        }
        out
    }

    #[test]
    fn syncs_from_start_height_to_tip() {
        let mut r = rig(Arc::new(MemSource::new(999)), 10, None);
        let n = r.syncer.step(&AtomicBool::new(false)).unwrap();
        assert_eq!(n, 990);
        let c = r.syncer.chain.load();
        assert_eq!(c.tip().unwrap().0, 999);
        assert!(c.get(10).is_some() && c.get(9).is_none());
        assert_eq!(c.get(999).unwrap().hash, r.src.hash(999));
        assert_eq!(
            r.syncer.db.reader().tip().unwrap(),
            Some((999, r.src.hash(999)))
        );
        assert!(r.syncer.status.follow.load(Ordering::SeqCst));
        assert_eq!(r.syncer.step(&AtomicBool::new(false)).unwrap(), 0);
    }

    #[test]
    fn blocks_within_288_of_tip_get_undo() {
        let r = {
            let mut r = rig(Arc::new(MemSource::new(1000)), 0, None);
            r.syncer.step(&AtomicBool::new(false)).unwrap();
            r
        };
        let c = r.syncer.chain.load();
        let err = r
            .syncer
            .db
            .rollback(711, c.get(711).unwrap(), None)
            .unwrap_err();
        assert!(err.downcast_ref::<NoUndo>().is_some(), "{err:#}");
        let prev = Some((999, c.get(999).unwrap().hash));
        r.syncer
            .db
            .rollback(1000, c.get(1000).unwrap(), prev)
            .unwrap();
    }

    #[test]
    fn reorg_of_two_blocks_matches_a_fresh_sync() {
        let mut r = rig(Arc::new(MemSource::new(1000)), 0, None);
        r.syncer.step(&AtomicBool::new(false)).unwrap();
        r.src.reorg(999, 1001, 1);
        r.syncer.step(&AtomicBool::new(false)).unwrap();
        assert_eq!(tip(&r.syncer), 1001);
        assert_eq!(
            r.syncer.chain.load().get(999).unwrap().hash,
            r.src.hash(999)
        );

        let fresh_src = Arc::new(MemSource::new(1000));
        fresh_src.reorg(999, 1001, 1);
        let mut fresh = rig(fresh_src, 0, None);
        fresh.syncer.step(&AtomicBool::new(false)).unwrap();
        assert_eq!(state(&r.syncer, 1001), state(&fresh.syncer, 1001));
        assert_eq!(
            r.syncer.db.load_blocks().unwrap(),
            fresh.syncer.db.load_blocks().unwrap()
        );
    }

    #[test]
    fn a_shorter_node_chain_is_unwound() {
        let mut r = rig(Arc::new(MemSource::new(1000)), 0, None);
        r.syncer.step(&AtomicBool::new(false)).unwrap();
        r.src.reorg(998, 998, 1);
        r.syncer.step(&AtomicBool::new(false)).unwrap();
        assert_eq!(tip(&r.syncer), 998);
        assert_eq!(
            r.syncer.chain.load().get(998).unwrap().hash,
            r.src.hash(998)
        );
    }

    #[test]
    fn reorg_deeper_than_undo_is_fatal() {
        let mut r = rig(Arc::new(MemSource::new(1000)), 0, None);
        r.syncer.step(&AtomicBool::new(false)).unwrap();
        r.src.reorg(701, 1000, 1);
        let err = r.syncer.step(&AtomicBool::new(false)).unwrap_err();
        assert!(is_fatal(&err), "{err:#}");
        let err = r
            .syncer
            .run(&AtomicBool::new(false), &mut || {})
            .unwrap_err();
        assert!(is_fatal(&err));
    }

    #[test]
    fn transient_source_errors_back_off_and_recover() {
        let src = Arc::new(MemSource::new(300));
        src.tip_failures.store(3, Ordering::SeqCst);
        let mut r = rig(src, 0, Some(300));
        let mut passes = 0;
        r.syncer
            .run(&AtomicBool::new(false), &mut || passes += 1)
            .unwrap();
        assert_eq!(tip(&r.syncer), 300);
        assert!(r.syncer.status.retries.load(Ordering::SeqCst) >= 3);
        assert!(passes >= 1);
    }

    #[test]
    fn stop_height_stops_cleanly() {
        let mut r = rig(Arc::new(MemSource::new(1000)), 0, Some(500));
        r.syncer.run(&AtomicBool::new(false), &mut || {}).unwrap();
        assert_eq!(tip(&r.syncer), 500);
        assert_eq!(r.syncer.db.reader().tip().unwrap().unwrap().0, 500);
        // Bulk data was flushed: a reopen sees it.
        let path = r._dir.path().to_path_buf();
        drop(r.syncer);
        let db = Db::open(&path, &DbConfig { cache_mb: 8 }, "btc").unwrap();
        assert_eq!(db.load_blocks().unwrap().len(), 501);
    }

    #[test]
    fn stop_flag_ends_run() {
        let src = Arc::new(MemSource::new(1000));
        src.set_delay(|_| Duration::from_millis(1));
        let mut r = rig(src, 0, None);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            s2.store(true, Ordering::SeqCst);
        });
        r.syncer.run(&stop, &mut || {}).unwrap();
        t.join().unwrap();
        let reader = r.syncer.db.reader();
        let db_tip = reader.tip().unwrap().map(|t| t.0);
        assert_eq!(db_tip, r.syncer.chain.load().tip().map(|t| t.0));
    }
}
