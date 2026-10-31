//! `Node::pay` / `Node::payments` against the real `fortis-lsp`: the client
//! (the `Node` under test) gets a channel from a first JIT receive paid by
//! the harness [`Payer`] (a stock ldk-node with a channel to the LSP), then
//! pays invoices the `Payer` creates, routed client → LSP → `Payer`.
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test pay -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The tests that are not `#[ignore]`d need no binaries: target checks run
//! before the node is consulted.

mod common;

use std::str::FromStr;
use std::time::{Duration, Instant};

use common::{poll, test_seed, wait_event, Lsp, Payer, Regtest, TempDir, Vss};
use fortis_lightning::{
    test_hooks, ChainSource, Direction, LnError, LnEvent, LspInfo, LspLimits, Node, NodeConfig,
    Payment, PaymentTarget, Status, VssConfig,
};
use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};
use ldk_node::bitcoin::Network;
use ldk_node::lightning::types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};
use ldk_node::lightning_invoice::{
    Bolt11Invoice, Bolt11InvoiceDescription, Currency, Description, InvoiceBuilder,
};
use ldk_node::payment::PaymentDirection;
use ldk_node::Event;

const SAT: u64 = 1_000;

const CLIENT_SEED: u8 = 11;
/// The largest amount a test pays over the `with_channel` channel.
const MAX_TEST_PAY_MSAT: u64 = 30_000 * SAT;

/// Regtest, VSS, `fortis-lsp`, a started client `Node` and (optionally) the
/// harness `Payer`. Fields drop in order: client, payer, LSP, VSS, chain, dir.
struct Env {
    client: Node,
    payer: Option<Payer>,
    lsp: Lsp,
    vss: Option<Vss>,
    vss_url: String,
    /// `test_seed` is random per call: keep the client's for a restart.
    seed: [u8; 64],
    rt: Regtest,
    dirs: TempDir,
}

impl Env {
    fn start() -> Env {
        let rt = Regtest::start();
        let vss = Vss::start();
        let lsp = Lsp::start(&rt, &[]);
        let dirs = TempDir::new("pay");
        let vss_url = vss.url.clone();
        let seed = test_seed(CLIENT_SEED);
        let client = Node::new(client_config(seed, &rt, &lsp, &vss_url, &dirs)).unwrap();
        client.start().expect("client start against fortis-lsp");
        Env {
            client,
            payer: None,
            lsp,
            vss: Some(vss),
            vss_url,
            seed,
            rt,
            dirs,
        }
    }

    /// [`Env::start`], then the `Payer` pays the client's first (JIT)
    /// receive of 100 000 sat, so the client has ~99 000 sat outbound over
    /// the JIT channel and the `Payer` has inbound from the LSP.
    fn with_channel() -> Env {
        let mut env = Env::start();
        let payer = Payer::start(&env.rt, &env.lsp);
        let q = env.client.receive(100_000 * SAT, "first").expect("quote");
        assert!(q.needs_channel);
        payer.pay(&q.invoice);
        env.wait_event("PaymentReceived", 120, |e| {
            matches!(e, LnEvent::PaymentReceived { .. })
        });
        env.rt.mine(6);
        // What `pay` itself checks (`InsufficientLightning`): a usable
        // channel whose next HTLC covers the largest test payment. The
        // balance alone can read 99 000 while the channel is not usable yet.
        poll("client channel usable", 60, || {
            (env.client.balances().lightning_sat == 99_000
                && test_hooks::sendable_msat(&env.client) >= MAX_TEST_PAY_MSAT)
                .then_some(())
        });
        env.payer = Some(payer);
        env
    }

    fn payer(&self) -> &Payer {
        self.payer.as_ref().expect("env has a payer")
    }

    /// A new `Node` for the same wallet (seed, data dir, VSS).
    fn config(&self) -> NodeConfig {
        client_config(self.seed, &self.rt, &self.lsp, &self.vss_url, &self.dirs)
    }

    fn wait_event(&self, what: &str, secs: u64, pred: impl Fn(&LnEvent) -> bool) -> LnEvent {
        wait_ln_event(&self.client, what, secs, pred)
    }

    /// An invoice from the `Payer` (the recipient).
    fn payer_invoice(&self, amount_msat: u64, desc: &str, expiry_secs: u32) -> Bolt11Invoice {
        self.payer()
            .node
            .bolt11_payment()
            .receive(amount_msat, &direct(desc), expiry_secs)
            .expect("payer invoice")
    }
}

fn client_config(
    seed: [u8; 64],
    rt: &Regtest,
    lsp: &Lsp,
    vss_url: &str,
    dirs: &TempDir,
) -> NodeConfig {
    NodeConfig {
        bip39_seed: seed.to_vec(),
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
            url: vss_url.to_string(),
            auth_header: "Bearer test".into(),
        },
        limits: lsp.limits(),
    }
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

fn direct(desc: &str) -> Bolt11InvoiceDescription {
    Bolt11InvoiceDescription::Direct(Description::new(desc.to_string()).unwrap())
}

fn bolt11(inv: &Bolt11Invoice) -> PaymentTarget {
    PaymentTarget::Bolt11 {
        invoice: inv.to_string(),
        amount_msat: inv.amount_milli_satoshis(),
        description: String::new(),
        expiry_secs: inv.expiry_time().as_secs(),
    }
}

fn find(node: &Node, id: &str) -> Option<Payment> {
    node.payments().into_iter().find(|p| p.id == id)
}

fn outs(node: &Node) -> Vec<Payment> {
    node.payments()
        .into_iter()
        .filter(|p| p.direction == Direction::Out)
        .collect()
}

/// Waits for `PaymentSent { id }` (panics on `PaymentFailed { id }`);
/// returns its fee.
fn wait_sent(node: &Node, id: &str) -> u64 {
    let e = wait_ln_event(node, "PaymentSent", 120, |e| match e {
        LnEvent::PaymentSent { id: i, .. } => i == id,
        LnEvent::PaymentFailed { id: i, reason } if i == id => panic!("payment failed: {reason}"),
        _ => false,
    });
    match e {
        LnEvent::PaymentSent { fee_msat, .. } => fee_msat,
        _ => unreachable!(),
    }
}

/// The routing-fee cap ldk-node applies by default: 1 % + 50 sat.
fn fee_cap(amount_msat: u64) -> u64 {
    amount_msat / 100 + 50_000
}

/// A signed BOLT11 invoice from a throwaway key (for the target checks).
fn foreign_invoice(currency: Currency, amount_msat: Option<u64>) -> Bolt11Invoice {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[0x42; 32]).unwrap();
    let b = InvoiceBuilder::new(currency)
        .description("test".into())
        .payment_hash(sha256::Hash::from_slice(&[1; 32]).unwrap())
        .payment_secret(PaymentSecret([2; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .expiry_time(Duration::from_secs(3_600));
    let b = match amount_msat {
        Some(a) => b.amount_milli_satoshis(a),
        None => b,
    };
    b.build_signed(|m| secp.sign_ecdsa_recoverable(m, &key))
        .unwrap()
}

/// A `Node` that is never started (target checks come first).
fn unstarted_node(dirs: &TempDir) -> Node {
    Node::new(NodeConfig {
        bip39_seed: test_seed(CLIENT_SEED).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path("client"),
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
            min_payment_msat: 1_000,
            max_payment_msat: 1_000_000_000,
            channel_min_sat: 100_000,
            channel_max_sat: 500_000,
        },
    })
    .unwrap()
}

// --------------------------------------------------------------------------
// no binaries needed
// --------------------------------------------------------------------------

#[test]
fn wrong_network_invoice_rejected() {
    let dirs = TempDir::new("pay-wrongnet");
    let node = unstarted_node(&dirs);
    for currency in [
        Currency::Bitcoin,
        Currency::BitcoinTestnet,
        Currency::Signet,
    ] {
        let name = format!("{currency:?}");
        let inv = foreign_invoice(currency, Some(10_000 * SAT));
        assert_eq!(
            node.pay(&bolt11(&inv), None),
            Err(LnError::WrongNetwork),
            "{name}"
        );
    }
    // The node's own network is the reference, not what `parse` was told.
    let inv = foreign_invoice(Currency::Regtest, Some(10_000 * SAT));
    assert_eq!(
        node.pay(&bolt11(&inv), None),
        Err(LnError::Other("not_started".into()))
    );
}

#[test]
fn amount_mismatch_rejected() {
    let dirs = TempDir::new("pay-mismatch");
    let node = unstarted_node(&dirs);
    let inv = foreign_invoice(Currency::Regtest, Some(10_000 * SAT));
    for other in [10_000 * SAT + 1, 10_000 * SAT - 1, 1] {
        assert_eq!(
            node.pay(&bolt11(&inv), Some(other)),
            Err(LnError::Other("amount_mismatch".into()))
        );
    }
    // The same amount is not a mismatch.
    assert_eq!(
        node.pay(&bolt11(&inv), Some(10_000 * SAT)),
        Err(LnError::Other("not_started".into()))
    );
    // A no-amount invoice needs one.
    let open = foreign_invoice(Currency::Regtest, None);
    assert_eq!(
        node.pay(&bolt11(&open), None),
        Err(LnError::Other("amount_required".into()))
    );
    assert_eq!(
        node.pay(&bolt11(&open), Some(0)),
        Err(LnError::Other("amount_required".into()))
    );
}

#[test]
fn on_chain_target_is_not_payable_by_pay() {
    let dirs = TempDir::new("pay-onchain");
    let node = unstarted_node(&dirs);
    let t = PaymentTarget::OnChain {
        address: "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080".into(),
        amount_sat: Some(1_000),
    };
    assert_eq!(
        node.pay(&t, None),
        Err(LnError::Other("not_lightning".into()))
    );
    // A unified target pays its lightning part.
    let inv = foreign_invoice(Currency::Bitcoin, Some(10_000 * SAT));
    let u = PaymentTarget::Unified {
        address: "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080".into(),
        amount_sat: Some(10_000),
        lightning: Box::new(bolt11(&inv)),
    };
    assert_eq!(node.pay(&u, None), Err(LnError::WrongNetwork));
}

#[test]
fn estimate_fee_is_the_routing_fee_cap() {
    let dirs = TempDir::new("pay-estimate");
    let node = unstarted_node(&dirs);
    let inv = foreign_invoice(Currency::Regtest, Some(1_000_000 * SAT));
    // A fixed-amount invoice: its own amount counts, not the argument.
    assert_eq!(
        node.estimate_fee_msat(&bolt11(&inv), 1),
        fee_cap(1_000_000 * SAT)
    );
    let open = foreign_invoice(Currency::Regtest, None);
    assert_eq!(node.estimate_fee_msat(&bolt11(&open), 0), 50_000);
    assert_eq!(
        node.estimate_fee_msat(&bolt11(&open), 20_000 * SAT),
        fee_cap(20_000 * SAT)
    );
}

// --------------------------------------------------------------------------
// real stack
// --------------------------------------------------------------------------

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn pays_invoice_via_lsp_to_second_client() {
    let env = Env::with_channel();
    let amount = 30_000 * SAT;
    let inv = env.payer_invoice(amount, "coffee", 3_600);
    let t = Instant::now();
    let id = env.client.pay(&bolt11(&inv), None).expect("pay");
    eprintln!("pay returned after {:?}", t.elapsed());
    assert_eq!(id, hex::encode(inv.payment_hash().to_byte_array()));

    let fee = wait_sent(&env.client, &id);
    // The recipient got exactly the amount.
    let got = wait_event(&env.payer().node, "payer PaymentReceived", 60, |e| {
        matches!(e, Event::PaymentReceived { .. })
    });
    assert!(
        matches!(got, Event::PaymentReceived { amount_msat, .. } if amount_msat == amount),
        "{got:?}"
    );

    let p = poll("payment succeeded", 30, || {
        find(&env.client, &id).filter(|p| p.status == Status::Succeeded)
    });
    eprintln!("{p:?}");
    assert_eq!(p.direction, Direction::Out);
    assert_eq!(p.amount_msat, amount);
    assert_eq!(p.fee_msat, fee, "fee in history = fee in PaymentSent");
    assert!(fee > 0, "the LSP charges a forwarding fee");
    assert!(fee <= env.client.estimate_fee_msat(&bolt11(&inv), amount));
    assert_eq!(p.description, "coffee");
    assert!(p.timestamp > 1_700_000_000);
    poll("balance down by amount + fee", 30, || {
        (env.client.balances().lightning_sat == (99_000 * SAT - amount - fee) / SAT).then_some(())
    });

    // Paying it again is refused, not paid twice.
    assert!(env.client.pay(&bolt11(&inv), None).is_err());
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn no_amount_invoice_requires_amount_param() {
    let env = Env::with_channel();
    let inv = env
        .payer()
        .node
        .bolt11_payment()
        .receive_variable_amount(&direct("tip"), 3_600)
        .unwrap();
    assert_eq!(inv.amount_milli_satoshis(), None);
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::Other("amount_required".into()))
    );
    assert!(outs(&env.client).is_empty(), "nothing sent");

    let amount = 20_000 * SAT;
    let id = env.client.pay(&bolt11(&inv), Some(amount)).expect("pay");
    let fee = wait_sent(&env.client, &id);
    let p = poll("payment succeeded", 30, || {
        find(&env.client, &id).filter(|p| p.status == Status::Succeeded)
    });
    assert_eq!((p.amount_msat, p.fee_msat), (amount, fee));
    assert_eq!(p.description, "tip");
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn insufficient_lightning_balance_error() {
    let env = Env::with_channel();
    // 99 000 sat in the channel, less the reserve: 99 000 sat is too much.
    let inv = env.payer_invoice(99_000 * SAT, "too much", 3_600);
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::InsufficientLightning)
    );
    assert!(outs(&env.client).is_empty(), "nothing sent");
    assert_eq!(env.client.balances().lightning_sat, 99_000);
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn expired_invoice_error() {
    let mut env = Env::start();
    env.payer = Some(Payer::start(&env.rt, &env.lsp));
    let inv = env.payer_invoice(20_000 * SAT, "short", 1);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::InvoiceExpired)
    );
    assert!(outs(&env.client).is_empty());
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn pay_with_vss_down_returns_backup_unavailable_and_sends_nothing() {
    let env = Env::with_channel();
    let inv = env.payer_invoice(20_000 * SAT, "no backup", 3_600);
    let hash = PaymentHash(inv.payment_hash().to_byte_array());
    // VSS stops answering (the process is frozen, connections hang).
    env.vss.as_ref().unwrap().pause();

    // Target checks still come first (no VSS round trip for those).
    let wrong = foreign_invoice(Currency::Bitcoin, Some(20_000 * SAT));
    assert_eq!(
        env.client.pay(&bolt11(&wrong), None),
        Err(LnError::WrongNetwork)
    );

    let t = Instant::now();
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::BackupUnavailable)
    );
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

    // Nothing left the client, nothing reached the recipient.
    assert!(outs(&env.client).is_empty(), "{:?}", env.client.payments());
    std::thread::sleep(Duration::from_secs(5));
    let payer = &env.payer().node;
    while let Some(e) = payer.next_event() {
        payer.event_handled().unwrap();
        assert!(
            !matches!(
                e,
                Event::PaymentClaimable { .. } | Event::PaymentReceived { .. }
            ),
            "recipient saw {e:?}"
        );
    }
    let p = payer
        .list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound)
        .into_iter()
        .find(|p| matches!(p.kind, ldk_node::payment::PaymentKind::Bolt11 { hash: h, .. } if h == hash))
        .expect("the recipient's invoice is stored");
    assert_eq!(p.status, ldk_node::payment::PaymentStatus::Pending);
    // (Not `balances()` here: ldk-node blocks it while a wallet write to
    // the unreachable VSS is retried — see the Task 6 report.)

    // Back up: nothing was spent, and the same invoice pays.
    env.vss.as_ref().unwrap().resume();
    assert_eq!(env.client.balances().lightning_sat, 99_000);
    let id = env
        .client
        .pay(&bolt11(&inv), None)
        .expect("pay with vss back");
    wait_sent(&env.client, &id);
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn paying_own_invoice_errors_immediately() {
    let env = Env::start();
    let q = env.client.receive(100_000 * SAT, "mine").expect("quote");
    let inv = Bolt11Invoice::from_str(&q.invoice).unwrap();
    let t = Instant::now();
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::Other("pay_self".into()))
    );
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert!(outs(&env.client).is_empty());
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn payments_lists_both_directions() {
    let env = Env::with_channel();
    let inv = env.payer_invoice(20_000 * SAT, "lunch", 3_600);
    // Distinct timestamps for the ordering check.
    std::thread::sleep(Duration::from_millis(1_100));
    let id = env.client.pay(&bolt11(&inv), None).expect("pay");
    let fee = wait_sent(&env.client, &id);

    let list = poll("both payments settled", 30, || {
        let l = env.client.payments();
        (l.len() == 2 && l.iter().all(|p| p.status == Status::Succeeded)).then_some(l)
    });
    eprintln!("{list:#?}");
    let (out, inp) = (&list[0], &list[1]);
    assert!(out.timestamp >= inp.timestamp, "most recent first");
    assert_eq!(out.id, id);
    assert_eq!(out.direction, Direction::Out);
    assert_eq!((out.amount_msat, out.fee_msat), (20_000 * SAT, fee));
    assert_eq!(out.description, "lunch");
    // The JIT receive: net amount, the LSP's opening fee as fee.
    assert_eq!(inp.direction, Direction::In);
    assert_eq!(inp.amount_msat, 99_000 * SAT);
    assert_eq!(inp.fee_msat, 1_000 * SAT);
    assert_eq!(inp.description, "first");
    assert_ne!(inp.id, out.id);
}

/// Review Focus 1: an HTLC held by the recipient survives the payer's
/// `stop()` and a restart from the same data dir and seed; it is `Pending`
/// throughout and `Succeeded` once released, never `Failed`.
#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn payment_in_flight_survives_stop_and_restart() {
    let env = Env::with_channel();
    let payer = &env.payer().node;
    let amount = 25_000 * SAT;
    let preimage = PaymentPreimage([7; 32]);
    let hash = PaymentHash(sha256::Hash::hash(&preimage.0).to_byte_array());
    let inv = payer
        .bolt11_payment()
        .receive_for_hash(amount, &direct("held"), 3_600, hash)
        .unwrap();

    let id = env.client.pay(&bolt11(&inv), None).expect("pay");
    let claimable = match wait_event(
        payer,
        "PaymentClaimable",
        60,
        |e| matches!(e, Event::PaymentClaimable { payment_hash, .. } if *payment_hash == hash),
    ) {
        Event::PaymentClaimable {
            claimable_amount_msat,
            ..
        } => claimable_amount_msat,
        _ => unreachable!(),
    };
    assert_eq!(claimable, amount);
    assert_eq!(
        find(&env.client, &id).map(|p| p.status),
        Some(Status::Pending)
    );

    let t = Instant::now();
    env.client.stop();
    let stopped_in = t.elapsed();
    eprintln!("stop() took {stopped_in:?}");
    assert!(stopped_in < Duration::from_secs(10), "{stopped_in:?}");

    let client = Node::new(env.config()).unwrap();
    client.start().expect("restart from the same data dir");
    let p = find(&client, &id).expect("payment survives the restart");
    assert_eq!(p.status, Status::Pending, "{p:?}");
    assert_eq!(p.amount_msat, amount);
    assert_eq!(p.description, "held");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(find(&client, &id).map(|p| p.status), Some(Status::Pending));

    payer
        .bolt11_payment()
        .claim_for_hash(hash, claimable, preimage)
        .expect("release the HTLC");
    let fee = wait_sent(&client, &id);
    let p = poll("succeeded after release", 60, || {
        let p = find(&client, &id).expect("still listed");
        assert_ne!(p.status, Status::Failed, "{p:?}");
        (p.status == Status::Succeeded).then_some(p)
    });
    assert_eq!(p.fee_msat, fee);
    assert_eq!(p.amount_msat, amount);
    client.stop();
}

/// `pay` goes through the attempt record (wiring): an unknown-outcome
/// record for the invoice blocks it, an unreadable record file refuses it
/// (and is left alone), and a real send writes and then clears the record.
#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn pay_honours_the_attempt_record() {
    let env = Env::with_channel();
    let inv = env.payer_invoice(20_000 * SAT, "guarded", 3_600);
    let id = hex::encode(inv.payment_hash().to_byte_array());
    let file = env.dirs.path("client").join("fortis-payment-attempts.json");
    let far = 4_000_000_000u64;

    std::fs::write(&file, format!("{{\"{id}\":{far}}}")).unwrap();
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::PaymentStateUnknown)
    );

    std::fs::write(&file, b"not json").unwrap();
    assert_eq!(
        env.client.pay(&bolt11(&inv), None),
        Err(LnError::Other("attempt_record_unreadable".into()))
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"not json");
    assert!(outs(&env.client).is_empty(), "nothing sent");

    std::fs::remove_file(&file).unwrap();
    let paid = env.client.pay(&bolt11(&inv), None).expect("pay");
    assert_eq!(paid, id);
    wait_sent(&env.client, &id);
    // Written before the send (the file exists again), cleared after.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
}
