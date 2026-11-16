//! `POST /admin/send`: an exact on-chain payment from the real `fortis-lsp` binary's wallet, on
//! regtest, with its guard rails (network check, amount bounds) refusing before anything moves.
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-lightning --test admin_send -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use common::{builder, poll, test_seed, Lsp, Regtest, TempDir};
use serde_json::Value;

/// `POST <metrics>/admin/send` with the LSP's admin token: status and JSON body.
fn admin_send(lsp: &Lsp, body: &str) -> (u16, Value) {
    let token = std::fs::read_to_string(lsp.dir().join("data").join("admin.token"))
        .expect("admin.token")
        .trim()
        .to_string();
    let url = lsp.metrics_url.replace("/metrics", "/admin/send");
    let r = ureq::post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(body);
    let (status, text) = match r {
        Ok(resp) => (resp.status(), resp.into_string().unwrap()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap()),
        Err(e) => panic!("POST {url}: {e}"),
    };
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[test]
#[ignore = "needs fortis-lsp built and FORTIS_BTC_BITCOIND"]
fn the_lsp_sends_an_exact_amount_on_chain_and_refuses_bad_requests() {
    const SEND_SAT: u64 = 100_000;
    let rt = Regtest::start();

    // A stock ldk-node as the receiver: its on-chain address, and a balance we can watch.
    let dir = TempDir::new("receiver");
    let (b, _addr) = builder(&rt, test_seed(0x71), &dir.str("receiver"));
    let receiver = b.build_with_fs_store().unwrap();
    receiver.start().unwrap();
    let address = receiver.onchain_payment().new_address().unwrap().to_string();
    let balance = |n: &ldk_node::Node| {
        n.sync_wallets().ok();
        n.list_balances().total_onchain_balance_sats
    };
    assert_eq!(balance(&receiver), 0);

    let lsp = Lsp::start(&rt, &[]);
    let lsp_before = lsp.metric("fortis_lsp_onchain_sat");

    // Refused before anything moves: another network's address, out-of-bounds amounts, junk.
    let mainnet = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    let (s, v) = admin_send(&lsp, &format!("{{\"address\":\"{mainnet}\",\"amount_sat\":{SEND_SAT}}}"));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("another network"), "{v}");
    for body in [
        format!("{{\"address\":\"{address}\",\"amount_sat\":999}}"),
        format!("{{\"address\":\"{address}\",\"amount_sat\":1000001}}"),
        format!("{{\"address\":\"{address}\",\"amount_sat\":\"all\"}}"),
        "{\"address\":\"not-an-address\",\"amount_sat\":50000}".to_string(),
    ] {
        let (s, v) = admin_send(&lsp, &body);
        assert_eq!(s, 400, "{body} -> {v}");
    }
    rt.mine(1);
    assert_eq!(balance(&receiver), 0, "a refused request must not move funds");
    assert_eq!(lsp.metric("fortis_lsp_onchain_sat"), lsp_before);

    // The real send: exactly SEND_SAT arrives, and the LSP's balance drops by that plus a fee.
    let (s, v) = admin_send(&lsp, &format!("{{\"address\":\"{address}\",\"amount_sat\":{SEND_SAT}}}"));
    assert_eq!(s, 200, "{v}\n{}", lsp.stderr());
    assert_eq!(v["txid"].as_str().map(str::len), Some(64), "{v}");
    rt.mine(1);
    let got = poll("the receiver sees the payment", 90, || {
        let b = balance(&receiver);
        (b > 0).then_some(b)
    });
    assert_eq!(got, SEND_SAT, "exactly the requested amount arrives");
    let lsp_after = poll("the LSP wallet reflects the send", 90, || {
        let m = lsp.metric("fortis_lsp_onchain_sat");
        (m < lsp_before).then_some(m)
    });
    let spent = lsp_before - lsp_after;
    assert!(
        (SEND_SAT..SEND_SAT + 5_000).contains(&spent),
        "the LSP lost {spent} sat for a {SEND_SAT} sat send: amount plus a small fee"
    );
    assert!(lsp.stderr().contains("admin send of 100000 sat"), "{}", lsp.stderr());
    receiver.stop().unwrap();
}
