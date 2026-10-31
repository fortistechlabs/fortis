//! `Node::start_with(StartMode::Exit)`: the recovery start that needs no
//! LSP, so a user can always force-close and sweep (spec: "users can
//! force-close a channel unilaterally at any time"). Real-stack tests are
//! `#[ignore]`d; run them with:
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test exit -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Amounts as in `tests/channel.rs` (regtest fees fall back to 1 sat/vB).

mod common;

use std::str::FromStr;
use std::time::{Duration, Instant};

use common::{poll, test_seed, Lsp, Payer, Regtest, TempDir, Vss};
use fortis_lightning::{
    Balances, ChainSource, ChannelState, LnError, LnEvent, LspInfo, LspLimits, Node, NodeConfig,
    PaymentTarget, StartMode, VssConfig,
};
use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use ldk_node::bitcoin::Network;
use ldk_node::lightning::types::payment::PaymentSecret;
use ldk_node::lightning_invoice::{
    Bolt11InvoiceDescription, Currency, Description, InvoiceBuilder,
};

const SAT: u64 = 1_000;
const CHANNEL_SAT: u64 = 150_000;
const TRANSIT_SAT: u64 = 300_000;
/// See `tests/channel.rs`: the funder's commitment fee and two anchors.
const FUNDER_COMMITMENT_COST_SAT: u64 = 253 * 1124 / 1000 + 2 * 330;
/// The brief's start bound.
const START_BOUND: Duration = Duration::from_secs(15);
/// "Promptly": every refused call returns within this.
const PROMPT: Duration = Duration::from_secs(3);

/// Regtest, VSS, `fortis-lsp` and a client `Node` (started Normal). Fields
/// drop in order: client, LSP, VSS, chain, dir.
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
        let dirs = TempDir::new("exit");
        let client = Node::new(client_config(
            &rt,
            lsp.lsp_info(),
            lsp.limits(),
            &vss,
            &dirs,
        ))
        .unwrap();
        client.start().expect("client start against fortis-lsp");
        assert_eq!(client.start_mode(), Some(StartMode::Normal));
        Env {
            client,
            lsp,
            vss,
            rt,
            _dirs: dirs,
        }
    }

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

    fn mempool(&self) -> Vec<String> {
        self.rt
            .json(&["getrawmempool"])
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    }

    fn wait_mempool(&self, what: &str) -> Vec<String> {
        poll(what, 60, || {
            let m = self.mempool();
            (!m.is_empty()).then_some(m)
        })
    }

    fn mempool_fee_sat(&self, txid: &str) -> u64 {
        let e = self.rt.json(&["getmempoolentry", txid]);
        (e["fees"]["base"].as_f64().unwrap() * 1e8).round() as u64
    }

    fn external_address(&self) -> String {
        self.rt.cli(&["-rpcwallet=miner", "getnewaddress"])
    }

    fn received_sat(&self, address: &str, confs: u32) -> u64 {
        let btc = self.rt.cli(&[
            "-rpcwallet=miner",
            "getreceivedbyaddress",
            address,
            &confs.to_string(),
        ]);
        let (whole, frac) = btc.split_once('.').unwrap_or((&btc, "0"));
        let frac = format!("{frac:0<8}");
        whole.parse::<u64>().unwrap() * 100_000_000 + frac[..8].parse::<u64>().unwrap()
    }

    /// Funds the transit wallet with [`TRANSIT_SAT`], opens a
    /// [`CHANNEL_SAT`] channel and mines it until `Ready`. Returns what the
    /// node then holds for the user (exact, see `tests/channel.rs`).
    fn open_ready_channel(&self) -> u64 {
        let addr = self.client.transit_address().unwrap();
        self.rt.fund(&addr, TRANSIT_SAT);
        self.rt.mine(1);
        poll("transit funds spendable", 60, || {
            (self.client.balances().onchain_transit_sat == TRANSIT_SAT).then_some(())
        });
        self.client.open_channel(CHANNEL_SAT).expect("open_channel");
        let funding = self.wait_mempool("funding broadcast");
        assert_eq!(funding.len(), 1, "{funding:?}");
        let fee = self.mempool_fee_sat(&funding[0]);
        self.rt.mine(6);
        self.wait_event("ChannelReady", 90, |e| matches!(e, LnEvent::ChannelReady));
        poll("channel Ready", 60, || {
            (self.client.channel_state() == ChannelState::Ready).then_some(())
        });
        let held = TRANSIT_SAT - fee - FUNDER_COMMITMENT_COST_SAT;
        assert_eq!(sum(&self.client.balances()), held);
        held
    }

    /// Stops the client and the LSP: the LSP is gone.
    fn lsp_gone(&mut self) {
        self.client.stop();
        assert_eq!(self.client.start_mode(), None);
        assert!(self.lsp.stop().is_some_and(|s| s.success()));
    }
}

fn sum(b: &Balances) -> u64 {
    b.lightning_sat
        + b.onchain_transit_sat
        + b.pending_close_sat
        + b.reserved_sat
        + b.pending_transit_sat
}

fn client_config(
    rt: &Regtest,
    lsp: LspInfo,
    limits: LspLimits,
    vss: &Vss,
    dirs: &TempDir,
) -> NodeConfig {
    NodeConfig {
        bip39_seed: test_seed(41).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path("client"),
        chain_source: ChainSource::BitcoindRpc {
            host: "127.0.0.1".into(),
            port: rt.rpc_port,
            user: "fortis".into(),
            password: "fortis".into(),
        },
        lsp,
        vss: VssConfig {
            url: vss.url.clone(),
            auth_header: "Bearer test".into(),
        },
        limits,
    }
}

fn have_binaries() -> bool {
    common::env_bin("FORTIS_BTC_BITCOIND").is_some()
        && common::env_bin("FORTIS_VSS_SERVER").is_some()
}

/// Starts `client` in `mode`, asserting the brief's bound.
fn timed_start(client: &Node, mode: StartMode) -> Result<(), LnError> {
    let t = Instant::now();
    let res = client.start_with(mode);
    eprintln!("start_with({mode:?}): {res:?} after {:?}", t.elapsed());
    assert!(t.elapsed() < START_BOUND, "took {:?}", t.elapsed());
    res
}

/// A signed regtest BOLT11 invoice from a throwaway key.
fn some_invoice() -> PaymentTarget {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[0x42; 32]).unwrap();
    let inv = InvoiceBuilder::new(Currency::Regtest)
        .description("exit".into())
        .payment_hash(sha256::Hash::from_slice(&[1; 32]).unwrap())
        .payment_secret(PaymentSecret([2; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .expiry_time(Duration::from_secs(3_600))
        .amount_milli_satoshis(10_000 * SAT)
        .build_signed(|m| secp.sign_ecdsa_recoverable(m, &key))
        .unwrap();
    PaymentTarget::Bolt11 {
        invoice: inv.to_string(),
        amount_msat: inv.amount_milli_satoshis(),
        description: String::new(),
        expiry_secs: inv.expiry_time().as_secs(),
    }
}

/// Asserts `f` returns `LspUnreachable` within [`PROMPT`].
fn refused_promptly<T: std::fmt::Debug>(what: &str, f: impl FnOnce() -> Result<T, LnError>) {
    let t = Instant::now();
    let res = f();
    eprintln!("{what}: {res:?} after {:?}", t.elapsed());
    assert!(
        matches!(res, Err(LnError::LspUnreachable)),
        "{what}: {res:?}"
    );
    assert!(t.elapsed() < PROMPT, "{what} took {:?}", t.elapsed());
}

// --------------------------------------------------------------------------
// no binaries needed
// --------------------------------------------------------------------------

/// No network at all (TEST-NET-1): Exit mode skips the LSP but still needs
/// VSS, so it ends in `BackupUnavailable`, within the bound; the mode is
/// only reported while running.
#[test]
fn start_mode_reports_mode() {
    let dirs = TempDir::new("exit-nonet");
    let node = Node::new(NodeConfig {
        bip39_seed: test_seed(43).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path("client"),
        chain_source: ChainSource::Esplora {
            url: "http://192.0.2.1:3002".into(),
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
            min_payment_msat: 1_000,
            max_payment_msat: 1_000_000_000,
            channel_min_sat: 100_000,
            channel_max_sat: 500_000,
        },
    })
    .unwrap();
    assert_eq!(node.start_mode(), None);
    assert_eq!(
        timed_start(&node, StartMode::Exit),
        Err(LnError::BackupUnavailable)
    );
    assert_eq!(node.start_mode(), None);
    // Normal mode keeps its order: the LSP first.
    assert_eq!(
        timed_start(&node, StartMode::Normal),
        Err(LnError::LspUnreachable)
    );
    assert_eq!(node.start_mode(), None);
    // Not running: the Exit-mode gate does not apply, the usual errors do.
    assert_eq!(
        node.receive(10_000 * SAT, "x"),
        Err(LnError::Other("not_started".into()))
    );
}

// --------------------------------------------------------------------------
// real stack
// --------------------------------------------------------------------------

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn exit_mode_starts_with_lsp_down_within_bound() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.open_ready_channel();
    env.lsp_gone();

    assert_eq!(
        timed_start(&env.client, StartMode::Normal),
        Err(LnError::LspUnreachable)
    );
    assert_eq!(env.client.start_mode(), None);
    timed_start(&env.client, StartMode::Exit).expect("exit start");
    assert_eq!(env.client.start_mode(), Some(StartMode::Exit));
    // Already running: Ok, the mode is unchanged.
    timed_start(&env.client, StartMode::Normal).expect("already running");
    assert_eq!(env.client.start_mode(), Some(StartMode::Exit));
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn exit_mode_force_close_and_sweep_recovers_funds() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    let held = env.open_ready_channel();
    env.lsp_gone();
    timed_start(&env.client, StartMode::Exit).expect("exit start");

    // Readable offline: the channel is still confirmed and open.
    assert_eq!(env.client.channel_state(), ChannelState::Ready);
    let b = env.client.balances();
    assert_eq!(sum(&b), held, "{b:?}");
    assert!(b.lightning_sat > 0, "{b:?}");
    assert!(env.client.transit_address().is_ok());

    env.client.force_close_channel().expect("force close");
    let commitment = env.wait_mempool("commitment broadcast");
    assert_eq!(commitment.len(), 1, "commitment only: {commitment:?}");
    env.rt.mine(1);
    poll("closing", 60, || {
        (env.client.channel_state() == ChannelState::Closing).then_some(())
    });
    let to = env.external_address();
    assert_eq!(
        env.client.sweep_transit(&to),
        Err(LnError::Other("closing".into())),
        "to_self output under its CSV"
    );

    // Past the CSV delay ldk-node sweeps our output into transit.
    env.rt.mine(140);
    let mut sweep = None;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(1_500));
        if let Some(t) = env.mempool().first() {
            sweep = Some(t.clone());
            break;
        }
        env.rt.mine(1);
    }
    let sweep = sweep.expect("ldk-node broadcast the to_local sweep");
    let sweep_fee = env.mempool_fee_sat(&sweep);
    env.rt.mine(1);
    poll("force close resolved", 90, || {
        let b = env.client.balances();
        (b.pending_close_sat == 0 && env.client.channel_state() == ChannelState::None).then_some(())
    });
    let ledger = held - sweep_fee;
    let b = env.client.balances();
    assert_eq!(sum(&b), ledger, "{b:?}");
    assert_eq!(b.onchain_transit_sat, ledger, "{b:?}");

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
    assert_eq!(env.received_sat(&to, 1), ledger - fee);
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn exit_mode_refuses_lightning_actions_promptly() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.open_ready_channel();
    env.lsp_gone();
    timed_start(&env.client, StartMode::Exit).expect("exit start");

    let target = some_invoice();
    refused_promptly("pay", || env.client.pay(&target, None));
    refused_promptly("receive (plain-sized)", || {
        env.client.receive(1_000 * SAT, "x")
    });
    refused_promptly("receive (jit-sized)", || {
        env.client.receive(200_000 * SAT, "x")
    });
    refused_promptly("open_channel", || env.client.open_channel(CHANNEL_SAT));
    refused_promptly("close_channel", || env.client.close_channel());
    // Nothing happened: still one ready channel, nothing broadcast.
    assert_eq!(env.client.channel_state(), ChannelState::Ready);
    assert!(env.mempool().is_empty());
}

/// The LSP host is gone (TEST-NET-1: packets dropped, no refusal), chain
/// source and VSS up: Normal mode is `LspUnreachable`, Exit mode starts,
/// paying at most the 2 s best-effort probe for the LSP.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND and FORTIS_VSS_SERVER"]
fn exit_mode_blackholed_lsp_starts_promptly() {
    if !have_binaries() {
        return;
    }
    let rt = Regtest::start();
    let vss = Vss::start();
    let dirs = TempDir::new("exit-blackhole");
    let lsp = LspInfo {
        node_id: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".into(),
        host: "192.0.2.1".into(),
        port: 9735,
        token: None,
    };
    let limits = LspLimits {
        min_payment_msat: 1_000,
        max_payment_msat: 1_000_000_000,
        channel_min_sat: 100_000,
        channel_max_sat: 500_000,
    };
    let client = Node::new(client_config(&rt, lsp, limits, &vss, &dirs)).unwrap();
    assert_eq!(
        timed_start(&client, StartMode::Normal),
        Err(LnError::LspUnreachable)
    );
    let t = Instant::now();
    timed_start(&client, StartMode::Exit).expect("exit start");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(client.start_mode(), Some(StartMode::Exit));
    assert_eq!(client.channel_state(), ChannelState::None);
    assert!(client.transit_address().is_ok());
    refused_promptly("receive", || client.receive(1_000 * SAT, "x"));
    client.stop();
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn exit_mode_backup_down_still_refuses_to_start() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.lsp_gone();
    env.vss.pause();
    let res = timed_start(&env.client, StartMode::Exit);
    env.vss.resume();
    assert_eq!(res, Err(LnError::BackupUnavailable));
    assert_eq!(env.client.start_mode(), None);
    // VSS back: Exit mode starts.
    timed_start(&env.client, StartMode::Exit).expect("exit start, vss back");
}

#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn exit_mode_then_normal_when_lsp_returns() {
    if !have_binaries() {
        return;
    }
    let mut env = Env::start();
    env.open_ready_channel();
    env.lsp_gone();
    timed_start(&env.client, StartMode::Exit).expect("exit start");
    refused_promptly("receive while the LSP is down", || {
        env.client.receive(200_000 * SAT, "x")
    });

    env.lsp.start_again();
    env.client.stop();
    timed_start(&env.client, StartMode::Normal).expect("normal start, LSP back");
    assert_eq!(env.client.start_mode(), Some(StartMode::Normal));
    poll("channel usable again", 60, || {
        (env.client.balances().lightning_sat > 0
            && env.client.channel_state() == ChannelState::Ready)
            .then_some(())
    });

    // Payments work again: a JIT receive paid by a second node over the
    // LSP, then a payment back to it.
    let payer = Payer::start(&env.rt, &env.lsp);
    let q = env.client.receive(100_000 * SAT, "back").expect("quote");
    assert!(q.needs_channel, "the client funded its channel: no inbound");
    payer.pay(&q.invoice);
    env.wait_event("PaymentReceived", 120, |e| {
        matches!(e, LnEvent::PaymentReceived { .. })
    });
    let inv = payer
        .node
        .bolt11_payment()
        .receive(
            10_000 * SAT,
            &Bolt11InvoiceDescription::Direct(Description::new("to payer".into()).unwrap()),
            3_600,
        )
        .unwrap();
    let target = fortis_lightning::parse(&inv.to_string(), Network::Regtest).unwrap();
    let id = env.client.pay(&target, None).expect("pay");
    env.wait_event("PaymentSent", 120, |e| match e {
        LnEvent::PaymentSent { id: i, .. } => *i == id,
        LnEvent::PaymentFailed { id: i, reason } if *i == id => panic!("failed: {reason}"),
        _ => false,
    });
}

/// Kill-switch state (`GET /ln/info` `"enabled": false` while the LSP is still
/// online): starting in `StartMode::Exit` connects to the LSP best-effort so
/// paying out and closing existing channels work, while refusing `open_channel`
/// and JIT `receive` (`needs_channel == true`).
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and fortis-lsp built"]
fn exit_mode_with_lsp_online_blocks_new_channels_and_jit_but_allows_pay_and_close() {
    if !have_binaries() {
        return;
    }
    let env = Env::start();
    env.open_ready_channel();
    env.client.stop();

    timed_start(&env.client, StartMode::Exit).expect("exit start with LSP online");
    assert_eq!(env.client.start_mode(), Some(StartMode::Exit));
    poll("channel ready in exit mode with LSP online", 30, || {
        (env.client.channel_state() == ChannelState::Ready
            && env.client.balances().lightning_sat > 0)
            .then_some(())
    });

    // New channel open and JIT receive are refused in Exit mode even when the LSP is connected.
    assert_eq!(
        env.client.open_channel(CHANNEL_SAT),
        Err(LnError::Other("lightning_disabled".into()))
    );
    assert_eq!(
        env.client.receive(200_000 * SAT, "jit blocked"),
        Err(LnError::Other("lightning_disabled".into()))
    );

    // Paying out over the existing channel still works.
    let payer = Payer::start(&env.rt, &env.lsp);
    // The payer's channel to the LSP is funded by the payer alone, so the LSP
    // has nothing on its side to forward to it and the client's payment would
    // have no route (`RouteNotFound`). The other tests give it a balance by
    // having the payer pay a JIT invoice first; Exit mode refuses that (as
    // asserted above), so the payer sends the LSP a keysend instead.
    let lsp_id = PublicKey::from_str(&env.lsp.node_id).unwrap();
    let keysend = payer
        .node
        .spontaneous_payment()
        .send(100_000 * SAT, lsp_id, None)
        .expect("payer sends the LSP a keysend");
    common::wait_event(&payer.node, "payer keysend to the LSP", 60, |e| match e {
        ldk_node::Event::PaymentSuccessful { payment_id, .. } => *payment_id == Some(keysend),
        ldk_node::Event::PaymentFailed {
            payment_id, reason, ..
        } if *payment_id == Some(keysend) => panic!("keysend failed: {reason:?}"),
        _ => false,
    });
    let inv = payer
        .node
        .bolt11_payment()
        .receive(
            10_000 * SAT,
            &Bolt11InvoiceDescription::Direct(Description::new("exit pay".into()).unwrap()),
            3_600,
        )
        .unwrap();
    let target = fortis_lightning::parse(&inv.to_string(), Network::Regtest).unwrap();
    let id = env.client.pay(&target, None).expect("pay in exit mode");
    env.wait_event("PaymentSent", 120, |e| match e {
        LnEvent::PaymentSent { id: i, .. } => *i == id,
        LnEvent::PaymentFailed { id: i, reason } if *i == id => panic!("failed: {reason}"),
        _ => false,
    });

    // Cooperative close also works while the LSP is online.
    env.client.close_channel().expect("close in exit mode");
}
