//! Blockchain.com's Haskoin Store as a *batch* source for BTC address data.
//!
//! `GET /address/{unspent,transactions/full}?addresses=a,b,c,…` answer a whole
//! wallet's gap-limit scan in two requests. `POST /btc/prewarm` (in `main`) feeds
//! the edge a wallet's address set, we fetch it here, reshape each address's
//! slice into the Esplora `/address/{a}/utxo` and `/address/{a}/txs` bodies the
//! client already parses, and the caller drops them in the response cache — so
//! the per-address scan that follows is served entirely from memory.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::btc_history::BtcHistory;
use crate::scan_cache::{ScanCache, Signature};

/// Addresses per Haskoin call — a comma-joined query param, so an unbounded
/// list would trip a URL-length limit (`414`) somewhere on the way.
const ADDRS_PER_CALL: usize = 150;
/// Cap on simultaneous in-flight chunk requests — see [`HaskoinStore::
/// get_chunked`]'s doc comment for the live 503 evidence behind this number.
/// 3 concurrent requests succeeded in that same test; kept to 2 for margin
/// rather than riding the exact observed edge of an undocumented limit.
const MAX_CONCURRENT_CHUNKS: usize = 2;
/// Haskoin's `limit` on `address/transactions/full` for [`HaskoinStore::warm`]
/// is *set-wide*, not per address: a response this size may have been cut
/// off, so it can't be trusted as any single address's full history.
const WARM_TXS_LIMIT: usize = 100;
const WARM_UNSPENT_LIMIT: usize = 1000;
const UNSPENT_PAGE: usize = 1000;

pub struct HaskoinStore {
    base: String,
    key: Option<String>,
    agent: ureq::Agent,
}

/// One address's Esplora-shaped bodies, ready to cache. `None` where the batch
/// response may have been truncated — caching a cut-off list as if it were the
/// address's whole history is worse than a slower per-address fetch.
pub struct Warmed {
    pub utxo: Option<Vec<u8>>,
    pub txs: Option<Vec<u8>>,
}

/// A whole address set's balance and history, Esplora-shaped — the BTC side of
/// `POST /btc/scan` (see `scan.rs`).
pub struct Scanned {
    pub used: Vec<String>,
    pub utxos: Vec<Value>,
    pub txs: Vec<Value>,
}

impl HaskoinStore {
    pub fn new(base: &str, key: Option<String>) -> Self {
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(15));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        Self { base: base.trim_end_matches('/').to_string(), key, agent: b.build() }
    }

    fn get(&self, path: &str) -> Result<Value> {
        let mut req = self.agent.get(&format!("{}/{}", self.base, path));
        if let Some(k) = &self.key {
            req = req.set("X-API-Key", k);
        }
        Ok(serde_json::from_str(&req.call()?.into_string()?)?)
    }

    /// Two batch calls per chunk → each address's `(utxo, txs)` Esplora JSON.
    /// Chunked at [ADDRS_PER_CALL] addresses per HTTP call (a comma-joined
    /// query param, not a request body — an unbounded single call risks
    /// tripping a URL-length limit somewhere between here and Haskoin once a
    /// caller's guess window gets genuinely wide) rather than one call for
    /// the whole list, so `POST /btc/prewarm` can accept a wallet's *real*
    /// full depth (hundreds of addresses for an actively-used wallet, not
    /// just a guessed-small window) without that risk.
    pub fn warm(&self, addresses: &[String]) -> Result<HashMap<String, Warmed>> {
        if addresses.is_empty() {
            return Ok(HashMap::new());
        }
        let want: HashSet<&str> = addresses.iter().map(String::as_str).collect();
        let mut utxo_by: HashMap<String, Vec<Value>> = HashMap::new();
        let mut txs_by: HashMap<String, Vec<Value>> = HashMap::new();
        let mut truncated_utxo: HashSet<&str> = HashSet::new();
        let mut truncated_txs: HashSet<&str> = HashSet::new();

        for chunk in addresses.chunks(ADDRS_PER_CALL) {
            let csv = chunk.join(",");
            let unspent = self.get(&format!("address/unspent?addresses={csv}&limit={WARM_UNSPENT_LIMIT}"))?;
            let history =
                self.get(&format!("address/transactions/full?addresses={csv}&limit={WARM_TXS_LIMIT}"))?;
            if unspent.as_array().is_some_and(|a| a.len() >= WARM_UNSPENT_LIMIT) {
                truncated_utxo.extend(chunk.iter().map(String::as_str));
            }
            if history.as_array().is_some_and(|a| a.len() >= WARM_TXS_LIMIT) {
                truncated_txs.extend(chunk.iter().map(String::as_str));
            }

            for u in unspent.as_array().into_iter().flatten() {
                if let Some(a) = u.get("address").and_then(Value::as_str).filter(|a| want.contains(a)) {
                    utxo_by.entry(a.to_string()).or_default().push(esplora_utxo(u));
                }
            }
            for t in history.as_array().into_iter().flatten() {
                let touched: HashSet<&str> = ["inputs", "outputs"]
                    .iter()
                    .flat_map(|side| t.get(side).and_then(Value::as_array).into_iter().flatten())
                    .filter_map(|io| io.get("address").and_then(Value::as_str))
                    .filter(|a| want.contains(a))
                    .collect();
                if touched.is_empty() {
                    continue;
                }
                let e = esplora_tx(t);
                for a in touched {
                    txs_by.entry(a.to_string()).or_default().push(e.clone());
                }
            }
        }

        Ok(addresses
            .iter()
            .map(|a| {
                let utxo = utxo_by.remove(a.as_str()).unwrap_or_default();
                let txs = txs_by.remove(a.as_str()).unwrap_or_default();
                let body = |v: &Vec<Value>| serde_json::to_vec(v).unwrap_or_else(|_| b"[]".to_vec());
                (
                    a.clone(),
                    Warmed {
                        utxo: (!truncated_utxo.contains(a.as_str())).then(|| body(&utxo)),
                        txs: (!truncated_txs.contains(a.as_str())).then(|| body(&txs)),
                    },
                )
            })
            .collect())
    }

    /// Runs one GET per [`ADDRS_PER_CALL`]-sized chunk of `addresses`, up to
    /// [`MAX_CONCURRENT_CHUNKS`] at once rather than fully sequential — a
    /// single wide one-shot scan (the client now sends its whole watch-set in
    /// one request, up to 1000 addresses) is up to 7 chunks, and doing them
    /// one after another cost ~1.9s even though Haskoin answers each in
    /// ~0.2-0.3s alone. Originally fired all chunks at once with no cap;
    /// confirmed live, 2026-09-28, that this was wrong — 7 truly simultaneous
    /// requests to Haskoin got 4 of them back as `503` in under 100ms each,
    /// while every one of those same 7 chunks succeeds individually and even
    /// 3-at-once succeeded in the same test. A concurrent-request limit, not
    /// a processing-time one — capping how many are ever in flight at once
    /// (rather than removing the parallelism entirely) keeps most of the
    /// speedup while staying under whatever that real limit is. `path_for`
    /// builds the full `address/...` path+query for one chunk's comma-joined
    /// address list; results come back in the same order as
    /// `addresses.chunks(ADDRS_PER_CALL)`.
    fn get_chunked(&self, addresses: &[String], path_for: impl Fn(&str) -> String + Sync) -> Result<Vec<Value>> {
        let chunks: Vec<&[String]> = addresses.chunks(ADDRS_PER_CALL).collect();
        let mut out = Vec::with_capacity(chunks.len());
        for group in chunks.chunks(MAX_CONCURRENT_CHUNKS) {
            let results: Vec<Result<Value>> = std::thread::scope(|scope| {
                let handles: Vec<_> = group
                    .iter()
                    .map(|chunk| {
                        let path = path_for(&chunk.join(","));
                        scope.spawn(move || self.get(&path))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("haskoin request thread panicked"))))
                    .collect()
            });
            for r in results {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// `(address, signature)` for every requested address, in request order.
    /// Errors if any requested address has no row: treating a missing row as
    /// "unused" would understate a wallet with no sign anything was wrong.
    pub fn balances(&self, addresses: &[String]) -> Result<Vec<(String, Signature)>> {
        let replies = self.get_chunked(addresses, |csv| format!("address/balances?addresses={csv}"))?;
        let mut by_addr: HashMap<&str, &Value> = HashMap::new();
        for reply in &replies {
            let rows = reply.as_array().ok_or_else(|| anyhow!("haskoin balances: expected an array"))?;
            by_addr.extend(rows.iter().filter_map(|r| Some((r.get("address")?.as_str()?, r))));
        }
        let mut out = Vec::with_capacity(addresses.len());
        for a in addresses {
            let r = by_addr.get(a.as_str()).ok_or_else(|| anyhow!("haskoin returned no balance row for {a}"))?;
            let n = |k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0);
            out.push((a.clone(), Signature { txs: n("txs"), utxo: n("utxo"), unconfirmed: n("unconfirmed") }));
        }
        Ok(out)
    }

    /// `address/unspent`, paginated, for exactly the addresses passed in
    /// (the caller narrows this to funded addresses). Chunks run at once,
    /// same as [`Self::balances`]; pagination *within* a chunk stays
    /// sequential — each page's size decides whether there's a next one, so
    /// it can't be parallelized the same way, but a chunk needing more than
    /// [`UNSPENT_PAGE`] UTXOs at once is not the common case this optimizes.
    pub fn unspent(&self, addresses: &[String]) -> Result<Vec<Value>> {
        let pages = self.get_chunked(addresses, |csv| {
            format!("address/unspent?addresses={csv}&limit={UNSPENT_PAGE}&offset=0")
        })?;
        let mut utxos: Vec<Value> = Vec::new();
        for (chunk, first_page) in addresses.chunks(ADDRS_PER_CALL).zip(pages) {
            let rows = first_page.as_array().ok_or_else(|| anyhow!("haskoin unspent: expected an array"))?;
            utxos.extend(rows.iter().map(esplora_utxo_for_address));
            if rows.len() < UNSPENT_PAGE {
                continue;
            }
            let csv = chunk.join(",");
            let mut offset = UNSPENT_PAGE;
            loop {
                let reply =
                    self.get(&format!("address/unspent?addresses={csv}&limit={UNSPENT_PAGE}&offset={offset}"))?;
                let rows = reply.as_array().ok_or_else(|| anyhow!("haskoin unspent: expected an array"))?;
                utxos.extend(rows.iter().map(esplora_utxo_for_address));
                if rows.len() < UNSPENT_PAGE {
                    break;
                }
                offset += UNSPENT_PAGE;
            }
        }
        Ok(utxos)
    }

    /// Each of `addresses`' own esplora-shaped transactions (pending and
    /// confirmed alike) — the same "which of the requested addresses does
    /// this tx touch" association [`Self::warm`] already uses for its own
    /// per-address split, keyed here for whichever addresses actually need a
    /// fresh look (see [`Self::scan`]) rather than a prewarm's whole set.
    fn transactions_by_address(&self, addresses: &[String], history: usize) -> Result<HashMap<String, Vec<Value>>> {
        let want: HashSet<&str> = addresses.iter().map(String::as_str).collect();
        let replies = self
            .get_chunked(addresses, |csv| format!("address/transactions/full?addresses={csv}&limit={history}"))?;
        let mut by_addr: HashMap<String, Vec<Value>> = HashMap::new();
        for reply in &replies {
            let rows = reply.as_array().ok_or_else(|| anyhow!("haskoin transactions: expected an array"))?;
            for t in rows {
                let touched: HashSet<&str> = ["inputs", "outputs"]
                    .iter()
                    .flat_map(|side| t.get(side).and_then(Value::as_array).into_iter().flatten())
                    .filter_map(|io| io.get("address").and_then(Value::as_str))
                    .filter(|a| want.contains(a))
                    .collect();
                let e = esplora_tx(t);
                for a in touched {
                    by_addr.entry(a.to_string()).or_default().push(e.clone());
                }
            }
        }
        Ok(by_addr)
    }

    /// Balance and recent history for a whole address set. `address/balances`
    /// (one call per [`ADDRS_PER_CALL`]) says which addresses are used/funded
    /// and gives each a [`Signature`]; `address/unspent` is asked only about
    /// the funded ones.
    ///
    /// For transaction history, `btc_history`/`scan_cache` (when configured
    /// — always in production, optional so tests and a degraded deploy still
    /// work) turn this from "re-fetch every used address's full history on
    /// every single poll" into "re-fetch only the addresses whose signature
    /// actually changed since last time": an address whose `Signature`
    /// matches what was stored after its last real fetch cannot have
    /// anything new (Haskoin's own tx count / utxo count / unconfirmed
    /// balance are unchanged), so its confirmed history is served from
    /// `btc_history`'s permanent store and its pending list from
    /// `scan_cache`'s last snapshot — no network call at all for that
    /// address this round. A fresh fetch, when needed, writes its confirmed
    /// entries into `btc_history` (forever) and its pending ones plus the
    /// signature it was fetched under into `scan_cache`, so the *next* poll
    /// can skip it too if still nothing changed.
    ///
    /// Nothing here is per-address in the sense of one HTTP call each, so a
    /// several-hundred-address wallet on a cold cache is still a handful of
    /// calls, and on a warm one is often none beyond the cheap balances
    /// check; a missing balance row or a malformed reply is an error, never
    /// an address quietly treated as empty.
    pub fn scan(
        &self,
        addresses: &[String],
        history: usize,
        btc_history: Option<&BtcHistory>,
        scan_cache: Option<&ScanCache>,
    ) -> Result<Scanned> {
        let signatures = self.balances(addresses)?;
        let is_used = |s: &Signature| s.txs > 0 || s.utxo > 0 || s.unconfirmed != 0;
        let is_funded = |s: &Signature| s.utxo > 0 || s.unconfirmed != 0;
        let sig_by_addr: HashMap<&str, Signature> = signatures.iter().map(|(a, s)| (a.as_str(), *s)).collect();
        let used: Vec<String> = signatures.iter().filter(|(_, s)| is_used(s)).map(|(a, _)| a.clone()).collect();
        let funded: Vec<String> = signatures.iter().filter(|(_, s)| is_funded(s)).map(|(a, _)| a.clone()).collect();

        let utxos = self.unspent(&funded)?;

        let mut needs_fetch: Vec<String> = Vec::new();
        let mut txs: Vec<Value> = Vec::new();
        for addr in &used {
            let sig = sig_by_addr[addr.as_str()];
            match scan_cache.and_then(|c| c.signature(addr)) {
                Some(known) if known == sig => txs.extend(scan_cache.unwrap().pending(addr)),
                _ => needs_fetch.push(addr.clone()),
            }
        }

        if !needs_fetch.is_empty() {
            let fresh = self.transactions_by_address(&needs_fetch, history)?;
            for addr in &needs_fetch {
                let addr_txs = fresh.get(addr).cloned().unwrap_or_default();
                let (confirmed, pending): (Vec<Value>, Vec<Value>) =
                    addr_txs.into_iter().partition(|t| t["status"]["confirmed"] == true);
                if let Some(bh) = btc_history {
                    bh.merge(addr, &confirmed);
                }
                if let Some(sc) = scan_cache {
                    sc.update(addr, sig_by_addr[addr.as_str()], &pending);
                }
                txs.extend(confirmed);
                txs.extend(pending);
            }
        }
        if let Some(bh) = btc_history {
            txs.extend(used.iter().flat_map(|a| bh.get(a)));
        }
        Ok(Scanned { used, utxos, txs: merge_history(txs, history) })
    }
}

/// Each tx once; unconfirmed first, then confirmed newest-first, the confirmed
/// list cut to `history`. Each `address/transactions/full` call returns its
/// own newest `history` for one chunk of addresses, so the newest overall are
/// among the union — nothing needed is lost by capping per call first.
pub(crate) fn merge_history(txs: Vec<Value>, history: usize) -> Vec<Value> {
    let mut seen = HashSet::new();
    let (mut pending, mut confirmed): (Vec<Value>, Vec<Value>) = txs
        .into_iter()
        .filter(|t| seen.insert(t["txid"].as_str().unwrap_or_default().to_string()))
        .partition(|t| t["status"]["confirmed"] != true);
    confirmed.sort_by(|a, b| {
        b["status"]["block_height"]
            .as_i64()
            .cmp(&a["status"]["block_height"].as_i64())
            .then_with(|| a["txid"].as_str().cmp(&b["txid"].as_str()))
    });
    confirmed.truncate(history);
    pending.extend(confirmed);
    pending
}

fn esplora_utxo_for_address(u: &Value) -> Value {
    let mut e = esplora_utxo(u);
    e["address"] = u.get("address").cloned().unwrap_or(Value::Null);
    e
}

/// Confirmed block height, or `None` for a mempool entry (`height` absent or -1).
fn height(v: &Value) -> Option<i64> {
    v.get("block")
        .and_then(|b| b.get("height"))
        .and_then(Value::as_i64)
        .filter(|h| *h >= 0)
}

fn esplora_utxo(u: &Value) -> Value {
    let h = height(u);
    json!({
        "txid": u.get("txid"),
        "vout": u.get("index"),
        "value": u.get("value"),
        "status": { "confirmed": h.is_some(), "block_height": h.unwrap_or(0) },
    })
}

fn esplora_tx(t: &Value) -> Value {
    let h = height(t);
    let map_io = |side: &str, f: fn(&Value) -> Value| -> Vec<Value> {
        t.get(side).and_then(Value::as_array).map(|a| a.iter().map(f).collect()).unwrap_or_default()
    };
    json!({
        "txid": t.get("txid"),
        "fee": t.get("fee"),
        "vin": map_io("inputs", |i| json!({
            "prevout": { "scriptpubkey_address": i.get("address"), "value": i.get("value") }
        })),
        "vout": map_io("outputs", |o| json!({
            "scriptpubkey_address": o.get("address"), "value": o.get("value")
        })),
        "status": { "confirmed": h.is_some(), "block_height": h.unwrap_or(0), "block_time": t.get("time") },
    })
}

#[cfg(test)]
mod tests {
    use super::{esplora_tx, esplora_utxo, esplora_utxo_for_address, merge_history, HaskoinStore, UNSPENT_PAGE};
    use crate::btc_history::BtcHistory;
    use crate::scan_cache::ScanCache;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn bal(addr: &str, txs: i64, utxo: i64, unconfirmed: i64) -> Value {
        json!({ "address": addr, "txs": txs, "utxo": utxo, "unconfirmed": unconfirmed, "confirmed": 0, "received": 0 })
    }

    #[test]
    fn balances_classify_used_and_funded_via_a_real_fetch() {
        let (store, _server) = stub(vec![bal("a", 0, 0, 0), bal("b", 3, 0, 0), bal("c", 2, 1, 0), bal("d", 0, 0, 500)], vec![], vec![]);
        let asked: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let rows = store.balances(&asked).unwrap();
        let used: Vec<&str> = rows.iter().filter(|(_, s)| s.txs > 0 || s.utxo > 0 || s.unconfirmed != 0).map(|(a, _)| a.as_str()).collect();
        let funded: Vec<&str> = rows.iter().filter(|(_, s)| s.utxo > 0 || s.unconfirmed != 0).map(|(a, _)| a.as_str()).collect();
        assert_eq!(used, vec!["b", "c", "d"]); // a: never seen; d: only a pending deposit
        assert_eq!(funded, vec!["c", "d"]);
    }

    #[test]
    fn a_missing_balance_row_is_an_error_not_an_unused_address() {
        let (store, _server) = stub(vec![bal("a", 1, 0, 0)], vec![], vec![]);
        let asked = vec!["a".to_string(), "b".to_string()];
        assert!(store.balances(&asked).is_err());
    }

    /// A stub that echoes back a real row for whatever addresses each
    /// individual request actually asked about (via its own `addresses=`
    /// query param), rather than one fixed canned response for every call —
    /// needed to prove multi-chunk results are correctly merged per-address,
    /// not just that *a* response came back.
    fn per_address_stub() -> (HaskoinStore, std::sync::Arc<AtomicUsize>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                calls2.fetch_add(1, Ordering::SeqCst);
                let url = req.url().to_string();
                let addrs_param = url.split("addresses=").nth(1).unwrap_or("").split('&').next().unwrap_or("");
                let rows: Vec<Value> = addrs_param.split(',').filter(|a| !a.is_empty()).map(|a| bal(a, 1, 0, 0)).collect();
                let _ = req.respond(tiny_http::Response::from_string(json!(rows).to_string()));
            }
        });
        (HaskoinStore::new(&format!("http://{addr}"), None), calls)
    }

    #[test]
    fn balances_across_multiple_chunks_are_all_present_and_correctly_merged() {
        let (store, calls) = per_address_stub();
        // More than ADDRS_PER_CALL (150) so this genuinely spans 2 chunks —
        // 2 parallel requests, not a re-run of the single-chunk path.
        let addrs: Vec<String> = (0..200).map(|i| format!("addr{i}")).collect();
        let rows = store.balances(&addrs).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "200 addresses at 150/call must be exactly 2 chunks");
        assert_eq!(rows.len(), 200);
        // Every requested address got its own real row back, none dropped,
        // none duplicated, regardless of which of the 2 concurrent chunk
        // requests actually answered for it.
        let got: std::collections::HashSet<&str> = rows.iter().map(|(a, _)| a.as_str()).collect();
        for a in &addrs {
            assert!(got.contains(a.as_str()), "missing {a} from a multi-chunk balances() result");
        }
    }

    #[test]
    fn unspent_still_pages_past_the_first_full_page() {
        // The parallel-chunk rewrite only fetches offset=0 concurrently;
        // this pins that a chunk whose first page comes back completely
        // full still walks the remaining pages sequentially afterward,
        // exactly as before.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let offset: usize = url.split("offset=").nth(1).unwrap_or("0").parse().unwrap_or(0);
                let rows: Vec<Value> = if offset == 0 {
                    (0..UNSPENT_PAGE).map(|i| json!({"address": "a", "txid": format!("t{i}"), "index": 0, "value": 1})).collect()
                } else {
                    vec![json!({"address": "a", "txid": "last", "index": 0, "value": 1})]
                };
                let _ = req.respond(tiny_http::Response::from_string(json!(rows).to_string()));
            }
        });
        let store = HaskoinStore::new(&format!("http://{addr}"), None);
        let utxos = store.unspent(&["a".to_string()]).unwrap();
        assert_eq!(utxos.len(), UNSPENT_PAGE + 1, "must have followed the second page, not stopped at the first");
    }

    // ---- stub Haskoin server + scan()-level integration tests --------------

    /// A tiny local HTTP stub standing in for api.haskoin.com: serves fixed
    /// `balances`/`unspent`/`transactions/full` JSON regardless of exactly
    /// which addresses were asked (fine for these tests, which only care
    /// about *whether* a given endpoint was hit, not per-address routing),
    /// and counts hits per endpoint so a test can assert a fetch was
    /// skipped.
    struct Stub {
        hits: HitCounts,
    }
    #[derive(Clone, Default)]
    struct HitCounts {
        balances: Arc<AtomicUsize>,
        unspent: Arc<AtomicUsize>,
        transactions: Arc<AtomicUsize>,
    }
    fn stub(balances: Vec<Value>, unspent: Vec<Value>, transactions: Vec<Value>) -> (HaskoinStore, Stub) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let hits = HitCounts::default();
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let body = if url.contains("balances") {
                    hits2.balances.fetch_add(1, Ordering::SeqCst);
                    json!(balances)
                } else if url.contains("unspent") {
                    hits2.unspent.fetch_add(1, Ordering::SeqCst);
                    json!(unspent)
                } else if url.contains("transactions/full") {
                    hits2.transactions.fetch_add(1, Ordering::SeqCst);
                    json!(transactions)
                } else {
                    json!([])
                };
                let resp = tiny_http::Response::from_string(body.to_string());
                let _ = req.respond(resp);
            }
        });
        (HaskoinStore::new(&format!("http://{addr}"), None), Stub { hits })
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fortis-edge-haskoin-scan-test-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn haskoin_tx(txid: &str, addr: &str, confirmed: bool, height: i64) -> Value {
        json!({
            "txid": txid, "fee": 100,
            "inputs": [{ "coinbase": false, "address": addr, "value": 1000 }],
            "outputs": [{ "address": addr, "value": 900 }],
            "block": if confirmed { json!({ "height": height }) } else { json!({ "mempool": 1 }) },
        })
    }

    #[test]
    fn a_cold_scan_fetches_everything_and_populates_both_caches() {
        let bh = BtcHistory::new(&temp_dir("cold-bh")).unwrap();
        let sc = ScanCache::new(&temp_dir("cold-sc")).unwrap();
        let (store, stub) = stub(
            vec![bal("addr1", 1, 0, 0)],
            vec![],
            vec![haskoin_tx("t1", "addr1", true, 100)],
        );
        let result = store.scan(&["addr1".into()], 50, Some(&bh), Some(&sc)).unwrap();
        assert_eq!(result.txs.len(), 1);
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 1);
        // Confirmed history is now permanently known, and the scan cache
        // has the signature this address was fetched under.
        assert_eq!(bh.get("addr1").len(), 1);
        assert!(sc.signature("addr1").is_some());
    }

    #[test]
    fn an_unchanged_signature_skips_the_transactions_fetch_entirely() {
        let bh = BtcHistory::new(&temp_dir("warm-bh")).unwrap();
        let sc = ScanCache::new(&temp_dir("warm-sc")).unwrap();
        let (store, stub) = stub(
            vec![bal("addr1", 1, 0, 0)],
            vec![],
            vec![haskoin_tx("t1", "addr1", true, 100)],
        );
        store.scan(&["addr1".into()], 50, Some(&bh), Some(&sc)).unwrap();
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 1);

        // Same balances signature on the second call — must not re-fetch.
        let second = store.scan(&["addr1".into()], 50, Some(&bh), Some(&sc)).unwrap();
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 1, "unchanged address must not be re-fetched");
        assert_eq!(second.txs.len(), 1, "confirmed history must still come back, served from the cache");
        assert_eq!(second.txs[0]["txid"], "t1");
    }

    #[test]
    fn a_changed_signature_triggers_a_fresh_fetch_for_just_that_address() {
        let bh = BtcHistory::new(&temp_dir("changed-bh")).unwrap();
        let sc = ScanCache::new(&temp_dir("changed-sc")).unwrap();
        sc.update("addr1", crate::scan_cache::Signature { txs: 1, utxo: 0, unconfirmed: 0 }, &[]);
        bh.merge("addr1", &[haskoin_tx("t1", "addr1", true, 100)]);

        // Haskoin now reports a *different* signature (txs: 2) — a new tx arrived.
        let (store, stub) = stub(
            vec![bal("addr1", 2, 0, 0)],
            vec![],
            vec![haskoin_tx("t1", "addr1", true, 100), haskoin_tx("t2", "addr1", true, 101)],
        );
        let result = store.scan(&["addr1".into()], 50, Some(&bh), Some(&sc)).unwrap();
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 1, "a changed signature must trigger a fresh fetch");
        let ids: std::collections::HashSet<&str> = result.txs.iter().map(|t| t["txid"].as_str().unwrap()).collect();
        assert!(ids.contains("t1") && ids.contains("t2"));
    }

    #[test]
    fn a_pending_entry_is_cached_and_reused_while_unchanged() {
        let bh = BtcHistory::new(&temp_dir("pending-bh")).unwrap();
        let sc = ScanCache::new(&temp_dir("pending-sc")).unwrap();
        let (store, stub) = stub(
            vec![bal("addr1", 0, 0, 500)],
            vec![],
            vec![haskoin_tx("p1", "addr1", false, 0)],
        );
        let first = store.scan(&["addr1".into()], 50, Some(&bh), Some(&sc)).unwrap();
        assert_eq!(first.txs[0]["status"]["confirmed"], false);
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 1);

        let second = store.scan(&["addr1".into()], 50, Some(&bh), Some(&sc)).unwrap();
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 1, "unchanged pending state must not be re-fetched");
        assert_eq!(second.txs.len(), 1);
        assert_eq!(second.txs[0]["txid"], "p1");
    }

    #[test]
    fn scan_still_works_with_no_caches_configured_at_all() {
        let (store, stub) = stub(
            vec![bal("addr1", 1, 0, 0)],
            vec![],
            vec![haskoin_tx("t1", "addr1", true, 100)],
        );
        let result = store.scan(&["addr1".into()], 50, None, None).unwrap();
        assert_eq!(result.txs.len(), 1);
        // With no cache, every call re-fetches — no regression versus the
        // pre-caching behavior when the caches aren't configured.
        store.scan(&["addr1".into()], 50, None, None).unwrap();
        assert_eq!(stub.hits.transactions.load(Ordering::SeqCst), 2);
    }

    fn etx(txid: &str, confirmed: bool, height: i64) -> Value {
        json!({ "txid": txid, "status": { "confirmed": confirmed, "block_height": height } })
    }

    #[test]
    fn history_merges_pending_first_then_newest_confirmed_each_once() {
        let merged = merge_history(
            vec![
                etx("old", true, 100),
                etx("new", true, 300),
                etx("pend", false, 0),
                etx("mid", true, 200),
                etx("new", true, 300), // the same tx seen via a second chunk
            ],
            2,
        );
        let ids: Vec<&str> = merged.iter().map(|t| t["txid"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["pend", "new", "mid"]);
    }

    #[test]
    fn utxo_for_address_carries_the_owning_address() {
        let e = esplora_utxo_for_address(
            &json!({ "address": "bc1qa", "txid": "t", "index": 1, "value": 9, "block": { "height": 5 } }),
        );
        assert_eq!((e["address"].as_str(), e["vout"].as_u64(), e["value"].as_u64()), (Some("bc1qa"), Some(1), Some(9)));
    }

    #[test]
    fn utxo_confirmed_and_mempool() {
        let c = esplora_utxo(&json!({ "txid": "a", "index": 2, "value": 500, "block": { "height": 900 } }));
        assert_eq!(c["vout"], 2);
        assert_eq!(c["status"]["confirmed"], true);
        assert_eq!(c["status"]["block_height"], 900);

        let m = esplora_utxo(&json!({ "txid": "b", "index": 0, "value": 1, "block": { "height": -1 } }));
        assert_eq!(m["status"]["confirmed"], false);
        assert_eq!(m["status"]["block_height"], 0);
    }

    #[test]
    fn tx_maps_inputs_outputs_fee_status() {
        let e = esplora_tx(&json!({
            "txid": "t1", "fee": 250, "time": 1788,
            "inputs": [{ "address": "in1", "value": 1000, "coinbase": false }, { "coinbase": true }],
            "outputs": [{ "address": "out1", "value": 700 }, { "address": "out2", "value": 50 }],
            "block": { "height": 42 },
        }));
        assert_eq!(e["fee"], 250);
        assert_eq!(e["vin"][0]["prevout"]["scriptpubkey_address"], "in1");
        assert_eq!(e["vin"][0]["prevout"]["value"], 1000);
        assert_eq!(e["vin"][1]["prevout"]["scriptpubkey_address"], Value::Null);
        assert_eq!(e["vout"][1]["scriptpubkey_address"], "out2");
        assert_eq!(e["status"]["confirmed"], true);
        assert_eq!(e["status"]["block_time"], 1788);
    }
}
