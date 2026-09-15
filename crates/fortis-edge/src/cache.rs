//! A TTL cache for GET responses, backed by SQLite so it survives a restart.
//! Address queries and the tip height change slowly on a block timescale; fee
//! estimates slower still. Caching them here collapses a burst of wallet polls
//! into one upstream hit — and confirmed transaction history never changes at
//! all, so a cache entry for it is worth keeping across restarts, not just
//! within one process's lifetime. Found live, 2026-09-15: a pure in-memory
//! cache meant every one of the day's several `redeploy-backend.ps1` restarts
//! threw away everything already fetched, forcing a full-depth wallet scan to
//! re-pay for data it had already paid for (in upstream latency and, for the
//! metered Maestro tier, real credits) minutes earlier.
//!
//! Wall-clock (`SystemTime`), not `Instant`, for expiry — `Instant` has no
//! fixed epoch and can't be persisted or compared across a process restart.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

#[derive(Clone)]
pub struct Cached {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

struct Entry {
    until: u64, // unix seconds
    value: Cached,
}

pub struct Cache {
    max_entries: usize,
    map: Mutex<HashMap<String, Entry>>,
    db: Mutex<Connection>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

impl Cache {
    /// `db_path` — `None` keeps everything in-memory only (e.g. for tests);
    /// `Some(path)` opens/creates a SQLite file there. A failure to open the
    /// file (bad permissions, disk full) falls back to in-memory-only rather
    /// than refusing to start — caching is a performance nicety, not a
    /// correctness requirement.
    pub fn new(max_entries: usize, db_path: Option<&Path>) -> Self {
        let conn = db_path
            .and_then(|p| match Connection::open(p) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("fortis-edge: cache db at {p:?} unavailable ({e:#}); caching in-memory only, not persisted");
                    None
                }
            })
            .unwrap_or_else(|| Connection::open_in_memory().expect("in-memory sqlite"));
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache (
                key TEXT PRIMARY KEY,
                status INTEGER NOT NULL,
                content_type TEXT NOT NULL,
                body BLOB NOT NULL,
                until INTEGER NOT NULL
            );",
        )
        .expect("create cache table");
        // Load everything still live into the in-memory layer so a hot path
        // never pays a disk round trip; expired rows are swept lazily below
        // rather than scanned for at startup (a cold cache just starts empty).
        let now = now_secs();
        let mut map = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT key, status, content_type, body, until FROM cache WHERE until > ?1")
                .expect("prepare load");
            let rows = stmt
                .query_map([now], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Entry {
                            until: r.get(4)?,
                            value: Cached { status: r.get(1)?, content_type: r.get(2)?, body: r.get(3)? },
                        },
                    ))
                })
                .expect("query load");
            for row in rows.flatten() {
                map.insert(row.0, row.1);
            }
        }
        Self { max_entries: max_entries.max(1), map: Mutex::new(map), db: Mutex::new(conn) }
    }

    pub fn get(&self, key: &str) -> Option<Cached> {
        let map = self.map.lock().unwrap();
        map.get(key).filter(|e| e.until > now_secs()).map(|e| e.value.clone())
    }

    /// A fresh-or-recently-expired value — for serving stale data when the
    /// upstream is failing. `grace` is how far past the TTL is still acceptable.
    pub fn get_stale(&self, key: &str, grace: Duration) -> Option<Cached> {
        let map = self.map.lock().unwrap();
        map.get(key).filter(|e| e.until + grace.as_secs() > now_secs()).map(|e| e.value.clone())
    }

    pub fn put(&self, key: &str, ttl: Duration, value: Cached) {
        let until = now_secs() + ttl.as_secs();
        {
            let mut map = self.map.lock().unwrap();
            if map.len() >= self.max_entries && !map.contains_key(key) {
                let now = now_secs();
                map.retain(|_, e| e.until > now);
                if map.len() >= self.max_entries {
                    // still full of live entries — drop an arbitrary one
                    if let Some(k) = map.keys().next().cloned() {
                        map.remove(&k);
                    }
                }
            }
            map.insert(key.to_string(), Entry { until, value: value.clone() });
        }
        let db = self.db.lock().unwrap();
        // Best-effort: a write failure here just means this entry doesn't
        // survive a restart, not that the request fails.
        let _ = db.execute(
            "INSERT INTO cache (key, status, content_type, body, until) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET status=excluded.status, content_type=excluded.content_type,
                body=excluded.body, until=excluded.until",
            rusqlite::params![key, value.status, value.content_type, value.body, until as i64],
        );
    }
}

/// How long a given path may be served from cache. `None` = don't cache.
/// `body` — when present, lets `/txs` responses get a much longer TTL once
/// every transaction in them is confirmed (nothing about a confirmed
/// transaction can change); `None` (the pre-fetch call, deciding whether to
/// even check the cache) falls back to the short TTL, which is what "don't
/// know yet" should default to.
pub fn ttl_for(path: &str, body: Option<&[u8]>) -> Option<Duration> {
    if path.ends_with("/blocks/tip/height") {
        Some(Duration::from_secs(5))
    } else if path.ends_with("/v1/fees/recommended") || path.ends_with("/fee-estimates") {
        Some(Duration::from_secs(30))
    } else if path.ends_with("/v1/prices") {
        Some(Duration::from_secs(60))
    } else if path.contains("/address/") && path.ends_with("/txs") {
        // A response with no `"confirmed":false` has nothing pending — every
        // entry is final, so the *body* can't go stale. It can still go
        // *incomplete* (a new deposit lands), which is why this is hours, not
        // forever: bounding how long a real new payment can take to appear.
        let all_confirmed = body.is_some_and(|b| !contains(b, br#""confirmed":false"#));
        Some(Duration::from_secs(if all_confirmed { 6 * 3600 } else { 60 }))
    } else if path.contains("/address/") {
        // `/utxo` genuinely changes (spends, new deposits) independent of
        // confirmation status, so it keeps the short, pre-existing TTL.
        // BTC rides a paced public upstream — a scan can take longer than a
        // short TTL, so hold results long enough to cover the next poll.
        // XBT is a local index; keep it fresh so a new deposit shows fast.
        Some(Duration::from_secs(if path.starts_with("/btc/") { 60 } else { 5 }))
    } else {
        None
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(b: &str) -> Cached {
        Cached { status: 200, content_type: "application/json".into(), body: b.as_bytes().to_vec() }
    }

    #[test]
    fn hits_within_ttl_and_misses_after() {
        let c = Cache::new(8, None);
        c.put("k", Duration::from_secs(60), val("v"));
        assert_eq!(c.get("k").unwrap().body, b"v");
        c.put("k", Duration::from_secs(0), val("v")); // force-expire for the test
        assert!(c.get("k").is_none());
        // still available as stale within the grace window
        assert_eq!(c.get_stale("k", Duration::from_secs(5)).unwrap().body, b"v");
        assert!(c.get_stale("k", Duration::from_secs(0)).is_none());
    }

    #[test]
    fn evicts_when_full() {
        let c = Cache::new(2, None);
        c.put("a", Duration::from_secs(60), val("a"));
        c.put("b", Duration::from_secs(60), val("b"));
        c.put("c", Duration::from_secs(60), val("c"));
        let live = ["a", "b", "c"].iter().filter(|k| c.get(k).is_some()).count();
        assert!(live <= 2);
    }

    #[test]
    fn ttl_policy() {
        assert!(ttl_for("/xbt/blocks/tip/height", None).is_some());
        assert!(ttl_for("/btc/v1/fees/recommended", None).is_some());
        assert!(ttl_for("/xbt/v1/prices", None).is_some());
        assert!(ttl_for("/btc/tx", None).is_none());
        // BTC utxo results are held far longer than XBT's.
        assert!(ttl_for("/btc/address/bc1x/utxo", None) > ttl_for("/xbt/address/bc1x/utxo", None));
        // A fully-confirmed txs body earns the long TTL; one with a pending
        // entry gets the short, revisit-soon TTL.
        let confirmed = br#"[{"txid":"a","status":{"confirmed":true}}]"#;
        let pending = br#"[{"txid":"a","status":{"confirmed":false}}]"#;
        assert!(ttl_for("/btc/address/bc1x/txs", Some(confirmed)) > ttl_for("/btc/address/bc1x/txs", Some(pending)));
    }

    #[test]
    fn survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("fortis-edge-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_file(&path);
        {
            let c = Cache::new(8, Some(&path));
            c.put("k", Duration::from_secs(3600), val("v"));
        }
        let c2 = Cache::new(8, Some(&path));
        assert_eq!(c2.get("k").unwrap().body, b"v");
        let _ = std::fs::remove_file(&path);
    }
}
