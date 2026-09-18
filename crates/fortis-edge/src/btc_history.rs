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

use rocksdb::{IteratorMode, Options, DB};
use serde_json::Value;

pub struct BtcHistory {
    db: DB,
}

// `0x00` never appears in a valid bech32/base58 address, so
// `address_bytes ++ DELIM` is a safe, unambiguous scan prefix even though
// addresses are variable-length (unlike fortis-index's fixed-width spk).
const DELIM: u8 = 0x00;

fn key(address: &str, height: i64, txid: &str) -> Vec<u8> {
    let inv_height = (i64::MAX as u64).wrapping_sub(height.max(0) as u64);
    let mut k = Vec::with_capacity(address.len() + 1 + 8 + txid.len());
    k.extend_from_slice(address.as_bytes());
    k.push(DELIM);
    k.extend_from_slice(&inv_height.to_be_bytes());
    k.extend_from_slice(txid.as_bytes());
    k
}

fn scan_prefix(address: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(address.len() + 1);
    p.extend_from_slice(address.as_bytes());
    p.push(DELIM);
    p
}

impl BtcHistory {
    /// `None` if the db can't be opened (bad permissions, disk full, or the
    /// path already locked by another process) — callers treat that the
    /// same as "not configured": permanent history is a resilience/
    /// performance nicety, not a correctness requirement, since every
    /// response still comes from upstream regardless.
    pub fn new(dir: &Path) -> Option<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        match DB::open(&opts, dir) {
            Ok(db) => Some(Self { db }),
            Err(e) => {
                eprintln!(
                    "fortis-edge: btc history db at {dir:?} unavailable ({e}); \
                     confirmed BTC history won't persist across restarts"
                );
                None
            }
        }
    }

    /// Merge a freshly-fetched `/txs` response for `address` into the
    /// permanent store (confirmed entries only — pending ones can still
    /// change or vanish, so they're never written) and return the full
    /// response to serve: every confirmed tx ever seen for this address,
    /// newest-first, followed by the pending entries from this fetch.
    /// Idempotent — re-merging the same confirmed tx overwrites with
    /// equivalent (immutable) data, a harmless no-op in effect.
    pub fn merge(&self, address: &str, txs: &[Value]) -> Vec<Value> {
        let mut pending = Vec::new();
        for tx in txs {
            if is_confirmed(tx) {
                let Some(txid) = tx.get("txid").and_then(|t| t.as_str()) else { continue };
                let height =
                    tx.get("status").and_then(|s| s.get("block_height")).and_then(|h| h.as_i64()).unwrap_or(0);
                let json = serde_json::to_vec(tx).unwrap_or_default();
                let _ = self.db.put(key(address, height, txid), json);
            } else {
                pending.push(tx.clone());
            }
        }
        let mut out = pending;
        out.extend(self.confirmed_for(address));
        out
    }

    /// Everything permanently known for `address`, confirmed-only — the
    /// last-resort fallback when upstream (and every configured fallback)
    /// is failing outright and even the short-TTL stale-cache grace has
    /// expired. Empty if this address has never been successfully queried
    /// before.
    pub fn get(&self, address: &str) -> Vec<Value> {
        self.confirmed_for(address)
    }

    fn confirmed_for(&self, address: &str) -> Vec<Value> {
        let prefix = scan_prefix(address);
        self.db
            .iterator(IteratorMode::From(&prefix, rocksdb::Direction::Forward))
            .take_while(|item| item.as_ref().is_ok_and(|(k, _)| k.starts_with(&prefix)))
            .filter_map(|item| item.ok())
            .filter_map(|(_, v)| serde_json::from_slice(&v).ok())
            .collect()
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

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fortis-edge-btc-history-test-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn merges_and_persists_confirmed_only() {
        let h = BtcHistory::new(&temp_dir("merge")).unwrap();
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
        // newest-first
        assert_eq!(fourth[0]["txid"], "c");

        // Permanent fallback survives even with no live fetch at all.
        let known = h.get("addr1");
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn unknown_address_is_empty_not_an_error() {
        let h = BtcHistory::new(&temp_dir("unknown")).unwrap();
        assert!(h.get("never-seen").is_empty());
    }

    #[test]
    fn addresses_sharing_a_textual_prefix_do_not_bleed_into_each_other() {
        // Without the DELIM terminator, a prefix scan for "addr1" could in
        // principle also match "addr10", "addr1x", etc.
        let h = BtcHistory::new(&temp_dir("prefix")).unwrap();
        h.merge("addr1", &[tx("a", true, 100)]);
        h.merge("addr10", &[tx("b", true, 100)]);
        h.merge("addr1x", &[tx("c", true, 100)]);
        let known = h.get("addr1");
        assert_eq!(known.len(), 1);
        assert_eq!(known[0]["txid"], "a");
    }
}
