//! NeoxEX `BTCB2_BTC` reference price for the P2P swap: sat of BTC per 1
//! XBT. Swaps trade at the users' own limit prices; this is only shown next
//! to them (`GET /swap/quote`), with `ok` saying whether the rule below passes.
//!
//! The Android app re-implements this exact rule against the same fixtures
//! (`tests/fixtures/neoxex/`), so keep it simple. Prices are BTC per XBT,
//! sizes are XBT.
//!
//! Wire shapes (public, no auth):
//! - `GET /api/exchange/orderbook/BTCB2_BTC` ->
//!   `{"bids":[{"price":p,"quantity":q,..}..],"asks":[..],..}`
//!   (no timestamp in the response).
//! - `GET /api/exchange/ticker/BTCB2_BTC` ->
//!   `{"ticker":{"lastPrice":p,"computedAt":ms,..}}`.
//! - `GET /api/exchange/pairs` -> `{"pairs":[{"pair":"BTCB2_BTC",
//!   "tradingPaused":bool,..}..]}`.
//!
//! Rule:
//! 1. Per side, sort best first (bids high to low, asks low to high), then
//!    walk levels taking `price * size` BTC until `DEPTH_BTC` (0.05) is
//!    reached, truncating the last level. A side with less depth uses what
//!    exists. The side price is the volume-weighted average price taken
//!    (BTC taken / XBT taken).
//! 2. `mid = (bid_vwap + ask_vwap) / 2`; `spread = (ask_vwap - bid_vwap) / mid`.
//! 3. Refuse, in this order: `Paused`, `Stale` (`now - computedAt/1000 >
//!    MAX_AGE_SECS`, using the ticker's upstream `computedAt` ms timestamp,
//!    never our own fetch time; a missing or non-numeric `computedAt` is an
//!    `Upstream` error, i.e. fail closed; the check is re-run on every
//!    call, also for cached data), `Empty` (a side has no levels), `WideSpread`
//!    (`spread > MAX_SPREAD`), `FarFromLast` (`|mid - last| / last >
//!    MAX_FROM_LAST`).
//! 4. Result is `round(mid * 1e8)`.
//!
//! "Paused": the ticker has no pause field. Paused is true when the ticker
//! object carries `tradingPaused` or `paused` = true, or when the pair's
//! entry in `/api/exchange/pairs` has `tradingPaused` = true. The orderbook
//! has no timestamp, so the ticker's `computedAt` is the freshness clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub const DEPTH_BTC: f64 = 0.05;
pub const MAX_SPREAD: f64 = 0.02;
pub const MAX_AGE_SECS: u64 = 60;
pub const MAX_FROM_LAST: f64 = 0.02;
const CACHE_SECS: u64 = 10;
const PAIR: &str = "BTCB2_BTC";

#[derive(Debug, Clone, PartialEq)]
pub enum QuoteError {
    Stale,
    Paused,
    WideSpread,
    FarFromLast,
    Empty,
    Upstream(String),
}

/// (price in BTC per XBT, size in XBT) levels.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ticker {
    pub last: f64,
    pub paused: bool,
    /// Upstream `computedAt`, unix seconds.
    pub computed_at: u64,
}

fn up<E: std::fmt::Display>(e: E) -> QuoteError {
    QuoteError::Upstream(e.to_string())
}

fn levels(v: &Value, side: &str, descending: bool) -> Result<Vec<(f64, f64)>, QuoteError> {
    let arr = v.get(side).and_then(Value::as_array).ok_or_else(|| up(format!("no {side}")))?;
    let mut out = Vec::with_capacity(arr.len());
    for l in arr {
        let p = l.get("price").and_then(Value::as_f64);
        let q = l.get("quantity").and_then(Value::as_f64);
        match (p, q) {
            (Some(p), Some(q)) if p.is_finite() && q.is_finite() && p > 0.0 && q > 0.0 => out.push((p, q)),
            _ => return Err(up(format!("bad {side} level"))),
        }
    }
    out.sort_by(|a, b| {
        let o = a.0.partial_cmp(&b.0).unwrap();
        if descending { o.reverse() } else { o }
    });
    Ok(out)
}

pub fn parse_book(body: &str) -> Result<Book, QuoteError> {
    let v: Value = serde_json::from_str(body).map_err(up)?;
    Ok(Book { bids: levels(&v, "bids", true)?, asks: levels(&v, "asks", false)? })
}

pub fn parse_ticker(body: &str) -> Result<Ticker, QuoteError> {
    let v: Value = serde_json::from_str(body).map_err(up)?;
    let t = v.get("ticker").ok_or_else(|| up("no ticker"))?;
    let last = t
        .get("lastPrice")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| up("no lastPrice"))?;
    let flag = |k: &str| t.get(k).and_then(Value::as_bool).unwrap_or(false);
    let computed_ms = t
        .get("computedAt")
        .and_then(Value::as_u64)
        .ok_or_else(|| up("no computedAt"))?;
    Ok(Ticker { last, paused: flag("tradingPaused") || flag("paused"), computed_at: computed_ms / 1000 })
}

fn pair_paused(body: &str, pair: &str) -> Result<bool, QuoteError> {
    let v: Value = serde_json::from_str(body).map_err(up)?;
    v.get("pairs")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|p| p.get("pair").and_then(Value::as_str) == Some(pair)))
        .map(|p| p.get("tradingPaused").and_then(Value::as_bool).unwrap_or(false))
        .ok_or_else(|| up("pair not listed"))
}

/// VWAP of the best levels up to `DEPTH_BTC` of notional; `None` if no levels.
fn side_vwap(levels: &[(f64, f64)]) -> Option<f64> {
    let (mut btc, mut xbt, mut rem) = (0.0, 0.0, DEPTH_BTC);
    for &(p, s) in levels {
        let take = (p * s).min(rem);
        btc += take;
        xbt += take / p;
        rem -= take;
        if rem <= 0.0 {
            break;
        }
    }
    (xbt > 0.0).then(|| btc / xbt)
}

/// The NeoxEX reference: the depth-weighted mid, whatever the checks say,
/// and whether [`price_from`] would accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
    pub price_sat_per_xbt: u64,
    pub ok: bool,
}

fn mid(book: &Book) -> Option<(f64, f64, f64)> {
    let (bid, ask) = (side_vwap(&book.bids)?, side_vwap(&book.asks)?);
    Some((bid, ask, (bid + ask) / 2.0))
}

/// The reference at `now`; `None` when a side of the book is empty.
pub fn reference_from(book: &Book, ticker: &Ticker, now: u64) -> Option<Reference> {
    let (_, _, mid) = mid(book)?;
    Some(Reference {
        price_sat_per_xbt: (mid * 1e8).round() as u64,
        ok: price_from(book, ticker, now).is_ok(),
    })
}

/// sat of BTC per 1 XBT. See the module doc for the rule.
pub fn price_from(book: &Book, ticker: &Ticker, now: u64) -> Result<u64, QuoteError> {
    if ticker.paused {
        return Err(QuoteError::Paused);
    }
    if now.saturating_sub(ticker.computed_at) > MAX_AGE_SECS {
        return Err(QuoteError::Stale);
    }
    let (bid, ask, mid) = mid(book).ok_or(QuoteError::Empty)?;
    if (ask - bid) / mid > MAX_SPREAD {
        return Err(QuoteError::WideSpread);
    }
    if (mid - ticker.last).abs() / ticker.last > MAX_FROM_LAST {
        return Err(QuoteError::FarFromLast);
    }
    Ok((mid * 1e8).round() as u64)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct Snapshot {
    at: Instant,
    book: Book,
    ticker: Ticker,
}

pub struct NeoxEx {
    base: String,
    agent: ureq::Agent,
    /// The last good snapshot. Only ever locked briefly: never across a fetch.
    cache: Mutex<Option<Snapshot>>,
    /// When the last fetch failed; no refetch until `CACHE_SECS` after it, so
    /// an outage doesn't make every request wait out the agent timeout.
    failed_at: Mutex<Option<Instant>>,
    /// Set while one caller fetches; everyone else answers from `cache`.
    refreshing: AtomicBool,
    #[cfg(test)]
    fetches: std::sync::atomic::AtomicUsize,
}

/// Clears `refreshing` when the fetching caller is done, even on a panic.
struct Refreshing<'a>(&'a AtomicBool);

impl Drop for Refreshing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl NeoxEx {
    pub fn new(base_url: String) -> Self {
        let mut b = ureq::AgentBuilder::new().timeout(Duration::from_secs(15));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        Self { base: base_url.trim_end_matches('/').to_string(), agent: b.build(),
            cache: Mutex::new(None),
            failed_at: Mutex::new(None),
            refreshing: AtomicBool::new(false),
            #[cfg(test)]
            fetches: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn get(&self, path: &str) -> Result<String, QuoteError> {
        let url = format!("{}/api/exchange/{path}", self.base);
        self.agent.get(&url).call().map_err(up)?.into_string().map_err(up)
    }

    fn fetch(&self) -> Result<Snapshot, QuoteError> {
        #[cfg(test)]
        self.fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let book = parse_book(&self.get(&format!("orderbook/{PAIR}"))?)?;
        let mut ticker = parse_ticker(&self.get(&format!("ticker/{PAIR}"))?)?;
        ticker.paused |= pair_paused(&self.get("pairs")?, PAIR)?;
        Ok(Snapshot { at: Instant::now(), book, ticker })
    }

    /// Whether the cache is due a refetch: no snapshot or one older than
    /// `CACHE_SECS`, and no failed fetch within the last `CACHE_SECS`.
    fn due(&self) -> bool {
        let stale = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|s| s.at.elapsed().as_secs() >= CACHE_SECS);
        stale
            && !self
                .failed_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|t| t.elapsed().as_secs() < CACHE_SECS)
    }

    /// The reference now; refetches at most every 10 s, and never within
    /// 10 s of a failed fetch. Single-flight: one caller fetches (without
    /// holding the cache lock) while every concurrent caller answers at once
    /// from the last good snapshot, whose age makes `ok` false once stale.
    /// `None` when there has never been a good snapshot or its book has an
    /// empty side.
    pub fn reference(&self) -> Option<Reference> {
        if self.due()
            && self
                .refreshing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            let _done = Refreshing(&self.refreshing);
            // Another caller may have refreshed between `due` and winning the flag.
            if self.due() {
                match self.fetch() {
                    Ok(snap) => {
                        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
                        *self.failed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    }
                    Err(_) => {
                        *self.failed_at.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Instant::now());
                    }
                }
            }
        }
        let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let s = c.as_ref()?;
        reference_from(&s.book, &s.ticker, unix_now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK_OK: &str = include_str!("../../tests/fixtures/neoxex/orderbook_ok.json");
    const BOOK_WIDE: &str = include_str!("../../tests/fixtures/neoxex/orderbook_wide.json");
    const TICKER_OK: &str = include_str!("../../tests/fixtures/neoxex/ticker_ok.json");
    const TICKER_PAUSED: &str = include_str!("../../tests/fixtures/neoxex/ticker_paused.json");
    const TICKER_FAR: &str = include_str!("../../tests/fixtures/neoxex/ticker_far.json");
    const BOOK_LIVE: &str = include_str!("../../tests/fixtures/neoxex/orderbook_live.json");
    const TICKER_LIVE: &str = include_str!("../../tests/fixtures/neoxex/ticker_live.json");
    const PAIRS_LIVE: &str = include_str!("../../tests/fixtures/neoxex/pairs_live.json");
    const PAIRS_PAUSED: &str = include_str!("../../tests/fixtures/neoxex/pairs_paused.json");

    /// `computedAt` (ms) of every ticker fixture, in seconds.
    const COMPUTED: u64 = 1_791_670_572;

    fn book(s: &str) -> Book {
        parse_book(s).unwrap()
    }
    fn ticker(s: &str) -> Ticker {
        parse_ticker(s).unwrap()
    }

    #[test]
    fn weighted_mid_of_fixture() {
        // bid vwap 0.00999637, ask vwap 0.01018323 -> mid 0.01008980 BTC/XBT.
        assert_eq!(price_from(&book(BOOK_OK), &ticker(TICKER_OK), COMPUTED + 10), Ok(1_008_980));
    }

    #[test]
    fn wide_spread_refused() {
        assert_eq!(
            price_from(&book(BOOK_WIDE), &ticker(TICKER_OK), COMPUTED + 10),
            Err(QuoteError::WideSpread)
        );
    }

    #[test]
    fn live_snapshot_is_too_wide_to_quote() {
        // The real 2026-10-10 book: 1.1% at the touch but ~10.6% over 0.05 BTC of depth.
        assert_eq!(
            price_from(&book(BOOK_LIVE), &ticker(TICKER_LIVE), COMPUTED + 10),
            Err(QuoteError::WideSpread)
        );
    }

    #[test]
    fn paused_refused() {
        assert!(ticker(TICKER_PAUSED).paused);
        assert!(!ticker(TICKER_OK).paused);
        assert_eq!(
            price_from(&book(BOOK_OK), &ticker(TICKER_PAUSED), COMPUTED + 10),
            Err(QuoteError::Paused)
        );
    }

    #[test]
    fn stale_refused() {
        // Age is measured from the upstream computedAt, not our fetch clock.
        assert_eq!(
            price_from(&book(BOOK_OK), &ticker(TICKER_OK), COMPUTED + 61),
            Err(QuoteError::Stale)
        );
        assert!(price_from(&book(BOOK_OK), &ticker(TICKER_OK), COMPUTED + 60).is_ok());
    }

    #[test]
    fn missing_computed_at_is_upstream_error() {
        let no_ts = TICKER_OK.replace("computedAt", "computedAtX");
        assert!(matches!(parse_ticker(&no_ts), Err(QuoteError::Upstream(_))));
        let bad = TICKER_OK.replace("1791670572294", "\"soon\"");
        assert!(matches!(parse_ticker(&bad), Err(QuoteError::Upstream(_))));
    }

    #[test]
    fn pair_paused_reads_pairs_endpoint() {
        assert_eq!(pair_paused(PAIRS_LIVE, "BTCB2_BTC"), Ok(false));
        assert_eq!(pair_paused(PAIRS_PAUSED, "BTCB2_BTC"), Ok(true));
        // Other pairs in the paused fixture are untouched.
        assert_eq!(pair_paused(PAIRS_PAUSED, "BTCB2_USDT"), Ok(false));
        assert!(matches!(pair_paused(PAIRS_LIVE, "NOPE_BTC"), Err(QuoteError::Upstream(_))));
        assert!(matches!(pair_paused("{\"pairs\":[]}", "BTCB2_BTC"), Err(QuoteError::Upstream(_))));
        assert!(matches!(pair_paused("junk", "BTCB2_BTC"), Err(QuoteError::Upstream(_))));
    }

    #[test]
    fn paused_pair_refuses_price() {
        let mut t = ticker(TICKER_OK);
        t.paused |= pair_paused(PAIRS_LIVE, "BTCB2_BTC").unwrap();
        assert!(price_from(&book(BOOK_OK), &t, COMPUTED + 10).is_ok());
        t.paused |= pair_paused(PAIRS_PAUSED, "BTCB2_BTC").unwrap();
        assert_eq!(price_from(&book(BOOK_OK), &t, COMPUTED + 10), Err(QuoteError::Paused));
    }

    #[test]
    fn far_from_last_refused() {
        assert_eq!(
            price_from(&book(BOOK_OK), &ticker(TICKER_FAR), COMPUTED + 10),
            Err(QuoteError::FarFromLast)
        );
    }

    #[test]
    fn thin_book_uses_what_exists() {
        let t = Ticker { last: 0.01, paused: false, computed_at: COMPUTED };
        let b = Book { bids: vec![(0.0099, 1.0)], asks: vec![(0.0101, 2.0)] };
        // 0.0099 BTC and 0.0202 BTC of depth, both under 0.05: mid = 0.01 exactly.
        assert_eq!(price_from(&b, &t, COMPUTED + 10), Ok(1_000_000));
    }

    #[test]
    fn empty_side_refused() {
        let t = Ticker { last: 0.01, paused: false, computed_at: COMPUTED };
        let b = Book { bids: vec![], asks: vec![(0.0101, 2.0)] };
        assert_eq!(price_from(&b, &t, COMPUTED + 10), Err(QuoteError::Empty));
        let b = Book { bids: vec![(0.0099, 1.0)], asks: vec![] };
        assert_eq!(price_from(&b, &t, COMPUTED + 10), Err(QuoteError::Empty));
    }

    #[test]
    fn reference_of_fixture_is_ok() {
        assert_eq!(
            reference_from(&book(BOOK_OK), &ticker(TICKER_OK), COMPUTED + 10),
            Some(Reference { price_sat_per_xbt: 1_008_980, ok: true })
        );
    }

    #[test]
    fn reference_of_wide_book_is_not_ok() {
        let b = book(BOOK_WIDE);
        let mid = (side_vwap(&b.bids).unwrap() + side_vwap(&b.asks).unwrap()) / 2.0;
        assert_eq!(
            reference_from(&b, &ticker(TICKER_OK), COMPUTED + 10),
            Some(Reference { price_sat_per_xbt: (mid * 1e8).round() as u64, ok: false })
        );
    }

    #[test]
    fn reference_is_not_ok_when_stale_paused_or_far_from_last() {
        let not_ok = Some(Reference { price_sat_per_xbt: 1_008_980, ok: false });
        assert_eq!(reference_from(&book(BOOK_OK), &ticker(TICKER_OK), COMPUTED + 61), not_ok);
        assert_eq!(reference_from(&book(BOOK_OK), &ticker(TICKER_PAUSED), COMPUTED + 10), not_ok);
        assert_eq!(reference_from(&book(BOOK_OK), &ticker(TICKER_FAR), COMPUTED + 10), not_ok);
    }

    #[test]
    fn reference_of_empty_book_is_none() {
        let t = Ticker { last: 0.01, paused: false, computed_at: COMPUTED };
        let b = Book { bids: vec![], asks: vec![(0.0101, 2.0)] };
        assert_eq!(reference_from(&b, &t, COMPUTED + 10), None);
        let b = Book { bids: vec![(0.0099, 1.0)], asks: vec![] };
        assert_eq!(reference_from(&b, &t, COMPUTED + 10), None);
    }

    #[test]
    fn failed_fetch_is_not_retried_within_cache_window() {
        use std::sync::atomic::Ordering::SeqCst;
        // A port nothing listens on: connections are refused.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let n = NeoxEx::new(format!("http://127.0.0.1:{port}"));
        assert_eq!(n.reference(), None);
        assert_eq!(n.fetches.load(SeqCst), 1);
        assert_eq!(n.reference(), None);
        assert_eq!(n.fetches.load(SeqCst), 1, "second call must not refetch");
        // Once the window has passed, it tries again.
        *n.failed_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(CACHE_SECS + 1));
        assert_eq!(n.reference(), None);
        assert_eq!(n.fetches.load(SeqCst), 2);
    }

    fn ok_snapshot(age_secs: u64) -> Snapshot {
        let mut t = ticker(TICKER_OK);
        t.computed_at = unix_now();
        Snapshot { at: Instant::now() - Duration::from_secs(age_secs), book: book(BOOK_OK), ticker: t }
    }

    /// A NeoxEX that accepts connections and holds each for `secs` before
    /// hanging up without an answer.
    fn slow_stub(secs: u64) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(secs));
                    drop(c);
                });
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn concurrent_reference_does_not_wait_for_a_fetch_in_flight() {
        use std::sync::atomic::Ordering::SeqCst;
        let n = std::sync::Arc::new(NeoxEx::new(slow_stub(2)));
        // A last good snapshot, due for a refresh.
        *n.cache.lock().unwrap() = Some(ok_snapshot(CACHE_SECS + 1));
        let bg = {
            let n = n.clone();
            std::thread::spawn(move || n.reference())
        };
        let t0 = Instant::now();
        while n.fetches.load(SeqCst) == 0 {
            assert!(t0.elapsed() < Duration::from_secs(5), "fetch never started");
            std::thread::yield_now();
        }
        let t = Instant::now();
        let r = n.reference();
        assert!(t.elapsed() < Duration::from_millis(500), "waited {:?} for the fetch in flight", t.elapsed());
        assert_eq!(r, Some(Reference { price_sat_per_xbt: 1_008_980, ok: true }));
        assert_eq!(n.fetches.load(SeqCst), 1, "only one caller fetches");
        // The fetch fails; the fetcher still gets the last good snapshot.
        assert_eq!(bg.join().unwrap(), r);
    }

    #[test]
    fn failed_refetch_keeps_last_good_snapshot() {
        use std::sync::atomic::Ordering::SeqCst;
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let n = NeoxEx::new(format!("http://127.0.0.1:{port}"));
        *n.cache.lock().unwrap() = Some(ok_snapshot(CACHE_SECS + 1));
        let good = Some(Reference { price_sat_per_xbt: 1_008_980, ok: true });
        assert_eq!(n.reference(), good);
        assert_eq!(n.fetches.load(SeqCst), 1);
        assert_eq!(n.reference(), good);
        assert_eq!(n.fetches.load(SeqCst), 1, "no refetch within the window after a failure");
    }

    #[test]
    fn junk_json_is_upstream_error() {
        assert!(matches!(parse_book("{}"), Err(QuoteError::Upstream(_))));
        assert!(matches!(parse_ticker("{\"ticker\":{}}"), Err(QuoteError::Upstream(_))));
    }
}
