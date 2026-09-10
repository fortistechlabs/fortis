//! A tiny TTL cache for GET responses. Address queries and the tip height change
//! slowly on a block timescale; fee estimates slower still. Caching them here
//! collapses a burst of wallet polls into one upstream hit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Cached {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

struct Entry {
    until: Instant,
    value: Cached,
}

pub struct Cache {
    max_entries: usize,
    map: Mutex<HashMap<String, Entry>>,
}

impl Cache {
    pub fn new(max_entries: usize) -> Self {
        Self { max_entries: max_entries.max(1), map: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, key: &str) -> Option<Cached> {
        let map = self.map.lock().unwrap();
        map.get(key).filter(|e| e.until > Instant::now()).map(|e| e.value.clone())
    }

    /// A fresh-or-recently-expired value — for serving stale data when the
    /// upstream is failing. `grace` is how far past the TTL is still acceptable.
    pub fn get_stale(&self, key: &str, grace: Duration) -> Option<Cached> {
        let map = self.map.lock().unwrap();
        map.get(key).filter(|e| e.until + grace > Instant::now()).map(|e| e.value.clone())
    }

    pub fn put(&self, key: &str, ttl: Duration, value: Cached) {
        let mut map = self.map.lock().unwrap();
        if map.len() >= self.max_entries {
            let now = Instant::now();
            map.retain(|_, e| e.until > now);
            if map.len() >= self.max_entries {
                // still full of live entries — drop an arbitrary one
                if let Some(k) = map.keys().next().cloned() {
                    map.remove(&k);
                }
            }
        }
        map.insert(key.to_string(), Entry { until: Instant::now() + ttl, value });
    }
}

/// How long a given path may be served from cache. `None` = don't cache.
pub fn ttl_for(path: &str) -> Option<Duration> {
    if path.ends_with("/blocks/tip/height") {
        Some(Duration::from_secs(5))
    } else if path.ends_with("/v1/fees/recommended") || path.ends_with("/fee-estimates") {
        Some(Duration::from_secs(30))
    } else if path.ends_with("/v1/prices") {
        Some(Duration::from_secs(60))
    } else if path.contains("/address/") {
        // BTC rides a paced public upstream — a scan can take longer than a
        // short TTL, so hold address results long enough to cover the next poll.
        // XBT is a local index; keep it fresh so a new deposit shows fast.
        Some(Duration::from_secs(if path.starts_with("/btc/") { 60 } else { 5 }))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(b: &str) -> Cached {
        Cached { status: 200, content_type: "application/json".into(), body: b.as_bytes().to_vec() }
    }

    #[test]
    fn hits_within_ttl_and_misses_after() {
        let c = Cache::new(8);
        c.put("k", Duration::from_millis(40), val("v"));
        assert_eq!(c.get("k").unwrap().body, b"v");
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.get("k").is_none());
        // still available as stale within the grace window
        assert_eq!(c.get_stale("k", Duration::from_secs(1)).unwrap().body, b"v");
        assert!(c.get_stale("k", Duration::from_millis(0)).is_none());
    }

    #[test]
    fn evicts_when_full() {
        let c = Cache::new(2);
        c.put("a", Duration::from_secs(60), val("a"));
        c.put("b", Duration::from_secs(60), val("b"));
        c.put("c", Duration::from_secs(60), val("c"));
        let live = ["a", "b", "c"].iter().filter(|k| c.get(k).is_some()).count();
        assert!(live <= 2);
    }

    #[test]
    fn ttl_policy() {
        assert!(ttl_for("/xbt/blocks/tip/height").is_some());
        assert!(ttl_for("/btc/v1/fees/recommended").is_some());
        assert!(ttl_for("/xbt/v1/prices").is_some());
        assert!(ttl_for("/btc/tx").is_none());
        // BTC address results are held far longer than XBT's.
        assert!(ttl_for("/btc/address/bc1x/utxo") > ttl_for("/xbt/address/bc1x/utxo"));
    }
}
