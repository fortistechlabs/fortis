//! A TTL cache for GET responses, backed by RocksDB so it survives a
//! restart. Address queries and the tip height change slowly on a block
//! timescale; fee estimates slower still. Caching them here collapses a
//! burst of wallet polls into one upstream hit — and confirmed transaction
//! history never changes at all, so a cache entry for it is worth keeping
//! across restarts, not just within one process's lifetime. Found live,
//! 2026-09-15: a pure in-memory cache meant every one of the day's several
//! `redeploy-backend.ps1` restarts threw away everything already fetched,
//! forcing a full-depth wallet scan to re-pay for data it had already paid
//! for (in upstream latency and, for the metered Maestro tier, real
//! credits) minutes earlier.
//!
//! Wall-clock (`SystemTime`), not `Instant`, for expiry — `Instant` has no
//! fixed epoch and can't be persisted or compared across a process restart.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rocksdb::{IteratorMode, Options, DB};

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
    db: DB,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn encode(until: u64, value: &Cached) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + 2 + 4 + value.content_type.len() + value.body.len());
    v.extend_from_slice(&until.to_be_bytes());
    v.extend_from_slice(&value.status.to_be_bytes());
    v.extend_from_slice(&(value.content_type.len() as u32).to_be_bytes());
    v.extend_from_slice(value.content_type.as_bytes());
    v.extend_from_slice(&value.body);
    v
}

fn decode(bytes: &[u8]) -> Option<(u64, Cached)> {
    if bytes.len() < 8 + 2 + 4 {
        return None;
    }
    let until = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
    let status = u16::from_be_bytes(bytes[8..10].try_into().ok()?);
    let ct_len = u32::from_be_bytes(bytes[10..14].try_into().ok()?) as usize;
    let ct_start: usize = 14;
    let ct_end = ct_start.checked_add(ct_len)?;
    if bytes.len() < ct_end {
        return None;
    }
    let content_type = String::from_utf8(bytes[ct_start..ct_end].to_vec()).ok()?;
    let body = bytes[ct_end..].to_vec();
    Some((until, Cached { status, content_type, body }))
}

impl Cache {
    /// Opens (creating if needed) a RocksDB directory at `dir`. A failure
    /// to open it (bad permissions, disk full, or the path already locked
    /// by another process — a new failure mode RocksDB has that SQLite's
    /// multi-process tolerance didn't) falls back to a fresh scratch temp
    /// directory rather than refusing to start — caching is a performance
    /// nicety, not a correctness requirement, so a degraded (non-persistent
    /// across restarts, but still working) cache beats not starting at all.
    pub fn new(max_entries: usize, dir: &Path) -> Self {
        let db = open_or_scratch(dir, "fortis-edge-cache");
        let now = now_secs();
        let mut map = HashMap::new();
        for item in db.iterator(IteratorMode::Start) {
            let Ok((key, value)) = item else { continue };
            let Some((until, cached)) = decode(&value) else { continue };
            if until > now {
                if let Ok(key) = String::from_utf8(key.to_vec()) {
                    map.insert(key, Entry { until, value: cached });
                }
            }
        }
        Self { max_entries: max_entries.max(1), map: Mutex::new(map), db }
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
        // Best-effort: a write failure here just means this entry doesn't
        // survive a restart, not that the request fails.
        let _ = self.db.put(key.as_bytes(), encode(until, &value));
    }
}

/// `DB::open` at `dir`, falling back to a fresh OS temp directory on any
/// failure (see `Cache::new`'s doc comment for why this degrades instead
/// of refusing to start).
fn open_or_scratch(dir: &Path, label: &str) -> DB {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    match DB::open(&opts, dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!(
                "fortis-edge: {label} db at {dir:?} unavailable ({e}); \
                 using a scratch directory instead, not persisted across restarts"
            );
            let scratch = std::env::temp_dir()
                .join(format!("{label}-scratch-{}-{}", std::process::id(), now_secs()));
            DB::open(&opts, &scratch).unwrap_or_else(|e2| {
                panic!("fortis-edge: {label} scratch db at {scratch:?} also failed: {e2}")
            })
        }
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
        // A response with at least one confirmed entry and nothing pending
        // has nothing that can change — every entry is final, so the *body*
        // can't go stale. It can still go *incomplete* (a new deposit
        // lands), which is why this is hours, not forever: bounding how
        // long a real new payment can take to appear.
        //
        // Deliberately requires a confirmed entry to *exist*, not just the
        // absence of a pending one: an empty `[]` (an address with no
        // history *yet*) trivially contains neither `"confirmed":false` nor
        // `"confirmed":true`, so checking only for the absence of the
        // former made an empty response vacuously "all confirmed" and
        // cached it for 6 hours. Found live, 2026-09-18: a real watch-only
        // wallet had addresses queried (during a gap-limit scan, or while
        // fortis-index was still catching up to a given height) before
        // their first transaction was indexed — that empty answer got
        // cached for 6 hours, so the address kept showing no history long
        // after the real transaction actually arrived, undercounting the
        // wallet's balance on both chains with no error anywhere. An empty
        // response is exactly the case most likely to change soon (an
        // address about to receive its first payment), so it belongs on
        // the short TTL, not the long one.
        let has_confirmed = body.is_some_and(|b| contains(b, br#""confirmed":true"#));
        let has_pending = body.is_some_and(|b| contains(b, br#""confirmed":false"#));
        let all_confirmed = has_confirmed && !has_pending;
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

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fortis-edge-cache-test-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn hits_within_ttl_and_misses_after() {
        let c = Cache::new(8, &temp_dir("ttl"));
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
        let c = Cache::new(2, &temp_dir("evict"));
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
    fn an_empty_txs_response_gets_the_short_ttl_not_the_long_one() {
        // An empty `[]` contains neither `"confirmed":true` nor `false` --
        // checking only for the absence of a pending entry made this
        // vacuously "all confirmed" and cached an address's not-yet-arrived
        // first transaction as empty for 6 hours. Found live, 2026-09-18: a
        // real watch-only wallet undercounted its balance on both chains
        // because of exactly this.
        let empty = br#"[]"#;
        let confirmed = br#"[{"txid":"a","status":{"confirmed":true}}]"#;
        let empty_ttl = ttl_for("/xbt/address/bc1x/txs", Some(empty)).unwrap();
        let confirmed_ttl = ttl_for("/xbt/address/bc1x/txs", Some(confirmed)).unwrap();
        assert!(empty_ttl < confirmed_ttl, "empty={empty_ttl:?} confirmed={confirmed_ttl:?}");
        assert_eq!(empty_ttl, Duration::from_secs(60));
    }

    #[test]
    fn survives_a_restart() {
        let dir = temp_dir("restart");
        {
            let c = Cache::new(8, &dir);
            c.put("k", Duration::from_secs(3600), val("v"));
        }
        let c2 = Cache::new(8, &dir);
        assert_eq!(c2.get("k").unwrap().body, b"v");
    }
}
