//! A permanent, never-evicted store of confirmed BTC transactions per
//! address — `fortis-index`'s idea (remember everything confirmed, forever),
//! applied to BTC, which has no local node to walk. `cache.rs`'s TTL'd
//! response cache stays useful for "is there anything new since I last
//! checked" (and for BTC/XBT paths this module doesn't touch), but it's an
//! evictable performance layer, not a source of truth: even a fully-confirmed
//! `/txs` response there is capped at a 6h TTL and can be evicted early under
//! load once the entry cap is hit. A confirmed transaction never changes, so
//! once seen here it's kept forever, independent of that cache's eviction
//! policy, and merged into every response — giving fuller history than any
//! single upstream page, and a permanent fallback when upstream is failing
//! outright (beyond the response cache's existing 600s stale-serving grace).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde_json::Value;

pub struct BtcHistory {
    conn: Mutex<Connection>,
}

impl BtcHistory {
    /// `None` if the db can't be opened (bad permissions, disk full) —
    /// callers treat that the same as "not configured": permanent history
    /// is a resilience/performance nicety, not a correctness requirement,
    /// since every response still comes from upstream regardless.
    pub fn new(db_path: &Path) -> Option<Self> {
        let conn = match Connection::open(db_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "fortis-edge: btc history db at {db_path:?} unavailable ({e:#}); \
                     confirmed BTC history won't persist across restarts"
                );
                return None;
            }
        };
        if let Err(e) = conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=10000;
             CREATE TABLE IF NOT EXISTS btc_confirmed_txs (
                 address TEXT NOT NULL,
                 txid    TEXT NOT NULL,
                 height  INTEGER NOT NULL,
                 json    BLOB NOT NULL,
                 PRIMARY KEY (address, txid)
             );
             CREATE INDEX IF NOT EXISTS btc_confirmed_txs_addr ON btc_confirmed_txs(address);",
        ) {
            eprintln!("fortis-edge: btc history schema setup failed ({e:#}); disabling");
            return None;
        }
        Some(Self { conn: Mutex::new(conn) })
    }

    /// Merge a freshly-fetched `/txs` response for `address` into the
    /// permanent store (confirmed entries only — pending ones can still
    /// change or vanish, so they're never written) and return the full
    /// response to serve: every confirmed tx ever seen for this address,
    /// newest-first, followed by the pending entries from this fetch.
    /// Idempotent — re-merging the same confirmed tx is a no-op.
    pub fn merge(&self, address: &str, txs: &[Value]) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();
        let mut pending = Vec::new();
        {
            let mut ins = match conn.prepare_cached(
                "INSERT OR IGNORE INTO btc_confirmed_txs(address,txid,height,json) VALUES(?1,?2,?3,?4)",
            ) {
                Ok(s) => s,
                Err(_) => return txs.to_vec(),
            };
            for tx in txs {
                if is_confirmed(tx) {
                    let Some(txid) = tx.get("txid").and_then(|t| t.as_str()) else { continue };
                    let height = tx
                        .get("status")
                        .and_then(|s| s.get("block_height"))
                        .and_then(|h| h.as_i64())
                        .unwrap_or(0);
                    let json = serde_json::to_vec(tx).unwrap_or_default();
                    let _ = ins.execute(params![address, txid, height, json]);
                } else {
                    pending.push(tx.clone());
                }
            }
        }
        let mut out = pending;
        out.extend(self.confirmed_for(&conn, address));
        out
    }

    /// Everything permanently known for `address`, confirmed-only — the
    /// last-resort fallback when upstream (and every configured fallback)
    /// is failing outright and even the short-TTL stale-cache grace has
    /// expired. Empty if this address has never been successfully queried
    /// before.
    pub fn get(&self, address: &str) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();
        self.confirmed_for(&conn, address)
    }

    fn confirmed_for(&self, conn: &Connection, address: &str) -> Vec<Value> {
        let mut stmt = match conn
            .prepare_cached("SELECT json FROM btc_confirmed_txs WHERE address = ?1 ORDER BY height DESC")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![address], |r| r.get::<_, Vec<u8>>(0))
            .map(|rows| rows.flatten().filter_map(|b| serde_json::from_slice(&b).ok()).collect())
            .unwrap_or_default()
    }
}

fn is_confirmed(tx: &Value) -> bool {
    tx.get("status").and_then(|s| s.get("confirmed")).and_then(|c| c.as_bool()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tx(txid: &str, confirmed: bool, height: i64) -> Value {
        json!({ "txid": txid, "status": { "confirmed": confirmed, "block_height": height } })
    }

    #[test]
    fn merges_and_persists_confirmed_only() {
        let dir = std::env::temp_dir().join(format!("fortis-edge-btc-history-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let _ = std::fs::remove_file(&path);

        let h = BtcHistory::new(&path).unwrap();
        let first = h.merge("addr1", &[tx("a", true, 100), tx("b", false, 0)]);
        assert_eq!(first.len(), 2); // 1 confirmed + 1 pending

        // A later fetch that no longer includes the pending tx (it dropped
        // out of the mempool, say) still returns the permanently-known
        // confirmed one.
        let second = h.merge("addr1", &[tx("a", true, 100)]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["txid"], "a");

        // Re-merging the same confirmed txid is a no-op, not a duplicate
        // or an error (this is exactly the bug fixed in fortis-index this
        // session, applied here too).
        let third = h.merge("addr1", &[tx("a", true, 100)]);
        assert_eq!(third.len(), 1);

        // A brand new confirmed tx is added alongside the old one.
        let fourth = h.merge("addr1", &[tx("c", true, 200)]);
        assert_eq!(fourth.len(), 2);

        // Permanent fallback survives even with no live fetch at all.
        let known = h.get("addr1");
        assert_eq!(known.len(), 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_address_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!("fortis-edge-btc-history-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let _ = std::fs::remove_file(&path);
        let h = BtcHistory::new(&path).unwrap();
        assert!(h.get("never-seen").is_empty());
        std::fs::remove_file(&path).ok();
    }
}
