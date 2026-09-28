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

use rocksdb::{Options, DB};
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

pub struct ScanCache {
    db: DB,
}

impl ScanCache {
    /// `None` if the db can't be opened — same "resilience/performance
    /// nicety, not a correctness requirement" posture as `BtcHistory::new`:
    /// every address is still answered fully via a live fetch, just without
    /// the skip-when-unchanged shortcut.
    pub fn new(dir: &Path) -> Option<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        match DB::open(&opts, dir) {
            Ok(db) => Some(Self { db }),
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
        if let Ok(bytes) = serde_json::to_vec(&entry) {
            let _ = self.db.put(address.as_bytes(), bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fortis-edge-scan-cache-test-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn unknown_address_has_no_signature_and_no_pending() {
        let c = ScanCache::new(&temp_dir("unknown")).unwrap();
        assert_eq!(c.signature("addr1"), None);
        assert!(c.pending("addr1").is_empty());
    }

    #[test]
    fn remembers_the_last_signature_and_pending_set() {
        let c = ScanCache::new(&temp_dir("remember")).unwrap();
        let sig = Signature { txs: 3, utxo: 1, unconfirmed: 500 };
        c.update("addr1", sig, &[json!({"txid": "p1"})]);
        assert_eq!(c.signature("addr1"), Some(sig));
        assert_eq!(c.pending("addr1").len(), 1);
        assert_eq!(c.pending("addr1")[0]["txid"], "p1");
    }

    #[test]
    fn a_later_update_replaces_the_earlier_one_outright() {
        let c = ScanCache::new(&temp_dir("replace")).unwrap();
        c.update("addr1", Signature { txs: 1, utxo: 0, unconfirmed: 0 }, &[json!({"txid": "old"})]);
        c.update("addr1", Signature { txs: 2, utxo: 1, unconfirmed: 100 }, &[]);
        assert_eq!(c.signature("addr1"), Some(Signature { txs: 2, utxo: 1, unconfirmed: 100 }));
        assert!(c.pending("addr1").is_empty());
    }

    #[test]
    fn addresses_are_independent() {
        let c = ScanCache::new(&temp_dir("independent")).unwrap();
        c.update("addr1", Signature { txs: 1, utxo: 0, unconfirmed: 0 }, &[]);
        assert_eq!(c.signature("addr2"), None);
    }
}
