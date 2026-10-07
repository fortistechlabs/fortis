//! The v2 RocksDB store: column families, the meta guard (schema + chain),
//! block application as blind puts/deletes, undo records, and snapshot reads.
//! Layout and rationale: `docs/superpowers/specs/2026-10-07-fortis-index-v2-design.md`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, OutPoint, Txid};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, DBCompressionType,
    FlushOptions, Options, ReadOptions, SliceTransform, Snapshot, WriteBatch, WriteOptions, DB,
};

use crate::extract::ParsedBlock;
use crate::keys::{
    height_key, history_key, history_txnum, tip_from, tip_value, txnum_key, utxo_key,
    utxo_outpoint, BlockRec, Program, TxNum, UtxoVal, MAX_TXNUM,
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
const ALL_CFS: [&str; 7] = [
    CF_META, CF_BLOCKS, CF_TXIDS, CF_HISTORY, CF_UTXO, CF_UNDO, CF_RENDER,
];

const META_SCHEMA: &[u8] = b"schema";
const META_CHAIN: &[u8] = b"chain";
const META_TIP: &[u8] = b"tip";

const PROGRAM_LEN: usize = 20;

pub struct DbConfig {
    pub cache_mb: usize,
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
}

#[derive(Debug, thiserror::Error)]
#[error("address too heavy")]
pub struct TooHeavy;

#[derive(Debug, thiserror::Error)]
#[error("no undo for height {height}: reorg deeper than {MAX_REORG_DEPTH}; rebuild with --start-height or restore")]
pub struct NoUndo {
    pub height: u32,
}

#[derive(Clone)]
pub struct Db {
    db: Arc<DB>,
}

/// What `rollback` needs to undo one block: the keys it added and the UTXOs
/// it deleted (with their values). Txids and render rows come from the
/// block's txnum range.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Undo {
    pub first_txnum: TxNum,
    pub n_txs: u32,
    pub added_history: Vec<[u8; 25]>,
    pub added_utxo: Vec<[u8; 56]>,
    pub deleted_utxo: Vec<([u8; 56], UtxoVal)>,
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
        if !r.is_empty() {
            bail!("undo record has {} trailing bytes", r.len());
        }
        Ok(Undo {
            first_txnum,
            n_txs,
            added_history,
            added_utxo,
            deleted_utxo,
        })
    }
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
        let cache = Cache::new_lru_cache(cfg.cache_mb.max(1) << 20);
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_atomic_flush(true);
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        opts.increase_parallelism(cores as i32);
        let cfs = ALL_CFS.iter().map(|&name| {
            let big = name == CF_HISTORY || name == CF_UTXO;
            ColumnFamilyDescriptor::new(name, cf_options(&cache, big, big))
        });
        let db = DB::open_cf_descriptors(&opts, path, cfs)
            .with_context(|| format!("opening index database {}", path.display()))?;
        let db = Db { db: Arc::new(db) };
        db.guard(chain_id)?;
        Ok(db)
    }

    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db
            .cf_handle(name)
            .expect("column family created at open")
    }

    fn guard(&self, chain_id: &str) -> Result<()> {
        let meta = self.cf(CF_META);
        let schema = self.db.get_cf(meta, META_SCHEMA)?;
        let chain = self.db.get_cf(meta, META_CHAIN)?;
        match (schema, chain) {
            (None, None) => {
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
        let (cf_txids, cf_hist, cf_utxo) =
            (self.cf(CF_TXIDS), self.cf(CF_HISTORY), self.cf(CF_UTXO));
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
                    let k = history_key(p, n);
                    wb.put_cf(cf_hist, k, []);
                    if keep_undo {
                        undo.added_history.push(k);
                    }
                }
                n += 1;
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
    /// delete the keys it added, restore the UTXOs it spent, drop its txids,
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
            snap: self.db.snapshot(),
        }
    }

    /// Cache the rendered Esplora JSON of confirmed transaction `n`.
    pub fn render_put(&self, n: TxNum, json: &[u8]) -> Result<()> {
        self.db.put_cf(self.cf(CF_RENDER), txnum_key(n), json)?;
        Ok(())
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
    snap: Snapshot<'a>,
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

impl Reader<'_> {
    /// `meta.tip`: the height and hash of the last applied block.
    pub fn tip(&self) -> Result<Option<(u32, BlockHash)>> {
        self.snap
            .get_cf(self.db.cf(CF_META), META_TIP)?
            .map(|v| tip_from(&v))
            .transpose()
    }

    /// Txnums touching `p`, newest first, at most `limit`.
    pub fn history(&self, p: &Program, limit: usize) -> Result<Vec<TxNum>> {
        let mut it = self
            .snap
            .raw_iterator_cf_opt(self.db.cf(CF_HISTORY), prefix_opts(p));
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

    /// Every unspent output of `p`; [`TooHeavy`] if there are more than `max_rows`.
    pub fn utxos(&self, p: &Program, max_rows: usize) -> Result<Vec<(OutPoint, UtxoVal)>> {
        let mut it = self
            .snap
            .raw_iterator_cf_opt(self.db.cf(CF_UTXO), prefix_opts(p));
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
        self.snap
            .get_cf(self.db.cf(CF_UTXO), utxo_key(p, op))?
            .map(|v| UtxoVal::decode(&v))
            .transpose()
    }

    pub fn txid(&self, n: TxNum) -> Result<Option<Txid>> {
        match self.snap.get_cf(self.db.cf(CF_TXIDS), txnum_key(n))? {
            None => Ok(None),
            Some(v) => {
                let a: [u8; 32] = v[..].try_into().context("txid value length")?;
                Ok(Some(Txid::from_byte_array(a)))
            }
        }
    }

    pub fn render_get(&self, n: TxNum) -> Result<Option<Vec<u8>>> {
        Ok(self.snap.get_cf(self.db.cf(CF_RENDER), txnum_key(n))?)
    }
}

#[cfg(test)]
mod tests;
