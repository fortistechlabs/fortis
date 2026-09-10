//! A handful of counters, rendered as Prometheus text at `/metrics`.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub registered: AtomicU64,
    pub cache_hits: AtomicU64,
    pub rate_limited: AtomicU64,
    pub unauthorized: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub crash_reports: AtomicU64,
    pub fee_rejected: AtomicU64,
    pub panics: AtomicU64,
}

impl Metrics {
    pub fn inc(field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let g = |f: &AtomicU64| f.load(Ordering::Relaxed);
        let mut s = String::new();
        for (name, help, val) in [
            ("fortis_edge_requests_total", "requests received", g(&self.requests)),
            ("fortis_edge_registered_total", "tokens issued", g(&self.registered)),
            ("fortis_edge_cache_hits_total", "responses served from cache", g(&self.cache_hits)),
            ("fortis_edge_rate_limited_total", "requests rejected by the limiter", g(&self.rate_limited)),
            ("fortis_edge_unauthorized_total", "requests with a missing/bad token", g(&self.unauthorized)),
            ("fortis_edge_upstream_errors_total", "failed upstream calls", g(&self.upstream_errors)),
            ("fortis_edge_crash_reports_total", "crash reports accepted", g(&self.crash_reports)),
            ("fortis_edge_fee_rejected_total", "broadcasts rejected for not paying the service fee", g(&self.fee_rejected)),
            ("fortis_edge_panics_total", "request handlers that panicked and were caught", g(&self.panics)),
        ] {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n"));
        }
        s
    }
}
