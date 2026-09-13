//! Forward a request to a chain's upstream (a `fortis-index` instance for XBT, an
//! Esplora API for BTC) and return the raw response.

use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::Method;

pub struct Upstream {
    base: String,
    agent: ureq::Agent,
}

pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Upstream {
    pub fn new(base: &str) -> Self {
        // Short per-request timeouts: an address lookup that healthy explorers
        // answer in ~1 s is hanging if it takes longer, and a wallet scan can't
        // afford to wait — fail fast and let `forward`'s retry move on.
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(8))
            .timeout_write(std::time::Duration::from_secs(8));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        Self { base: base.trim_end_matches('/').to_string(), agent: b.build() }
    }

    /// `rest` is the path after `/{chain}` (no leading slash), `query` without `?`.
    ///
    /// A GET that hits a transport error or a `429` / `5xx` is retried twice with
    /// backoff — public explorers throw those intermittently under a wallet's
    /// fan-out scan, and one bad response would otherwise abort the whole scan.
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
        let retries = if *method == Method::Get { 2 } else { 0 };

        let mut last_err = None;
        for attempt in 0..=retries {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200 * attempt as u64));
            }
            let resp = match method {
                Method::Get => self.agent.get(&url).call(),
                _ => self.agent.post(&url).set("Content-Type", "text/plain").send_bytes(body),
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
    pub fn spawn(upstream: Upstream, interval: Duration) -> Self {
        let current = Arc::new(Mutex::new(None));
        let bg = current.clone();
        std::thread::spawn(move || loop {
            // Only failures are logged — like PriceCache, a healthy cycle stays
            // silent. This is what caught the mempool.space issue: without it,
            // "no successful height yet" and "actively failing every attempt"
            // looked identical from the outside.
            let t0 = std::time::Instant::now();
            let result = upstream.forward(&Method::Get, "blocks/tip/height", "", &[]);
            let height = match &result {
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
