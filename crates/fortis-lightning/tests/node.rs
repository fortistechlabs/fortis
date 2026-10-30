//! `Node` start/stop against regtest, a stand-in LSP (a stock ldk-node) and
//! VSS. Tests needing external binaries are `#[ignore]`d; run them with:
//!
//! ```sh
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test node -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{builder, poll, test_seed, Regtest, TempDir, Vss};
use fortis_lightning::{
    test_hooks, ChainSource, LnError, LspInfo, LspLimits, Node, NodeConfig, VssConfig,
};
use ldk_node::bitcoin::Network;

const LIMITS: LspLimits = LspLimits {
    min_payment_msat: 10_000_000,
    max_payment_msat: 500_000_000,
    channel_min_sat: 100_000,
    channel_max_sat: 500_000,
};

/// A stock ldk-node standing in for `fortis-lsp` (Task 8): listens on a free
/// local port, filesystem store.
struct StandInLsp {
    node: ldk_node::Node,
    port: u16,
    _dir: TempDir,
}

impl StandInLsp {
    fn start(rt: &Regtest) -> StandInLsp {
        let dir = TempDir::new("lsp");
        let (b, addr) = builder(rt, test_seed(0xA5), &dir.str("lsp"));
        let node = b.build_with_fs_store().unwrap();
        node.start().unwrap();
        let port = addr
            .to_string()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        StandInLsp {
            node,
            port,
            _dir: dir,
        }
    }

    fn info(&self) -> LspInfo {
        LspInfo {
            node_id: self.node.node_id().to_string(),
            host: "127.0.0.1".into(),
            port: self.port,
            token: None,
        }
    }

    fn has_peer(&self, node_id: &str) -> bool {
        self.node
            .list_peers()
            .iter()
            .any(|p| p.node_id.to_string() == node_id && p.is_connected)
    }
}

impl Drop for StandInLsp {
    fn drop(&mut self) {
        let _ = self.node.stop();
    }
}

fn regtest_chain(rt: &Regtest) -> ChainSource {
    ChainSource::BitcoindRpc {
        host: "127.0.0.1".into(),
        port: rt.rpc_port,
        user: "fortis".into(),
        password: "fortis".into(),
    }
}

fn config(
    seed: [u8; 64],
    data_dir: &TempDir,
    name: &str,
    chain: ChainSource,
    lsp: LspInfo,
    vss_url: &str,
) -> NodeConfig {
    NodeConfig {
        bip39_seed: seed.to_vec(),
        network: Network::Regtest,
        data_dir: data_dir.path(name),
        chain_source: chain,
        lsp,
        vss: VssConfig {
            url: vss_url.to_string(),
            auth_header: "Bearer test".into(),
        },
        limits: LIMITS,
    }
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn starts_connects_to_lsp_and_stops() {
    let rt = Regtest::start();
    let vss = Vss::start();
    let lsp = StandInLsp::start(&rt);
    let dirs = TempDir::new("node-start");
    let seed = test_seed(1);

    let node = Node::new(config(
        seed,
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    ))
    .unwrap();
    let id = node.node_id();
    assert_eq!(id.len(), 66, "hex compressed pubkey: {id}");
    assert_eq!(node.limits(), LIMITS);
    assert!(!lsp.has_peer(&id), "Node::new must not touch the network");

    let t = Instant::now();
    node.start().expect("start");
    eprintln!("start took {:?}", t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(15));
    assert_eq!(
        node.node_id(),
        id,
        "node id before start is ldk-node's node id"
    );
    poll("LSP sees the client as a peer", 10, || {
        lsp.has_peer(&id).then_some(())
    });
    let b = node.balances();
    assert_eq!(
        (
            b.lightning_sat,
            b.onchain_transit_sat,
            b.pending_close_sat,
            b.inbound_sat
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(node.next_event(Duration::from_millis(300)), None);
    node.stop();
    poll("LSP sees the client disconnect", 10, || {
        (!lsp.has_peer(&id)).then_some(())
    });
    drop(node);

    // Restart from the same seed and data dir: same node id, reconnects.
    let node = Node::new(config(
        seed,
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    ))
    .unwrap();
    assert_eq!(node.node_id(), id);
    node.start().expect("restart");
    poll("LSP sees the restarted client", 10, || {
        lsp.has_peer(&id).then_some(())
    });
    node.stop();
    drop(node);

    // Same seed, wiped data dir: state comes back from VSS, same node id.
    let node = Node::new(config(
        seed,
        &dirs,
        "a-restored",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    ))
    .unwrap();
    assert_eq!(node.node_id(), id);
    node.start().expect("start from a fresh data dir");
    assert_eq!(node.node_id(), id);
    node.stop();
    drop(node);

    // Another seed: another node id.
    let other = Node::new(config(
        test_seed(2),
        &dirs,
        "b",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    ))
    .unwrap();
    assert_ne!(other.node_id(), id);
    other.start().expect("start other");
    poll("LSP sees the other client", 10, || {
        lsp.has_peer(&other.node_id()).then_some(())
    });
    other.stop();
}

/// Review Focus 4: no network at all (LSP, VSS and chain source unroutable —
/// TEST-NET-1, RFC 5737) ends in `LspUnreachable`, within 15 s. Needs no
/// external binaries.
#[test]
fn start_without_network_returns_lsp_unreachable_within_15s() {
    let dirs = TempDir::new("node-nonet");
    let lsp = LspInfo {
        node_id: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".into(),
        host: "192.0.2.1".into(),
        port: 9735,
        token: Some("t".into()),
    };
    let chain = ChainSource::Esplora {
        url: "http://192.0.2.1:3002".into(),
    };
    let node = Node::new(config(
        test_seed(3),
        &dirs,
        "a",
        chain,
        lsp,
        "http://192.0.2.1/vss",
    ))
    .unwrap();
    let t = Instant::now();
    let res = node.start();
    eprintln!("start without network: {res:?} after {:?}", t.elapsed());
    assert_eq!(res, Err(LnError::LspUnreachable));
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "took {:?}",
        t.elapsed()
    );
    // Still usable: a stopped node reports zero and no events.
    assert_eq!(node.balances().lightning_sat, 0);
    assert_eq!(node.next_event(Duration::from_millis(10)), None);
    node.stop();
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn vss_down_start_returns_backup_unavailable() {
    let rt = Regtest::start();
    let lsp = StandInLsp::start(&rt);
    let dirs = TempDir::new("node-vssdown");
    let seed = test_seed(4);

    // A node that has run with VSS before …
    let vss = Vss::start();
    let url = vss.url.clone();
    let node = Node::new(config(
        seed,
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &url,
    ))
    .unwrap();
    node.start().expect("start with vss up");
    node.stop();
    drop(node);
    // … then VSS goes away (and the LSP is still reachable).
    drop(vss);

    let node = Node::new(config(
        seed,
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &url,
    ))
    .unwrap();
    let t = Instant::now();
    let res = node.start();
    eprintln!("start with vss down: {res:?} after {:?}", t.elapsed());
    assert_eq!(res, Err(LnError::BackupUnavailable));
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "took {:?}",
        t.elapsed()
    );
    assert!(
        !lsp.has_peer(&node.node_id()),
        "must not connect without a backup"
    );
}

/// No ldk-node may be up for `id` (polls for the LSP to see the disconnect,
/// then checks it stays down).
fn assert_not_connected(lsp: &StandInLsp, id: &str) {
    poll("LSP sees no client", 10, || {
        (!lsp.has_peer(id)).then_some(())
    });
    std::thread::sleep(Duration::from_secs(1));
    assert!(!lsp.has_peer(id), "a node came up after stop");
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn concurrent_starts_build_one_node() {
    let rt = Regtest::start();
    let vss = Vss::start();
    let lsp = StandInLsp::start(&rt);
    let dirs = TempDir::new("node-concurrent");
    let cfg = config(
        test_seed(5),
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    );
    let node = Arc::new(Node::new(cfg).unwrap());
    let before = test_hooks::builds();
    test_hooks::reset_max_live();
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let n = Arc::clone(&node);
            std::thread::spawn(move || n.start())
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    eprintln!("concurrent starts: {results:?}");
    assert_eq!(results, vec![Ok(()), Ok(())]);
    assert_eq!(
        test_hooks::builds() - before,
        1,
        "exactly one ldk-node built"
    );
    assert_eq!(test_hooks::max_live(), 1);
    poll("LSP sees the client", 10, || {
        lsp.has_peer(&node.node_id()).then_some(())
    });
    node.stop();
    assert_not_connected(&lsp, &node.node_id());
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn stop_during_pending_start_leaves_no_node_running() {
    let rt = Regtest::start();
    let vss = Vss::start();
    let lsp = StandInLsp::start(&rt);
    let dirs = TempDir::new("node-stop-pending");
    let seed = test_seed(6);
    for delay_ms in [0u64, 5, 20, 40, 80] {
        let cfg = config(seed, &dirs, "a", regtest_chain(&rt), lsp.info(), &vss.url);
        let node = Arc::new(Node::new(cfg).unwrap());
        let id = node.node_id();
        test_hooks::reset_max_live();
        let n = Arc::clone(&node);
        let pending = std::thread::spawn(move || n.start());
        std::thread::sleep(Duration::from_millis(delay_ms));
        node.stop();
        // A second Node for the same wallet, while the cancelled worker may
        // still be running: it waits for it, never runs alongside it.
        let again = Node::new(config(
            seed,
            &dirs,
            "a",
            regtest_chain(&rt),
            lsp.info(),
            &vss.url,
        ))
        .unwrap();
        let again_res = again.start();
        let res = pending.join().unwrap();
        eprintln!("stop after {delay_ms} ms: start returned {res:?}, second Node {again_res:?}");
        assert!(
            res == Ok(()) || res == Err(LnError::Other("cancelled".into())),
            "{res:?}"
        );
        if again_res.is_err() {
            // Only when the first start call landed after stop() (stop cancels
            // a start in flight, not a later one): its node runs, so the second
            // Node is refused, and starts once the first stops.
            assert_eq!(again_res, Err(LnError::Other("already_running".into())));
            assert_eq!(res, Ok(()));
            node.stop();
            again
                .start()
                .expect("second Node starts once the first stopped");
        }
        assert!(
            test_hooks::max_live() <= 1,
            "two ldk-nodes were alive at once"
        );
        poll("LSP sees the second node", 10, || {
            lsp.has_peer(&id).then_some(())
        });
        again.stop();
        assert_not_connected(&lsp, &id);
    }
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn second_node_on_same_wallet_cannot_start_alongside() {
    let rt = Regtest::start();
    let vss = Vss::start();
    let lsp = StandInLsp::start(&rt);
    let dirs = TempDir::new("node-second");
    let seed = test_seed(7);
    let first = Node::new(config(
        seed,
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    ))
    .unwrap();
    first.start().unwrap();

    let before = test_hooks::builds();
    test_hooks::reset_max_live();
    let second = Node::new(config(
        seed,
        &dirs,
        "a",
        regtest_chain(&rt),
        lsp.info(),
        &vss.url,
    ))
    .unwrap();
    let t = Instant::now();
    let res = second.start();
    eprintln!(
        "second start while first runs: {res:?} after {:?}",
        t.elapsed()
    );
    assert!(
        matches!(&res, Err(LnError::Other(m)) if m == "already_running"),
        "{res:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(15));
    assert_eq!(
        test_hooks::builds(),
        before,
        "the second Node built nothing"
    );
    assert_eq!(test_hooks::max_live(), 1);
    second.stop(); // must not stop the first
    std::thread::sleep(Duration::from_millis(500));
    assert!(lsp.has_peer(&first.node_id()));

    // Once the first stops, the second may start.
    first.stop();
    second
        .start()
        .expect("second starts after the first stopped");
    second.stop();
}
