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
    /// Fortis accounts: all balances held for users (sat).
    pub custody_sat: u64,
    /// The network graph we route over: channels and nodes known.
    pub graph_channels: u64,
    pub graph_nodes: u64,
    /// The largest single payment our channels accept in (LDK's limit), sat.
    pub inbound_htlc_max_sat: u64,
    /// The largest payment the network is told it may send us over a
    /// channel (the peer's `htlc_maximum_msat` towards us in gossip), sat.
    pub inbound_policy_max_sat: u64,
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
            custody_sat: 0,
            inbound_htlc_max_sat: channels
                .iter()
                .filter(|c| c.is_usable)
                .filter_map(|c| c.inbound_htlc_maximum_msat)
                .max()
                .unwrap_or(0)
                / 1_000,
            inbound_policy_max_sat: {
                let g = node.network_graph();
                let me = ldk_node::lightning::routing::gossip::NodeId::from_pubkey(&node.node_id());
                channels
                    .iter()
                    .filter_map(|c| c.short_channel_id)
                    .filter_map(|scid| g.channel(scid))
                    .filter_map(|i| {
                        // The direction towards us: from the other node.
                        if i.node_two == me { i.one_to_two.clone() } else { i.two_to_one.clone() }
                    })
                    .map(|u| u.htlc_maximum_msat / 1_000)
                    .max()
                    .unwrap_or(0)
            },
            graph_channels: node.network_graph().list_channels().len() as u64,
            graph_nodes: node.network_graph().list_nodes().len() as u64,
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
        (
            "fortis_lsp_custody_sat",
            "gauge",
            "Fortis account balances held for users (sat)",
            s.custody_sat,
        ),
        (
            "fortis_lsp_inbound_htlc_max_sat",
            "gauge",
            "largest single payment our channels accept in (sat)",
            s.inbound_htlc_max_sat,
        ),
        (
            "fortis_lsp_inbound_policy_max_sat",
            "gauge",
            "largest payment the network may send us per its gossip (sat)",
            s.inbound_policy_max_sat,
        ),
        (
            "fortis_lsp_graph_channels",
            "gauge",
            "channels in the network graph we route over",
            s.graph_channels,
        ),
        (
            "fortis_lsp_graph_nodes",
            "gauge",
            "nodes in the network graph we route over",
            s.graph_nodes,
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
    accounts: Option<Arc<crate::account_api::AccountService>>,
    admin_token: Option<String>,
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
        let mut req = req;
        let resp = match (req.method(), req.url()) {
            (tiny_http::Method::Get, "/metrics") => {
                let mut s = Snapshot::read(&node, &counters);
                s.custody_sat = accounts.as_ref().map_or(0, |a| a.custody_sat());
                tiny_http::Response::from_string(render(&s)).with_header(text.clone())
            }
            (tiny_http::Method::Post, "/admin/pay" | "/admin/close") => {
                let is_pay = req.url() == "/admin/pay";
                let (status, body) = match &admin_token {
                    Some(token) => {
                        let auth = req
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("Authorization"))
                            .map(|h| h.value.as_str().to_string());
                        let mut buf = Vec::new();
                        let _ = std::io::Read::read_to_end(
                            &mut std::io::Read::take(req.as_reader(), 16 * 1024),
                            &mut buf,
                        );
                        if is_pay {
                            crate::admin::pay(&*node, auth.as_deref(), token, &buf)
                        } else {
                            crate::admin::close(&*node, auth.as_deref(), token, &buf)
                        }
                    }
                    None => (404, serde_json::json!({ "error": "admin disabled" })),
                };
                tiny_http::Response::from_string(body.to_string())
                    .with_status_code(status)
                    .with_header(json.clone())
            }
            (tiny_http::Method::Post, "/account") => {
                let (status, body) = match &accounts {
                    Some(a) => {
                        let sig = req
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("X-Fortis-Sig"))
                            .map(|h| h.value.as_str().to_string());
                        let mut buf = Vec::new();
                        let limit = crate::account_api::MAX_BODY as u64 + 1;
                        let _ = std::io::Read::read_to_end(
                            &mut std::io::Read::take(req.as_reader(), limit),
                            &mut buf,
                        );
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs());
                        a.handle(&buf, sig.as_deref(), now)
                    }
                    None => (404, serde_json::json!({ "error": "accounts_disabled" })),
                };
                tiny_http::Response::from_string(body.to_string())
                    .with_status_code(status)
                    .with_header(json.clone())
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
            custody_sat: 11,
            graph_channels: 12,
            graph_nodes: 13,
            inbound_htlc_max_sat: 14,
            inbound_policy_max_sat: 15,
        };
        let text = render(&s);
        for (name, kind, val) in [
            ("fortis_lsp_onchain_sat", "gauge", 5_000_000),
            ("fortis_lsp_channels", "gauge", 3),
            ("fortis_lsp_graph_channels", "gauge", 12),
            ("fortis_lsp_graph_nodes", "gauge", 13),
            ("fortis_lsp_inbound_sat", "gauge", 40),
            ("fortis_lsp_outbound_sat", "gauge", 50),
            ("fortis_lsp_failed_forwards_total", "counter", 6),
            ("fortis_lsp_pending_htlcs", "gauge", 7),
            ("fortis_lsp_peers", "gauge", 2),
            ("fortis_lsp_forwards_total", "counter", 9),
            ("fortis_lsp_custody_sat", "gauge", 11),
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
