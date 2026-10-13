//! A token-bucket rate limiter keyed by an arbitrary string (a token, or a client
//! IP for `/register`). In-memory and per-instance — distributed limiting is a
//! later concern.
//!
//! Also [`Pacer`] — an *outbound* throttle that makes a caller wait its turn
//! rather than fail, so a fan-out (a wallet's gap-limit address scan) doesn't
//! spike a rate-limited public upstream.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Keys a limiter holds before it drops the buckets that have refilled.
const MAX_KEYS: usize = 10_000;

pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    max_keys: usize,
    buckets: Mutex<Buckets>,
}

struct Buckets {
    map: HashMap<String, Bucket>,
    /// Map size at which a new key first drops the refilled buckets.
    next_cleanup: usize,
}

impl RateLimiter {
    /// `per_min` sustained requests, `burst` bucket capacity.
    pub fn new(per_min: u32, burst: u32) -> Self {
        Self {
            capacity: burst.max(1) as f64,
            refill_per_sec: (per_min.max(1) as f64) / 60.0,
            max_keys: MAX_KEYS,
            buckets: Mutex::new(Buckets { map: HashMap::new(), next_cleanup: MAX_KEYS }),
        }
    }

    #[cfg(test)]
    fn with_max_keys(mut self, n: usize) -> Self {
        self.max_keys = n;
        self.buckets.get_mut().unwrap().next_cleanup = n;
        self
    }

    /// Take one token. `true` = allowed, `false` = over the limit.
    pub fn check(&self, key: &str) -> bool {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &str, now: Instant) -> bool {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let buckets = &mut *guard;

        // Bound the map: past the threshold a new key drops every bucket that
        // has refilled to capacity — a fresh bucket is just the same, so that
        // loses nothing, and a key still being limited stays limited. Should
        // nothing go (a flood of keys all still limited), the next cleanup
        // waits for the map to double, so it never rescans on every call.
        if !buckets.map.contains_key(key) && buckets.map.len() >= buckets.next_cleanup {
            let (cap, rate) = (self.capacity, self.refill_per_sec);
            buckets.map.retain(|_, b| b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * rate < cap);
            buckets.next_cleanup = self.max_keys.max(buckets.map.len() * 2);
        }

        let b = buckets.map.entry(key.to_string()).or_insert(Bucket { tokens: self.capacity, last: now });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
impl RateLimiter {
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().map.len()
    }
    fn next_cleanup(&self) -> usize {
        self.buckets.lock().unwrap().next_cleanup
    }
}

/// Spaces out calls to at most `per_sec` by sleeping the caller. Idle time isn't
/// banked, so a request after a quiet spell goes straight through; a fan-out is
/// released one every `1/per_sec`.
pub struct Pacer {
    interval: Duration,
    next: Mutex<Instant>,
}

impl Pacer {
    pub fn new(per_sec: f64) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / per_sec.max(0.01)),
            next: Mutex::new(Instant::now()),
        }
    }

    /// Block until this caller's slot, then return.
    pub fn wait(&self) {
        let go = {
            let mut next = self.next.lock().unwrap();
            let go = (*next).max(Instant::now());
            *next = go + self.interval;
            go
        };
        let remaining = go.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            std::thread::sleep(remaining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    /// Past the key cap only refilled buckets go: forgetting one loses
    /// nothing, while a key still being limited stays limited.
    #[test]
    fn a_flood_of_keys_never_frees_a_limited_one() {
        let l = RateLimiter::new(60, 1).with_max_keys(4); // 1 token, back in 1 s
        let t0 = Instant::now();
        assert!(l.check_at("victim-of-limit", t0));
        assert!(!l.check_at("victim-of-limit", t0));
        for i in 0..20 {
            l.check_at(&format!("flood-{i}"), secs(t0, 0.1));
        }
        assert!(!l.check_at("victim-of-limit", secs(t0, 0.2)), "still limited");
    }

    #[test]
    fn refilled_buckets_are_dropped_past_the_key_cap() {
        let l = RateLimiter::new(60, 1).with_max_keys(4);
        let t0 = Instant::now();
        for i in 0..4 {
            l.check_at(&format!("k{i}"), t0);
        }
        // All four have refilled two seconds on: the next new key clears them.
        l.check_at("later", secs(t0, 2.0));
        assert_eq!(l.len(), 1);
    }

    /// A map full of keys still being limited isn't rescanned on every call:
    /// the next cleanup waits until the map has doubled.
    #[test]
    fn cleanup_is_amortised_when_nothing_can_go() {
        let l = RateLimiter::new(1, 1).with_max_keys(4);
        let t0 = Instant::now();
        for i in 0..5 {
            l.check_at(&format!("k{i}"), t0);
        }
        l.check_at("k5", t0);
        assert_eq!(l.next_cleanup(), 8, "the cleanup at the fifth key freed nothing");
        for i in 6..20 {
            l.check_at(&format!("k{i}"), secs(t0, 0.5));
        }
        assert_eq!(l.len(), 20);
    }

    #[test]
    fn pacer_spaces_a_burst() {
        let p = Pacer::new(50.0); // 20 ms apart
        let start = Instant::now();
        for _ in 0..5 {
            p.wait();
        }
        // 5 calls => ~4 intervals => ~80 ms; allow slack for a slow CI box.
        assert!(start.elapsed() >= Duration::from_millis(70), "{:?}", start.elapsed());
    }

    #[test]
    fn pacer_does_not_bank_idle_time() {
        let p = Pacer::new(100.0);
        p.wait();
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        p.wait(); // caught up while idle — should not sleep
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    #[test]
    fn allows_a_burst_then_blocks() {
        let rl = RateLimiter::new(60, 3);
        assert!(rl.check("k"));
        assert!(rl.check("k"));
        assert!(rl.check("k"));
        assert!(!rl.check("k")); // burst of 3 spent
        assert!(rl.check("other")); // independent key
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new(6_000, 1); // 100/sec
        assert!(rl.check("k"));
        assert!(!rl.check("k"));
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(rl.check("k")); // ~3 tokens refilled
    }
}
