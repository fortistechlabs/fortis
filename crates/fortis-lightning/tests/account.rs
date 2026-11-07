//! The Fortis account (spec §7) against the real `fortis-lsp`: small amounts
//! received into and paid from the account, Fortis-to-Fortis inside the
//! ledger, conversion into the user's own channel over the cap, and the
//! custody limit.
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test account -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::{poll, test_seed, wait_event, Lsp, Payer, Regtest, TempDir, Vss};
use fortis_lightning::{ChainSource, Node, NodeConfig, VssConfig};
use ldk_node::bitcoin::Network;
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::Event;
use serde_json::{json, Value};

const SAT: u64 = 1_000;

fn client(rt: &Regtest, lsp: &Lsp, vss: &Vss, dirs: &TempDir, tag: &str, seed: u8) -> Node {
    let node = Node::new(NodeConfig {
        chain: fortis_lightning::LnChain::Btc,
        bip39_seed: test_seed(seed).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path(tag),
        chain_source: ChainSource::BitcoindRpc {
            host: "127.0.0.1".into(),
            port: rt.rpc_port,
            user: "fortis".into(),
            password: "fortis".into(),
        },
        lsp: lsp.lsp_info(),
        vss: VssConfig { url: vss.url.clone(), auth_header: "Bearer test".into() },
        limits: lsp.limits(),
    })
    .unwrap();
    node.start().expect("client start");
    node
}

/// Calls `op` on `node`'s account, signed by its node key. Status and body.
fn account(lsp: &Lsp, node: &Node, op: &str, params: Value) -> (u16, Value) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let nonce = format!("test-nonce-{:020}", N.fetch_add(1, Ordering::SeqCst));
    let body = json!({
        "node_id": node.node_id(),
        "op": op,
        "params": params,
        "nonce": nonce,
        "expires_at": now + 120,
    })
    .to_string();
    let sig = node.sign_account_request(&body).unwrap();
    let url = lsp.metrics_url.replace("/metrics", "/account");
    match ureq::post(&url).set("X-Fortis-Sig", &sig).send_string(&body) {
        Ok(r) => (r.status(), serde_json::from_str(&r.into_string().unwrap()).unwrap()),
        Err(ureq::Error::Status(s, r)) => (s, serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or(Value::Null)),
        Err(e) => panic!("account {op}: {e}"),
    }
}

fn balance_msat(lsp: &Lsp, node: &Node) -> u64 {
    let (s, v) = account(lsp, node, "balance", json!({}));
    assert_eq!(s, 200, "{v}");
    v["balance_msat"].as_u64().unwrap()
}

fn payer_invoice(payer: &Payer, amount_msat: u64) -> Bolt11Invoice {
    payer
        .node
        .bolt11_payment()
        .receive(amount_msat, &Bolt11InvoiceDescription::Direct(Description::new("t".into()).unwrap()), 3600)
        .unwrap()
}

#[test]
#[ignore = "needs fortis-lsp built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn small_amounts_live_in_the_account_and_convert_over_the_cap() {
    let rt = Regtest::start();
    let vss = Vss::start();
    let lsp = Lsp::start(&rt, &[("custody_max_sat", "1000000"), ("account_max_sat", "50000")]);
    let dirs = TempDir::new("account");
    let a = client(&rt, &lsp, &vss, &dirs, "a", 0x41);
    let b = client(&rt, &lsp, &vss, &dirs, "b", 0x42);
    let payer = Payer::start(&rt, &lsp);
    // The LSP's own liquidity towards the outside (on mainnet: its channel to
    // a routing node), for account sends.
    payer.give_lsp_liquidity(&rt, &lsp, 500_000, 300_000);

    // 1. 100 sat into a brand-new wallet's account.
    let (s, v) = account(&lsp, &a, "invoice", json!({ "amount_msat": 100 * SAT, "description": "tip" }));
    assert_eq!(s, 200, "{v}");
    payer.pay(v["invoice"].as_str().unwrap());
    poll("100 sat credited", 30, || (balance_msat(&lsp, &a) == 100 * SAT).then_some(()));
    assert!(lsp.metrics().contains("\nfortis_lsp_custody_sat 100\n"), "{}", lsp.metrics());

    // 2. Pay 30 sat out of the account to an outside node.
    let inv = payer_invoice(&payer, 30 * SAT);
    let (s, v) = account(&lsp, &a, "pay", json!({ "invoice": inv.to_string() }));
    assert_eq!((s, v["internal"].as_bool()), (200, Some(false)), "{v}\n{}", lsp.stderr());
    let got = wait_event(&payer.node, "payer receives 30 sat", 60, |e| matches!(e, Event::PaymentReceived { .. }));
    assert!(matches!(got, Event::PaymentReceived { amount_msat, .. } if amount_msat == 30 * SAT), "{got:?}");
    poll("send settled", 30, || {
        let (_, v) = account(&lsp, &a, "balance", json!({}));
        (v["held_msat"] == 0).then_some(v["balance_msat"].as_u64().unwrap())
    });
    let after_send = balance_msat(&lsp, &a);
    assert!(after_send <= 70 * SAT && after_send >= 70 * SAT - 1_000, "70 sat less a free LSP forward: {after_send}");

    // 3. Fortis to Fortis: B's account invoice paid from A's account.
    let (_, v) = account(&lsp, &b, "invoice", json!({ "amount_msat": 40 * SAT }));
    let (s, v) = account(&lsp, &a, "pay", json!({ "invoice": v["invoice"] }));
    assert_eq!((s, v["internal"].as_bool(), v["fee_msat"].as_u64()), (200, Some(true), Some(0)), "{v}");
    assert_eq!(balance_msat(&lsp, &a), after_send - 40 * SAT);
    assert_eq!(balance_msat(&lsp, &b), 40 * SAT);

    // 4. Over the 50,000 sat cap: A's account moves into its own channel.
    let (_, v) = account(&lsp, &a, "invoice", json!({ "amount_msat": 55_000 * SAT }));
    payer.pay(v["invoice"].as_str().unwrap());
    let held = poll("55,000 sat credited", 30, || {
        let bal = balance_msat(&lsp, &a);
        (bal > 50_000 * SAT).then_some(bal)
    });
    let fee = (held / 100).max(1_000 * SAT);
    poll("conversion channel pending", 120, || {
        a.balances();
        (lsp.stderr().contains("account conversion for")).then_some(())
    });
    eprintln!("{}", lsp.stderr().lines().filter(|l| l.contains("conversion")).collect::<Vec<_>>().join("\n"));
    rt.mine(6);
    poll("account moved into A's channel", 180, || {
        (balance_msat(&lsp, &a) == 0 && a.balances().lightning_sat > 0).then_some(())
    });
    let ln = a.balances().lightning_sat;
    let moved_sat = (held - fee) / SAT;
    assert!(ln + 2 >= moved_sat && ln <= moved_sat, "A's channel holds ~{moved_sat} sat, has {ln}");
    let (_, h) = account(&lsp, &a, "history", json!({ "limit": 1 }));
    assert_eq!((h[0]["kind"].as_str(), h[0]["fee_msat"].as_u64()), (Some("converted"), Some(fee)), "{h}");

    // 5. The total custody limit: B can't take 999,990 sat more.
    let (s, v) = account(&lsp, &b, "invoice", json!({ "amount_msat": 999_990 * SAT }));
    assert_eq!((s, v["error"].as_str()), (409, Some("custody_full")), "{v}");

    // 6. Out of the account to an on-chain address: the LSP pays it from its wallet.
    let (_, v) = account(&lsp, &b, "invoice", json!({ "amount_msat": 20_000 * SAT }));
    payer.pay(v["invoice"].as_str().unwrap());
    poll("20,000 sat credited", 30, || (balance_msat(&lsp, &b) == 20_040 * SAT).then_some(()));
    let to = rt.cli(&["-rpcwallet=miner", "getnewaddress"]);
    let (s, v) = account(&lsp, &b, "withdraw", json!({ "amount_msat": 15_000 * SAT, "address": to, "feerate_sat_vb": 2 }));
    assert_eq!((s, v["payout_sat"].as_u64(), v["fee_sat"].as_u64()), (200, Some(14_000), Some(1_000)), "{v}\n{}", lsp.stderr());
    assert_eq!(balance_msat(&lsp, &b), 5_040 * SAT);
    // ldk-node broadcasts in the background: mine until it is in a block.
    poll("withdrawal confirmed", 60, || {
        rt.mine(1);
        let got = rt.cli(&["-rpcwallet=miner", "getreceivedbyaddress", &to, "1"]);
        (got.trim().parse::<f64>().unwrap_or(0.0) == 0.00014).then_some(())
    });
    // 8. Held payments into A's own channel (A has one since step 4).
    // 8a. A is offline when paid: the LSP holds it and delivers on reconnect.
    let (s, v) = account(&lsp, &a, "channel_invoice", json!({ "amount_msat": 5_000 * SAT }));
    assert_eq!(s, 200, "{v}");
    let inv: Bolt11Invoice = v["invoice"].as_str().unwrap().parse().unwrap();
    let before_ln = a.balances().lightning_sat;
    a.stop();
    let id = payer.node.bolt11_payment().send(&inv, None).expect("payer sends");
    poll("LSP holds it", 60, || lsp.stderr().contains("holding 5000000 msat").then_some(()));
    std::thread::sleep(std::time::Duration::from_secs(12));
    assert!(!lsp.stderr().contains("delivering held"), "not delivered while A is offline");
    a.start().expect("A back online");
    poll("held payment delivered", 90, || {
        (a.balances().lightning_sat >= before_ln + 5_000).then_some(())
    });
    wait_event(&payer.node, "payer's held payment settles", 60, |e| {
        matches!(e, Event::PaymentSuccessful { payment_id, .. } if *payment_id == Some(id))
    });
    assert!(lsp.stderr().contains("held payment delivered"), "{}", lsp.stderr());

    // 8b. A stays offline until the payment would time out: it lands on A's
    // Fortis balance instead, and the payer still succeeds.
    let (_, v) = account(&lsp, &a, "channel_invoice", json!({ "amount_msat": 2_000 * SAT }));
    let inv: Bolt11Invoice = v["invoice"].as_str().unwrap().parse().unwrap();
    let acct_before = balance_msat(&lsp, &a);
    a.stop();
    let id = payer.node.bolt11_payment().send(&inv, None).expect("payer sends");
    poll("LSP holds it", 60, || lsp.stderr().matches("holding 2000000 msat").count().ge(&1).then_some(()));
    poll("settled into the account near the timeout", 300, || {
        rt.mine(2);
        std::thread::sleep(std::time::Duration::from_secs(3));
        let e = lsp.stderr();
        let tail: Vec<_> = e.lines().filter(|l| l.contains("held") || l.contains("holding") || l.contains("moving") || l.contains("deliver") || l.contains("did not")).collect();
        eprintln!("height {} | {}", rt.height(), tail.iter().rev().take(3).cloned().collect::<Vec<_>>().join(" || "));
        e.contains("did not connect in time").then_some(())
    });
    wait_event(&payer.node, "payer's payment settles", 60, |e| {
        matches!(e, Event::PaymentSuccessful { payment_id, .. } if *payment_id == Some(id))
    });
    let ln_before = a.balances().lightning_sat;
    assert!(lsp.stderr().contains("kept on its Fortis balance"), "{}", lsp.stderr());
    // Back online: the balance moves into A's own channel by itself, free.
    a.start().expect("A back online");
    poll("balance moved into A's channel", 90, || {
        (balance_msat(&lsp, &a) == acct_before && a.balances().lightning_sat >= ln_before + 2_000).then_some(())
    });
    let (_, h) = account(&lsp, &a, "history", json!({ "limit": 1 }));
    assert_eq!((h[0]["kind"].as_str(), h[0]["fee_msat"].as_u64()), (Some("converted"), Some(0)), "{h}");

    // (The reusable BOLT12 offer is not covered here: an offer needs the LSP
    // to be in the network graph as an announced node, which this regtest
    // LSP is not. Unit-tested in account_api.rs; checked on mainnet.)
}
