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

pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// `per_min` sustained requests, `burst` bucket capacity.
    pub fn new(per_min: u32, burst: u32) -> Self {
        Self {
            capacity: burst.max(1) as f64,
            refill_per_sec: (per_min.max(1) as f64) / 60.0,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token. `true` = allowed, `false` = over the limit.
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.buckets.lock().unwrap();

        // Opportunistic cleanup so the map can't grow without bound.
        if map.len() > 10_000 {
            map.retain(|_, b| now.duration_since(b.last).as_secs() < 3600);
        }

        let b = map.entry(key.to_string()).or_insert(Bucket { tokens: self.capacity, last: now });
        let elapsed = now.duration_since(b.last).as_secs_f64();
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
