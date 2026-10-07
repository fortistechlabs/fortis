//! RocksDB-backed address index. Replaced a SQLite B-tree store (see git
//! history) after live measurement at ~115GB / real production scale showed
//! its per-input spend-check `UPDATE`+`SELECT` pair pegging CPU at 75-100%
//! doing genuine (not wasted) B-tree write work — an LSM engine converts
//! that random-key write pattern into sequential ones, which is also why
//! Bitcoin Core itself uses an LSM engine (LevelDB) for its own UTXO set.
//!
//! Everything is keyed by scriptPubKey hex (`spk`) — an address is turned
//! into its spk on the way in. `sync.rs::block_txs` only ever stores P2WPKH
//! outputs, so `spk` is always exactly 22 bytes (`0014` + 20-byte hash) —
//! every key below relies on that as a hard invariant, not an assumption.
//!
//! One process, one `Arc<DB>` shared between the writer (sync thread) and
//! reader (HTTP server) — see `open_shared_db`. RocksDB is internally
//! thread-safe for concurrent get/put/write/iterate on a shared handle; no
//! external locking is needed, and its sequence-number MVCC guarantees a
//! concurrent reader sees either the full pre-batch or full post-batch
//! state of an `apply_blocks`/`rollback_from` call, never torn.
//!
//! Five column families:
//! - `blocks`: height(BE u64) -> raw 32-byte hash. `tip()` = seek-to-last.
//! - `outputs`: txid(32)+vout(BE u32) -> `OutputRecord` (primary data, the
//!   hot point-lookup/point-write path a spend-check hits).
//! - `utxo_by_spk`: spk+height(BE)+txid+vout -> value_sat. Contains ONLY
//!   currently-unspent outputs, so `utxos_for` is a pure ordered
//!   prefix-scan with no post-filtering.
//! - `history_by_spk`: spk+inv_height(BE)+txid -> empty, where
//!   `inv_height = u64::MAX - height` so a forward scan gives `height DESC,
//!   txid ASC` directly (the actual query order) without reverse iteration.
//! - `undo_log`: height(BE) -> `UndoRecord`, everything needed to reverse
//!   that height's writes on rollback. One `Put` per height instead of
//!   three more permanent secondary indexes mirroring SQLite's
//!   `outputs_height`/`outputs_spent_height`/`history_height` — this is
//!   self-cleaning (deleted after a successful rollback) and doesn't add
//!   cost to the hot write path the way three more per-event indexed writes
//!   would. See `rollback_from`'s doc comment for the one sharp edge this
//!   design has: migrated (pre-existing) data has no undo-log coverage.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use rocksdb::{
    BlockBasedOptions, ColumnFamilyDescriptor, DBCompressionType, DBRecoveryMode, Direction,
    IteratorMode, Options, ReadOptions, SliceTransform, WriteBatch, WriteOptions, DB,
};

#[cfg(test)]
mod tests;

pub(crate) const CF_BLOCKS: &str = "blocks";
pub(crate) const CF_OUTPUTS: &str = "outputs";
pub(crate) const CF_UTXO_BY_SPK: &str = "utxo_by_spk";
pub(crate) const CF_HISTORY_BY_SPK: &str = "history_by_spk";
pub(crate) const CF_UNDO_LOG: &str = "undo_log";

pub(crate) const SPK_LEN: usize = 22;
pub(crate) const TXID_LEN: usize = 32;

/// Duplicate coinbase txids (the pre-BIP34 BIP30 case) are consensus-
/// impossible from this height on.
const BIP34_HEIGHT: u64 = 227_931;

/// One output as the index stores it. `vout` is this output's true position
/// in its transaction — `block_txs` (`sync.rs`) filters most outputs out
/// before they ever reach here, so the surviving ones are no longer
/// contiguous from 0 and can't be re-derived by enumerating this list.
pub struct TxOut {
    pub vout: u32,
    pub spk_hex: String,
    pub value_sat: u64,
}

/// One prevout reference (the outpoint an input spends).
pub struct TxIn {
    pub txid: String,
    pub vout: u32,
}

/// A transaction reduced to what the index needs.
pub struct IndexedTx {
    pub txid: String,
    pub inputs: Vec<TxIn>, // empty for a coinbase
    pub outputs: Vec<TxOut>,
}

pub struct Utxo {
    pub txid: String,
    pub vout: u32,
    pub value_sat: u64,
    pub height: u64,
}

/// A `(txid, height, block_hash)` row from an address's history, newest first.
pub struct HistTx {
    pub txid: String,
    pub height: u64,
    pub block_hash: String,
}

// ---------------------------------------------------------------------
// key/value encoding
// ---------------------------------------------------------------------

pub(crate) fn txid_bytes(txid: &str) -> Result<[u8; TXID_LEN]> {
    let v = hex::decode(txid).with_context(|| format!("txid not hex: {txid}"))?;
    v.try_into().map_err(|v: Vec<u8>| anyhow!("txid wrong length: {} bytes", v.len()))
}

pub(crate) fn spk_bytes(spk_hex: &str) -> Result<[u8; SPK_LEN]> {
    let v = hex::decode(spk_hex).with_context(|| format!("spk not hex: {spk_hex}"))?;
    v.try_into().map_err(|v: Vec<u8>| anyhow!("spk wrong length: {} bytes (P2WPKH must be 22)", v.len()))
}

pub(crate) fn outputs_key(txid: &[u8; TXID_LEN], vout: u32) -> [u8; TXID_LEN + 4] {
    let mut k = [0u8; TXID_LEN + 4];
    k[..TXID_LEN].copy_from_slice(txid);
    k[TXID_LEN..].copy_from_slice(&vout.to_be_bytes());
    k
}

pub(crate) fn blocks_key(height: u64) -> [u8; 8] {
    height.to_be_bytes()
}

pub(crate) fn utxo_by_spk_key(spk: &[u8; SPK_LEN], height: u64, txid: &[u8; TXID_LEN], vout: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(SPK_LEN + 8 + TXID_LEN + 4);
    k.extend_from_slice(spk);
    k.extend_from_slice(&height.to_be_bytes());
    k.extend_from_slice(txid);
    k.extend_from_slice(&vout.to_be_bytes());
    k
}

/// `inv_height` (not plain height) so a forward scan gives `height DESC,
/// txid ASC` directly — reverse-iterating a plain-height key would give
/// `txid DESC` within a tied height, the wrong secondary sort.
pub(crate) fn history_by_spk_key(spk: &[u8; SPK_LEN], height: u64, txid: &[u8; TXID_LEN]) -> Vec<u8> {
    let mut k = Vec::with_capacity(SPK_LEN + 8 + TXID_LEN);
    k.extend_from_slice(spk);
    k.extend_from_slice(&(u64::MAX - height).to_be_bytes());
    k.extend_from_slice(txid);
    k
}

pub(crate) fn undo_log_key(height: u64) -> [u8; 8] {
    height.to_be_bytes()
}

#[derive(Clone)]
pub(crate) struct OutputRecord {
    pub spk: [u8; SPK_LEN],
    pub value_sat: u64,
    pub height: u64,
    pub spent: Option<(u64, [u8; TXID_LEN])>, // (spent_height, spent_txid)
}

impl OutputRecord {
    const ENCODED_LEN: usize = SPK_LEN + 8 + 8 + 1 + 8 + TXID_LEN;

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(Self::ENCODED_LEN);
        v.extend_from_slice(&self.spk);
        v.extend_from_slice(&self.value_sat.to_be_bytes());
        v.extend_from_slice(&self.height.to_be_bytes());
        match &self.spent {
            None => {
                v.push(0);
                v.extend_from_slice(&[0u8; 8 + TXID_LEN]);
            }
            Some((spent_height, spent_txid)) => {
                v.push(1);
                v.extend_from_slice(&spent_height.to_be_bytes());
                v.extend_from_slice(spent_txid);
            }
        }
        v
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(anyhow!("corrupt outputs record: {} bytes, expected {}", bytes.len(), Self::ENCODED_LEN));
        }
        let mut c = Cursor::new(bytes);
        let spk = c.fixed::<SPK_LEN>()?;
        let value_sat = c.u64()?;
        let height = c.u64()?;
        let flag = c.byte()?;
        let spent_height = c.u64()?;
        let spent_txid = c.fixed::<TXID_LEN>()?;
        let spent = if flag == 1 { Some((spent_height, spent_txid)) } else { None };
        Ok(Self { spk, value_sat, height, spent })
    }
}

/// Everything needed to reverse one height's writes — see the module doc
/// comment for why this exists instead of more permanent secondary indexes.
#[derive(Default)]
pub(crate) struct UndoRecord {
    /// Outputs genuinely created at this height (not a BIP30-ignored dup).
    pub created: Vec<([u8; TXID_LEN], u32, [u8; SPK_LEN], u64)>,
    /// Outputs spent at this height: (txid, vout, spk, value_sat, the
    /// spent output's *original creation height* — needed because
    /// `utxo_by_spk` is keyed by creation height, not spend height, so
    /// un-spending must reconstruct the original key).
    pub spent: Vec<([u8; TXID_LEN], u32, [u8; SPK_LEN], u64, u64)>,
    /// History keys added at this height (covers both "paid" and
    /// "was-spent" rows uniformly).
    pub history: Vec<([u8; SPK_LEN], [u8; TXID_LEN])>,
}

impl UndoRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(self.created.len() as u32).to_be_bytes());
        for (txid, vout, spk, value_sat) in &self.created {
            v.extend_from_slice(txid);
            v.extend_from_slice(&vout.to_be_bytes());
            v.extend_from_slice(spk);
            v.extend_from_slice(&value_sat.to_be_bytes());
        }
        v.extend_from_slice(&(self.spent.len() as u32).to_be_bytes());
        for (txid, vout, spk, value_sat, orig_height) in &self.spent {
            v.extend_from_slice(txid);
            v.extend_from_slice(&vout.to_be_bytes());
            v.extend_from_slice(spk);
            v.extend_from_slice(&value_sat.to_be_bytes());
            v.extend_from_slice(&orig_height.to_be_bytes());
        }
        v.extend_from_slice(&(self.history.len() as u32).to_be_bytes());
        for (spk, txid) in &self.history {
            v.extend_from_slice(spk);
            v.extend_from_slice(txid);
        }
        v
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(bytes);
        let n = c.u32()?;
        let mut created = Vec::with_capacity(n as usize);
        for _ in 0..n {
            created.push((c.fixed::<TXID_LEN>()?, c.u32()?, c.fixed::<SPK_LEN>()?, c.u64()?));
        }
        let n = c.u32()?;
        let mut spent = Vec::with_capacity(n as usize);
        for _ in 0..n {
            spent.push((c.fixed::<TXID_LEN>()?, c.u32()?, c.fixed::<SPK_LEN>()?, c.u64()?, c.u64()?));
        }
        let n = c.u32()?;
        let mut history = Vec::with_capacity(n as usize);
        for _ in 0..n {
            history.push((c.fixed::<SPK_LEN>()?, c.fixed::<TXID_LEN>()?));
        }
        Ok(Self { created, spent, history })
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.bytes.len() {
            return Err(anyhow!("corrupt record: truncated at byte {}", self.pos));
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
}

// ---------------------------------------------------------------------
// shared DB handle
// ---------------------------------------------------------------------

type Registry = Mutex<HashMap<PathBuf, Arc<DB>>>;
static SHARED_DB: OnceLock<Registry> = OnceLock::new();

/// `main.rs` calls `Store::open` (writer) then `Store::open_readonly`
/// (reader) once each, on the same `--db` path, every time — RocksDB
/// permits only one read-write handle per DB directory (enforced via a
/// LOCK file), so opening the same path twice in one process must reuse
/// the same `DB::open_cf_descriptors` call rather than attempt a second
/// one. Keyed by path (not a single slot) so independent stores at
/// different paths -- the normal case in tests -- work too.
fn open_shared_db(path: &str) -> Result<Arc<DB>> {
    let registry = SHARED_DB.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().expect("store registry poisoned");
    let target = PathBuf::from(path);
    if let Some(db) = guard.get(&target) {
        return Ok(db.clone());
    }
    let db = Arc::new(build_db(&target)?);
    guard.insert(target, db.clone());
    Ok(db)
}

pub(crate) fn cf_descriptors() -> Vec<ColumnFamilyDescriptor> {
    let point_lookup_opts = |prefix_len: Option<usize>| -> Options {
        let mut o = Options::default();
        o.set_compression_type(DBCompressionType::Zstd);
        let mut bbto = BlockBasedOptions::default();
        bbto.set_bloom_filter(10.0, false); // whole-key or prefix, ~1% FPR at 10 bits/key
        o.set_block_based_table_factory(&bbto);
        if let Some(n) = prefix_len {
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(n));
        }
        o
    };
    vec![
        ColumnFamilyDescriptor::new(CF_BLOCKS, {
            let mut o = Options::default();
            o.set_compression_type(DBCompressionType::Zstd);
            o
        }),
        ColumnFamilyDescriptor::new(CF_OUTPUTS, point_lookup_opts(None)),
        ColumnFamilyDescriptor::new(CF_UTXO_BY_SPK, point_lookup_opts(Some(SPK_LEN))),
        ColumnFamilyDescriptor::new(CF_HISTORY_BY_SPK, point_lookup_opts(Some(SPK_LEN))),
        ColumnFamilyDescriptor::new(CF_UNDO_LOG, {
            let mut o = Options::default();
            o.set_compression_type(DBCompressionType::Zstd);
            o
        }),
    ]
}

pub(crate) fn build_db(path: &Path) -> Result<DB> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    let mut db_opts = Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);
    let workers = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4);
    db_opts.set_max_background_jobs(workers);
    db_opts.set_level_compaction_dynamic_level_bytes(true);
    db_opts.set_wal_recovery_mode(DBRecoveryMode::PointInTime);

    DB::open_cf_descriptors(&db_opts, path, cf_descriptors())
        .with_context(|| format!("opening rocksdb store at {}", path.display()))
}

fn write_opts_synced() -> WriteOptions {
    // Real financial data, and `apply_blocks`/`rollback_from` are already
    // coalesced to one commit per multi-block batch, not per block -- the
    // fsync cost is amortized over a whole batch, so the stronger
    // (power-loss-safe, not just process-crash-safe) guarantee is cheap
    // here. See the module doc comment for the recovery-mode half of this.
    let mut wo = WriteOptions::default();
    wo.set_sync(true);
    wo
}

// ---------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------

pub struct Store {
    pub(crate) db: Arc<DB>,
    is_writer: bool,
}

impl Store {
    /// Open (creating if needed) the writer handle.
    pub fn open(path: &str) -> Result<Self> {
        Ok(Self { db: open_shared_db(path)?, is_writer: true })
    }

    /// Open the reader handle (shares the writer's `Arc<DB>` if it's already
    /// open in this process — see `open_shared_db`; the directory must
    /// already exist if the writer hasn't opened it first, since this
    /// doesn't set `create_if_missing`... actually it does, via the shared
    /// `build_db` path, so this also works called alone).
    pub fn open_readonly(path: &str) -> Result<Self> {
        Ok(Self { db: open_shared_db(path)?, is_writer: false })
    }

    fn cf(&self, name: &str) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(name).unwrap_or_else(|| panic!("missing column family {name}"))
    }

    pub fn tip(&self) -> Result<Option<(u64, String)>> {
        let cf = self.cf(CF_BLOCKS);
        let mut iter = self.db.iterator_cf(&cf, IteratorMode::End);
        match iter.next() {
            None => Ok(None),
            Some(item) => {
                let (k, v) = item?;
                let height = u64::from_be_bytes(k.as_ref().try_into().context("corrupt blocks key")?);
                Ok(Some((height, hex::encode(v))))
            }
        }
    }

    /// Undo every block at `height` and above (a reorg), by replaying
    /// `undo_log` records from the current tip down to `height`,
    /// descending, un-spending before deleting-created at each step — see
    /// the module doc comment for why that ordering is required.
    ///
    /// **Sharp edge**: this only works for heights the *current* engine
    /// wrote (every `apply_blocks` call logs an undo record for every
    /// height it touches, unconditionally). Data migrated from the old
    /// SQLite store has no undo-log coverage. Reaching a height with no
    /// undo record is refused with an error rather than silently deleting
    /// `blocks` entries with nothing to actually undo them by — this is a
    /// deliberate fail-loud choice: a reorg reaching back past wherever
    /// live sync started is not a realistic scenario on a live chain (this
    /// codebase's own reorg handling elsewhere floors at `start_height`
    /// for the same reason), so refusing outright is safer than guessing.
    pub fn rollback_from(&mut self, height: u64) -> Result<()> {
        if !self.is_writer {
            panic!("rollback_from is writer-only");
        }
        let Some((tip_height, _)) = self.tip()? else { return Ok(()) };
        if tip_height < height {
            return Ok(());
        }

        let cf_blocks = self.cf(CF_BLOCKS);
        let cf_outputs = self.cf(CF_OUTPUTS);
        let cf_utxo = self.cf(CF_UTXO_BY_SPK);
        let cf_hist = self.cf(CF_HISTORY_BY_SPK);
        let cf_undo = self.cf(CF_UNDO_LOG);

        let mut batch = WriteBatch::default();
        let mut h = tip_height;
        loop {
            let Some(bytes) = self.db.get_cf(&cf_undo, undo_log_key(h))? else {
                return Err(anyhow!(
                    "rollback reached height {h} with no undo-log entry (migrated or \
                     pre-migration data) -- refusing to roll back past the point where \
                     undo history is available"
                ));
            };
            let undo = UndoRecord::decode(&bytes)?;

            for (txid, vout, spk, value_sat, orig_height) in &undo.spent {
                let rec = OutputRecord { spk: *spk, value_sat: *value_sat, height: *orig_height, spent: None };
                batch.put_cf(&cf_outputs, outputs_key(txid, *vout), rec.encode());
                batch.put_cf(&cf_utxo, utxo_by_spk_key(spk, *orig_height, txid, *vout), value_sat.to_be_bytes());
            }
            for (txid, vout, spk, _value_sat) in &undo.created {
                batch.delete_cf(&cf_outputs, outputs_key(txid, *vout));
                batch.delete_cf(&cf_utxo, utxo_by_spk_key(spk, h, txid, *vout));
            }
            for (spk, txid) in &undo.history {
                batch.delete_cf(&cf_hist, history_by_spk_key(spk, h, txid));
            }
            batch.delete_cf(&cf_undo, undo_log_key(h));
            batch.delete_cf(&cf_blocks, blocks_key(h));

            if h == height {
                break;
            }
            h -= 1;
        }

        self.db.write_opt(batch, &write_opts_synced())?;
        Ok(())
    }

    /// Test-only: single-block convenience wrapper — see `apply_blocks`.
    #[cfg(test)]
    pub fn apply_block(&mut self, height: u64, hash: &str, txs: &[IndexedTx]) -> Result<()> {
        self.apply_blocks(&[(height, hash, txs)])
    }

    /// Apply several already-validated, connected-in-order blocks as one
    /// atomic `WriteBatch`. See the module doc comment for the column
    /// families this touches, and below for the one correctness subtlety a
    /// naive port would miss.
    pub fn apply_blocks(&mut self, blocks: &[(u64, &str, &[IndexedTx])]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        if !self.is_writer {
            panic!("apply_blocks is writer-only");
        }

        let cf_blocks = self.cf(CF_BLOCKS);
        let cf_outputs = self.cf(CF_OUTPUTS);
        let cf_utxo = self.cf(CF_UTXO_BY_SPK);
        let cf_hist = self.cf(CF_HISTORY_BY_SPK);
        let cf_undo = self.cf(CF_UNDO_LOG);

        let mut batch = WriteBatch::default();
        // A WriteBatch's pending writes are invisible to get_cf until
        // committed, but a spend can land in the same fetch batch as its
        // own creation during a fast catch-up sync (this is normal, not
        // rare -- proven by the existing
        // apply_blocks_batches_several_blocks_in_one_transaction test).
        // Track this call's own not-yet-committed outputs so the spend
        // check and the BIP30 existence-guard both see them.
        let mut pending: HashMap<([u8; TXID_LEN], u32), OutputRecord> = HashMap::new();
        let mut seen_history: HashSet<([u8; SPK_LEN], [u8; TXID_LEN])> = HashSet::new();

        for (height, hash, txs) in blocks {
            let height = *height;
            let mut undo = UndoRecord::default();

            for t in *txs {
                let txid = txid_bytes(&t.txid)?;

                for o in &t.outputs {
                    let spk = spk_bytes(&o.spk_hex)?;
                    let key = outputs_key(&txid, o.vout);

                    // BIP30: two known pre-BIP34 mainnet blocks
                    // (~91722/91842, ~91812/91880) have a coinbase txid
                    // colliding with an earlier coinbase's -- keep the
                    // first, ignore the duplicate (valid only because the
                    // original must already be fully spent). Consensus-
                    // impossible from BIP34 (227,931) on, so this extra
                    // existence check only ever runs for a from-genesis
                    // sync, never the default/production start height.
                    if height < BIP34_HEIGHT {
                        let exists = pending.contains_key(&(txid, o.vout))
                            || self.db.get_cf(&cf_outputs, key)?.is_some();
                        if exists {
                            continue;
                        }
                    }

                    let rec = OutputRecord { spk, value_sat: o.value_sat, height, spent: None };
                    batch.put_cf(&cf_outputs, key, rec.encode());
                    batch.put_cf(&cf_utxo, utxo_by_spk_key(&spk, height, &txid, o.vout), o.value_sat.to_be_bytes());
                    pending.insert((txid, o.vout), rec);
                    undo.created.push((txid, o.vout, spk, o.value_sat));

                    if seen_history.insert((spk, txid)) {
                        batch.put_cf(&cf_hist, history_by_spk_key(&spk, height, &txid), []);
                        undo.history.push((spk, txid));
                    }
                }

                for inp in &t.inputs {
                    let ptxid = txid_bytes(&inp.txid)?;
                    let pkey = outputs_key(&ptxid, inp.vout);

                    let found = match pending.get(&(ptxid, inp.vout)) {
                        Some(rec) => Some(rec.clone()),
                        None => match self.db.get_cf(&cf_outputs, pkey)? {
                            Some(bytes) => Some(OutputRecord::decode(&bytes)?),
                            None => None,
                        },
                    };
                    let Some(mut rec) = found else { continue };
                    if rec.spent.is_some() {
                        continue; // already spent -- shouldn't happen on a valid chain
                    }

                    let orig_height = rec.height;
                    rec.spent = Some((height, txid));
                    batch.put_cf(&cf_outputs, pkey, rec.encode());
                    batch.delete_cf(&cf_utxo, utxo_by_spk_key(&rec.spk, orig_height, &ptxid, inp.vout));
                    undo.spent.push((ptxid, inp.vout, rec.spk, rec.value_sat, orig_height));
                    pending.insert((ptxid, inp.vout), rec.clone());

                    if seen_history.insert((rec.spk, txid)) {
                        batch.put_cf(&cf_hist, history_by_spk_key(&rec.spk, height, &txid), []);
                        undo.history.push((rec.spk, txid));
                    }
                }
            }

            batch.put_cf(&cf_blocks, blocks_key(height), hex::decode(hash).context("block hash not hex")?);
            batch.put_cf(&cf_undo, undo_log_key(height), undo.encode());
        }

        self.db.write_opt(batch, &write_opts_synced())?;
        Ok(())
    }

    pub fn utxos_for(&self, spk_hex: &str) -> Result<Vec<Utxo>> {
        let spk = spk_bytes(spk_hex)?;
        let cf = self.cf(CF_UTXO_BY_SPK);
        let mut out = Vec::new();
        for item in self.db.prefix_iterator_cf(&cf, spk) {
            let (k, v) = item?;
            if !k.starts_with(&spk) {
                break; // prefix_iterator seeks to the prefix but doesn't stop at its end
            }
            let height = u64::from_be_bytes(k[SPK_LEN..SPK_LEN + 8].try_into().unwrap());
            let txid = hex::encode(&k[SPK_LEN + 8..SPK_LEN + 8 + TXID_LEN]);
            let vout = u32::from_be_bytes(k[SPK_LEN + 8 + TXID_LEN..].try_into().unwrap());
            let value_sat = u64::from_be_bytes(v.as_ref().try_into().context("corrupt utxo value")?);
            out.push(Utxo { txid, vout, value_sat, height });
        }
        Ok(out)
    }

    /// `(spk_hex, value_sat)` for one output, any spent status — used to
    /// backfill the prevout of a mempool spend the node reported without one.
    pub fn output_at(&self, txid: &str, vout: u32) -> Result<Option<(String, u64)>> {
        let txid_b = txid_bytes(txid)?;
        let cf = self.cf(CF_OUTPUTS);
        match self.db.get_cf(&cf, outputs_key(&txid_b, vout))? {
            Some(bytes) => {
                let rec = OutputRecord::decode(&bytes)?;
                Ok(Some((hex::encode(rec.spk), rec.value_sat)))
            }
            None => Ok(None),
        }
    }

    pub fn history_for(&self, spk_hex: &str, limit: usize) -> Result<Vec<HistTx>> {
        let spk = spk_bytes(spk_hex)?;
        let cf_hist = self.cf(CF_HISTORY_BY_SPK);
        let cf_blocks = self.cf(CF_BLOCKS);
        // One snapshot for both the scan and the per-row block-hash lookup,
        // so a concurrent rollback landing between them can't produce a
        // missing hash for a row the scan already returned.
        let snapshot = self.db.snapshot();

        let mut ro = ReadOptions::default();
        ro.set_prefix_same_as_start(true);
        let mut out = Vec::with_capacity(limit.min(1024));
        let iter = snapshot.iterator_cf_opt(&cf_hist, ro, IteratorMode::From(&spk, Direction::Forward));
        for item in iter {
            if out.len() >= limit {
                break;
            }
            let (k, _v) = item?;
            if !k.starts_with(&spk) {
                break;
            }
            let inv_height = u64::from_be_bytes(k[SPK_LEN..SPK_LEN + 8].try_into().unwrap());
            let height = u64::MAX - inv_height;
            let txid = hex::encode(&k[SPK_LEN + 8..SPK_LEN + 8 + TXID_LEN]);
            let block_hash = match snapshot.get_cf(&cf_blocks, blocks_key(height))? {
                Some(h) => hex::encode(h),
                None => continue, // a block for a recorded history height must exist; skip defensively
            };
            out.push(HistTx { txid, height, block_hash });
        }
        Ok(out)
    }
}
