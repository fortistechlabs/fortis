//! Moving funds onto Lightning and back: `Node::transit_address`,
//! `open_channel`, `close_channel`, `force_close_channel`, `sweep_transit`
//! and `channel_state`, against the real `fortis-lsp`. Needs the binary
//! built and the regtest stack:
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test channel -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The tests that are not `#[ignore]`d need no binaries: the address checks
//! run before the node is consulted.
//!
//! **Numbers.** On regtest ldk-node's fee estimates fall back to 1 sat/vB
//! (250 sat/kWU, floored by LDK to 253): the client opens an anchor channel
//! (the LSP, a default ldk-node, negotiates `anchors_zero_fee_htlc_tx`), so
//! as funder it pays the commitment fee (253 × 1124 / 1000 = 284 sat) and
//! the two 330-sat anchors out of its side, and keeps ldk-node's default
//! 25 000 sat anchor reserve back in the transit wallet while the channel
//! exists. The LSP (stock LDK `minimum_depth`) wants 6 confirmations of the
//! funding transaction.

mod common;

use std::str::FromStr;
use std::time::{Duration, Instant};

use common::{poll, test_seed, Lsp, Regtest, TempDir, Vss};
use fortis_lightning::{
    ChainSource, ChannelState, LnError, LnEvent, LspInfo, LspLimits, Node, NodeConfig, VssConfig,
};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::Builder;

const SAT: u64 = 1_000;
const CHANNEL_SAT: u64 = 150_000;
const TRANSIT_SAT: u64 = 300_000;
/// ldk-node's default `AnchorChannelsConfig::per_channel_reserve_sats`.
const ANCHOR_RESERVE_SAT: u64 = 25_000;
/// What the funder of an anchor channel gives up on its side at 253 sat/kWU:
/// the commitment fee (1124 WU) and two 330-sat anchors.
const FUNDER_COMMITMENT_COST_SAT: u64 = 253 * 1124 / 1000 + 2 * 330;
/// Upper bound for one regtest wallet transaction's fee at ~1 sat/vB.
const MAX_TX_FEE_SAT: u64 = 1_000;

/// Regtest, VSS, `fortis-lsp` and a started client `Node`. Fields drop in
/// order: client first, the temp dir last.
struct Env {
    client: Node,
    lsp: Lsp,
    vss: Vss,
    rt: Regtest,
    _dirs: TempDir,
}

impl Env {
    fn start() -> Env {
        let rt = Regtest::start();
        let vss = Vss::start();
        let lsp = Lsp::start(&rt, &[]);
        let dirs = TempDir::new("channel");
        let client = Node::new(NodeConfig {
            bip39_seed: test_seed(23).to_vec(),
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
            limits: lsp.limits(),
        })
        .unwrap();
        client.start().expect("client start against fortis-lsp");
        Env {
            client,
            lsp,
            vss,
            rt,
            _dirs: dirs,
        }
    }

    /// Waits for the client's next event matching `pred`, skipping others.
    fn wait_event(&self, what: &str, secs: u64, pred: impl Fn(&LnEvent) -> bool) -> LnEvent {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            if let Some(e) = self.client.next_event(left.min(Duration::from_secs(1))) {
                if pred(&e) {
                    return e;
                }
                eprintln!("({what}: skipping {e:?})");
            }
        }
    }

    /// Sends `sat` to a fresh transit address, confirms it once and waits
    /// until the node counts it as spendable.
    fn fund_transit(&self, sat: u64) {
        let before = self.client.balances().onchain_transit_sat;
        let addr = self.client.transit_address().expect("transit address");
        assert!(addr.starts_with("bcrt1"), "a regtest address: {addr}");
        self.rt.fund(&addr, sat);
        self.rt.mine(1);
        poll("transit funds spendable", 60, || {
            (self.client.balances().onchain_transit_sat == before + sat).then_some(())
        });
    }

    /// Txids in bitcoind's mempool.
    fn mempool(&self) -> Vec<String> {
        self.rt
            .json(&["getrawmempool"])
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    }

    /// Waits until bitcoind's mempool holds at least one transaction.
    fn wait_mempool(&self, what: &str) -> Vec<String> {
        poll(what, 60, || {
            let m = self.mempool();
            (!m.is_empty()).then_some(m)
        })
    }

    /// Funds the transit wallet, opens a [`CHANNEL_SAT`] channel to the LSP
    /// and mines it until `Ready`.
    fn open_ready_channel(&self) {
        self.fund_transit(TRANSIT_SAT);
        self.client.open_channel(CHANNEL_SAT).expect("open_channel");
        assert_eq!(self.client.channel_state(), ChannelState::Opening);
        self.wait_mempool("funding transaction broadcast");
        self.rt.mine(6);
        self.wait_event("ChannelReady", 90, |e| matches!(e, LnEvent::ChannelReady));
        poll("channel Ready", 60, || {
            (self.client.channel_state() == ChannelState::Ready).then_some(())
        });
    }

    /// The sum of a transaction's outputs.
    fn tx_outputs_sat(&self, txid: &str) -> u64 {
        let tx = self.rt.json(&["getrawtransaction", txid, "true"]);
        tx["vout"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (o["value"].as_f64().unwrap() * 1e8).round() as u64)
            .sum()
    }

    /// A fresh address of bitcoind's miner wallet, outside the client.
    fn external_address(&self) -> String {
        self.rt.cli(&["-rpcwallet=miner", "getnewaddress"])
    }

    /// Sat received by `address` with at least `confs` confirmations.
    fn received_sat(&self, address: &str, confs: u32) -> u64 {
        let btc = self.rt.cli(&[
            "-rpcwallet=miner",
            "getreceivedbyaddress",
            address,
            &confs.to_string(),
        ]);
        btc_to_sat(&btc)
    }
}

fn btc_to_sat(btc: &str) -> u64 {
    let (whole, frac) = btc.split_once('.').unwrap_or((btc, "0"));
    let frac = format!("{frac:0<8}");
    whole.parse::<u64>().unwrap() * 100_000_000 + frac[..8].parse::<u64>().unwrap()
}

fn have_binaries() -> bool {
    common::env_bin("FORTIS_BTC_BITCOIND").is_some()
        && common::env_bin("FORTIS_VSS_SERVER").is_some()
}

/// A client `Node` that is never started (address checks only).
fn offline_node(tag: &str) -> (Node, TempDir) {
    let dir = TempDir::new(tag);
    let node = Node::new(NodeConfig {
        bip39_seed: test_seed(29).to_vec(),
        network: Network::Regtest,
        data_dir: dir.path("client"),
        chain_source: ChainSource::Esplora {
            url: "http://192.0.2.1".into(),
        },
        lsp: LspInfo {
            node_id: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".into(),
            host: "192.0.2.1".into(),
            port: 9735,
            token: None,
        },
        vss: VssConfig {
            url: "http://192.0.2.1/vss".into(),
            auth_header: String::new(),
        },
        limits: LspLimits {
            min_payment_msat: 51_000 * SAT,
            max_payment_msat: 250_000 * SAT,
            channel_min_sat: 100_000,
            channel_max_sat: 500_000,
        },
    })
    .unwrap();
    (node, dir)
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn fund_transit_then_open_channel_becomes_ready_after_one_conf() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    assert_eq!(env.client.channel_state(), ChannelState::None);

    // One confirmation of the transit deposit makes it spendable for the open.
    env.fund_transit(TRANSIT_SAT);
    assert_eq!(env.client.balances().onchain_transit_sat, TRANSIT_SAT);

    let t0 = Instant::now();
    env.client.open_channel(CHANNEL_SAT).expect("open_channel");
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "open_channel does not wait for confirmations"
    );
    assert_eq!(env.client.channel_state(), ChannelState::Opening);
    env.wait_mempool("funding transaction broadcast");
    assert_eq!(env.client.channel_state(), ChannelState::Opening);
    // Short of the LSP's 6 confirmations: still opening.
    env.rt.mine(5);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(env.client.channel_state(), ChannelState::Opening);

    env.rt.mine(1);
    env.wait_event("ChannelReady", 90, |e| matches!(e, LnEvent::ChannelReady));
    poll("channel Ready", 60, || {
        (env.client.channel_state() == ChannelState::Ready).then_some(())
    });

    let b = env.client.balances();
    eprintln!("balances after open: {b:?}");
    assert_eq!(
        b.lightning_sat,
        CHANNEL_SAT - FUNDER_COMMITMENT_COST_SAT,
        "{b:?}"
    );
    assert_eq!(b.pending_close_sat, 0, "{b:?}");
    // Transit keeps the rest less the funding fee, minus the anchor reserve
    // ldk-node holds back while the channel exists.
    let rest = TRANSIT_SAT - CHANNEL_SAT - ANCHOR_RESERVE_SAT;
    assert!(
        b.onchain_transit_sat < rest && b.onchain_transit_sat > rest - MAX_TX_FEE_SAT,
        "{b:?}"
    );
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn open_below_channel_min_errors() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    let limits = env.client.limits();
    assert_eq!(
        (limits.channel_min_sat, limits.channel_max_sat),
        (100_000, 500_000)
    );
    let range = LnError::AmountOutOfRange {
        min_msat: limits.channel_min_sat * SAT,
        max_msat: limits.channel_max_sat * SAT,
    };
    assert_eq!(
        env.client.open_channel(limits.channel_min_sat - 1),
        Err(range.clone())
    );
    assert_eq!(
        env.client.open_channel(limits.channel_max_sat + 1),
        Err(range.clone())
    );
    assert_eq!(env.client.open_channel(0), Err(range));

    // In range but the transit wallet is empty.
    assert_eq!(
        env.client.open_channel(CHANNEL_SAT),
        Err(LnError::Other("insufficient_transit".into()))
    );
    // Enough for the channel but not for the channel plus the anchor reserve.
    env.fund_transit(CHANNEL_SAT + ANCHOR_RESERVE_SAT / 2);
    assert_eq!(
        env.client.open_channel(CHANNEL_SAT),
        Err(LnError::Other("insufficient_transit".into()))
    );
    assert_eq!(env.client.channel_state(), ChannelState::None);
    assert!(env.mempool().is_empty(), "nothing was broadcast");
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn cooperative_close_returns_funds_to_transit_then_sweeps_to_address() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    env.open_ready_channel();
    let before = env.client.balances();
    eprintln!("balances before close: {before:?}");

    env.client.close_channel().expect("close_channel");
    let state = env.client.channel_state();
    assert!(
        matches!(state, ChannelState::Closing | ChannelState::None),
        "closing right after close_channel: {state:?}"
    );
    env.wait_event("ChannelClosed", 60, |e| {
        matches!(e, LnEvent::ChannelClosed { .. })
    });
    let closing_tx = env.wait_mempool("closing transaction broadcast");
    // Broadcast, not confirmed: the channel balance is pending, not spendable.
    assert_eq!(env.client.channel_state(), ChannelState::Closing);
    let b = env.client.balances();
    assert_eq!(b.lightning_sat, 0, "{b:?}");
    assert_eq!(b.pending_close_sat, before.lightning_sat, "{b:?}");

    assert_eq!(
        closing_tx.len(),
        1,
        "one closing transaction: {closing_tx:?}"
    );
    // The LSP has no balance in the channel: the closing tx's only output is
    // ours, the channel value less the closing fee (we pay it as funder).
    let closing_out_sat = env.tx_outputs_sat(&closing_tx[0]);
    eprintln!("closing tx pays us {closing_out_sat} sat");
    assert!(
        closing_out_sat < CHANNEL_SAT && closing_out_sat > CHANNEL_SAT - 2 * MAX_TX_FEE_SAT,
        "{closing_out_sat}"
    );

    env.rt.mine(1);
    assert!(env.mempool().is_empty(), "closing tx {closing_tx:?} mined");
    // The close pays the transit wallet directly; once there is no channel,
    // the anchor reserve is spendable again.
    let expected = before.onchain_transit_sat + ANCHOR_RESERVE_SAT + closing_out_sat;
    let back = poll("closed funds spendable in transit", 60, || {
        let b = env.client.balances();
        (b.pending_close_sat == 0 && b.onchain_transit_sat == expected).then_some(b)
    });
    eprintln!("balances after close: {back:?}");
    assert_eq!(back.lightning_sat, 0);
    assert_eq!(env.client.channel_state(), ChannelState::None);

    let to = env.external_address();
    let txid = env
        .client
        .sweep_transit(&to)
        .expect("sweep")
        .expect("something to sweep");
    poll("sweep in mempool", 30, || {
        env.mempool().contains(&txid).then_some(())
    });
    env.rt.mine(1);
    let got = env.received_sat(&to, 1);
    assert!(
        got <= back.onchain_transit_sat && got > back.onchain_transit_sat - MAX_TX_FEE_SAT,
        "address got {got} of {back:?}"
    );
    poll("transit empty", 30, || {
        (env.client.balances().onchain_transit_sat == 0).then_some(())
    });

    env.rt.mine(6);
    assert_eq!(env.client.channel_state(), ChannelState::None);
    assert_eq!(env.client.balances().pending_close_sat, 0);
    assert_eq!(env.client.sweep_transit(&to), Ok(None));
}

/// Review Focus 5. With the client stopped the LSP can only force-close
/// (a cooperative close needs both peers online; LDK refuses `shutdown`
/// with a disconnected peer). `fortis-lsp` has no close control, so the
/// test stops it and drives its own data dir (same seed file, same SQLite
/// store) with a stock ldk-node that force-closes the channel, then starts
/// `fortis-lsp` again so the client can start.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn lsp_initiated_close_while_stopped_shows_transit_balance_on_next_start() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.open_ready_channel();
    let before = env.client.balances();
    eprintln!("balances before close: {before:?}");
    let client_id = PublicKey::from_str(&env.client.node_id()).unwrap();

    env.client.stop();
    assert_eq!(
        env.client.channel_state(),
        ChannelState::None,
        "not running"
    );

    let status = env.lsp.stop();
    assert!(
        status.is_some_and(|s| s.success()),
        "fortis-lsp stopped: {status:?}"
    );
    lsp_force_close(
        &env.rt,
        &env.lsp.dir().join("data"),
        &env.lsp.node_id,
        client_id,
    );
    env.rt.mine(6);
    assert_eq!(env.lsp.start_again(), env.lsp.node_id, "same LSP node id");

    env.client.start().expect("client starts again");
    // The commitment transaction is on chain: the channel is gone and its
    // balance is on its way to the transit wallet.
    poll("closing seen", 60, || {
        (env.client.channel_state() == ChannelState::Closing).then_some(())
    });
    let b = env.client.balances();
    eprintln!("balances on restart: {b:?}");
    assert_eq!(b.lightning_sat, 0, "{b:?}");
    assert!(b.pending_close_sat > 0, "{b:?}");
    assert!(b.pending_close_sat <= before.lightning_sat, "{b:?}");

    // ldk-node sweeps the to_remote output to its wallet; mine until it lands.
    let deadline = Instant::now() + Duration::from_secs(180);
    let landed = loop {
        let b = env.client.balances();
        if b.pending_close_sat == 0 && b.onchain_transit_sat > before.onchain_transit_sat {
            break b;
        }
        assert!(
            Instant::now() < deadline,
            "funds never reached transit: {b:?}"
        );
        env.rt.mine(1);
        std::thread::sleep(Duration::from_secs(2));
    };
    eprintln!("balances once swept to transit: {landed:?}");
    assert_eq!(env.client.channel_state(), ChannelState::None);
    let expected = before.onchain_transit_sat + ANCHOR_RESERVE_SAT + before.lightning_sat;
    assert!(
        landed.onchain_transit_sat <= expected
            && landed.onchain_transit_sat > expected - MAX_TX_FEE_SAT,
        "{landed:?}"
    );

    let to = env.external_address();
    let txid = env
        .client
        .sweep_transit(&to)
        .expect("sweep")
        .expect("something to sweep");
    poll("sweep in mempool", 30, || {
        env.mempool().contains(&txid).then_some(())
    });
    env.rt.mine(1);
    assert!(env.received_sat(&to, 1) > landed.onchain_transit_sat - MAX_TX_FEE_SAT);
}

/// Stops nothing: `fortis-lsp` must already be stopped. Builds a stock
/// ldk-node over its data dir (its seed file and SQLite store), force-closes
/// the channel with `client`, waits until the commitment is in bitcoind's
/// mempool, and stops the node.
fn lsp_force_close(rt: &Regtest, data: &std::path::Path, lsp_id: &str, client: PublicKey) {
    let mut b = Builder::new();
    rt.chain_source(&mut b);
    b.set_storage_dir_path(data.to_str().unwrap().to_string());
    b.set_entropy_seed_path(data.join("keys_seed").to_str().unwrap().to_string());
    let node = b
        .build()
        .expect("stock ldk-node over fortis-lsp's data dir");
    node.start().unwrap();
    assert_eq!(
        node.node_id().to_string(),
        lsp_id,
        "same node as fortis-lsp"
    );
    let ch = node
        .list_channels()
        .into_iter()
        .find(|c| c.counterparty_node_id == client)
        .expect("the LSP has the client's channel");
    node.force_close_channel(&ch.user_channel_id, client, Some("test".into()))
        .expect("LSP force-closes");
    poll("commitment broadcast", 60, || {
        let m = rt.json(&["getrawmempool"]);
        (!m.as_array().unwrap().is_empty()).then_some(())
    });
    node.stop().unwrap();
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn sweep_with_empty_transit_returns_none() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    assert_eq!(env.client.balances().onchain_transit_sat, 0);
    let to = env.external_address();
    assert_eq!(env.client.sweep_transit(&to), Ok(None));
    assert!(env.mempool().is_empty());
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn sweep_blocked_when_vss_down() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    env.fund_transit(100_000);
    let to = env.external_address();

    env.vss.pause();
    let res = env.client.sweep_transit(&to);
    env.vss.resume();
    assert_eq!(res, Err(LnError::BackupUnavailable));
    std::thread::sleep(Duration::from_secs(2));
    assert!(env.mempool().is_empty(), "nothing broadcast");
    assert_eq!(env.client.balances().onchain_transit_sat, 100_000);
    assert_eq!(env.received_sat(&to, 0), 0);

    // Back up: the sweep goes through.
    let txid = env.client.sweep_transit(&to).unwrap().expect("swept");
    poll("sweep in mempool", 30, || {
        env.mempool().contains(&txid).then_some(())
    });
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn close_without_channel_errors() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    assert_eq!(
        env.client.close_channel(),
        Err(LnError::Other("no_channel".into()))
    );
    assert_eq!(
        env.client.force_close_channel(),
        Err(LnError::Other("no_channel".into()))
    );
}

/// The explicit last resort: our own force close. The funds come back to
/// transit after the to_self delay (the LSP's `to_self_delay` for our
/// output), so this test only checks the immediate effect.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn force_close_channel_broadcasts_and_shows_closing() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    env.open_ready_channel();
    let before = env.client.balances();
    env.client.force_close_channel().expect("force close");
    env.wait_mempool("commitment broadcast");
    env.rt.mine(1);
    poll("closing", 30, || {
        (env.client.channel_state() == ChannelState::Closing).then_some(())
    });
    let b = env.client.balances();
    assert_eq!(b.lightning_sat, 0, "{b:?}");
    assert!(
        b.pending_close_sat > 0 && b.pending_close_sat <= before.lightning_sat,
        "{b:?}"
    );
}

#[test]
fn sweep_rejects_wrong_network_address() {
    let (node, _dir) = offline_node("sweep-addr");
    // BIP-173 vectors: mainnet and testnet P2WPKH.
    assert_eq!(
        node.sweep_transit("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"),
        Err(LnError::WrongNetwork)
    );
    assert_eq!(
        node.sweep_transit("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"),
        Err(LnError::WrongNetwork)
    );
    assert_eq!(
        node.sweep_transit("not an address"),
        Err(LnError::Other("bad_address".into()))
    );
    assert_eq!(
        node.sweep_transit(""),
        Err(LnError::Other("bad_address".into()))
    );
    // A valid regtest address gets past the check: the node is not started.
    assert_eq!(
        node.sweep_transit("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"),
        Err(LnError::Other("not_started".into()))
    );
}

#[test]
fn channel_calls_before_start() {
    let (node, _dir) = offline_node("channel-offline");
    assert_eq!(node.channel_state(), ChannelState::None);
    assert_eq!(
        node.transit_address(),
        Err(LnError::Other("not_started".into()))
    );
    assert_eq!(
        node.open_channel(99_999),
        Err(LnError::AmountOutOfRange {
            min_msat: 100_000 * SAT,
            max_msat: 500_000 * SAT
        })
    );
    assert_eq!(
        node.open_channel(150_000),
        Err(LnError::Other("not_started".into()))
    );
    assert_eq!(
        node.close_channel(),
        Err(LnError::Other("not_started".into()))
    );
    assert_eq!(node.balances().pending_close_sat, 0);
}
