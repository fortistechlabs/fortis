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

use std::str::FromStr;
use std::time::Duration;

use common::{
    builder, poll, procs_referencing, test_seed, Lsp, Regtest, TempDir, Vss, LSP_FUNDING_SAT,
};
use fortis_lightning::{ChainSource, Node, NodeConfig, VssConfig};
use ldk_node::bitcoin::secp256k1::PublicKey;
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

/// `[[outbound]]` in lsp.toml: the LSP opens one announced channel of the
/// configured size to the listed routing node once its wallet has synced,
/// and does not open a second one after a restart while the first exists.
#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND"]
fn opens_the_configured_outbound_channel_once() {
    const OUTBOUND_SAT: u64 = 150_000;
    let rt = Regtest::start();

    // A stock ldk-node as the routing node. Like a real one it is public
    // (an alias: ldk-node rejects announced channels without one), and it
    // needs a little on-chain balance to accept an anchor channel.
    let dir = TempDir::new("router");
    let (mut b, router_addr) = builder(&rt, test_seed(0x6B), &dir.str("router"));
    b.set_node_alias("test-router".into()).unwrap();
    let router = b.build_with_fs_store().unwrap();
    router.start().unwrap();
    let addr = router.onchain_payment().new_address().unwrap();
    rt.fund(&addr.to_string(), 200_000);
    rt.mine(6);
    poll("router sees its funding", 90, || {
        router.sync_wallets().ok();
        (router.list_balances().spendable_onchain_balance_sats > 0).then_some(())
    });

    let outbound = format!(
        "[{{ peer = \"{}@{}\", sat = {OUTBOUND_SAT} }}]",
        router.node_id(),
        router_addr
    );
    let mut lsp = Lsp::start(
        &rt,
        &[
            ("alias", "\"fortis-test\""),
            ("announce_addresses", "[\"127.0.0.2:9735\"]"),
            ("outbound", &outbound),
        ],
    );
    let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !router
        .list_channels()
        .iter()
        .any(|c| c.counterparty_node_id == lsp_id)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the LSP did not open the outbound channel:\n{}",
            lsp.stderr()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    rt.mine(6);
    let ch = poll("the outbound channel is ready", 120, || {
        router.sync_wallets().ok();
        router
            .list_channels()
            .into_iter()
            .find(|c| c.counterparty_node_id == lsp_id && c.is_channel_ready)
    });
    assert_eq!(ch.channel_value_sats, OUTBOUND_SAT);
    assert!(ch.is_announced, "outbound channels are announced");
    assert!(!ch.is_outbound, "the LSP opened it, not the router");
    let opens = |l: &Lsp| l.stderr().matches("opening announced channel").count();
    assert_eq!(opens(&lsp), 1, "{}", lsp.stderr());

    // A restart sees the channel and opens nothing.
    lsp.restart();
    std::thread::sleep(Duration::from_secs(25));
    assert_eq!(opens(&lsp), 1, "reopened after restart:\n{}", lsp.stderr());
    assert_eq!(
        router
            .list_channels()
            .iter()
            .filter(|c| c.counterparty_node_id == lsp_id)
            .count(),
        1
    );
    router.stop().unwrap();
}
