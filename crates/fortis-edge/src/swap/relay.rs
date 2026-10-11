//! Per-match mailbox wake-ups. Messages live in the store; this only lets a
//! waiting `GET /swap/matches/{id}/messages` return as soon as one is pushed.
//! Push relay messages with [`push`], not `Store::push_msg`, so waiters wake
//! at once (a direct `push_msg` is still picked up within `RECHECK`).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::store::Store;

/// How often a waiter re-reads the store even without a wake-up.
const RECHECK: Duration = Duration::from_secs(1);

/// One per match with a waiter; `version` bumps on every push.
#[derive(Default)]
struct Slot {
    version: Mutex<u64>,
    cv: Condvar,
}

fn slots() -> &'static Mutex<HashMap<String, Arc<Slot>>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Slot>>>> = OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Appends a message for `match_id` and wakes its waiters; returns the sequence number.
pub fn push(store: &Store, match_id: &str, from_role: &str, body: Value) -> anyhow::Result<u64> {
    let seq = store.push_msg(match_id, from_role, body)?;
    if let Some(slot) = lock(slots()).get(match_id) {
        *lock(&slot.version) += 1;
        slot.cv.notify_all();
    }
    Ok(seq)
}

/// Messages after `after`, waiting up to `timeout` for the first one to arrive.
/// Empty on timeout.
pub fn wait(store: &Store, match_id: &str, after: u64, timeout: Duration) -> Vec<(u64, String, Value)> {
    let deadline = Instant::now() + timeout;
    let slot = Arc::clone(lock(slots()).entry(match_id.to_string()).or_default());
    let msgs = loop {
        // Read the version before the store so a push in between is not missed.
        let seen = *lock(&slot.version);
        let msgs = store.msgs_after(match_id, after);
        let now = Instant::now();
        if !msgs.is_empty() || now >= deadline {
            break msgs;
        }
        let g = lock(&slot.version);
        let _ = slot
            .cv
            .wait_timeout_while(g, (deadline - now).min(RECHECK), |v| *v == seen)
            .unwrap_or_else(|e| e.into_inner());
    };
    let mut map = lock(slots());
    drop(slot);
    if map.get(match_id).is_some_and(|s| Arc::strong_count(s) == 1) {
        map.remove(match_id);
    }
    msgs
}
