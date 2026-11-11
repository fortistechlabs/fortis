//! Backup, restore and the one-active-device guard (spec: Decisions *Backup*
//! and *Multiple devices*, Findings (Task 1) (b)): a `fortis/device-lock`
//! record in the node's VSS store, written with conditional versions, is
//! held by the running node; a second device is refused before it builds
//! anything, `takeover` moves the lock, and the old device stops itself at
//! its next check. Real-stack tests are `#[ignore]`d; run them with:
//!
//! ```sh
//! cargo build -p fortis-lsp -p fortis-edge
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test restore -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Two "devices" in one test process: the second `Node` for the same seed
//! gets its own data dir (so its own device id) and
//! [`test_hooks::own_claims`], so the process-wide one-node-per-wallet claim
//! (Task 4) does not refuse it before it reaches VSS — on real phones that
//! claim never spans devices.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use common::{free_port, poll, test_seed, Lsp, Payer, Regtest, TempDir, Vss};
use fortis_lightning::vss_client::client::VssClient;
use fortis_lightning::vss_client::error::VssError;
use fortis_lightning::vss_client::types::GetObjectRequest;
use fortis_lightning::vss_client::util::retry::{ExponentialBackoffRetryPolicy, RetryPolicy};
use fortis_lightning::{
    test_hooks, vss_state_exists, ChainSource, ChannelState, LnChain, LnError, LnEvent, Node,
    NodeConfig,
    PaymentTarget, StartMode, VssConfig,
};
use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};
use ldk_node::bitcoin::Network;
use ldk_node::lightning::types::payment::PaymentSecret;
use ldk_node::lightning_invoice::{
    Bolt11Invoice, Bolt11InvoiceDescription, Currency, Description, InvoiceBuilder,
};

const SAT: u64 = 1_000;
const START_BOUND: Duration = Duration::from_secs(15);
const DEVICE_ID_FILE: &str = "fortis-device-id";

/// Regtest, VSS, `fortis-lsp`, and (optionally) the harness `Payer`. Fields
/// drop in order: payer, LSP, VSS, chain, dir. Nodes are owned by the tests
/// (declared after the `Env`, so dropped first).
struct Env {
    payer: Option<Payer>,
    lsp: Lsp,
    vss: Vss,
    rt: Regtest,
    /// `test_seed` is random per call: one seed per test.
    seed: [u8; 64],
    dirs: TempDir,
}

impl Env {
    fn start(label: &str) -> Env {
        let rt = Regtest::start();
        let vss = Vss::start();
        let lsp = Lsp::start(&rt, &[]);
        Env {
            payer: None,
            lsp,
            vss,
            rt,
            seed: test_seed(0x10),
            dirs: TempDir::new(label),
        }
    }

    /// A `NodeConfig` for this wallet with data dir `dir` against `vss`.
    fn config_via(&self, dir: &str, vss: VssConfig) -> NodeConfig {
        NodeConfig {
            chain: fortis_lightning::LnChain::Btc,
            bip39_seed: self.seed.to_vec(),
            network: Network::Regtest,
            data_dir: self.dirs.path(dir),
            chain_source: ChainSource::BitcoindRpc {
                host: "127.0.0.1".into(),
                port: self.rt.rpc_port,
                user: "fortis".into(),
                password: "fortis".into(),
            },
            lsp: self.lsp.lsp_info(),
            vss,
            limits: self.lsp.limits(),
        }
    }

    fn vss_direct(&self) -> VssConfig {
        VssConfig {
            url: self.vss.url.clone(),
            auth_header: "Bearer test".into(),
        }
    }

    /// A `Node` for this wallet in data dir `dir` (one device per dir).
    fn node(&self, dir: &str) -> Node {
        Node::new(self.config_via(dir, self.vss_direct())).unwrap()
    }

    /// Another device in this process: own data dir and own claim scope.
    fn other_device(&self, dir: &str) -> Node {
        let mut n = self.node(dir);
        test_hooks::own_claims(&mut n, dir);
        n
    }

    /// The `Payer` pays `node`'s first (JIT) receive of 100 000 sat: a
    /// channel holding 99 000 sat for the client.
    fn give_channel(&mut self, node: &Node) {
        let payer = Payer::start(&self.rt, &self.lsp);
        let q = node.receive(100_000 * SAT, "first").expect("quote");
        assert!(q.needs_channel);
        payer.pay(&q.invoice);
        wait_ln_event(node, "PaymentReceived", 120, |e| {
            matches!(e, LnEvent::PaymentReceived { .. })
        });
        self.rt.mine(6);
        wait_restored(node);
        self.payer = Some(payer);
    }

    /// The lock record as stored in VSS: `(device_id, version)`, or `None`.
    fn lock_record(&self, node: &Node) -> Option<(String, i64)> {
        read_lock(&self.vss.url, &test_hooks::store_id(node))
    }
}

/// A node restored from VSS holds the 99 000 sat channel, `Ready`.
fn wait_restored(node: &Node) {
    poll("channel Ready with 99 000 sat", 90, || {
        (node.balances().lightning_sat == 99_000 && node.channel_state() == ChannelState::Ready)
            .then_some(())
    });
}

fn read_lock(vss_url: &str, store_id: &str) -> Option<(String, i64)> {
    let rt = ldk_node::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let policy =
        ExponentialBackoffRetryPolicy::<VssError>::new(Duration::from_millis(10)).with_max_attempts(1);
    let client = VssClient::new(vss_url.to_string(), policy);
    let req = GetObjectRequest {
        store_id: store_id.to_string(),
        key: test_hooks::DEVICE_LOCK_KEY.into(),
    };
    match rt.block_on(client.get_object(&req)) {
        Ok(r) => {
            let kv = r.value.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&kv.value).expect("lock record is JSON");
            Some((v["device_id"].as_str().unwrap().to_string(), kv.version))
        }
        Err(VssError::NoSuchKeyError(_)) => None,
        Err(e) => panic!("read lock: {e}"),
    }
}

fn device_id(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(DEVICE_ID_FILE)).expect("device id file")
}

fn wait_ln_event(node: &Node, what: &str, secs: u64, pred: impl Fn(&LnEvent) -> bool) -> LnEvent {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        if let Some(e) = node.next_event(left.min(Duration::from_secs(1))) {
            if pred(&e) {
                return e;
            }
            eprintln!("({what}: skipping {e:?})");
        }
    }
}

/// Every event `node` has queued now (waits at most `wait` for the first).
fn drain_events(node: &Node, wait: Duration) -> Vec<LnEvent> {
    let mut out = Vec::new();
    let mut w = wait;
    while let Some(e) = node.next_event(w) {
        out.push(e);
        w = Duration::from_millis(200);
    }
    out
}

fn lock_lost_count(events: &[LnEvent]) -> usize {
    events.iter().filter(|e| **e == LnEvent::DeviceLockLost).count()
}

fn timed<T: std::fmt::Debug>(what: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    eprintln!("{what}: {r:?} after {:?}", t.elapsed());
    assert!(t.elapsed() < START_BOUND, "{what} took {:?}", t.elapsed());
    r
}

fn wait_quiescent(node: &Node) {
    poll("node quiescent", 15, || node.is_quiescent().then_some(()));
}

/// A signed regtest BOLT11 invoice from a throwaway key: passes every
/// target check, so `pay` reaches the pre-send gate.
fn some_invoice() -> PaymentTarget {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[0x42; 32]).unwrap();
    let inv = InvoiceBuilder::new(Currency::Regtest)
        .description("restore".into())
        .payment_hash(sha256::Hash::from_slice(&[1; 32]).unwrap())
        .payment_secret(PaymentSecret([2; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .expiry_time(Duration::from_secs(3_600))
        .amount_milli_satoshis(10_000 * SAT)
        .build_signed(|m| secp.sign_ecdsa_recoverable(m, &key))
        .unwrap();
    bolt11(&inv)
}

fn bolt11(inv: &Bolt11Invoice) -> PaymentTarget {
    PaymentTarget::Bolt11 {
        invoice: inv.to_string(),
        amount_msat: inv.amount_milli_satoshis(),
        description: String::new(),
        expiry_secs: inv.expiry_time().as_secs(),
    }
}

/// The restored node pays a `Payer` invoice: the channel works.
fn pays_through_restored_channel(env: &Env, node: &Node) {
    let inv = env
        .payer
        .as_ref()
        .unwrap()
        .node
        .bolt11_payment()
        .receive(
            10_000 * SAT,
            &Bolt11InvoiceDescription::Direct(Description::new("after restore".into()).unwrap()),
            3_600,
        )
        .unwrap();
    let id = node.pay(&bolt11(&inv), None).expect("pay over the restored channel");
    wait_ln_event(node, "PaymentSent", 120, |e| match e {
        LnEvent::PaymentSent { id: i, .. } => *i == id,
        LnEvent::PaymentFailed { id: i, reason } if *i == id => panic!("payment failed: {reason}"),
        _ => false,
    });
}

/// Restores the default background-check interval when a test ends.
struct IntervalGuard;

impl IntervalGuard {
    fn set(d: Duration) -> IntervalGuard {
        test_hooks::set_lock_check_interval(d);
        IntervalGuard
    }
}

impl Drop for IntervalGuard {
    fn drop(&mut self) {
        test_hooks::set_lock_check_interval(test_hooks::DEFAULT_LOCK_CHECK_INTERVAL);
    }
}

// --------------------------------------------------------------------------
// restore
// --------------------------------------------------------------------------

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn restore_from_seed_via_vss_recovers_channel_and_balance() {
    let mut env = Env::start("restore-clean");
    let a = env.node("a");
    a.start().expect("original starts");
    env.give_channel(&a);
    let node_id = a.node_id();

    // The original closes the app cleanly, then the phone is lost.
    a.stop();
    wait_quiescent(&a);
    drop(a);
    std::fs::remove_dir_all(env.dirs.path("a")).unwrap();
    assert_eq!(env.lock_record(&env.node("probe")), None, "a clean stop frees the lock");

    // New phone: same seed, empty data dir, same VSS. No takeover needed.
    let b = env.node("b");
    timed("restored start", || b.start()).expect("restore starts without takeover");
    assert_eq!(b.node_id(), node_id);
    wait_restored(&b);
    pays_through_restored_channel(&env, &b);
    b.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn restore_after_crash_needs_takeover_then_recovers() {
    let mut env = Env::start("restore-crash");
    let a = env.node("a");
    a.start().expect("original starts");
    env.give_channel(&a);
    let a_device = device_id(&env.dirs.path("a"));

    // The original dies without releasing its lock (crash, battery, …).
    test_hooks::crash(&a);
    wait_quiescent(&a);
    drop(a);
    std::fs::remove_dir_all(env.dirs.path("a")).unwrap();

    let b = env.node("b");
    let builds = test_hooks::builds();
    let res = timed("restored start, lock still held", || b.start());
    assert_eq!(res, Err(LnError::ActiveOnAnotherDevice));
    assert_eq!(test_hooks::builds(), builds, "nothing built");
    assert_eq!(b.start_mode(), None);
    assert_eq!(env.lock_record(&b).map(|r| r.0), Some(a_device));

    timed("takeover", || b.takeover(StartMode::Normal)).expect("takeover");
    assert_eq!(b.start_mode(), Some(StartMode::Normal));
    assert_eq!(
        env.lock_record(&b).map(|r| r.0),
        Some(device_id(&env.dirs.path("b")))
    );
    wait_restored(&b);
    pays_through_restored_channel(&env, &b);
    b.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn vss_state_exists_reports_false_then_true() {
    let env = Env::start("restore-exists");
    let vss = env.vss_direct();
    assert_eq!(vss_state_exists(&vss, &env.seed, Network::Regtest, LnChain::Btc), Ok(false));

    let a = env.node("a");
    a.start().unwrap();
    assert_eq!(vss_state_exists(&vss, &env.seed, Network::Regtest, LnChain::Btc), Ok(true));
    a.stop();
    // The lock is gone after a clean stop; the node's state is not.
    assert_eq!(vss_state_exists(&vss, &env.seed, Network::Regtest, LnChain::Btc), Ok(true));
    // Another wallet's store is still empty.
    let other = test_seed(0x77);
    assert_eq!(vss_state_exists(&vss, &other, Network::Regtest, LnChain::Btc), Ok(false));
    // VSS down: an error, never a guess.
    env.vss.pause();
    let t = Instant::now();
    let res = vss_state_exists(&vss, &env.seed, Network::Regtest, LnChain::Btc);
    env.vss.resume();
    assert_eq!(res, Err(LnError::BackupUnavailable));
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

// --------------------------------------------------------------------------
// one active device
// --------------------------------------------------------------------------

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn second_device_start_is_rejected_without_starting_anything() {
    let env = Env::start("restore-second");
    let a = env.node("a");
    a.start().unwrap();

    let b = env.other_device("b");
    let builds = test_hooks::builds();
    test_hooks::reset_max_live();
    for mode in [StartMode::Normal, StartMode::Exit] {
        let res = timed("second device start", || b.start_with(mode));
        assert_eq!(res, Err(LnError::ActiveOnAnotherDevice), "{mode:?}");
        assert_eq!(b.start_mode(), None);
    }
    assert_eq!(test_hooks::builds(), builds, "the second device built no ldk-node");
    assert_eq!(test_hooks::max_live(), 1);
    assert!(b.is_quiescent());
    assert_eq!(b.next_event(Duration::from_millis(100)), None);
    // The first device is untouched and still holds the lock.
    assert_eq!(a.start_mode(), Some(StartMode::Normal));
    assert_eq!(
        env.lock_record(&a).map(|r| r.0),
        Some(device_id(&env.dirs.path("a")))
    );
    assert!(a.pay(&some_invoice(), None).is_err_and(|e| e != LnError::ActiveOnAnotherDevice));
    assert_eq!(lock_lost_count(&drain_events(&a, Duration::from_millis(300))), 0);
    a.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn takeover_makes_the_old_device_stop() {
    let env = Env::start("restore-takeover");
    // Part 1: the old device finds out at its next money action.
    let _slow = IntervalGuard::set(Duration::from_secs(3_600));
    let a = env.node("a");
    a.start().unwrap();
    let b = env.other_device("b");
    assert_eq!(b.start(), Err(LnError::ActiveOnAnotherDevice));
    assert_eq!(
        b.takeover(StartMode::Exit),
        Ok(()),
        "takeover works in either mode"
    );
    assert_eq!(b.takeover(StartMode::Normal), Err(LnError::Other("already_running".into())));
    assert_eq!(b.start_mode(), Some(StartMode::Exit));

    let t = Instant::now();
    assert_eq!(a.pay(&some_invoice(), None), Err(LnError::ActiveOnAnotherDevice));
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(a.start_mode(), None, "the old device stopped its node");
    wait_quiescent(&a);
    let events = drain_events(&a, Duration::from_secs(1));
    assert_eq!(lock_lost_count(&events), 1, "{events:?}");
    assert_eq!(a.next_event(Duration::from_millis(300)), None, "reported once");
    // It stays refused until it takes over itself; the channel calls too.
    assert_eq!(a.start(), Err(LnError::ActiveOnAnotherDevice));
    assert_eq!(a.open_channel(100_000), Err(LnError::Other("not_started".into())));
    assert_eq!(
        env.lock_record(&a).map(|r| r.0),
        Some(device_id(&env.dirs.path("b")))
    );

    // Part 2: the background check, with nothing else happening.
    let _fast = IntervalGuard::set(Duration::from_secs(1));
    b.stop();
    b.start().expect("b restarts (it released the lock on stop)");
    // Exit mode: while b's ldk-node (same node id) is connected to the
    // LSP, the LSP refuses a second connection, so a Normal start would
    // end in `LspUnreachable`.
    a.takeover(StartMode::Exit).expect("a takes the lock back");
    let t = Instant::now();
    poll("b's background check stops it", 15, || b.start_mode().is_none().then_some(()));
    eprintln!("background check stopped the old device after {:?}", t.elapsed());
    wait_quiescent(&b);
    let events = drain_events(&b, Duration::from_secs(1));
    assert_eq!(lock_lost_count(&events), 1, "{events:?}");
    // The holder's own background checks keep it running.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(a.start_mode(), Some(StartMode::Exit));
    assert_eq!(lock_lost_count(&drain_events(&a, Duration::from_millis(300))), 0);
    a.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn own_crash_restart_is_not_another_device() {
    let env = Env::start("restore-own-crash");
    let a = env.node("a");
    a.start().unwrap();
    let held = env.lock_record(&a).expect("lock held");
    test_hooks::crash(&a);
    wait_quiescent(&a);
    assert_eq!(env.lock_record(&a), Some(held.clone()), "a crash leaves the lock");

    // The same `Node` again, then a fresh `Node` on the same data dir (an
    // app restart after the crash): both are this device.
    timed("restart after crash", || a.start()).expect("same device restarts");
    let again = env.lock_record(&a).expect("lock held");
    assert_eq!(again.0, held.0);
    assert!(again.1 > held.1, "re-acquired: version bumped");
    test_hooks::crash(&a);
    wait_quiescent(&a);
    drop(a);
    let a2 = env.node("a");
    timed("new process after crash", || a2.start()).expect("same data dir restarts");
    a2.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn concurrent_takeovers_exactly_one_wins() {
    let env = Env::start("restore-race");
    let a = env.node("a");
    a.start().unwrap();
    test_hooks::crash(&a);
    wait_quiescent(&a);

    let b = Arc::new(env.other_device("b"));
    let c = Arc::new(env.other_device("c"));
    // Both read the lock before either writes.
    test_hooks::set_takeover_barrier(Some(Arc::new(Barrier::new(2))));
    let builds = test_hooks::builds();
    let threads: Vec<_> = [&b, &c]
        .into_iter()
        .map(|n| {
            let n = Arc::clone(n);
            std::thread::spawn(move || n.takeover(StartMode::Normal))
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    test_hooks::set_takeover_barrier(None);
    eprintln!("concurrent takeovers: {results:?}");
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{results:?}");
    assert!(results.contains(&Err(LnError::ActiveOnAnotherDevice)), "{results:?}");
    assert_eq!(test_hooks::builds() - builds, 1, "only the winner built");
    let (winner, loser, wdir) = if results[0].is_ok() {
        (&b, &c, "b")
    } else {
        (&c, &b, "c")
    };
    assert!(winner.start_mode().is_some());
    assert_eq!(loser.start_mode(), None);
    assert_eq!(
        env.lock_record(winner).map(|r| r.0),
        Some(device_id(&env.dirs.path(wdir)))
    );
    winner.stop();
}

// --------------------------------------------------------------------------
// VSS down
// --------------------------------------------------------------------------

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn vss_down_at_start_is_backup_unavailable() {
    let env = Env::start("restore-vss-down-start");
    let a = env.node("a");
    let builds = test_hooks::builds();
    env.vss.pause();
    let res = timed("start with vss down", || a.start());
    env.vss.resume();
    assert_eq!(res, Err(LnError::BackupUnavailable));
    assert_eq!(test_hooks::builds(), builds, "no ldk-node built without the lock");
    assert_eq!(a.start_mode(), None);
    wait_quiescent(&a);
    // VSS back: the same device starts (whatever of its lock writes landed
    // after the freeze is its own).
    timed("start, vss back", || a.start()).expect("start with vss back");
    a.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn vss_down_midway_stops_payments() {
    let env = Env::start("restore-vss-down-midway");
    let _fast = IntervalGuard::set(Duration::from_secs(1));
    let a = env.node("a");
    a.start().unwrap();

    env.vss.pause();
    let t = Instant::now();
    assert_eq!(a.pay(&some_invoice(), None), Err(LnError::BackupUnavailable));
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    // Unreachable is not a lost lock: the node keeps running (its own
    // background checks fail too and change nothing).
    std::thread::sleep(Duration::from_secs(4));
    assert!(a.start_mode().is_some());
    env.vss.resume();

    // The refresh that timed out lands once VSS thaws (a version this node
    // never saw): still ours, the gate passes (and the empty node then
    // has no liquidity).
    let res = a.pay(&some_invoice(), None);
    assert_eq!(res, Err(LnError::InsufficientLightning));
    std::thread::sleep(Duration::from_secs(3));
    assert!(a.start_mode().is_some());
    assert_eq!(lock_lost_count(&drain_events(&a, Duration::from_millis(300))), 0);
    a.stop();
}

// --------------------------------------------------------------------------
// exit mode, stop
// --------------------------------------------------------------------------

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn lock_survives_exit_mode() {
    let env = Env::start("restore-exit");
    let _fast = IntervalGuard::set(Duration::from_secs(1));
    let a = env.node("a");
    a.start_with(StartMode::Exit).expect("exit-mode start takes the lock");
    let b = env.other_device("b");
    assert_eq!(b.start_with(StartMode::Exit), Err(LnError::ActiveOnAnotherDevice));
    assert_eq!(b.start(), Err(LnError::ActiveOnAnotherDevice));
    a.stop();
    b.start_with(StartMode::Exit).expect("lock free after a's stop");
    assert_eq!(a.start_with(StartMode::Exit), Err(LnError::ActiveOnAnotherDevice));
    a.takeover(StartMode::Exit).expect("exit-mode takeover");
    assert_eq!(a.start_mode(), Some(StartMode::Exit));
    // b, an Exit-mode node, loses the lock like any other (here: its
    // background check).
    poll("exit-mode b stops itself", 15, || b.start_mode().is_none().then_some(()));
    assert_eq!(lock_lost_count(&drain_events(&b, Duration::from_secs(1))), 1);
    assert_eq!(a.start_mode(), Some(StartMode::Exit));
    a.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn stop_releases_the_lock() {
    let env = Env::start("restore-release");
    let a = env.node("a");
    a.start().unwrap();
    assert_eq!(
        env.lock_record(&a).map(|r| r.0),
        Some(device_id(&env.dirs.path("a")))
    );
    let t = Instant::now();
    a.stop();
    eprintln!("stop took {:?}", t.elapsed());
    assert!(a.is_quiescent());
    assert_eq!(env.lock_record(&a), None, "released on stop");
    // Phone A closed, phone B opened: no takeover.
    let b = env.other_device("b");
    b.start().expect("lock free");
    b.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn stop_when_vss_down_keeps_our_lock() {
    let mut env = Env::start("restore-stop-down");
    let a = env.node("a");
    a.start().unwrap();
    let held = env.lock_record(&a).unwrap();
    // Down for real (connections refused), not frozen: a frozen vss-server
    // would execute the queued release once thawed.
    env.vss.down();
    let t = Instant::now();
    a.stop();
    eprintln!("stop with vss down took {:?}", t.elapsed());
    env.vss.up();
    assert_eq!(a.start_mode(), None);
    wait_quiescent(&a);
    assert_eq!(env.lock_record(&a).map(|r| r.0), Some(held.0), "still ours");
    let b = env.other_device("b");
    assert_eq!(b.start(), Err(LnError::ActiveOnAnotherDevice));
    drop(a);
    let a2 = env.node("a");
    a2.start().expect("this device starts again");
    a2.stop();
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn device_id_file_is_0600_and_stable() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::start("restore-device-id");
    let a = env.node("a");
    a.start().unwrap();
    let path = env.dirs.path("a").join(DEVICE_ID_FILE);
    let id = std::fs::read_to_string(&path).unwrap();
    assert_eq!(id.len(), 32, "{id:?}");
    assert!(id.bytes().all(|c| c.is_ascii_hexdigit()), "{id:?}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    a.stop();
    a.start().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), id, "stable across starts");
    a.stop();
    // Another data dir is another device.
    let b = env.node("b");
    b.start().unwrap();
    assert_ne!(device_id(&env.dirs.path("b")), id);
    b.stop();
}

// --------------------------------------------------------------------------
// through the edge
// --------------------------------------------------------------------------

/// The `fortis-edge` binary next to this test binary (`target/<profile>/`).
fn edge_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let p = exe.parent().and_then(|d| d.parent()).unwrap().join("fortis-edge");
    assert!(
        p.exists(),
        "{} not built — run `cargo build -p fortis-edge` first",
        p.display()
    );
    p
}

/// A `fortis-edge` process proxying `/ln/vss/*` to `vss_upstream`; killed
/// on drop.
struct Edge {
    url: String,
    child: std::process::Child,
}

impl Edge {
    fn start(dir: &TempDir, vss_upstream: &str) -> Edge {
        let port = free_port();
        let p = |n: &str| dir.str(n);
        let log = std::fs::File::create(dir.path("edge.log")).unwrap();
        let child = std::process::Command::new(edge_bin())
            .args([
                "--bind", &format!("127.0.0.1:{port}"),
                "--xbt-upstream", "http://127.0.0.1:9",
                "--btc-upstream-fallback=",
                "--secret-file", &p("edge.secret"),
                "--cache-dir", &p("edge-cache"),
                "--btc-history-dir", &p("edge-btc-history"),
                "--btc-scan-cache-dir", &p("edge-btc-scan-cache"),
                "--register-per-hour", "1000",
                "--workers", "8",
                // ldk-node is chatty with VSS; the limiter is not under test.
                "--rate-per-min", "100000",
                "--rate-burst", "100000",
                "--vss-upstream", vss_upstream,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn fortis-edge");
        let edge = Edge {
            url: format!("http://127.0.0.1:{port}"),
            child,
        };
        poll("fortis-edge up", 30, || ureq::get(&format!("{}/", edge.url)).call().ok());
        edge
    }

    fn token(&self) -> String {
        let r = ureq::post(&format!("{}/register", self.url))
            .send_bytes(b"")
            .expect("register");
        let v: serde_json::Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["token"].as_str().unwrap().to_string()
    }

    fn metric(&self, name: &str) -> u64 {
        let text = ureq::get(&format!("{}/metrics", self.url))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} not in /metrics"))
            .parse()
            .unwrap()
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "needs fortis-lsp and fortis-edge built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn restore_through_the_edge_proxy() {
    let mut env = Env::start("restore-edge");
    let edge_dir = TempDir::new("restore-edge-proc");
    let edge = Edge::start(&edge_dir, &env.vss.url);
    let via_edge = VssConfig {
        url: format!("{}/ln/vss", edge.url),
        auth_header: format!("Bearer {}", edge.token()),
    };
    assert_eq!(vss_state_exists(&via_edge, &env.seed, Network::Regtest, LnChain::Btc), Ok(false));

    let a = Node::new(env.config_via("a", via_edge.clone())).unwrap();
    timed("start via the edge", || a.start()).expect("start through the edge");
    env.give_channel(&a);
    // Another device through the edge: the conditional write's 409 is
    // relayed as is (`ConflictError` → `ActiveOnAnotherDevice`).
    let mut other = Node::new(env.config_via("other", via_edge.clone())).unwrap();
    test_hooks::own_claims(&mut other, "other");
    assert_eq!(other.start(), Err(LnError::ActiveOnAnotherDevice));
    a.stop();
    drop(a);
    std::fs::remove_dir_all(env.dirs.path("a")).unwrap();
    assert_eq!(vss_state_exists(&via_edge, &env.seed, Network::Regtest, LnChain::Btc), Ok(true));

    // Restore on a new phone through the edge (a fresh install token).
    let restored_vss = VssConfig {
        url: via_edge.url.clone(),
        auth_header: format!("Bearer {}", edge.token()),
    };
    let b = Node::new(env.config_via("b", restored_vss)).unwrap();
    timed("restore via the edge", || b.start()).expect("restore through the edge");
    wait_restored(&b);
    pays_through_restored_channel(&env, &b);
    // All of it went through the proxy, none of it failed upstream.
    let forwarded = edge.metric("fortis_edge_vss_requests_total");
    eprintln!("edge forwarded {forwarded} VSS requests");
    assert!(forwarded > 20, "{forwarded}");
    assert_eq!(edge.metric("fortis_edge_vss_upstream_errors_total"), 0);
    assert_eq!(
        env.lock_record(&b).map(|r| r.0),
        Some(device_id(&env.dirs.path("b")))
    );
    b.stop();
    assert_eq!(env.lock_record(&b), None);
    drop(other);
}
