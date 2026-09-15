//! Forward a request to a chain's upstream (a `fortis-index` instance for XBT, an
//! Esplora API for BTC) and return the raw response.

use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::Method;

pub struct Upstream {
    base: String,
    agent: ureq::Agent,
    /// A managed provider (e.g. Maestro's Esplora-compatible API) needs an
    /// `api-key` header rather than a bearer token or query param — set once
    /// here so `forward` doesn't need to know which kind of upstream it's
    /// talking to.
    extra_header: Option<(String, String)>,
    /// GET retries on a 429/5xx/transport error. 2 for a lone upstream (the
    /// common case — nothing else to fall back to, so it's worth waiting out
    /// a blip). A fallback-chain member (see `fortis-edge`'s `btc_fallbacks`)
    /// uses 0: the chain itself is the redundancy, and a *hanging* member
    /// retried 2 more times at the full read timeout is exactly what turns
    /// "one slow provider" into "the whole chain takes 30s to answer" —
    /// confirmed live, 2026-09-15, when mempool.space started hanging (not
    /// even erroring) while a *later*, working fallback (Maestro) sat
    /// unreached behind it.
    retries: u32,
}

pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Upstream {
    pub fn new(base: &str) -> Self {
        Self::build(base, None, 2, Duration::from_secs(5), Duration::from_secs(8))
    }

    pub fn with_header(base: &str, extra_header: Option<(String, String)>) -> Self {
        Self::build(base, extra_header, 2, Duration::from_secs(5), Duration::from_secs(8))
    }

    /// For a member of a fallback chain — see the `retries` field doc. Also
    /// slightly shorter connect/read timeouts (4s/6s vs the default 5s/8s)
    /// and one retry (not zero): the chain tries each candidate in turn, so a
    /// member that's *entirely* dead should cost a few seconds before moving
    /// on rather than a lone upstream's full timeout budget — but zero
    /// retries turned out too aggressive in practice. Found live, 2026-09-15:
    /// once Maestro (the primary) had its credits temporarily exhausted,
    /// blockstream.info carried the real sustained load for the first time
    /// (previously it only ever saw occasional ad-hoc probes) and needed
    /// noticeably longer than its usual sub-second response — a single
    /// transient miss with zero retries turned into a client-visible failure
    /// on the majority of requests, compounding into a scan that took many
    /// minutes for no reason better than "the fallback tier didn't tolerate
    /// one slow response before giving up".
    pub fn fallback(base: &str, extra_header: Option<(String, String)>) -> Self {
        Self::build(base, extra_header, 1, Duration::from_secs(4), Duration::from_secs(6))
    }

    fn build(
        base: &str,
        extra_header: Option<(String, String)>,
        retries: u32,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Self {
        // Short per-request timeouts: an address lookup that healthy explorers
        // answer in ~1 s is hanging if it takes longer, and a wallet scan can't
        // afford to wait — fail fast and let `forward`'s retry move on.
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(connect_timeout)
            .timeout_read(read_timeout)
            .timeout_write(read_timeout);
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        Self { base: base.trim_end_matches('/').to_string(), agent: b.build(), extra_header, retries }
    }

    /// `rest` is the path after `/{chain}` (no leading slash), `query` without `?`.
    ///
    /// A GET is retried (see the `retries` field) with backoff on a `429` /
    /// `5xx` / transport error — public explorers throw those intermittently
    /// under a wallet's fan-out scan, and one bad response would otherwise
    /// abort the whole scan.
    pub fn forward(
        &self,
        method: &Method,
        rest: &str,
        query: &str,
        body: &[u8],
    ) -> Result<UpstreamResponse> {
        let url = if query.is_empty() {
            format!("{}/{}", self.base, rest)
        } else {
            format!("{}/{}?{}", self.base, rest, query)
        };
        if !matches!(method, Method::Get | Method::Post) {
            return Ok(UpstreamResponse {
                status: 405,
                content_type: "text/plain".into(),
                body: b"method not allowed".to_vec(),
            });
        }
        let retries = if *method == Method::Get { self.retries } else { 0 };

        let mut last_err = None;
        for attempt in 0..=retries {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200 * attempt as u64));
            }
            let resp = match method {
                Method::Get => {
                    let mut req = self.agent.get(&url);
                    if let Some((k, v)) = &self.extra_header {
                        req = req.set(k, v);
                    }
                    req.call()
                }
                _ => {
                    let mut req = self.agent.post(&url).set("Content-Type", "text/plain");
                    if let Some((k, v)) = &self.extra_header {
                        req = req.set(k, v);
                    }
                    req.send_bytes(body)
                }
            };
            let (status, r) = match resp {
                Ok(r) => (r.status(), r),
                Err(ureq::Error::Status(code, r)) => (code, r),
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            if attempt < retries && (status == 429 || (500..=599).contains(&status)) {
                continue;
            }
            let content_type = r.content_type().to_string();
            let mut out = Vec::new();
            r.into_reader().read_to_end(&mut out)?;
            return Ok(UpstreamResponse { status, content_type, body: out });
        }
        Err(last_err.map_or_else(|| anyhow::anyhow!("upstream unreachable"), Into::into))
    }
}

/// Background-refreshed chain tip height — same fix, same reason, as
/// `price::PriceCache`: any upstream this proxies to can go slow or
/// unreachable, and `forward`'s retry-with-backoff means recovering from
/// that can legitimately take many seconds on the request path. Found live,
/// 2026-09-13: `--btc-upstream https://mempool.space/api` reliably timed out
/// reading a response through this exact `ureq`-based client (confirmed
/// with added logging: consistent read timeouts, not occasional), while a
/// plain `curl` to the identical URL from the same machine answered in
/// under a second every time, and the same client code against
/// `https://blockstream.info/api` succeeded in ~0.5s — which is why the
/// deployed upstream is blockstream.info now, not mempool.space. This cache
/// exists regardless of which upstream is configured: `/blocks/tip/height`'s
/// old 5s response-cache TTL was shorter than a single bad-upstream retry
/// cycle can take, so almost every request paid the full synchronous cost
/// whenever the upstream degraded. `current()` only ever locks a mutex — it
/// never touches the network, so a degraded upstream can no longer stall a
/// request, it just leaves the served height briefly behind by up to one
/// `interval`.
pub struct TipCache {
    current: Arc<Mutex<Option<u64>>>,
}

impl TipCache {
    /// `interval` is how often to refresh on success; a failed fetch (or an
    /// unparseable body) retries sooner (5s) so a transient blip recovers
    /// quickly instead of leaving a stale height up for the full interval.
    ///
    /// `upstreams` is tried in order every cycle, exactly like the main
    /// per-request path's `btc_fallbacks` — a single upstream here has no
    /// failover of its own, so an outage specific to *just* the primary (a
    /// paid tier's credits running out, say) permanently starves this cache
    /// even though every other route already recovers via the fallback
    /// chain. Found live, 2026-09-15: Maestro (the primary once
    /// `--btc-maestro-key` is set) had its credits temporarily exhausted;
    /// address lookups kept working fine via the fallback chain, but this
    /// cache — constructed with only the primary — never got a single
    /// successful height and served 503 indefinitely, which starves
    /// `/blocks/tip/height` and, downstream, the whole wallet-refresh
    /// pipeline that calls it first.
    pub fn spawn(upstreams: Vec<Upstream>, interval: Duration) -> Self {
        let current = Arc::new(Mutex::new(None));
        let bg = current.clone();
        std::thread::spawn(move || loop {
            // Only failures are logged — like PriceCache, a healthy cycle stays
            // silent. This is what caught the mempool.space issue: without it,
            // "no successful height yet" and "actively failing every attempt"
            // looked identical from the outside.
            let mut height = None;
            for upstream in &upstreams {
                let t0 = std::time::Instant::now();
                let result = upstream.forward(&Method::Get, "blocks/tip/height", "", &[]);
                height = match &result {
                    Ok(r) if r.status == 200 => {
                        match std::str::from_utf8(&r.body).ok().and_then(|s| s.trim().parse::<u64>().ok()) {
                            Some(h) => Some(h),
                            None => {
                                eprintln!("tip-cache: unparseable body: {:?}", String::from_utf8_lossy(&r.body));
                                None
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("tip-cache: upstream returned status {}", r.status);
                        None
                    }
                    Err(e) => {
                        eprintln!("tip-cache: fetch failed after {:?}: {e:#}", t0.elapsed());
                        None
                    }
                };
                if height.is_some() {
                    break;
                }
            }
            let sleep_for = match height {
                Some(h) => {
                    *bg.lock().unwrap() = Some(h);
                    interval
                }
                None => Duration::from_secs(5),
            };
            std::thread::sleep(sleep_for);
        });
        Self { current }
    }

    /// The last successfully fetched tip height, or `None` before the first
    /// fetch completes (briefly, at startup) — never blocks.
    pub fn current(&self) -> Option<u64> {
        *self.current.lock().unwrap()
    }
}
