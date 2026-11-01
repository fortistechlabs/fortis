//! `Node::receive` against the real `fortis-lsp`: JIT quote, JIT channel on
//! first payment, plain invoice once a channel has inbound capacity, range
//! checks. Needs the binary built and the regtest stack:
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test receive -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::str::FromStr;
use std::time::Duration;

use common::{poll, test_seed, Lsp, Payer, Regtest, TempDir, Vss};
use fortis_lightning::{
    ChainSource, LnError, LnEvent, LspInfo, LspLimits, Node, NodeConfig, ReceiveQuote, VssConfig,
};
use ldk_node::bitcoin::Network;
use ldk_node::lightning_invoice::Bolt11Invoice;

const SAT: u64 = 1_000;

/// Regtest, VSS, `fortis-lsp` and a started client `Node`. Fields drop in
/// order: client first, the temp dir last.
struct Env {
    client: Node,
    lsp: Lsp,
    _vss: Vss,
    rt: Regtest,
    _dirs: TempDir,
}

impl Env {
    fn start(lsp_overrides: &[(&str, &str)]) -> Env {
        let rt = Regtest::start();
        let vss = Vss::start();
        let lsp = Lsp::start(&rt, lsp_overrides);
        let dirs = TempDir::new("receive");
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
            limits: lsp.limits(),
        })
        .unwrap();
        client.start().expect("client start against fortis-lsp");
        Env {
            client,
            lsp,
            _vss: vss,
            rt,
            _dirs: dirs,
        }
    }

    /// The LSPS2 opening fee `fortis-lsp` charges for `amount_msat`, from its
    /// `/info` (`open_fee_ppm`, `open_fee_min_sat`): max(⌈P·ppm/10⁶⌉, floor).
    fn expected_fee_msat(&self, amount_msat: u64) -> u64 {
        let ppm = self.lsp.info["open_fee_ppm"].as_u64().unwrap();
        let floor_msat = self.lsp.info["open_fee_min_sat"].as_u64().unwrap() * SAT;
        (amount_msat * ppm).div_ceil(1_000_000).max(floor_msat)
    }

    fn floor_msat(&self) -> u64 {
        self.lsp.info["open_fee_min_sat"].as_u64().unwrap() * SAT
    }

    /// Waits for the client's next event matching `pred`, skipping others.
    fn wait_event(&self, what: &str, secs: u64, pred: impl Fn(&LnEvent) -> bool) -> LnEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            if let Some(e) = self.client.next_event(left.min(Duration::from_secs(1))) {
                if pred(&e) {
                    return e;
                }
                eprintln!("({what}: skipping {e:?})");
            }
        }
    }
}

fn invoice_amount_msat(q: &ReceiveQuote) -> Option<u64> {
    Bolt11Invoice::from_str(&q.invoice)
        .expect("quote invoice is bolt11")
        .amount_milli_satoshis()
}

/// The first receive: JIT quote with fee, then the paid invoice opens the
/// channel. Returns the quote.
fn first_receive(env: &Env, payer: &Payer, amount_msat: u64) -> ReceiveQuote {
    let q = env
        .client
        .receive(amount_msat, "first")
        .expect("receive quote");
    assert!(q.needs_channel, "no channel yet: {q:?}");
    assert_eq!(q.amount_msat, amount_msat);
    assert_eq!(q.fee_msat, env.expected_fee_msat(amount_msat), "{q:?}");
    assert_eq!(
        invoice_amount_msat(&q),
        Some(amount_msat),
        "payer pays the full amount"
    );

    payer.pay(&q.invoice);
    env.wait_event("ChannelReady", 120, |e| matches!(e, LnEvent::ChannelReady));
    let got = env.wait_event("PaymentReceived", 120, |e| {
        matches!(e, LnEvent::PaymentReceived { .. })
    });
    assert!(
        matches!(got, LnEvent::PaymentReceived { amount_msat: a, .. } if a == amount_msat - q.fee_msat),
        "the client receives the amount less the LSP fee: {got:?}"
    );
    poll("lightning balance = amount - fee", 60, || {
        (env.client.balances().lightning_sat == (amount_msat - q.fee_msat) / SAT).then_some(())
    });
    q
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn first_receive_quotes_fee_and_opens_jit_channel() {
    let env = Env::start(&[("token", "\"receive-token\"")]);
    let payer = Payer::start(&env.rt, &env.lsp);
    let amount = 100_000 * SAT;
    let q = first_receive(&env, &payer, amount);
    assert_eq!(
        q.fee_msat,
        1_000 * SAT,
        "1% of 100 000 sat = the 1 000 sat floor"
    );
    assert_eq!(env.client.balances().lightning_sat, 99_000);
    assert!(env.client.balances().inbound_sat > 0);
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn second_receive_within_capacity_has_zero_fee() {
    let env = Env::start(&[]);
    let payer = Payer::start(&env.rt, &env.lsp);
    let first = 100_000 * SAT;
    let q1 = first_receive(&env, &payer, first);
    // The JIT funding is broadcast once the client claimed; confirm it.
    env.rt.mine(6);

    let inbound_sat = env.client.balances().inbound_sat;
    let second = 60_000 * SAT;
    assert!(
        inbound_sat * SAT > second,
        "the JIT channel leaves room for the second receive: inbound {inbound_sat} sat"
    );
    let q = env.client.receive(second, "second").expect("second quote");
    assert!(!q.needs_channel, "{q:?}");
    assert_eq!(q.fee_msat, 0);
    assert_eq!(q.amount_msat, second);
    assert_eq!(invoice_amount_msat(&q), Some(second));

    payer.pay(&q.invoice);
    let got = env.wait_event("second PaymentReceived", 120, |e| {
        matches!(e, LnEvent::PaymentReceived { .. })
    });
    assert!(
        matches!(got, LnEvent::PaymentReceived { amount_msat, .. } if amount_msat == second),
        "{got:?}"
    );
    let expect_sat = (first - q1.fee_msat + second) / SAT;
    poll("lightning balance after the second receive", 60, || {
        (env.client.balances().lightning_sat == expect_sat).then_some(())
    });
}

/// The client's reported inbound over the JIT channel is nominal: the LSP
/// funded it and pays its commitment fee and anchors out of that, so an
/// amount at (or just under) the reported inbound cannot be forwarded over
/// it. Such a receive must be quoted as JIT (and that quote be payable),
/// never as a plain invoice that then fails.
#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn receive_at_reported_inbound_needs_channel() {
    let env = Env::start(&[]);
    let payer = Payer::start(&env.rt, &env.lsp);
    first_receive(&env, &payer, 100_000 * SAT);
    env.rt.mine(6);

    let inbound_msat = env.client.balances().inbound_sat * SAT;
    eprintln!("reported inbound after the first JIT channel: {inbound_msat} msat");
    for amount in [inbound_msat, inbound_msat - 2_000 * SAT] {
        let q = env.client.receive(amount, "boundary").expect("quote");
        assert!(
            q.needs_channel,
            "{amount} msat against {inbound_msat} msat reported inbound must go JIT: {q:?}"
        );
        assert_eq!(q.fee_msat, env.expected_fee_msat(amount), "{q:?}");
    }

    // The JIT quote at the reported inbound is payable: a second JIT channel.
    let before = env.client.balances().lightning_sat;
    let q = env
        .client
        .receive(inbound_msat, "boundary paid")
        .expect("quote");
    payer.pay(&q.invoice);
    let got = env.wait_event("boundary PaymentReceived", 120, |e| {
        matches!(e, LnEvent::PaymentReceived { .. })
    });
    assert!(
        matches!(got, LnEvent::PaymentReceived { amount_msat, .. } if amount_msat == inbound_msat - q.fee_msat),
        "{got:?}"
    );
    poll("balance after the boundary receive", 60, || {
        (env.client.balances().lightning_sat == before + (inbound_msat - q.fee_msat) / SAT)
            .then_some(())
    });
}

/// The LSP's payment bounds are LSPS2 (JIT) bounds: over an existing
/// channel a receive below the LSP minimum is a plain, fee-free invoice.
#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn plain_receive_below_lsp_min_over_existing_channel() {
    let env = Env::start(&[]);
    let payer = Payer::start(&env.rt, &env.lsp);
    let q1 = first_receive(&env, &payer, 100_000 * SAT);
    env.rt.mine(6);

    let small = 10_000 * SAT;
    assert!(small < env.lsp.limits().min_payment_msat);
    let q = env
        .client
        .receive(small, "small")
        .expect("plain quote below the LSP min");
    assert!(!q.needs_channel, "{q:?}");
    assert_eq!(q.fee_msat, 0);
    assert_eq!(invoice_amount_msat(&q), Some(small));
    payer.pay(&q.invoice);
    let got = env.wait_event("small PaymentReceived", 120, |e| {
        matches!(e, LnEvent::PaymentReceived { .. })
    });
    assert!(
        matches!(got, LnEvent::PaymentReceived { amount_msat, .. } if amount_msat == small),
        "{got:?}"
    );
    let expect_sat = (100_000 * SAT - q1.fee_msat + small) / SAT;
    poll("balance after the small receive", 60, || {
        (env.client.balances().lightning_sat == expect_sat).then_some(())
    });
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn below_min_is_out_of_range() {
    let env = Env::start(&[]);
    let limits = env.lsp.limits();
    assert_eq!(env.client.limits(), limits);
    let out = Err(LnError::AmountOutOfRange {
        min_msat: limits.min_payment_msat,
        max_msat: limits.max_payment_msat,
    });
    assert_eq!(env.client.receive(limits.min_payment_msat - 1, "low"), out);
    assert_eq!(env.client.receive(limits.max_payment_msat + 1, "high"), out);
    assert_eq!(env.client.receive(0, "zero"), out);
    // The bounds themselves are accepted.
    let lo = env
        .client
        .receive(limits.min_payment_msat, "min")
        .expect("min");
    let hi = env
        .client
        .receive(limits.max_payment_msat, "max")
        .expect("max");
    assert!(lo.needs_channel && hi.needs_channel);
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn fee_floor_applies() {
    let env = Env::start(&[]);
    let amount = 51_000 * SAT;
    let q = env.client.receive(amount, "floor").expect("quote");
    let pct = (amount * env.lsp.info["open_fee_ppm"].as_u64().unwrap()).div_ceil(1_000_000);
    assert!(
        env.floor_msat() > pct,
        "the floor beats the percentage here"
    );
    assert!(q.needs_channel);
    assert_eq!(q.fee_msat, env.floor_msat());
    assert_eq!(q.fee_msat, 1_000 * SAT);
    assert_eq!(invoice_amount_msat(&q), Some(amount));
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn fee_percentage_applies() {
    let env = Env::start(&[]);
    let amount = 250_000 * SAT;
    let q = env.client.receive(amount, "percentage").expect("quote");
    assert!(
        env.expected_fee_msat(amount) > env.floor_msat(),
        "the percentage beats the floor"
    );
    assert!(q.needs_channel);
    assert_eq!(q.fee_msat, env.expected_fee_msat(amount));
    assert_eq!(q.fee_msat, 2_500 * SAT);
    assert_eq!(invoice_amount_msat(&q), Some(amount));
}

#[test]
fn receive_before_start_errors() {
    let dirs = TempDir::new("receive-unstarted");
    let node = Node::new(NodeConfig {
        bip39_seed: test_seed(3).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path("client"),
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
            min_payment_msat: 1_000,
            max_payment_msat: 1_000_000_000,
            channel_min_sat: 100_000,
            channel_max_sat: 500_000,
        },
    })
    .unwrap();
    assert_eq!(
        node.receive(100_000 * SAT, "x"),
        Err(LnError::Other("not_started".into()))
    );
}
