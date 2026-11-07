//! The v2 RocksDB store: column families, the meta guard (schema + chain),
//! block application as blind puts/deletes, undo records, and snapshot reads.
//! Layout and rationale: `docs/superpowers/specs/2026-10-07-fortis-index-v2-design.md`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, OutPoint, Txid};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, DBCompressionType,
    DBRawIterator, FlushOptions, Options, ReadOptions, SliceTransform, Snapshot, WriteBatch,
    WriteOptions, DB,
};

use crate::extract::ParsedBlock;
use crate::spendcf::{self, BlockSpend, SpendRec};
use crate::keys::{
    height_key, history_key, history_txnum, p2wpkh_scripthash, tip_from, tip_value, txnum_key,
    utxo_key, utxo_outpoint, BlockRec, Program, TxNum, UtxoVal, MAX_TXNUM,
};

pub const SCHEMA_VERSION: u32 = 2;
pub const MAX_REORG_DEPTH: u32 = 288;

const CF_META: &str = "meta";
const CF_BLOCKS: &str = "blocks";
const CF_TXIDS: &str = "txids";
const CF_HISTORY: &str = "history";
const CF_UTXO: &str = "utxo";
const CF_UNDO: &str = "undo";
const CF_RENDER: &str = "render";
/// Esplora script hash (SHA-256 of a P2WPKH script) → its 20-byte program,
/// for `GET /scripthash/:sh/txs` (the Lightning wallet's scan). Written for
/// every program a block touches from when this column family first existed;
/// the program's history itself is all in `history`. Never rolled back: a
/// stale entry only maps a hash to a program whose history says the rest.
const CF_SCRIPTHASH: &str = "scripthash";
/// Spent outpoint ([`spendcf::key`]) → its spender ([`SpendRec`]), for every
/// recorded input ([`spendcf::recorded`]) of every block from
/// [`DbConfig::spends_from`] on. Rolled back with its block (through the undo
/// record). Created empty when an older database is opened; `meta`'s
/// `spends_watermark` says how far it is complete.
const CF_SPENDS: &str = "spends";
const ALL_CFS: [&str; 9] = [
    CF_META, CF_BLOCKS, CF_TXIDS, CF_HISTORY, CF_UTXO, CF_UNDO, CF_RENDER, CF_SCRIPTHASH,
    CF_SPENDS,
];

const META_SCHEMA: &[u8] = b"schema";
const META_CHAIN: &[u8] = b"chain";
const META_TIP: &[u8] = b"tip";
/// The start height it was counted from, then the highest height `h` such
/// that every block from that start through `h` has its spends in `spends`
/// (4 + 4 bytes, BE). Absent: none yet. A mark counted from another start
/// than the configured `spends_from` counts as none (a lowered start leaves
/// the heights below the old one unrecorded).
const META_SPENDS_WATERMARK: &[u8] = b"spends_watermark";

const PROGRAM_LEN: usize = 20;

pub struct DbConfig {
    pub cache_mb: usize,
    /// Record spends (`spends` column family) for blocks at and above this
    /// height; `None` records none.
    pub spends_from: Option<u32>,
}

/// Bulk: no WAL (crash-safe only up to the last flush, which `meta.tip`
/// tracks). Durable: WAL + fsync on every batch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Durability {
    Bulk,
    Durable,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("database is for chain {found}, node is {expected}")]
    ChainMismatch { found: String, expected: String },
    #[error(
        "database schema {found} is not {SCHEMA_VERSION} (v1 index? use a new --db directory)"
    )]
    Schema { found: String },
    /// `--spends-from-height` below what this index can backfill.
    #[error("--spends-from-height {from} is below the first block this index holds")]
    SpendsFromBelowIndex { from: u32 },
    #[error("--spends-from-height {from} is below --start-height {start}")]
    SpendsFromBelowStart { from: u32, start: u32 },
}

#[derive(Debug, thiserror::Error)]
#[error("address too heavy")]
pub struct TooHeavy;

/// Unspent outputs left out of a listing past its cap: how many, worth how much.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Unlisted {
    pub count: u64,
    pub value: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("no undo for height {height}: reorg deeper than {MAX_REORG_DEPTH}; rebuild with --start-height or restore")]
pub struct NoUndo {
    pub height: u32,
}

/// Why [`Db::write_backfill`] wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackfillRefused {
    #[error("the watermark moved (planned from {planned:?}, now {now:?})")]
    MarkMoved { planned: Option<u32>, now: Option<u32> },
    #[error("block {height} is {node} on the node but {index:?} in the index")]
    NotIndexed {
        height: u32,
        node: BlockHash,
        index: Option<BlockHash>,
    },
}

#[derive(Clone)]
pub struct Db {
    db: Arc<DB>,
    /// A read-only secondary: no snapshots (RocksDB doesn't support them
    /// there); its view only changes on `catch_up`.
    secondary: bool,
    render: Arc<RenderCap>,
    spends_from: Option<u32>,
    /// Held by every write that reads the spends watermark and writes it
    /// back (block apply, rollback, a backfill batch), so the mark one makes
    /// is never based on a value another has since changed.
    spends_lock: Arc<Mutex<()>>,
}

/// Rendered transactions cached at most: every one any client asks about
/// is cached, so without a cap anyone walking addresses fills the disk.
/// Past it the oldest half goes (a dropped one is rendered from the node
/// again when asked for). About 1–2 GB at the cap.
pub const RENDER_MAX_ROWS: u64 = 1_000_000;

/// The render cache's row count (an estimate at open, exact after a trim)
/// and its cap.
struct RenderCap {
    rows: std::sync::atomic::AtomicU64,
    max: std::sync::atomic::AtomicU64,
    trimming: std::sync::atomic::AtomicBool,
}

impl RenderCap {
    fn new(db: &DB) -> Arc<RenderCap> {
        let rows = db
            .cf_handle(CF_RENDER)
            .and_then(|cf| db.property_int_value_cf(&cf, "rocksdb.estimate-num-keys").ok().flatten())
            .unwrap_or(0);
        Arc::new(RenderCap {
            rows: rows.into(),
            max: RENDER_MAX_ROWS.into(),
            trimming: false.into(),
        })
    }
}

/// What `rollback` needs to undo one block: the keys it added and the UTXOs
/// it deleted (with their values). Txids and render rows come from the
/// block's txnum range. `added_spends` is an optional trailing section,
/// written only when non-empty, so a record with none (every record written
/// before `spends` existed) has exactly the old layout.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Undo {
    pub first_txnum: TxNum,
    pub n_txs: u32,
    pub added_history: Vec<[u8; 25]>,
    pub added_utxo: Vec<[u8; 56]>,
    pub deleted_utxo: Vec<([u8; 56], UtxoVal)>,
    pub added_spends: Vec<[u8; 36]>,
}

impl Undo {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(
            21 + self.added_history.len() * 25
                + self.added_utxo.len() * 56
                + self.deleted_utxo.len() * 68,
        );
        b.extend_from_slice(&txnum_key(self.first_txnum));
        b.extend_from_slice(&self.n_txs.to_be_bytes());
        b.extend_from_slice(&(self.added_history.len() as u32).to_be_bytes());
        for k in &self.added_history {
            b.extend_from_slice(k);
        }
        b.extend_from_slice(&(self.added_utxo.len() as u32).to_be_bytes());
        for k in &self.added_utxo {
            b.extend_from_slice(k);
        }
        b.extend_from_slice(&(self.deleted_utxo.len() as u32).to_be_bytes());
        for (k, v) in &self.deleted_utxo {
            b.extend_from_slice(k);
            b.extend_from_slice(&v.encode());
        }
        if !self.added_spends.is_empty() {
            b.extend_from_slice(&(self.added_spends.len() as u32).to_be_bytes());
            for k in &self.added_spends {
                b.extend_from_slice(k);
            }
        }
        b
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut r = b;
        fn take<'a>(r: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
            if r.len() < n {
                bail!("undo record truncated");
            }
            let (h, t) = r.split_at(n);
            *r = t;
            Ok(h)
        }
        fn count(r: &mut &[u8]) -> Result<usize> {
            Ok(u32::from_be_bytes(take(r, 4)?.try_into().unwrap()) as usize)
        }
        let first_txnum = crate::keys::txnum_from(take(&mut r, 5)?);
        let n_txs = u32::from_be_bytes(take(&mut r, 4)?.try_into().unwrap());
        let n = count(&mut r)?;
        let added_history = (0..n)
            .map(|_| Ok(take(&mut r, 25)?.try_into().unwrap()))
            .collect::<Result<_>>()?;
        let n = count(&mut r)?;
        let added_utxo = (0..n)
            .map(|_| Ok(take(&mut r, 56)?.try_into().unwrap()))
            .collect::<Result<_>>()?;
        let n = count(&mut r)?;
        let deleted_utxo = (0..n)
            .map(|_| {
                let k: [u8; 56] = take(&mut r, 56)?.try_into().unwrap();
                Ok((k, UtxoVal::decode(take(&mut r, 12)?)?))
            })
            .collect::<Result<_>>()?;
        let added_spends = if r.is_empty() {
            Vec::new()
        } else {
            let n = count(&mut r)?;
            (0..n)
                .map(|_| Ok(take(&mut r, 36)?.try_into().unwrap()))
                .collect::<Result<_>>()?
        };
        if !r.is_empty() {
            bail!("undo record has {} trailing bytes", r.len());
        }
        Ok(Undo {
            first_txnum,
            n_txs,
            added_history,
            added_utxo,
            deleted_utxo,
            added_spends,
        })
    }
}

/// The `spends_watermark` meta value: start height, then watermark (BE).
fn spends_mark(from: u32, watermark: u32) -> [u8; 8] {
    let mut v = [0u8; 8];
    v[..4].copy_from_slice(&from.to_be_bytes());
    v[4..].copy_from_slice(&watermark.to_be_bytes());
    v
}

fn db_options(cfg: &DbConfig) -> (Options, Vec<ColumnFamilyDescriptor>) {
    let cache = Cache::new_lru_cache(cfg.cache_mb.max(1) << 20);
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);
    opts.set_atomic_flush(true);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    opts.increase_parallelism(cores as i32);
    let cfs = ALL_CFS
        .iter()
        .map(|&name| {
            let big = name == CF_HISTORY || name == CF_UTXO || name == CF_SPENDS;
            ColumnFamilyDescriptor::new(name, cf_options(&cache, big, big))
        })
        .collect();
    (opts, cfs)
}

fn cf_options(cache: &Cache, prefix_bloom: bool, big_buffers: bool) -> Options {
    let mut bb = BlockBasedOptions::default();
    bb.set_block_cache(cache);
    if prefix_bloom {
        bb.set_bloom_filter(10.0, false);
        bb.set_whole_key_filtering(false);
    }
    let mut o = Options::default();
    o.set_block_based_table_factory(&bb);
    o.set_compression_type(DBCompressionType::Lz4);
    o.set_bottommost_compression_type(DBCompressionType::Zstd);
    if prefix_bloom {
        o.set_prefix_extractor(SliceTransform::create_fixed_prefix(PROGRAM_LEN));
        o.set_memtable_prefix_bloom_ratio(0.1);
    }
    if big_buffers {
        o.set_write_buffer_size(256 << 20);
    }
    o
}

impl Db {
    /// Open (creating if absent) the database at `path` for `chain_id`
    /// (`"btc"` or `"xbt@<fork height>"`). Refuses a v1 database or one built
    /// for another chain with an [`OpenError`].
    pub fn open(path: &Path, cfg: &DbConfig, chain_id: &str) -> Result<Db> {
        if path.join("CURRENT").exists() {
            let existing = DB::list_cf(&Options::default(), path)
                .with_context(|| format!("listing column families of {}", path.display()))?;
            if !existing.iter().any(|c| c == CF_META) {
                let found = if existing.iter().any(|c| c == "outputs") {
                    "1"
                } else {
                    "none"
                };
                return Err(OpenError::Schema {
                    found: found.into(),
                }
                .into());
            }
        }
        let (opts, cfs) = db_options(cfg);
        let db = DB::open_cf_descriptors(&opts, path, cfs)
            .with_context(|| format!("opening index database {}", path.display()))?;
        let render = RenderCap::new(&db);
        let db = Db {
            db: Arc::new(db),
            secondary: false,
            render,
            spends_from: cfg.spends_from,
            spends_lock: Arc::default(),
        };
        db.guard(chain_id, true)?;
        Ok(db)
    }

    /// Open read-only as a RocksDB secondary of the (possibly running)
    /// primary at `path`, keeping its own files in `secondary`. Sees only what
    /// the primary has flushed or logged; refresh with [`Db::catch_up`].
    pub fn open_secondary(
        path: &Path,
        secondary: &Path,
        cfg: &DbConfig,
        chain_id: &str,
    ) -> Result<Db> {
        let (mut opts, cfs) = db_options(cfg);
        opts.create_if_missing(false);
        opts.set_max_open_files(-1);
        let db = DB::open_cf_descriptors_as_secondary(&opts, path, secondary, cfs)
            .with_context(|| format!("opening index database {} as secondary", path.display()))?;
        let render = RenderCap::new(&db);
        let db = Db {
            db: Arc::new(db),
            secondary: true,
            render,
            spends_from: cfg.spends_from,
            spends_lock: Arc::default(),
        };
        db.guard(chain_id, false)?;
        Ok(db)
    }

    /// Secondary only: pick up the primary's latest writes.
    pub fn catch_up(&self) -> Result<()> {
        self.db
            .try_catch_up_with_primary()
            .context("catching up with the primary index")
    }

    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db
            .cf_handle(name)
            .expect("column family created at open")
    }

    fn guard(&self, chain_id: &str, may_init: bool) -> Result<()> {
        let meta = self.cf(CF_META);
        let schema = self.db.get_cf(meta, META_SCHEMA)?;
        let chain = self.db.get_cf(meta, META_CHAIN)?;
        match (schema, chain) {
            (None, None) if may_init => {
                let mut wb = WriteBatch::default();
                wb.put_cf(meta, META_SCHEMA, SCHEMA_VERSION.to_be_bytes());
                wb.put_cf(meta, META_CHAIN, chain_id.as_bytes());
                self.db.write_opt(wb, &durable())?;
                Ok(())
            }
            (schema, chain) => {
                let found = match schema.as_deref() {
                    Some(b) if b.len() == 4 => {
                        u32::from_be_bytes(b.try_into().unwrap()).to_string()
                    }
                    Some(b) => format!("{b:?}"),
                    None => "none".into(),
                };
                if found != SCHEMA_VERSION.to_string() {
                    return Err(OpenError::Schema { found }.into());
                }
                let found =
                    String::from_utf8_lossy(chain.as_deref().unwrap_or_default()).into_owned();
                if found != chain_id {
                    return Err(OpenError::ChainMismatch {
                        found,
                        expected: chain_id.into(),
                    }
                    .into());
                }
                Ok(())
            }
        }
    }

    /// The height `spends` is recorded from, as configured.
    pub fn spends_from(&self) -> Option<u32> {
        self.spends_from
    }

    /// The highest height through which `spends` is complete (from
    /// [`Db::spends_from`]); `None` if no block is yet (or on a read error).
    pub fn spends_watermark(&self) -> Option<u32> {
        self.read_spends_watermark().ok().flatten()
    }

    /// The watermark, if it was counted from the configured `spends_from`.
    fn read_spends_watermark(&self) -> Result<Option<u32>> {
        Ok(self
            .read_spends_mark()?
            .filter(|(from, _)| Some(*from) == self.spends_from)
            .map(|(_, w)| w))
    }

    /// The stored (start height, watermark), whatever start is configured.
    fn read_spends_mark(&self) -> Result<Option<(u32, u32)>> {
        let raw = self.db.get_cf(self.cf(CF_META), META_SPENDS_WATERMARK)?;
        raw.map(|b| {
            let b: [u8; 8] = b[..].try_into().context("spends_watermark length")?;
            Ok((
                u32::from_be_bytes(b[..4].try_into().unwrap()),
                u32::from_be_bytes(b[4..].try_into().unwrap()),
            ))
        })
        .transpose()
    }

    /// Who spent `op`, if a recorded input in an indexed block did.
    pub fn spend_of(&self, op: &OutPoint) -> Result<Option<SpendRec>> {
        self.db
            .get_cf(self.cf(CF_SPENDS), spendcf::key(op))?
            .map(|v| SpendRec::decode(&v).context("spends row length"))
            .transpose()
    }

    fn lock_spends(&self) -> MutexGuard<'_, ()> {
        self.spends_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write one backfill batch: `blocks` (consecutive heights, each with its
    /// hash and recorded spends) as `spends` rows, and the watermark moved to
    /// the last of them — all in one durable batch. Written only if the
    /// watermark is still `expect` (what the backfill planned from, so the
    /// first height is `expect + 1`, or the start height when `None`) and
    /// every block is the index's own block at its height (none rolled back or
    /// not yet indexed). Otherwise nothing is written and the reason is
    /// returned: plan again.
    pub fn write_backfill(
        &self,
        expect: Option<u32>,
        blocks: &[(u32, BlockHash, Vec<BlockSpend>)],
    ) -> Result<Result<(), BackfillRefused>> {
        let Some(from) = self.spends_from else {
            bail!("spends backfill without a start height");
        };
        let Some(&(last, _, _)) = blocks.last() else {
            return Ok(Ok(()));
        };
        let first = expect.map_or(from, |w| w + 1);
        let _spends = self.lock_spends();
        let now = self.read_spends_watermark()?;
        if now != expect {
            return Ok(Err(BackfillRefused::MarkMoved { planned: expect, now }));
        }
        let cf_blocks = self.cf(CF_BLOCKS);
        for (i, (h, hash, _)) in blocks.iter().enumerate() {
            if *h != first + i as u32 {
                bail!("spends backfill batch: height {h} where {} was due", first + i as u32);
            }
            let ours = self.db.get_cf(cf_blocks, height_key(*h))?;
            let ours = ours.map(|v| BlockRec::decode(&v)).transpose()?.map(|r| r.hash);
            if ours != Some(*hash) {
                return Ok(Err(BackfillRefused::NotIndexed { height: *h, node: *hash, index: ours }));
            }
        }
        // Bulk (WAL-less) block writes may not be flushed yet: a crash must
        // not replay this batch's mark over blocks it lost.
        self.flush()?;
        let mut wb = WriteBatch::default();
        let cf_spends = self.cf(CF_SPENDS);
        for (h, _, spends) in blocks {
            for s in spends {
                let v = SpendRec { txid: s.txid, vin: s.vin, height: *h };
                wb.put_cf(cf_spends, spendcf::key(&s.prevout), v.encode());
            }
        }
        wb.put_cf(self.cf(CF_META), META_SPENDS_WATERMARK, spends_mark(from, last));
        self.db.write_opt(wb, &durable()).context("writing spends backfill batch")?;
        Ok(Ok(()))
    }

    /// Every `blocks` row, ascending by height.
    pub fn load_blocks(&self) -> Result<Vec<(u32, BlockRec)>> {
        self.db
            .iterator_cf(self.cf(CF_BLOCKS), rocksdb::IteratorMode::Start)
            .map(|kv| {
                let (k, v) = kv?;
                let h = u32::from_be_bytes(k[..].try_into().context("blocks key length")?);
                Ok((h, BlockRec::decode(&v)?))
            })
            .collect()
    }

    /// Apply `blocks` (consecutive heights) in one `WriteBatch`, numbering
    /// their transactions from `next_txnum`. With `keep_undo`, also writes an
    /// undo record per block. Returns the new `blocks` rows, in order.
    pub fn apply(
        &self,
        blocks: &[ParsedBlock],
        next_txnum: TxNum,
        d: Durability,
        keep_undo: bool,
    ) -> Result<Vec<BlockRec>> {
        let (cf_txids, cf_hist, cf_utxo, cf_sh, cf_spends) = (
            self.cf(CF_TXIDS),
            self.cf(CF_HISTORY),
            self.cf(CF_UTXO),
            self.cf(CF_SCRIPTHASH),
            self.cf(CF_SPENDS),
        );
        let _spends = self.lock_spends();
        let mut watermark = match self.spends_from {
            Some(_) => self.read_spends_watermark()?,
            None => None,
        };
        let mut wb = WriteBatch::default();
        let mut recs = Vec::with_capacity(blocks.len());
        let mut n = next_txnum;
        // UTXOs funded earlier in this batch: a later spend's undo needs their
        // value, which `multi_get` (reading the committed DB) cannot see.
        let mut batch_utxos: HashMap<[u8; 56], UtxoVal> = HashMap::new();
        for b in blocks {
            let first_txnum = n;
            let mut undo = Undo {
                first_txnum,
                n_txs: b.txs.len() as u32,
                ..Undo::default()
            };
            if keep_undo {
                undo.deleted_utxo = self.spent_values(b, &batch_utxos)?;
            }
            let mut added_utxo: HashSet<[u8; 56]> = HashSet::new();
            for tx in &b.txs {
                if n > MAX_TXNUM {
                    bail!("txnum space exhausted at height {}", b.height);
                }
                wb.put_cf(cf_txids, txnum_key(n), tx.txid.to_byte_array());
                let mut touched: HashSet<Program> = HashSet::new();
                for f in &tx.funded {
                    touched.insert(f.program);
                    let op = OutPoint {
                        txid: tx.txid,
                        vout: f.vout,
                    };
                    let k = utxo_key(&f.program, &op);
                    let v = UtxoVal {
                        value: f.value,
                        height: b.height,
                    };
                    wb.put_cf(cf_utxo, k, v.encode());
                    if keep_undo {
                        batch_utxos.insert(k, v);
                        added_utxo.insert(k);
                    }
                }
                for s in &tx.spent {
                    touched.insert(s.program);
                    let k = utxo_key(&s.program, &s.prevout);
                    wb.delete_cf(cf_utxo, k);
                    if keep_undo {
                        batch_utxos.remove(&k);
                        // Created and spent within this block: no net change,
                        // so rollback must neither delete nor restore it.
                        if added_utxo.remove(&k) {
                            undo.deleted_utxo.retain(|(dk, _)| dk != &k);
                        }
                    }
                }
                for p in &touched {
                    wb.put_cf(cf_sh, p2wpkh_scripthash(p), p);
                    let k = history_key(p, n);
                    wb.put_cf(cf_hist, k, []);
                    if keep_undo {
                        undo.added_history.push(k);
                    }
                }
                n += 1;
            }
            if let Some(from) = self.spends_from.filter(|f| b.height >= *f) {
                for s in &b.spends {
                    let k = spendcf::key(&s.prevout);
                    let v = SpendRec {
                        txid: s.txid,
                        vin: s.vin,
                        height: b.height,
                    };
                    wb.put_cf(cf_spends, k, v.encode());
                    if keep_undo {
                        undo.added_spends.push(k);
                    }
                }
                // Advance only over contiguous heights: from the start height
                // (whatever the old mark), or from the block before.
                let follows = watermark.and_then(|w| w.checked_add(1)) == Some(b.height);
                if b.height == from || follows {
                    watermark = Some(b.height);
                    wb.put_cf(
                        self.cf(CF_META),
                        META_SPENDS_WATERMARK,
                        spends_mark(from, b.height),
                    );
                }
            }
            let rec = BlockRec {
                hash: b.hash,
                time: b.time,
                first_txnum,
                n_txs: b.txs.len() as u32,
            };
            wb.put_cf(self.cf(CF_BLOCKS), height_key(b.height), rec.encode());
            if keep_undo {
                undo.added_utxo = added_utxo.into_iter().collect();
                wb.put_cf(self.cf(CF_UNDO), height_key(b.height), undo.encode());
            }
            recs.push(rec);
        }
        if let Some(last) = blocks.last() {
            wb.put_cf(
                self.cf(CF_META),
                META_TIP,
                tip_value(last.height, &last.hash),
            );
        }
        let wo = match d {
            Durability::Bulk => {
                let mut wo = WriteOptions::default();
                wo.disable_wal(true);
                wo
            }
            Durability::Durable => durable(),
        };
        self.db.write_opt(wb, &wo).context("writing block batch")?;
        Ok(recs)
    }

    /// The (key, value) of every UTXO `b` spends that exists before `b`
    /// (committed, or funded earlier in the same batch).
    fn spent_values(
        &self,
        b: &ParsedBlock,
        batch_utxos: &HashMap<[u8; 56], UtxoVal>,
    ) -> Result<Vec<([u8; 56], UtxoVal)>> {
        let keys: Vec<[u8; 56]> = b
            .txs
            .iter()
            .flat_map(|tx| tx.spent.iter().map(|s| utxo_key(&s.program, &s.prevout)))
            .collect();
        let cf = self.cf(CF_UTXO);
        let committed = self.db.multi_get_cf(keys.iter().map(|k| (cf, k)));
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (k, got) in keys.into_iter().zip(committed) {
            if !seen.insert(k) {
                continue;
            }
            let v = match batch_utxos.get(&k) {
                Some(v) => Some(*v),
                None => got?.map(|v| UtxoVal::decode(&v)).transpose()?,
            };
            if let Some(v) = v {
                out.push((k, v));
            }
        }
        Ok(out)
    }

    /// Undo block `height` (whose row is `rec`) in one durable `WriteBatch`:
    /// delete the keys it added (its `spends` rows included), lower the spends
    /// watermark below it, restore the UTXOs it spent, drop its txids,
    /// render cache rows, `blocks` and `undo` rows, and set `meta.tip` to
    /// `new_tip` (removed when `None`). [`NoUndo`] if it has no undo record.
    pub fn rollback(
        &self,
        height: u32,
        rec: &BlockRec,
        new_tip: Option<(u32, BlockHash)>,
    ) -> Result<()> {
        let cf_undo = self.cf(CF_UNDO);
        let Some(raw) = self.db.get_cf(cf_undo, height_key(height))? else {
            return Err(NoUndo { height }.into());
        };
        let undo =
            Undo::decode(&raw).with_context(|| format!("undo record for height {height}"))?;
        if (undo.first_txnum, undo.n_txs) != (rec.first_txnum, rec.n_txs) {
            bail!(
                "undo for height {height} covers txnums {}+{}, block row says {}+{}",
                undo.first_txnum,
                undo.n_txs,
                rec.first_txnum,
                rec.n_txs
            );
        }
        let (cf_hist, cf_utxo) = (self.cf(CF_HISTORY), self.cf(CF_UTXO));
        let _spends = self.lock_spends();
        let mut wb = WriteBatch::default();
        for k in &undo.added_history {
            wb.delete_cf(cf_hist, k);
        }
        for k in &undo.added_utxo {
            wb.delete_cf(cf_utxo, k);
        }
        for (k, v) in &undo.deleted_utxo {
            wb.put_cf(cf_utxo, k, v.encode());
        }
        let cf_spends = self.cf(CF_SPENDS);
        for k in &undo.added_spends {
            wb.delete_cf(cf_spends, k);
        }
        // Whatever start is configured now: a stored mark must never outlive
        // the block it covers.
        if let Some((from, w)) = self.read_spends_mark()? {
            if w == height {
                match height.checked_sub(1).filter(|h| *h >= from) {
                    Some(h) => wb.put_cf(self.cf(CF_META), META_SPENDS_WATERMARK, spends_mark(from, h)),
                    None => wb.delete_cf(self.cf(CF_META), META_SPENDS_WATERMARK),
                }
            }
        }
        let end = rec.first_txnum + rec.n_txs as TxNum;
        if end > rec.first_txnum {
            let (lo, hi) = (txnum_key(rec.first_txnum), txnum_key(end));
            wb.delete_range_cf(self.cf(CF_TXIDS), lo, hi);
            wb.delete_range_cf(self.cf(CF_RENDER), lo, hi);
        }
        wb.delete_cf(self.cf(CF_BLOCKS), height_key(height));
        wb.delete_cf(cf_undo, height_key(height));
        match new_tip {
            Some((h, hash)) => wb.put_cf(self.cf(CF_META), META_TIP, tip_value(h, &hash)),
            None => wb.delete_cf(self.cf(CF_META), META_TIP),
        }
        self.db
            .write_opt(wb, &durable())
            .context("writing rollback batch")
    }

    /// Drop undo records for heights below `below`.
    pub fn prune_undo(&self, below: u32) -> Result<()> {
        let mut wb = WriteBatch::default();
        wb.delete_range_cf(self.cf(CF_UNDO), height_key(0), height_key(below));
        self.db
            .write_opt(wb, &durable())
            .context("pruning undo records")
    }

    /// Persist every memtable (all column families, atomically).
    pub fn flush(&self) -> Result<()> {
        let cfs: Vec<&ColumnFamily> = ALL_CFS.iter().map(|n| self.cf(n)).collect();
        let mut fo = FlushOptions::default();
        fo.set_wait(true);
        self.db
            .flush_cfs_opt(&cfs, &fo)
            .context("flushing index database")
    }

    /// Full compaction of `history` and `utxo` on a background thread (after
    /// the bulk load, whose SST layout is write-optimised).
    pub fn compact_background(&self) {
        let db = self.db.clone();
        std::thread::spawn(move || {
            for name in [CF_HISTORY, CF_UTXO] {
                if let Some(cf) = db.cf_handle(name) {
                    db.compact_range_cf(cf, None::<&[u8]>, None::<&[u8]>);
                }
            }
            eprintln!("index: background compaction finished");
        });
    }

    pub fn reader(&self) -> Reader<'_> {
        Reader {
            db: self,
            snap: (!self.secondary).then(|| self.db.snapshot()),
        }
    }

    /// Cache the rendered Esplora JSON of confirmed transaction `n`, keeping
    /// the cache to [`RENDER_MAX_ROWS`].
    pub fn render_put(&self, n: TxNum, json: &[u8]) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        self.db.put_cf(self.cf(CF_RENDER), txnum_key(n), json)?;
        if self.render.rows.fetch_add(1, Relaxed) + 1 > self.render.max.load(Relaxed) {
            self.trim_render()?;
        }
        Ok(())
    }

    /// Drop the oldest render rows (lowest txnums), keeping half the cap.
    /// Two key scans: rare (once per half a cap of new rows), so cheap overall.
    fn trim_render(&self) -> Result<()> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        if self
            .render
            .trimming
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_err()
        {
            return Ok(());
        }
        struct Reset<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Release);
            }
        }
        let _guard = Reset(&self.render.trimming);
        let cf = self.cf(CF_RENDER);
        let keep = self.render.max.load(Relaxed) / 2;
        let total = self.db.iterator_cf(cf, rocksdb::IteratorMode::Start).count() as u64;
        if total > keep {
            if let Some(item) = self.db.iterator_cf(cf, rocksdb::IteratorMode::Start).nth((total - keep) as usize) {
                let (first_kept, _) = item?;
                self.db.delete_range_cf(cf, &txnum_key(0)[..], &first_kept[..])?;
            }
        }
        self.render.rows.store(total.min(keep), Relaxed);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_render_cap(self, max: u64) -> Db {
        self.render.max.store(max, std::sync::atomic::Ordering::Relaxed);
        self
    }
}

fn durable() -> WriteOptions {
    let mut wo = WriteOptions::default();
    wo.set_sync(true);
    wo
}

/// A consistent point-in-time view of the database.
pub struct Reader<'a> {
    db: &'a Db,
    snap: Option<Snapshot<'a>>,
}

fn prefix_opts(p: &Program) -> ReadOptions {
    let mut ro = ReadOptions::default();
    ro.set_prefix_same_as_start(true);
    ro.set_iterate_lower_bound(p.to_vec());
    let mut upper = p.to_vec();
    // Programs are fixed-length, so the next prefix is a safe exclusive bound
    // unless every byte is 0xff — then the prefix check below suffices.
    if let Some(i) = upper.iter().rposition(|&b| b != 0xff) {
        upper[i] += 1;
        upper.truncate(i + 1);
        ro.set_iterate_upper_bound(upper);
    }
    ro
}

impl<'a> Reader<'a> {
    fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let cf = self.db.cf(cf);
        Ok(match &self.snap {
            Some(s) => s.get_cf(cf, key)?,
            None => self.db.db.get_cf(cf, key)?,
        })
    }

    fn raw_iter(&self, cf: &str, ro: ReadOptions) -> DBRawIterator<'_> {
        let cf = self.db.cf(cf);
        match &self.snap {
            Some(s) => s.raw_iterator_cf_opt(cf, ro),
            None => self.db.db.raw_iterator_cf_opt(cf, ro),
        }
    }

    /// `meta.tip`: the height and hash of the last applied block.
    pub fn tip(&self) -> Result<Option<(u32, BlockHash)>> {
        self.get(CF_META, META_TIP)?
            .map(|v| tip_from(&v))
            .transpose()
    }

    /// The program of the first `history` (or `utxo`) key at or after `key`
    /// in total order — a random seek target for sampling.
    pub fn program_at_or_after(&self, history: bool, key: &[u8]) -> Result<Option<Program>> {
        let cf = if history { CF_HISTORY } else { CF_UTXO };
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);
        let mut it = self.raw_iter(cf, ro);
        it.seek(key);
        if !it.valid() {
            it.status()?;
            return Ok(None);
        }
        Ok(it
            .key()
            .and_then(|k| k.get(..PROGRAM_LEN))
            .map(|p| p.try_into().unwrap()))
    }

    /// The P2WPKH program whose script hashes to `sh` (Esplora's script hash:
    /// SHA-256 of the script, not reversed), if a block indexed since the
    /// `scripthash` column family existed touched it.
    pub fn program_for_scripthash(&self, sh: &[u8; 32]) -> Result<Option<Program>> {
        Ok(self
            .get(CF_SCRIPTHASH, sh)?
            .and_then(|v| <Program>::try_from(v.as_slice()).ok()))
    }

    /// Txnums touching `p`, newest first, at most `limit`.
    pub fn history(&self, p: &Program, limit: usize) -> Result<Vec<TxNum>> {
        let mut it = self.raw_iter(CF_HISTORY, prefix_opts(p));
        it.seek_for_prev(history_key(p, MAX_TXNUM));
        let mut out = Vec::new();
        while out.len() < limit && it.valid() {
            let k = it.key().unwrap();
            if !k.starts_with(p) {
                break;
            }
            out.push(history_txnum(k));
            it.prev();
        }
        it.status()?;
        Ok(out)
    }

    /// The `max_rows` largest unspent outputs of `p` that `keep` accepts, and
    /// the count and sum of the rest — for an address dusted past the cap,
    /// which still has a full balance. Streams the rows through a min-heap,
    /// so memory stays at `max_rows` however many there are.
    pub fn utxos_largest(
        &self,
        p: &Program,
        max_rows: usize,
        keep: impl Fn(&OutPoint) -> bool,
    ) -> Result<(Vec<(OutPoint, UtxoVal)>, Unlisted)> {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut it = self.raw_iter(CF_UTXO, prefix_opts(p));
        it.seek(p);
        // Smallest value on top; ties broken by outpoint so the order is total.
        let mut heap: BinaryHeap<Reverse<(u64, OutPoint, Vec<u8>)>> = BinaryHeap::new();
        let mut rest = Unlisted::default();
        while it.valid() {
            let k = it.key().unwrap();
            if !k.starts_with(p) {
                break;
            }
            let op = utxo_outpoint(k);
            if keep(&op) {
                let raw = it.value().unwrap().to_vec();
                let value = UtxoVal::decode(&raw)?.value;
                heap.push(Reverse((value, op, raw)));
                if heap.len() > max_rows {
                    let Reverse((v, ..)) = heap.pop().expect("over the cap");
                    rest.count += 1;
                    rest.value += v;
                }
            }
            it.next();
        }
        it.status()?;
        let listed = heap
            .into_iter()
            .map(|Reverse((_, op, raw))| Ok((op, UtxoVal::decode(&raw)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok((listed, rest))
    }

    /// Every unspent output of `p`; [`TooHeavy`] if there are more than `max_rows`.
    pub fn utxos(&self, p: &Program, max_rows: usize) -> Result<Vec<(OutPoint, UtxoVal)>> {
        let mut it = self.raw_iter(CF_UTXO, prefix_opts(p));
        it.seek(p);
        let mut out = Vec::new();
        while it.valid() {
            let k = it.key().unwrap();
            if !k.starts_with(p) {
                break;
            }
            if out.len() == max_rows {
                return Err(TooHeavy.into());
            }
            out.push((utxo_outpoint(k), UtxoVal::decode(it.value().unwrap())?));
            it.next();
        }
        it.status()?;
        Ok(out)
    }

    pub fn utxo(&self, p: &Program, op: &OutPoint) -> Result<Option<UtxoVal>> {
        self.get(CF_UTXO, &utxo_key(p, op))?
            .map(|v| UtxoVal::decode(&v))
            .transpose()
    }

    pub fn txid(&self, n: TxNum) -> Result<Option<Txid>> {
        match self.get(CF_TXIDS, &txnum_key(n))? {
            None => Ok(None),
            Some(v) => {
                let a: [u8; 32] = v[..].try_into().context("txid value length")?;
                Ok(Some(Txid::from_byte_array(a)))
            }
        }
    }

    pub fn render_get(&self, n: TxNum) -> Result<Option<Vec<u8>>> {
        self.get(CF_RENDER, &txnum_key(n))
    }
}

#[cfg(test)]
mod tests;
