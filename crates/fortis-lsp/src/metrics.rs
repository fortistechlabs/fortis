//! Prometheus text for `GET /metrics`, and the low-liquidity check.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ldk_node::LightningBalance;

/// One reading of the node's state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub onchain_sat: u64,
    pub channels: u64,
    pub inbound_sat: u64,
    pub outbound_sat: u64,
    pub failed_forwards_total: u64,
    pub pending_htlcs: u64,
    pub peers: u64,
    pub forwards_total: u64,
}

/// Counters fed by the event loop.
#[derive(Default)]
pub struct Counters {
    pub forwards: AtomicU64,
    /// Always 0: ldk-node 0.7.1 handles LDK's `HTLCHandlingFailed`
    /// internally and surfaces no event for it (see README).
    pub failed_forwards: AtomicU64,
}

impl Snapshot {
    /// Reads `node` (cheap, in-memory calls).
    pub fn read(node: &ldk_node::Node, counters: &Counters) -> Snapshot {
        let balances = node.list_balances();
        let channels = node.list_channels();
        // ldk-node 0.7.1 exposes no in-channel HTLC list; HTLCs left
        // unresolved on chain after a close are the closest it offers.
        let pending_htlcs = balances
            .lightning_balances
            .iter()
            .filter(|b| {
                matches!(
                    b,
                    LightningBalance::MaybeTimeoutClaimableHTLC { .. }
                        | LightningBalance::MaybePreimageClaimableHTLC { .. }
                        | LightningBalance::ContentiousClaimable { .. }
                )
            })
            .count() as u64;
        Snapshot {
            onchain_sat: balances.total_onchain_balance_sats,
            channels: channels.len() as u64,
            inbound_sat: channels
                .iter()
                .map(|c| c.inbound_capacity_msat / 1_000)
                .sum(),
            outbound_sat: channels
                .iter()
                .map(|c| c.outbound_capacity_msat / 1_000)
                .sum(),
            failed_forwards_total: counters.failed_forwards.load(Ordering::Relaxed),
            pending_htlcs,
            peers: node.list_peers().iter().filter(|p| p.is_connected).count() as u64,
            forwards_total: counters.forwards.load(Ordering::Relaxed),
        }
    }
}

pub fn render(s: &Snapshot) -> String {
    let mut out = String::new();
    for (name, kind, help, val) in [
        (
            "fortis_lsp_onchain_sat",
            "gauge",
            "on-chain wallet balance (sat)",
            s.onchain_sat,
        ),
        (
            "fortis_lsp_channels",
            "gauge",
            "channels (any state)",
            s.channels,
        ),
        (
            "fortis_lsp_inbound_sat",
            "gauge",
            "total inbound capacity (sat)",
            s.inbound_sat,
        ),
        (
            "fortis_lsp_outbound_sat",
            "gauge",
            "total outbound capacity (sat)",
            s.outbound_sat,
        ),
        (
            "fortis_lsp_failed_forwards_total",
            "counter",
            "failed forwards (always 0: not exposed by ldk-node 0.7.1)",
            s.failed_forwards_total,
        ),
        (
            "fortis_lsp_pending_htlcs",
            "gauge",
            "HTLCs awaiting on-chain resolution after a channel close",
            s.pending_htlcs,
        ),
        ("fortis_lsp_peers", "gauge", "connected peers", s.peers),
        (
            "fortis_lsp_forwards_total",
            "counter",
            "payments forwarded since start",
            s.forwards_total,
        ),
    ] {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {val}\n"
        ));
    }
    out
}

/// True when the on-chain balance is below the warning threshold
/// (`low_liquidity_sat`; 0 disables the warning).
pub fn low_liquidity(onchain_sat: u64, threshold_sat: u64) -> bool {
    onchain_sat < threshold_sat
}

/// Serves `GET /metrics` (Prometheus text) and `GET /info` (`info`, JSON) on
/// `server` until `stop` is set.
pub fn serve(
    server: tiny_http::Server,
    node: Arc<ldk_node::Node>,
    counters: Arc<Counters>,
    info: String,
    stop: Arc<AtomicBool>,
) {
    let text = tiny_http::Header::from_bytes("Content-Type", "text/plain; version=0.0.4").unwrap();
    let json = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
    while !stop.load(Ordering::SeqCst) {
        let req = match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("fortis-lsp: metrics listener: {e}");
                return;
            }
        };
        let resp = match (req.method(), req.url()) {
            (tiny_http::Method::Get, "/metrics") => {
                tiny_http::Response::from_string(render(&Snapshot::read(&node, &counters)))
                    .with_header(text.clone())
            }
            (tiny_http::Method::Get, "/info") => {
                tiny_http::Response::from_string(info.clone()).with_header(json.clone())
            }
            _ => tiny_http::Response::from_string("not found\n").with_status_code(404),
        };
        let _ = req.respond(resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_metric_with_its_value() {
        let s = Snapshot {
            onchain_sat: 5_000_000,
            channels: 3,
            inbound_sat: 40,
            outbound_sat: 50,
            failed_forwards_total: 6,
            pending_htlcs: 7,
            peers: 2,
            forwards_total: 9,
        };
        let text = render(&s);
        for (name, kind, val) in [
            ("fortis_lsp_onchain_sat", "gauge", 5_000_000),
            ("fortis_lsp_channels", "gauge", 3),
            ("fortis_lsp_inbound_sat", "gauge", 40),
            ("fortis_lsp_outbound_sat", "gauge", 50),
            ("fortis_lsp_failed_forwards_total", "counter", 6),
            ("fortis_lsp_pending_htlcs", "gauge", 7),
            ("fortis_lsp_peers", "gauge", 2),
            ("fortis_lsp_forwards_total", "counter", 9),
        ] {
            assert!(
                text.contains(&format!("# TYPE {name} {kind}\n")),
                "{name}: {text}"
            );
            assert!(
                text.contains(&format!("\n{name} {val}\n")),
                "{name}: {text}"
            );
        }
    }

    #[test]
    fn low_liquidity_is_strictly_below_the_threshold() {
        assert!(low_liquidity(1_999_999, 2_000_000));
        assert!(!low_liquidity(2_000_000, 2_000_000));
        assert!(!low_liquidity(5_000_000, 2_000_000));
        // A zero threshold switches the warning off.
        assert!(!low_liquidity(0, 0));
    }
}
