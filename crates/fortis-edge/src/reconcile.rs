//! Independently verifies a *pending* BTC transaction Haskoin's batch scan
//! reported before it's ever shown to a user.
//!
//! Every other BTC read path here (single-address `/txs`, `/tx/*`, ...) goes
//! through `--btc-upstream`/`--btc-maestro-key` — a real Esplora-compatible
//! service backed by a full node. `POST /{chain}/scan`'s BTC branch is the one
//! exception: it answers from `haskoin::HaskoinStore`, a third-party batch
//! indexer picked specifically because it can answer for a whole wallet's
//! address set in one call (see `haskoin.rs`'s module doc) — nothing else
//! configured here can do that. That's a real, load-bearing reason to keep
//! it, not a shortcut: it's what keeps a deep wallet's scan to a handful of
//! requests instead of one per address.
//!
//! The tradeoff is Haskoin's own mempool view isn't guaranteed to match
//! reality — confirmed live 2026-09-27: `api.haskoin.com` served a full,
//! well-formed, `deleted: false` unconfirmed transaction (RBF-enabled) with a
//! same-day timestamp that both blockstream.info and mempool.space 404 on
//! outright — it doesn't exist on the real network at all, only in Haskoin's
//! own index. `scan_chain` echoed that straight to the client forever, since
//! nothing anywhere cross-checked it. This module is that cross-check,
//! reusing the same upstream chain (`--btc-maestro-key`/`--btc-upstream`/
//! `--btc-upstream-fallback`) individual proxy requests already trust —
//! confirmed transactions are untouched; only entries Haskoin itself marks
//! pending are ever re-verified.

use std::collections::HashMap;

use serde_json::Value;
use tiny_http::Method;

use crate::proxy::Upstream;

/// Caps how many distinct pending txids one scan call will independently
/// verify — a wallet has a handful of genuinely pending transactions at
/// once, not dozens; if something upstream is badly wrong and this list is
/// unexpectedly large, the remainder are left as Haskoin reported them
/// (fail open) rather than turning one scan into dozens of sequential
/// round trips.
const MAX_VERIFY_PER_SCAN: usize = 20;

fn is_pending(v: &Value) -> bool {
    v["status"]["confirmed"] != true
}

/// Walks the same primary-then-fallback chain an individual proxy request
/// uses (see `main.rs`'s `bad()`/`is_bad_upstream`), asking each in turn
/// whether `txid` still exists at all. Stops at the first upstream that
/// gives a definitive answer — `200` (it's there, confirmed or not) or a
/// bare `404` on a `tx/*` path (genuinely unknown, not `is_bad_upstream`'s
/// "treat as a failure" case) — and returns `None` only if every upstream in
/// the chain was unreachable or erroring, so the caller can fail open
/// instead of hiding a possibly-real transaction because this check itself
/// had a bad day.
///
/// Deliberately the *full* `tx/{txid}` body, not the lighter `tx/{txid}/
/// status` — confirmed live against the exact phantom txid this module was
/// written for: blockstream.info's `/status` sub-resource has no concept of
/// "unknown" and answers `200 {"confirmed":false}` for a txid it has never
/// heard of, exactly as it would for a real pending one. Only the full body
/// endpoint gives the `404` that actually distinguishes the two.
fn tx_exists(primary: &Upstream, fallbacks: &[Upstream], txid: &str) -> Option<bool> {
    let path = format!("tx/{txid}");
    for up in std::iter::once(primary).chain(fallbacks.iter()) {
        match up.forward(&Method::Get, &path, "", &[]) {
            Ok(resp) if resp.status == 200 => return Some(true),
            Ok(resp) if resp.status == 404 => return Some(false),
            _ => continue, // this member errored/rate-limited — try the next
        }
    }
    None
}

/// Drops any pending `txs`/`utxos` entry whose txid the real network chain
/// (not Haskoin) confirms is gone. Confirmed entries, and any pending one
/// this can't get a definitive answer for, are left untouched.
pub fn drop_dead_pending(primary: &Upstream, fallbacks: &[Upstream], txs: &mut Vec<Value>, utxos: &mut Vec<Value>) {
    let mut checked: HashMap<String, bool> = HashMap::new();
    let mut budget = MAX_VERIFY_PER_SCAN;
    let mut is_dead = |txid: &str| -> bool {
        if let Some(&dead) = checked.get(txid) {
            return dead;
        }
        if budget == 0 {
            return false; // over budget — fail open for anything not already checked
        }
        budget -= 1;
        let dead = tx_exists(primary, fallbacks, txid) == Some(false);
        checked.insert(txid.to_string(), dead);
        dead
    };
    txs.retain(|t| {
        let txid = t["txid"].as_str().unwrap_or_default();
        !(is_pending(t) && is_dead(txid))
    });
    utxos.retain(|u| {
        let txid = u["txid"].as_str().unwrap_or_default();
        !(is_pending(u) && is_dead(txid))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tiny_http::Server;

    /// A tiny local HTTP stub standing in for a real Esplora upstream —
    /// answers exactly one status code (or hangs up, for "unreachable") for
    /// every `tx/*` request, and counts how many it received.
    struct Stub {
        upstream: Upstream,
        hits: std::sync::Arc<AtomicUsize>,
    }
    fn stub_returning(status: u16) -> Stub {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                hits2.fetch_add(1, Ordering::SeqCst);
                let resp = tiny_http::Response::from_string(if status == 200 {
                    r#"{"confirmed":true}"#
                } else {
                    "not found"
                })
                .with_status_code(status);
                let _ = req.respond(resp);
            }
        });
        Stub { upstream: Upstream::new(&format!("http://{addr}")), hits }
    }
    fn dead_upstream() -> Upstream {
        // Nothing listens here — every call errors out (connection refused).
        Upstream::new("http://127.0.0.1:1")
    }

    fn tx(txid: &str, confirmed: bool) -> Value {
        json!({ "txid": txid, "status": { "confirmed": confirmed } })
    }
    fn utxo(txid: &str, confirmed: bool) -> Value {
        json!({ "txid": txid, "vout": 0, "value": 1000, "status": { "confirmed": confirmed } })
    }

    #[test]
    fn a_pending_tx_the_real_network_has_never_heard_of_is_dropped() {
        let up = stub_returning(404);
        let mut txs = vec![tx("real", true), tx("phantom", false)];
        let mut utxos = vec![];
        drop_dead_pending(&up.upstream, &[], &mut txs, &mut utxos);
        let ids: Vec<&str> = txs.iter().map(|t| t["txid"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["real"]);
    }

    #[test]
    fn a_pending_tx_the_real_network_still_has_is_kept() {
        let up = stub_returning(200);
        let mut txs = vec![tx("genuinely-pending", false)];
        let mut utxos = vec![];
        drop_dead_pending(&up.upstream, &[], &mut txs, &mut utxos);
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn confirmed_entries_are_never_checked_or_dropped() {
        let up = stub_returning(404); // would drop it if this were ever consulted
        let mut txs = vec![tx("confirmed-one", true)];
        let mut utxos = vec![];
        drop_dead_pending(&up.upstream, &[], &mut txs, &mut utxos);
        assert_eq!(txs.len(), 1);
        assert_eq!(up.hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_unreachable_chain_fails_open_rather_than_hiding_a_real_pending_tx() {
        let mut txs = vec![tx("cant-verify", false)];
        let mut utxos = vec![];
        drop_dead_pending(&dead_upstream(), &[], &mut txs, &mut utxos);
        assert_eq!(txs.len(), 1, "an inconclusive check must never remove a pending entry");
    }

    #[test]
    fn a_failing_primary_falls_back_to_the_next_upstream_in_the_chain() {
        let primary = dead_upstream();
        let fallback = stub_returning(404);
        let mut txs = vec![tx("phantom", false)];
        let mut utxos = vec![];
        drop_dead_pending(&primary, std::slice::from_ref(&fallback.upstream), &mut txs, &mut utxos);
        assert!(txs.is_empty());
        assert_eq!(fallback.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_same_txid_is_only_verified_once_across_txs_and_utxos() {
        let up = stub_returning(200);
        let mut txs = vec![tx("shared", false)];
        let mut utxos = vec![utxo("shared", false), utxo("shared", false)];
        drop_dead_pending(&up.upstream, &[], &mut txs, &mut utxos);
        assert_eq!(up.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_dead_pending_utxo_is_dropped_the_same_as_a_dead_pending_tx() {
        let up = stub_returning(404);
        let mut txs = vec![];
        let mut utxos = vec![utxo("phantom-utxo", false), utxo("confirmed-utxo", true)];
        drop_dead_pending(&up.upstream, &[], &mut txs, &mut utxos);
        let ids: Vec<&str> = utxos.iter().map(|u| u["txid"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["confirmed-utxo"]);
    }

    /// Network-dependent — not run by default (`cargo test -- --ignored` to
    /// run it). Pins this module to the actual bug it was written for: this
    /// exact txid is a real, well-formed, undeleted "pending" entry in
    /// Haskoin's own index (confirmed live 2026-09-27) that both
    /// blockstream.info and mempool.space have never heard of at all —
    /// the phantom-pending report this module fixes.
    #[test]
    #[ignore = "hits the real blockstream.info API"]
    fn the_actual_phantom_txid_found_live_is_confirmed_gone_on_a_real_upstream() {
        let up = Upstream::new("https://blockstream.info/api");
        let phantom = "862e83142bed9ef0373c6d824db72e6eb9e97264ac1a5a1ac974467d7d32bbf9";
        assert_eq!(tx_exists(&up, &[], phantom), Some(false));

        // Sanity check the same call against a txid that definitely exists
        // (the genesis coinbase) — proves a 200 is still recognized as
        // "exists" through the exact same code path, not just able to see 404s.
        let genesis = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        assert_eq!(tx_exists(&up, &[], genesis), Some(true));
    }
}
