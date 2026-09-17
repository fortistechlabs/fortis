//! One-time migration from the old SQLite-backed store into RocksDB. Reads
//! the SQLite file directly in read-only mode -- the same safe pattern the
//! old `Store::open_readonly` used for years. WAL mode is designed for
//! exactly this: any number of concurrent readers alongside one live
//! writer, no corruption risk, unlike a raw file copy (a plain `cp` of a
//! live 115GB WAL-mode file corrupted the copy the same day this was
//! written -- a byte-level copy of a concurrently-modified file has no
//! consistency guarantee; opening it properly as a second SQLite connection
//! does).
//!
//! Deliberately does **not** populate `undo_log` for migrated data -- see
//! `Store::rollback_from`'s doc comment for what that means operationally
//! (a reorg reaching back past the migration point fails loudly instead of
//! silently corrupting data, which is judged an acceptable tradeoff since
//! that depth of reorg isn't realistic on a live chain).

use anyhow::{Context, Result};
use rocksdb::WriteBatch;
use rusqlite::{Connection, OpenFlags};

use crate::store::{
    blocks_key, history_by_spk_key, outputs_key, spk_bytes, txid_bytes, utxo_by_spk_key,
    OutputRecord, Store, CF_BLOCKS, CF_HISTORY_BY_SPK, CF_OUTPUTS, CF_UTXO_BY_SPK,
};

pub struct MigrationStats {
    pub blocks: u64,
    pub outputs: u64,
    pub unspent: u64,
    pub history: u64,
}

const BATCH_ROWS: usize = 50_000;

/// Streams every row out of the old SQLite file at `sqlite_path` into
/// `store` (a freshly-opened writer `Store`), committing in bounded-size
/// batches rather than holding the whole (potentially 100+GB) source in
/// memory at once.
pub fn migrate(sqlite_path: &str, store: &Store) -> Result<MigrationStats> {
    let conn = Connection::open_with_flags(
        sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {sqlite_path} read-only"))?;
    conn.execute_batch("PRAGMA busy_timeout=10000;")?;

    let cf_blocks = store.db.cf_handle(CF_BLOCKS).expect("cf");
    let cf_outputs = store.db.cf_handle(CF_OUTPUTS).expect("cf");
    let cf_utxo = store.db.cf_handle(CF_UTXO_BY_SPK).expect("cf");
    let cf_hist = store.db.cf_handle(CF_HISTORY_BY_SPK).expect("cf");

    let mut stats = MigrationStats { blocks: 0, outputs: 0, unspent: 0, history: 0 };

    eprintln!("migrate: blocks...");
    {
        let mut stmt = conn.prepare("SELECT height, hash FROM blocks ORDER BY height")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)))?;
        let mut batch = WriteBatch::default();
        let mut n = 0usize;
        for row in rows {
            let (height, hash) = row?;
            batch.put_cf(&cf_blocks, blocks_key(height), hex::decode(&hash).context("block hash not hex")?);
            stats.blocks += 1;
            n += 1;
            if n >= BATCH_ROWS {
                store.db.write(std::mem::take(&mut batch))?;
                n = 0;
            }
        }
        store.db.write(batch)?;
    }
    eprintln!("migrate: {} blocks done", stats.blocks);

    {
        let mut stmt =
            conn.prepare("SELECT txid, vout, spk, value, height, spent_height, spent_txid FROM outputs")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u32,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, i64>(4)? as u64,
                r.get::<_, Option<i64>>(5)?.map(|h| h as u64),
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut batch = WriteBatch::default();
        let mut n = 0usize;
        for row in rows {
            let (txid_hex, vout, spk_hex, value_sat, height, spent_height, spent_txid_hex) = row?;
            let txid = txid_bytes(&txid_hex)?;
            let spk = spk_bytes(&spk_hex)?;
            let spent = match (spent_height, spent_txid_hex) {
                (Some(sh), Some(stxid)) => Some((sh, txid_bytes(&stxid)?)),
                _ => None,
            };
            let rec = OutputRecord { spk, value_sat, height, spent };
            batch.put_cf(&cf_outputs, outputs_key(&txid, vout), rec.encode());
            if rec.spent.is_none() {
                batch.put_cf(&cf_utxo, utxo_by_spk_key(&spk, height, &txid, vout), value_sat.to_be_bytes());
                stats.unspent += 1;
            }
            stats.outputs += 1;
            n += 1;
            if n >= BATCH_ROWS {
                store.db.write(std::mem::take(&mut batch))?;
                n = 0;
                if stats.outputs % 1_000_000 == 0 {
                    eprintln!("migrate: {} outputs so far...", stats.outputs);
                }
            }
        }
        store.db.write(batch)?;
    }
    eprintln!("migrate: {} outputs done ({} unspent)", stats.outputs, stats.unspent);

    {
        let mut stmt = conn.prepare("SELECT spk, txid, height FROM history")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64))
        })?;
        let mut batch = WriteBatch::default();
        let mut n = 0usize;
        for row in rows {
            let (spk_hex, txid_hex, height) = row?;
            let spk = spk_bytes(&spk_hex)?;
            let txid = txid_bytes(&txid_hex)?;
            batch.put_cf(&cf_hist, history_by_spk_key(&spk, height, &txid), []);
            stats.history += 1;
            n += 1;
            if n >= BATCH_ROWS {
                store.db.write(std::mem::take(&mut batch))?;
                n = 0;
            }
        }
        store.db.write(batch)?;
    }
    eprintln!("migrate: {} history rows done", stats.history);

    // Force everything durably to disk before reporting success -- the
    // per-batch writes above use the (faster, WAL-only) default, not the
    // sync=true option the live write path uses, since this is a one-time
    // bulk load, not a batch that needs to survive a crash mid-migration
    // (a re-run from scratch is the recovery plan for that, not a
    // partially-applied migration continuing).
    store.db.flush().context("final flush after migration")?;

    Ok(stats)
}
