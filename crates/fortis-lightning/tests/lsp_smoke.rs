//! The real `fortis-lsp` binary as the harness LSP. Needs the binary built
//! and the regtest stack:
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test lsp_smoke -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::path::Path;

use common::{poll, procs_referencing, test_seed, Lsp, Regtest, TempDir, Vss, LSP_FUNDING_SAT};
use fortis_lightning::{ChainSource, Node, NodeConfig, VssConfig};
use ldk_node::bitcoin::Network;

const METRICS: [&str; 6] = [
    "fortis_lsp_onchain_sat",
    "fortis_lsp_channels",
    "fortis_lsp_inbound_sat",
    "fortis_lsp_outbound_sat",
    "fortis_lsp_failed_forwards_total",
    "fortis_lsp_pending_htlcs",
];

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn client_connects_to_fortis_lsp() {
    let rt = Regtest::start();
    let vss = Vss::start();
    // low_liquidity_sat above its funding: the first check (5 s after
    // start) must warn.
    let mut lsp = Lsp::start(
        &rt,
        &[
            ("token", "\"smoke-token\""),
            ("low_liquidity_sat", "1000000000"),
        ],
    );

    let text = lsp.metrics();
    for m in METRICS {
        assert!(
            text.contains(&format!("# TYPE {m} ")),
            "{m} missing:\n{text}"
        );
    }
    assert!(lsp.metric("fortis_lsp_onchain_sat") >= LSP_FUNDING_SAT);
    assert_eq!(lsp.metric("fortis_lsp_peers"), 0);
    assert_eq!(lsp.info["token_required"], true);
    assert!(!lsp.metrics().contains("smoke-token") && !lsp.stderr().contains("smoke-token"));
    let limits = lsp.limits();
    assert_eq!(
        (
            limits.min_payment_msat,
            limits.max_payment_msat,
            limits.channel_min_sat,
            limits.channel_max_sat
        ),
        (51_000_000, 250_000_000, 100_000, 500_000),
        "/info: effective payment bounds for the spec defaults"
    );

    // A client `Node` pointed at the printed node id: its start connects
    // (the noise handshake proves the LSP holds that key) and the LSP
    // counts it as a peer.
    let dirs = TempDir::new("lsp-smoke");
    let client = Node::new(NodeConfig {
        bip39_seed: test_seed(7).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path("client"),
        chain_source: ChainSource::BitcoindRpc {
            host: "127.0.0.1".into(),
            port: rt.rpc_port,
            user: "fortis".into(),
            password: "fortis".into(),
        },
        lsp: lsp.lsp_info(),
        vss: VssConfig {
            url: vss.url.clone(),
            auth_header: "Bearer test".into(),
        },
        limits,
    })
    .unwrap();
    client.start().expect("client start against fortis-lsp");
    poll("fortis-lsp counts the client as a peer", 15, || {
        (lsp.metric("fortis_lsp_peers") >= 1).then_some(())
    });
    client.stop();
    drop(client);
    poll("low-liquidity warning", 20, || {
        lsp.stderr().contains("WARNING low liquidity").then_some(())
    });

    // SIGTERM stops it cleanly; the seed in its data dir keeps the node id.
    let id = lsp.node_id.clone();
    assert_eq!(lsp.restart(), id, "same node id after a restart");
    let seed = lsp.dir().join("data/keys_seed");
    let mode = std::fs::metadata(&seed).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o077,
        0,
        "seed file private"
    );
    assert!(
        lsp.stderr().contains("fortis-lsp: stopped"),
        "{}",
        lsp.stderr()
    );

    // Dropping the harness leaves no process and no dir behind.
    let (pid, dir) = (lsp.pid().unwrap(), lsp.dir());
    // What `sweep_dead_runs` would find had this test process died.
    assert!(
        procs_referencing(dir.to_str().unwrap()).contains(&pid),
        "the dead-run sweep must find fortis-lsp by its temp dir"
    );
    drop(lsp);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "fortis-lsp still running"
    );
    assert!(!dir.exists(), "temp dir left behind");
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND"]
fn lsp_start_failure_cleans_up() {
    let rt = Regtest::start();
    // Rejected by the binary's config validation: exits during startup.
    let r = std::panic::catch_unwind(|| Lsp::start(&rt, &[("open_fee_ppm", "200000")]));
    let msg = r
        .err()
        .and_then(|e| e.downcast_ref::<String>().cloned())
        .expect("start must panic");
    assert!(msg.contains("open_fee_ppm"), "{msg}");
    let ours = format!("fortis-ln-test-fortis-lsp-{}-", std::process::id());
    assert!(procs_referencing(&ours).is_empty(), "stray fortis-lsp");
    let left: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&ours))
        .collect();
    assert!(left.is_empty(), "temp dirs left: {left:?}");
}
