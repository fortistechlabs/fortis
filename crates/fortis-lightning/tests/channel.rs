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
    Balances, ChainSource, ChannelState, LnError, LnEvent, LspInfo, LspLimits, Node, NodeConfig,
    VssConfig,
};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::Builder;

const SAT: u64 = 1_000;
const CHANNEL_SAT: u64 = 150_000;
const TRANSIT_SAT: u64 = 300_000;
/// No anchor reserve: the LSP is a trusted peer (`trusted_peers_no_reserve`).
const ANCHOR_RESERVE_SAT: u64 = 0;
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
            chain: fortis_lightning::LnChain::Btc,
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
        chain: fortis_lightning::LnChain::Btc,
        bip39_seed: test_seed(29).to_vec(),
        network: Network::Regtest,
        data_dir: dir.path("client"),
        chain_source: ChainSource::Esplora {
            url: "http://192.0.2.1".into(),
            auth_header: None,
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
    assert_eq!(b.reserved_sat, ANCHOR_RESERVE_SAT, "{b:?}");
    assert_eq!(b.pending_transit_sat, 0, "{b:?}");
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
    // The LSP's JIT minimum does not apply to channels the user funds.
    assert_eq!(
        (limits.channel_min_sat, limits.channel_max_sat),
        (100_000, 500_000)
    );
    let range = LnError::AmountOutOfRange {
        min_msat: fortis_lightning::USER_CHANNEL_MIN_SAT * SAT,
        max_msat: limits.channel_max_sat * SAT,
    };
    assert_eq!(
        env.client.open_channel(fortis_lightning::USER_CHANNEL_MIN_SAT - 1),
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
    assert_eq!(env.client.channel_state(), ChannelState::None);
    assert!(env.mempool().is_empty(), "nothing was broadcast");
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn a_small_channel_opens_without_an_anchor_reserve() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    const SMALL_SAT: u64 = 20_000;
    env.fund_transit(SMALL_SAT + 2_000);
    env.client.open_channel(SMALL_SAT).expect("open a 20,000 sat channel");
    env.wait_mempool("funding transaction broadcast");
    env.rt.mine(6);
    env.wait_event("ChannelReady", 90, |e| matches!(e, LnEvent::ChannelReady));
    let b = env.client.balances();
    assert_eq!(b.lightning_sat, SMALL_SAT - FUNDER_COMMITMENT_COST_SAT, "{b:?}");
    assert_eq!(b.reserved_sat, 0, "{b:?}");
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
/// The explicit last resort: our own force close. Our output waits out the
/// LSP's `to_self_delay` (LDK default 144 blocks); until ldk-node's sweep
/// of it confirms, `sweep_transit` refuses with `Other("closing")` (the
/// anchor reserve may be needed to fee-bump) and sends nothing.
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
    let in_mempool = env.wait_mempool("commitment broadcast");
    let to = env.external_address();
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("closing".into())),
        "commitment unconfirmed"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        env.mempool().len(),
        in_mempool.len(),
        "nothing new broadcast"
    );

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
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("closing".into())),
        "to_self output under its CSV"
    );
    assert!(env.mempool().is_empty(), "nothing broadcast");

    // Past the CSV delay ldk-node sweeps our output into transit.
    env.rt.mine(150);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let b = env.client.balances();
        if b.pending_close_sat == 0 && env.client.channel_state() == ChannelState::None {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "force close never resolved: {b:?}"
        );
        env.rt.mine(1);
        std::thread::sleep(Duration::from_secs(2));
    }
    let txid = env
        .client
        .sweep_transit(&to)
        .expect("sweep after resolution")
        .expect("something to sweep");
    poll("sweep in mempool", 30, || {
        env.mempool().contains(&txid).then_some(())
    });
}

/// `open_channel` returns before ldk-node builds the funding transaction
/// (and the funding transaction is not in the transit wallet until it is
/// seen in the mempool): a sweep right after must not take those funds.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn sweep_right_after_open_is_refused_and_open_completes() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    env.fund_transit(TRANSIT_SAT);
    env.client.open_channel(CHANNEL_SAT).expect("open_channel");
    let to = env.external_address();
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("channel_opening".into()))
    );
    // A second open is refused too until the first funding confirmed.
    assert_eq!(
        env.client.open_channel(CHANNEL_SAT),
        Err(LnError::Other("channel_opening".into()))
    );
    let m = env.wait_mempool("funding transaction broadcast");
    assert_eq!(m.len(), 1, "only the funding transaction: {m:?}");
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("channel_opening".into())),
        "funding unconfirmed"
    );
    env.rt.mine(6);
    env.wait_event("ChannelReady", 90, |e| matches!(e, LnEvent::ChannelReady));
    poll("channel Ready", 60, || {
        (env.client.channel_state() == ChannelState::Ready).then_some(())
    });
    assert_eq!(env.received_sat(&to, 0), 0, "the sweep sent nothing");
}

/// Funding confirmed but the LSP's `channel_ready` never arrived (LSP down
/// at the 6th confirmation): LDK reports the channel not ready, exactly as
/// for one that is shutting down. Without evidence of a shutdown it is
/// `Opening`, and `close_channel` acts (here: `LspUnreachable`, never a
/// silent `Ok`). After a restart with the LSP back it becomes `Ready`, and
/// `close_channel` then really closes it.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn funding_confirmed_without_channel_ready_is_opening_and_close_is_real() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.fund_transit(TRANSIT_SAT);
    env.client.open_channel(CHANNEL_SAT).expect("open_channel");
    env.wait_mempool("funding transaction broadcast");
    env.rt.mine(5);
    std::thread::sleep(Duration::from_secs(3));
    let status = env.lsp.stop();
    assert!(
        status.is_some_and(|s| s.success()),
        "fortis-lsp stopped: {status:?}"
    );
    env.rt.mine(3);
    // Let the client see the blocks (it polls bitcoind every few seconds).
    std::thread::sleep(Duration::from_secs(8));
    assert_eq!(env.client.channel_state(), ChannelState::Opening);
    assert_eq!(env.client.close_channel(), Err(LnError::LspUnreachable));
    assert_eq!(env.client.channel_state(), ChannelState::Opening);
    assert!(env.mempool().is_empty(), "nothing broadcast");

    env.client.stop();
    assert_eq!(env.lsp.start_again(), env.lsp.node_id);
    env.client.start().expect("client starts again");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let s = env.client.channel_state();
        assert_ne!(s, ChannelState::Closing, "never Closing without a close");
        if s == ChannelState::Ready {
            break;
        }
        assert!(Instant::now() < deadline, "channel never Ready: {s:?}");
        std::thread::sleep(Duration::from_millis(200));
    }

    env.client.close_channel().expect("close_channel");
    assert_eq!(env.client.channel_state(), ChannelState::Closing);
    env.wait_event("ChannelClosed", 60, |e| {
        matches!(e, LnEvent::ChannelClosed { .. })
    });
    env.wait_mempool("closing transaction broadcast");
    assert_eq!(env.client.channel_state(), ChannelState::Closing);
}

/// The five parts of `Balances`.
fn sum(b: &Balances) -> u64 {
    b.lightning_sat
        + b.onchain_transit_sat
        + b.pending_close_sat
        + b.reserved_sat
        + b.pending_transit_sat
}

impl Env {
    /// A mempool transaction's fee (sat).
    fn mempool_fee_sat(&self, txid: &str) -> u64 {
        let e = self.rt.json(&["getmempoolentry", txid]);
        (e["fees"]["base"].as_f64().unwrap() * 1e8).round() as u64
    }
}

impl Env {
    fn wallet_total(&self) -> u64 {
        fortis_lightning::test_hooks::wallet_total_sat(&self.client)
    }

    /// Waits until the client's wallet total is `sat` (it has seen the
    /// mempool or block it is told about).
    fn wait_wallet_total(&self, what: &str, sat: u64) {
        poll(what, 60, || (self.wallet_total() == sat).then_some(()));
    }

    /// One reading, asserted exactly: `sum(balances) == expected`.
    fn assert_sum(&self, stage: &str, expected: u64) -> Balances {
        let b = self.client.balances();
        eprintln!("[{stage}] sum {} expected {expected}: {b:?}", sum(&b));
        assert_eq!(sum(&b), expected, "[{stage}] {b:?}");
        b
    }

    /// Mines `skip` blocks, then one at a time until a transaction shows up
    /// in bitcoind's mempool (ldk-node broadcasting a sweep); its txid.
    fn mine_until_mempool(&self, what: &str, skip: u32, max_blocks: u32) -> String {
        assert!(self.mempool().is_empty(), "{what}: mempool not empty");
        if skip > 0 {
            self.rt.mine(skip);
        }
        for _ in 0..max_blocks {
            std::thread::sleep(Duration::from_millis(1_500));
            if let Some(t) = self.mempool().first() {
                return t.clone();
            }
            self.rt.mine(1);
        }
        panic!("{what}: nothing broadcast after {max_blocks} blocks");
    }

    /// Opens a [`CHANNEL_SAT`] channel and waits until it is ready; returns
    /// the funding fee.
    fn open_and_ready(&self, what: &str, transit_before: u64) -> u64 {
        self.client.open_channel(CHANNEL_SAT).unwrap();
        let funding = self.wait_mempool(what);
        assert_eq!(funding.len(), 1, "{what}: {funding:?}");
        let fee = self.mempool_fee_sat(&funding[0]);
        // The wallet has seen the funding transaction.
        self.wait_wallet_total(what, transit_before - CHANNEL_SAT - fee);
        fee
    }

    /// While ldk-node's sweep `txid` of a closed channel's `amount_sat`
    /// output is in the mempool: waits until the wallet counts it, then
    /// checks the reading against `held` (the total before the sweep).
    ///
    /// Production (Esplora): the wallet sync that picks the sweep up also
    /// enters it in ldk-node's payment store, so `pending_close_sat` drops
    /// it in the same step and the sum is `held − fee`. On this bitcoind
    /// chain source, ldk-node applies mempool transactions to the wallet
    /// without a payment-store entry (that comes with the next block), so
    /// for this window the output is counted twice: `held − fee + amount`.
    /// Test-only, bounded by the swept amount, gone at the next block.
    fn check_sweep_window(&self, stage: &str, txid: &str, held: u64, wallet_before: u64) -> u64 {
        let fee = self.mempool_fee_sat(txid);
        let out = self.tx_outputs_sat(txid);
        self.wait_wallet_total(stage, wallet_before + out);
        let amount = out + fee;
        self.assert_sum(stage, held - fee + amount);
        fee
    }
}

/// `lightning + transit + pending_close + reserved + pending_transit`
/// against a ledger kept from bitcoind: deposits less the fees of the
/// client's own transactions (`getmempoolentry`) less, while a channel's
/// balance is held in a commitment, the funder's commitment fee and anchors
/// ([`FUNDER_COMMITMENT_COST_SAT`], invisible by design). Every stage is one
/// reading taken after the client's wallet has seen the transaction in
/// question, and is asserted exactly; the only deviation is the documented
/// bitcoind-source double count of an unconfirmed sweep
/// (`Env::check_sweep_window`). Stages: deposit unconfirmed / confirmed,
/// opening, ready, cooperative close in the mempool (monitor balance and
/// the wallet's unconfirmed output both present: the dedup) and confirmed;
/// the LSP's force close (to_remote) awaiting confirmations, swept by
/// ldk-node (in the mempool, confirmed); our own force close (commitment in
/// the mempool, confirmed and under its CSV delay, swept by ldk-node); and
/// the final sweep to an external address.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn balances_reconcile_through_open_close_and_force_close() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();

    // Deposit, unconfirmed then confirmed.
    let addr = env.client.transit_address().unwrap();
    env.rt.fund(&addr, TRANSIT_SAT);
    env.wait_wallet_total("deposit seen", TRANSIT_SAT);
    let b = env.assert_sum("deposit unconfirmed", TRANSIT_SAT);
    assert_eq!(b.pending_transit_sat, TRANSIT_SAT, "{b:?}");
    env.rt.mine(1);
    poll("deposit spendable", 60, || {
        (env.client.balances().onchain_transit_sat == TRANSIT_SAT).then_some(())
    });
    env.assert_sum("deposit confirmed", TRANSIT_SAT);

    // Channel 1: opening, ready, cooperative close.
    let mut ledger = TRANSIT_SAT - env.open_and_ready("funding 1", TRANSIT_SAT);
    assert_eq!(env.client.channel_state(), ChannelState::Opening);
    let held = ledger - FUNDER_COMMITMENT_COST_SAT;
    let b = env.assert_sum("opening", held);
    assert_eq!(b.reserved_sat, ANCHOR_RESERVE_SAT, "{b:?}");
    env.rt.mine(6);
    env.wait_event("ChannelReady 1", 90, |e| matches!(e, LnEvent::ChannelReady));
    poll("Ready 1", 60, || {
        (env.client.channel_state() == ChannelState::Ready).then_some(())
    });
    env.assert_sum("ready", held);

    let wallet_before = env.wallet_total();
    env.client.close_channel().unwrap();
    env.wait_event("ChannelClosed 1", 60, |e| {
        matches!(e, LnEvent::ChannelClosed { .. })
    });
    let closing = env.wait_mempool("closing broadcast");
    let closing_out = env.tx_outputs_sat(&closing[0]);
    // Both present now: the monitor's ClaimableOnChannelClose and the
    // wallet's unconfirmed closing output. Counted once, at the monitor's
    // amount: the sum is still `held`.
    env.wait_wallet_total("closing output seen", wallet_before + closing_out);
    let b = env.assert_sum("coop close in mempool", held);
    assert_eq!(
        (b.lightning_sat, b.pending_close_sat, b.pending_transit_sat),
        (0, CHANNEL_SAT - FUNDER_COMMITMENT_COST_SAT, 0),
        "{b:?}"
    );
    env.rt.mine(1);
    poll("closing output spendable", 60, || {
        (env.client.balances().pending_close_sat == 0).then_some(())
    });
    ledger = wallet_before + closing_out;
    let b = env.assert_sum("coop close confirmed", ledger);
    assert_eq!(b.reserved_sat, 0, "{b:?}");

    // Channel 2: closed by the LSP (force) while the client is stopped.
    ledger -= env.open_and_ready("funding 2", ledger);
    let held = ledger - FUNDER_COMMITMENT_COST_SAT;
    env.rt.mine(6);
    env.wait_event("ChannelReady 2", 90, |e| matches!(e, LnEvent::ChannelReady));
    env.assert_sum("ready 2", held);
    let wallet_before = env.wallet_total();
    let client_id = PublicKey::from_str(&env.client.node_id()).unwrap();
    env.client.stop();
    assert!(env.lsp.stop().is_some_and(|s| s.success()));
    lsp_force_close(
        &env.rt,
        &env.lsp.dir().join("data"),
        &env.lsp.node_id,
        client_id,
    );
    env.rt.mine(1);
    env.lsp.start_again();
    env.client.start().unwrap();
    poll("closing seen", 60, || {
        (env.client.channel_state() == ChannelState::Closing).then_some(())
    });
    env.wait_wallet_total("reserve released", wallet_before);
    let b = env.assert_sum("LSP force close awaiting confirmations", held);
    assert!(b.pending_close_sat > 0 && b.lightning_sat == 0, "{b:?}");
    let sweep = env.mine_until_mempool("to_remote sweep", 0, 20);
    let fee = env.check_sweep_window("to_remote sweep in mempool", &sweep, held, wallet_before);
    env.rt.mine(1);
    poll("to_remote sweep confirmed", 60, || {
        (env.client.balances().pending_close_sat == 0).then_some(())
    });
    ledger = held - fee;
    env.assert_sum("to_remote swept", ledger);
    // `Closing` lasts until ldk-node's sweeper has seen the sweep confirm
    // (`closing_unresolved`: the reserve is kept while it might be needed),
    // which can be a sync after the wallet counted the funds.
    poll("sweeper saw the sweep confirm", 60, || {
        (env.client.channel_state() == ChannelState::None).then_some(())
    });

    // Channel 3: our own force close; our output waits out the CSV delay.
    ledger -= env.open_and_ready("funding 3", ledger);
    let held = ledger - FUNDER_COMMITMENT_COST_SAT;
    env.rt.mine(6);
    env.wait_event("ChannelReady 3", 90, |e| matches!(e, LnEvent::ChannelReady));
    env.assert_sum("ready 3", held);
    let wallet_before = env.wallet_total();
    env.client.force_close_channel().unwrap();
    let commitment = env.wait_mempool("commitment broadcast");
    // At regtest fees the commitment (253 sat/kWU) needs no CPFP: no fee
    // from the wallet besides what `held` already excludes.
    assert_eq!(commitment.len(), 1, "commitment only: {commitment:?}");
    env.assert_sum("our force close, commitment in mempool", held);
    env.rt.mine(1);
    poll("commitment confirmed", 60, || {
        let b = env.client.balances();
        (b.pending_close_sat > 0 && env.mempool().is_empty()).then_some(())
    });
    env.assert_sum("our force close, CSV wait", held);
    let to = env.external_address();
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("closing".into()))
    );
    let sweep = env.mine_until_mempool("to_local sweep", 140, 30);
    let fee = env.check_sweep_window("to_local sweep in mempool", &sweep, held, wallet_before);
    env.rt.mine(1);
    poll("to_local sweep confirmed", 60, || {
        (env.client.balances().pending_close_sat == 0
            && env.client.channel_state() == ChannelState::None)
            .then_some(())
    });
    ledger = held - fee;
    env.assert_sum("to_local swept", ledger);

    // Everything to an external address.
    let txid = env.client.sweep_transit(&to).unwrap().expect("swept");
    poll("sweep in mempool", 30, || {
        env.mempool().contains(&txid).then_some(())
    });
    let fee = env.mempool_fee_sat(&txid);
    env.rt.mine(1);
    assert_eq!(env.received_sat(&to, 1), ledger - fee);
    poll("transit empty", 60, || {
        (sum(&env.client.balances()) == 0).then_some(())
    });
}

/// A funding transaction that never confirms (here: dropped from bitcoind's
/// mempool) must not strand the transit wallet. While it is pending and no
/// close was asked for, the channel is `Opening` and sweeps refuse; after
/// the user's force close the channel's funding-derived balance is void,
/// and the sweep returns the deposit.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn unconfirmed_funding_dropped_then_force_close_frees_transit() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.fund_transit(TRANSIT_SAT);
    env.client.open_channel(CHANNEL_SAT).unwrap();
    let funding = env.wait_mempool("funding broadcast");
    assert_eq!(funding.len(), 1);
    let to = env.external_address();
    assert_eq!(env.client.channel_state(), ChannelState::Opening);
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("channel_opening".into())),
        "funding in the mempool, no close asked for"
    );

    env.rt.restart_dropping_mempool();
    // The wallet notices the funding transaction is gone: its inputs are
    // spendable again (less the reserve, the channel is still listed).
    poll("funding inputs back", 90, || {
        let b = env.client.balances();
        (b.onchain_transit_sat + b.reserved_sat == TRANSIT_SAT && b.pending_transit_sat == 0)
            .then_some(())
    });
    env.rt.mine(3);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        env.client.channel_state(),
        ChannelState::Opening,
        "no close asked for"
    );
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("channel_opening".into()))
    );

    env.client.force_close_channel().expect("force close");
    poll("nothing left to resolve", 30, || {
        (env.client.channel_state() == ChannelState::None).then_some(())
    });
    let b = env.client.balances();
    assert_eq!(
        (b.lightning_sat, b.pending_close_sat, b.onchain_transit_sat),
        (0, 0, TRANSIT_SAT),
        "{b:?}"
    );
    let txid = env
        .client
        .sweep_transit(&to)
        .expect("sweep")
        .expect("something to sweep");
    poll("sweep in mempool", 30, || {
        env.mempool().contains(&txid).then_some(())
    });
    let fee = env.mempool_fee_sat(&txid);
    env.rt.mine(1);
    assert_eq!(env.received_sat(&to, 1), TRANSIT_SAT - fee);
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
        node.open_channel(fortis_lightning::USER_CHANNEL_MIN_SAT - 1),
        Err(LnError::AmountOutOfRange {
            min_msat: fortis_lightning::USER_CHANNEL_MIN_SAT * SAT,
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

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn splice_in_grows_the_channel_and_splice_out_pays_an_address() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    env.open_ready_channel();
    let before = env.client.balances().lightning_sat;

    // A second transfer into Lightning: splice into the open channel.
    const ADD_SAT: u64 = 80_000;
    env.fund_transit(ADD_SAT + 2_000);
    env.client.splice_in(ADD_SAT).expect("splice_in");
    env.wait_mempool("splice-in transaction broadcast");
    env.rt.mine(6);
    let grown = poll("channel grown by the splice-in", 120, || {
        let b = env.client.balances();
        (b.lightning_sat >= before + ADD_SAT - MAX_TX_FEE_SAT).then_some(b)
    });
    eprintln!("after splice-in: {grown:?}");
    assert!(grown.lightning_sat <= before + ADD_SAT, "{grown:?}");
    assert_eq!(env.client.channel_state(), ChannelState::Ready);

    // Part of it back on-chain: splice out to an outside address.
    const OUT_SAT: u64 = 50_000;
    let to = env.external_address();
    // The splice-in must be locked on both sides first (splice_locked).
    let mut tries = 0;
    poll("splice_out accepted", 120, || {
        tries += 1;
        match env.client.splice_out(OUT_SAT, &to) {
            Ok(()) => Some(()),
            Err(e) => {
                eprintln!("splice_out try {tries}: {e:?}");
                env.rt.mine(1);
                None
            }
        }
    });
    env.wait_mempool("splice-out transaction broadcast");
    env.rt.mine(6);
    poll("splice-out paid", 120, || (env.received_sat(&to, 1) == OUT_SAT).then_some(()));
    let after = poll("channel shrunk", 120, || {
        let b = env.client.balances();
        (b.lightning_sat < grown.lightning_sat - OUT_SAT + MAX_TX_FEE_SAT).then_some(b)
    });
    eprintln!("after splice-out: {after:?}");
    assert!(after.lightning_sat >= grown.lightning_sat - OUT_SAT - MAX_TX_FEE_SAT, "{after:?}");
    assert_eq!(env.client.channel_state(), ChannelState::Ready);

    // Too much out is refused before anything is sent.
    assert!(matches!(
        env.client.splice_out(10_000_000, &to),
        Err(fortis_lightning::LnError::Other(r)) if r == "insufficient_channel"
    ));
}
