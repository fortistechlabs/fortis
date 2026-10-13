//! A small, permanent per-address cache of "what did Haskoin's balances call
//! last say, and what was pending at the time" — lets `haskoin::HaskoinStore
//! ::scan` skip re-fetching an address's full transaction history on every
//! poll when nothing about it has changed since the last one. Confirmed
//! transactions themselves live in `btc_history.rs`, forever, independent of
//! this; this cache only ever needs the most recent snapshot, not a history,
//! since a changed signature means "go fetch fresh" regardless of what the
//! old snapshot was.
//!
//! Safe by construction against staleness: this only ever skips a *fetch*,
//! never the independent pending-transaction reconciliation in
//! `reconcile.rs` — a cached pending entry gets exactly the same "is this
//! still real" check as a freshly-fetched one before it ever reaches a user.

use std::path::Path;
use std::sync::Mutex;

use rocksdb::{IteratorMode, Options, DB};
use serde_json::{json, Value};

/// Haskoin's own `address/balances` numbers for one address. Comparing this
/// against what was stored the last time this address was actually fetched
/// is the whole mechanism: if nothing here changed, Haskoin itself is
/// saying there's nothing new to learn about this address.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct Signature {
    pub txs: i64,
    pub utxo: i64,
    pub unconfirmed: i64,
}

impl Signature {
    fn to_json(self) -> Value {
        json!({ "txs": self.txs, "utxo": self.utxo, "unconfirmed": self.unconfirmed })
    }
    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            txs: v.get("txs")?.as_i64()?,
            utxo: v.get("utxo")?.as_i64()?,
            unconfirmed: v.get("unconfirmed")?.as_i64()?,
        })
    }
}

/// Addresses the cache holds at most: anyone may scan any address, so past
/// this a new address just isn't cached (its next scan fetches in full).
pub const MAX_KEYS: usize = 1_000_000;

pub struct ScanCache {
    db: crate::rdb::Db,
    max_keys: usize,
    /// Keys stored; the lock also makes check-then-write exact.
    count: Mutex<usize>,
}

impl ScanCache {
    /// `None` if the db can't be opened — same "resilience/performance
    /// nicety, not a correctness requirement" posture as `BtcHistory::new`:
    /// every address is still answered fully via a live fetch, just without
    /// the skip-when-unchanged shortcut.
    pub fn new(dir: &Path) -> Option<Self> {
        Self::with_max_keys(dir, MAX_KEYS)
    }

    pub fn with_max_keys(dir: &Path, max_keys: usize) -> Option<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        match crate::rdb::Db::open(|| DB::open(&opts, dir)) {
            Ok(db) => {
                let count = db.iterator(IteratorMode::Start).count();
                Some(Self { db, max_keys, count: Mutex::new(count) })
            }
            Err(e) => {
                eprintln!(
                    "fortis-edge: scan cache db at {dir:?} unavailable ({e}); \
                     every BTC scan will re-fetch full history each time"
                );
                None
            }
        }
    }

    fn entry(&self, address: &str) -> Value {
        self.db
            .get(address.as_bytes())
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
            .unwrap_or(Value::Null)
    }

    /// `Some(signature)` from the last time this address was actually
    /// fetched fresh — compare against a just-fetched `Signature` to decide
    /// whether anything could have changed.
    pub fn signature(&self, address: &str) -> Option<Signature> {
        Signature::from_json(self.entry(address).get("signature")?)
    }

    /// The pending entries this address had the last time it was fetched
    /// fresh — safe to reuse only when `signature` still matches the
    /// current balances call (checked by the caller), since an unchanged
    /// signature means Haskoin itself reports no change in tx count,
    /// unconfirmed balance, or utxo count for this address.
    pub fn pending(&self, address: &str) -> Vec<Value> {
        self.entry(address).get("pending").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    /// Record a fresh fetch's outcome: the balances signature it was fetched
    /// under, and whatever pending (unconfirmed) transactions came back with
    /// it — confirmed transactions are `btc_history`'s job, not this cache's.
    pub fn update(&self, address: &str, signature: Signature, pending: &[Value]) {
        let entry = json!({ "signature": signature.to_json(), "pending": pending });
        let Ok(bytes) = serde_json::to_vec(&entry) else { return };
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        let known = matches!(self.db.get_pinned(address.as_bytes()), Ok(Some(_)));
        if !known && *count >= self.max_keys {
            return;
        }
        if self.db.put(address.as_bytes(), bytes).is_ok() && !known {
            *count += 1;
        }
    }

    /// Drop whatever's cached for this address, so the next scan treats it
    /// as changed and re-fetches instead of trusting a stale signature match
    /// — used when an out-of-band signal (`mempool_ws`) reports real
    /// activity for an address sooner than the next Haskoin poll would
    /// otherwise notice it.
    pub fn forget(&self, address: &str) {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(self.db.get_pinned(address.as_bytes()), Ok(Some(_))) && self.db.delete(address.as_bytes()).is_ok() {
            *count = count.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(label: &str) -> crate::test_tmp::TempDir {
        crate::test_tmp::TempDir::new(&format!("scan-cache-test-{label}"))
    }

    #[test]
    fn unknown_address_has_no_signature_and_no_pending() {
        let c_dir = temp_dir("unknown");
        let c = ScanCache::new(&c_dir).unwrap();
        assert_eq!(c.signature("addr1"), None);
        assert!(c.pending("addr1").is_empty());
    }

    /// One key per address asked about: at the budget a new address isn't
    /// cached (its next scan just fetches in full); known ones still update.
    #[test]
    fn the_cache_stops_growing_at_its_key_budget() {
        let c_dir = temp_dir("budget");
        let c = ScanCache::with_max_keys(&c_dir, 2).unwrap();
        let sig = Signature { txs: 1, utxo: 1, unconfirmed: 0 };
        let newer = Signature { txs: 2, utxo: 1, unconfirmed: 0 };
        c.update("a", sig, &[]);
        c.update("b", sig, &[]);
        c.update("c", sig, &[]);
        assert_eq!(c.signature("c"), None);
        c.update("a", newer, &[]);
        assert_eq!(c.signature("a"), Some(newer));
        c.forget("a");
        c.forget("a"); // forgetting twice frees one slot, not two
        c.update("c", sig, &[]);
        assert_eq!(c.signature("c"), Some(sig));
        c.update("d", sig, &[]);
        assert_eq!(c.signature("d"), None);
        drop(c);
        let c = ScanCache::with_max_keys(&c_dir, 2).unwrap();
        c.update("e", sig, &[]);
        assert_eq!(c.signature("e"), None, "the count survives a restart");
    }

    #[test]
    fn remembers_the_last_signature_and_pending_set() {
        let c_dir = temp_dir("remember");
        let c = ScanCache::new(&c_dir).unwrap();
        let sig = Signature { txs: 3, utxo: 1, unconfirmed: 500 };
        c.update("addr1", sig, &[json!({"txid": "p1"})]);
        assert_eq!(c.signature("addr1"), Some(sig));
        assert_eq!(c.pending("addr1").len(), 1);
        assert_eq!(c.pending("addr1")[0]["txid"], "p1");
    }

    #[test]
    fn a_later_update_replaces_the_earlier_one_outright() {
        let c_dir = temp_dir("replace");
        let c = ScanCache::new(&c_dir).unwrap();
        c.update("addr1", Signature { txs: 1, utxo: 0, unconfirmed: 0 }, &[json!({"txid": "old"})]);
        c.update("addr1", Signature { txs: 2, utxo: 1, unconfirmed: 100 }, &[]);
        assert_eq!(c.signature("addr1"), Some(Signature { txs: 2, utxo: 1, unconfirmed: 100 }));
        assert!(c.pending("addr1").is_empty());
    }

    #[test]
    fn addresses_are_independent() {
        let c_dir = temp_dir("independent");
        let c = ScanCache::new(&c_dir).unwrap();
        c.update("addr1", Signature { txs: 1, utxo: 0, unconfirmed: 0 }, &[]);
        assert_eq!(c.signature("addr2"), None);
    }

    #[test]
    fn forget_clears_the_entry_so_the_next_scan_treats_it_as_changed() {
        let c_dir = temp_dir("forget");
        let c = ScanCache::new(&c_dir).unwrap();
        c.update("addr1", Signature { txs: 1, utxo: 0, unconfirmed: 0 }, &[json!({"txid": "p1"})]);
        c.forget("addr1");
        assert_eq!(c.signature("addr1"), None);
        assert!(c.pending("addr1").is_empty());
    }
}
