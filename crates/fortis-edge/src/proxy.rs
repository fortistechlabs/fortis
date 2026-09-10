//! Forward a request to a chain's upstream (a `fortis-index` instance for XBT, an
//! Esplora API for BTC) and return the raw response.

use anyhow::Result;
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
