//! SQLite-backed address index. One writer (the sync loop) and one reader (the
//! HTTP server) each hold their own connection to the same WAL-mode file.
//!
//! Everything is keyed by scriptPubKey hex (`spk`) — an address is turned into
//! its spk on the way in, so every script type works and the Electrum scripthash
//! convention isn't needed for a private index.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::spend_filter::SpendFilter;

#[cfg(test)]
mod tests;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blocks (
  height INTEGER PRIMARY KEY,
  hash   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS outputs (
  txid         TEXT NOT NULL,
  vout         INTEGER NOT NULL,
  spk          TEXT NOT NULL,
  value        INTEGER NOT NULL,
  height       INTEGER NOT NULL,
  spent_height INTEGER,
  spent_txid   TEXT,
  PRIMARY KEY (txid, vout)
);
CREATE INDEX IF NOT EXISTS outputs_spk           ON outputs(spk);
CREATE INDEX IF NOT EXISTS outputs_spent_height  ON outputs(spent_height);
CREATE INDEX IF NOT EXISTS outputs_height        ON outputs(height);
CREATE TABLE IF NOT EXISTS history (
  spk    TEXT NOT NULL,
  txid   TEXT NOT NULL,
  height INTEGER NOT NULL,
  PRIMARY KEY (spk, txid)
);
CREATE INDEX IF NOT EXISTS history_spk    ON history(spk, height);
CREATE INDEX IF NOT EXISTS history_height ON history(height);
";

/// One output as the index stores it. `vout` is this output's true position
/// in its transaction — `block_txs` (`sync.rs`) filters most outputs out
/// before they ever reach here (see its doc comment), so the surviving ones
/// are no longer contiguous from 0 and can't be re-derived by enumerating
/// this list.
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

pub struct Store {
    conn: Connection,
    // `None` on a read-only Store (the HTTP server's connection) -- only
    // `apply_blocks` (the writer) ever needs it.
    spend_filter: Option<SpendFilter>,
}

impl Store {
    /// Open (creating + migrating) the writer connection.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {path}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=10000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        let spend_filter = Self::build_spend_filter(&conn)?;
        Ok(Self { conn, spend_filter: Some(spend_filter) })
    }

    /// Every transaction input in every block gets checked against `outputs`
    /// -- not just ones that turn out to spend a tracked coin -- because
    /// there's no way to know which, up front, without checking. Measured
    /// live on this index's real ~115GB table: that per-input UPDATE+SELECT
    /// pair was 96% of total sync time (71.7s of a 74.3s batch). A Bloom
    /// filter built once here from the table's existing contents lets
    /// definite misses skip the database entirely -- live-measured at this
    /// point in the chain, that's roughly two-thirds of inputs (P2WPKH turns
    /// out to be in wide use here, not a rare case, so the remaining third
    /// are genuine hits paying real, unavoidable work). See `spend_filter`'s
    /// doc comment for why this can never cause a missed spend, only an
    /// occasional unnecessary check.
    fn build_spend_filter(conn: &Connection) -> Result<SpendFilter> {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM outputs", [], |r| r.get(0))?;
        let mut filter = SpendFilter::new(count.max(0) as u64);
        let mut stmt = conn.prepare("SELECT txid, vout FROM outputs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (txid, vout) = row?;
            filter.insert(&txid, vout as u32);
        }
        Ok(filter)
    }

    /// Open a read-only connection (the file must already exist).
    pub fn open_readonly(path: &str) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {path} read-only"))?;
        conn.execute_batch("PRAGMA busy_timeout=10000;")?;
        Ok(Self { conn, spend_filter: None })
    }

    pub fn tip(&self) -> Result<Option<(u64, String)>> {
        self.conn
            .query_row(
                "SELECT height, hash FROM blocks ORDER BY height DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)),
            )
            .optional()
            .context("reading tip")
    }

    /// Undo every block at `height` and above (a reorg). Spends made at or above
    /// `height` are un-marked; outputs and history created there are removed.
    pub fn rollback_from(&mut self, height: u64) -> Result<()> {
        let h = height as i64;
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE outputs SET spent_height=NULL, spent_txid=NULL WHERE spent_height >= ?1",
            [h],
        )?;
        tx.execute("DELETE FROM outputs WHERE height >= ?1", [h])?;
        tx.execute("DELETE FROM history WHERE height >= ?1", [h])?;
        tx.execute("DELETE FROM blocks  WHERE height >= ?1", [h])?;
        tx.commit()?;
        Ok(())
    }

    /// Apply one connected block as a single transaction. Test-only: the
    /// sync loop calls `apply_blocks` directly to batch a whole fetched
    /// round into one transaction, but single-block fixtures read better in
    /// tests than always building a one-element batch by hand.
    #[cfg(test)]
    pub fn apply_block(&mut self, height: u64, hash: &str, txs: &[IndexedTx]) -> Result<()> {
        self.apply_blocks(&[(height, hash, txs)])
    }

    /// Apply several already-validated, connected-in-order blocks as **one**
    /// transaction — the sync loop fetches a batch of blocks in parallel
    /// (see `sync.rs`) but must still apply them in height order; batching
    /// the apply step too means one commit (one WAL flush) for the whole
    /// batch instead of one per block, which is most of what made a from-
    /// SegWit-activation sync slow. Every commit still leaves the store at a
    /// consistent height — a crash mid-batch loses at most the not-yet-
    /// committed blocks in it, same as losing one block did before.
    pub fn apply_blocks(&mut self, blocks: &[(u64, &str, &[IndexedTx])]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        // Disjoint field borrows: `self.conn.transaction()` only borrows
        // `conn`, so this can stay live alongside it.
        let filter = self.spend_filter.as_mut().expect("apply_blocks is writer-only");
        let tx = self.conn.transaction()?;
        {
            // OR IGNORE: pre-BIP34 mainnet has two known blocks (~91722/91842 and
            // ~91812/91880) whose coinbase transaction collides with an earlier
            // coinbase's txid — a real, valid duplicate in chain history (BIP30),
            // not a bug in this indexer. Indexing from genesis (rather than the
            // XBT fork height, safely after both) hits them; keeping the first
            // row and ignoring the repeat is correct since the original coinbase
            // must already be fully spent for the duplicate block to be valid.
            let mut ins_out = tx.prepare_cached(
                "INSERT OR IGNORE INTO outputs(txid,vout,spk,value,height) VALUES(?1,?2,?3,?4,?5)",
            )?;
            // Single round-trip instead of an UPDATE + a separate SELECT:
            // RETURNING hands back the row's spk (for history) as part of
            // the same statement, when the input actually matches something.
            let mut spend = tx.prepare_cached(
                "UPDATE outputs SET spent_height=?1, spent_txid=?2 WHERE txid=?3 AND vout=?4 RETURNING spk",
            )?;
            let mut ins_hist = tx.prepare_cached(
                "INSERT OR IGNORE INTO history(spk,txid,height) VALUES(?1,?2,?3)",
            )?;
            let mut ins_block = tx.prepare_cached("INSERT INTO blocks(height,hash) VALUES(?1,?2)")?;

            for (height, hash, txs) in blocks {
                let h = *height as i64;
                for t in *txs {
                    for o in &t.outputs {
                        ins_out.execute(params![t.txid, o.vout as i64, o.spk_hex, o.value_sat as i64, h])?;
                        ins_hist.execute(params![o.spk_hex, t.txid, h])?;
                        filter.insert(&t.txid, o.vout);
                    }
                    for inp in &t.inputs {
                        // Live-measured: about a third of inputs at this
                        // point in the chain are genuine hits (P2WPKH is
                        // widely used, not a rare case), so this only skips
                        // the majority-but-not-overwhelming fraction of
                        // inputs that are definitely not one of ours -- real
                        // hits still pay a real UPDATE, which is unavoidable
                        // work, not overhead.
                        if !filter.might_contain(&inp.txid, inp.vout) {
                            continue;
                        }
                        let spk: Option<String> = spend
                            .query_row(params![h, t.txid, inp.txid, inp.vout as i64], |r| r.get(0))
                            .optional()?;
                        if let Some(spk) = spk {
                            ins_hist.execute(params![spk, t.txid, h])?;
                        }
                    }
                }
                ins_block.execute(params![h, hash])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn utxos_for(&self, spk_hex: &str) -> Result<Vec<Utxo>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT txid, vout, value, height FROM outputs
             WHERE spk=?1 AND spent_height IS NULL
             ORDER BY height, txid, vout",
        )?;
        let rows = stmt
            .query_map([spk_hex], |r| {
                Ok(Utxo {
                    txid: r.get(0)?,
                    vout: r.get::<_, i64>(1)? as u32,
                    value_sat: r.get::<_, i64>(2)? as u64,
                    height: r.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(spk_hex, value_sat)` for one confirmed output — used to backfill the
    /// prevout of a mempool spend the node reported without one.
    pub fn output_at(&self, txid: &str, vout: u32) -> Result<Option<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT spk, value FROM outputs WHERE txid=?1 AND vout=?2")?;
        stmt.query_row(params![txid, vout as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })
        .optional()
        .context("output_at")
    }

    pub fn history_for(&self, spk_hex: &str, limit: usize) -> Result<Vec<HistTx>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT h.txid, h.height, b.hash
             FROM history h JOIN blocks b ON b.height = h.height
             WHERE h.spk=?1
             ORDER BY h.height DESC, h.txid
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![spk_hex, limit as i64], |r| {
                Ok(HistTx {
                    txid: r.get(0)?,
                    height: r.get::<_, i64>(1)? as u64,
                    block_hash: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
