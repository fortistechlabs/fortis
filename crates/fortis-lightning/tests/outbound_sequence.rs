//! `[[outbound]]` channels open one at a time, and a channel whose funding never confirmed
//! can be abandoned (`POST /admin/abandon`) — the real `fortis-lsp` binary on regtest.
//!
//! Why: on 2026-11-15 the XBT LSP opened two outbound channels in the same second. Both
//! funding transactions spent the same coin, the second was rejected as a double spend, and
//! its channel could never confirm. The LSP has no way to drop such a channel.
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-lightning --test outbound_sequence -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::str::FromStr;
use std::time::Duration;

use common::{builder, poll, test_seed, Lsp, Regtest, TempDir};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::Node;
use serde_json::Value;

const SAT: u64 = 150_000;

/// A stock ldk-node as a routing node, public (alias set) and funded so it can accept a channel.
fn router(rt: &Regtest, tag: u8, label: &str) -> (Node, TempDir, String) {
    let dir = TempDir::new(label);
    let (mut b, addr) = builder(rt, test_seed(tag), &dir.str(label));
    b.set_node_alias(label.into()).unwrap();
    let node = b.build_with_fs_store().unwrap();
    node.start().unwrap();
    let a = node.onchain_payment().new_address().unwrap();
    rt.fund(&a.to_string(), 200_000);
    rt.mine(6);
    poll("router sees its funding", 90, || {
        node.sync_wallets().ok();
        (node.list_balances().spendable_onchain_balance_sats > 0).then_some(())
    });
    (node, dir, addr.to_string())
}

fn outbound_toml(peers: &[(&Node, &str)]) -> String {
    let items: Vec<String> = peers
        .iter()
        .map(|(n, addr)| format!("{{ peer = \"{}@{}\", sat = {SAT} }}", n.node_id(), addr))
        .collect();
    format!("[{}]", items.join(", "))
}

fn start_lsp(rt: &Regtest, peers: &[(&Node, &str)]) -> Lsp {
    Lsp::start(
        rt,
        &[
            ("alias", "\"fortis-test\""),
            ("announce_addresses", "[\"127.0.0.2:9735\"]"),
            ("outbound", &outbound_toml(peers)),
        ],
    )
}

fn has_channel_with(n: &Node, lsp: &PublicKey) -> bool {
    n.list_channels()
        .iter()
        .any(|c| c.counterparty_node_id == *lsp)
}

/// `POST <metrics>/admin/<what>` with the LSP's admin token: status and JSON body.
fn admin(lsp: &Lsp, what: &str, body: &str) -> (u16, Value) {
    let token = std::fs::read_to_string(lsp.dir().join("data").join("admin.token"))
        .expect("admin.token")
        .trim()
        .to_string();
    let url = lsp
        .metrics_url
        .replace("/metrics", &format!("/admin/{what}"));
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

/// Two targets, funds for both: the LSP opens ONE, waits while its funding is unconfirmed (a
/// full check interval passes), and opens the second only after the first confirmed.
#[test]
#[ignore = "needs fortis-lsp built and FORTIS_BTC_BITCOIND; takes ~3 minutes"]
fn outbound_channels_open_one_at_a_time() {
    let rt = Regtest::start();
    let (r1, _d1, a1) = router(&rt, 0x6B, "router-one");
    let (r2, _d2, a2) = router(&rt, 0x6C, "router-two");
    let lsp = start_lsp(&rt, &[(&r1, &a1), (&r2, &a2)]);
    let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();

    poll("the first channel opens", 120, || {
        has_channel_with(&r1, &lsp_id).then_some(())
    });
    assert!(
        !has_channel_with(&r2, &lsp_id),
        "the second channel must not open in the same pass:\n{}",
        lsp.stderr()
    );

    // The LSP checks every 60 s. Wait through at least one more check with the first funding
    // still unconfirmed (nothing mined): the second must still not open.
    std::thread::sleep(Duration::from_secs(75));
    assert!(
        !has_channel_with(&r2, &lsp_id),
        "opened the second channel while the first funding was unconfirmed:\n{}",
        lsp.stderr()
    );
    assert!(
        lsp.stderr().contains("one at a time"),
        "the hold-back should be logged:\n{}",
        lsp.stderr()
    );
    assert_eq!(
        lsp.stderr().matches("opening announced channel").count(),
        1,
        "{}",
        lsp.stderr()
    );

    // The first funding confirms: the next check opens the second channel.
    rt.mine(6);
    poll(
        "the second channel opens after the first confirmed",
        150,
        || has_channel_with(&r2, &lsp_id).then_some(()),
    );
    assert_eq!(
        lsp.stderr().matches("opening announced channel").count(),
        2,
        "{}",
        lsp.stderr()
    );
    r1.stop().unwrap();
    r2.stop().unwrap();
}

/// A channel the LSP funded whose funding has not confirmed is force-closed by
/// `/admin/abandon`, and the peer is free again.
#[test]
#[ignore = "needs fortis-lsp built and FORTIS_BTC_BITCOIND"]
fn an_unconfirmed_channel_is_abandoned() {
    let rt = Regtest::start();
    let (r1, _d1, a1) = router(&rt, 0x6D, "router-abandon");
    let lsp = start_lsp(&rt, &[(&r1, &a1)]);
    let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();
    let r1_id = r1.node_id().to_string();

    poll(
        "the channel opens (funding unconfirmed: nothing mined)",
        120,
        || has_channel_with(&r1, &lsp_id).then_some(()),
    );
    assert_eq!(lsp.metric("fortis_lsp_channels"), 1);

    let (status, body) = admin(&lsp, "abandon", &format!("{{\"peer\":\"{r1_id}\"}}"));
    assert_eq!(
        (status, body["abandoned"].as_u64()),
        (200, Some(1)),
        "{body}\n{}",
        lsp.stderr()
    );

    poll("the LSP no longer lists the channel", 60, || {
        (lsp.metric("fortis_lsp_channels") == 0).then_some(())
    });
    poll("the peer no longer lists it either", 60, || {
        r1.sync_wallets().ok();
        (!has_channel_with(&r1, &lsp_id)).then_some(())
    });
    r1.stop().unwrap();
}

/// A live (confirmed, ready) channel is NEVER force-closed by `/admin/abandon`.
#[test]
#[ignore = "needs fortis-lsp built and FORTIS_BTC_BITCOIND"]
fn a_confirmed_channel_is_not_abandoned() {
    let rt = Regtest::start();
    let (r1, _d1, a1) = router(&rt, 0x6E, "router-live");
    let lsp = start_lsp(&rt, &[(&r1, &a1)]);
    let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();
    let r1_id = r1.node_id().to_string();

    poll("the channel opens", 120, || {
        has_channel_with(&r1, &lsp_id).then_some(())
    });
    rt.mine(6);
    poll("the channel is ready", 120, || {
        r1.sync_wallets().ok();
        r1.list_channels()
            .into_iter()
            .find(|c| c.counterparty_node_id == lsp_id && c.is_channel_ready)
    });

    let (status, body) = admin(&lsp, "abandon", &format!("{{\"peer\":\"{r1_id}\"}}"));
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("/admin/close"),
        "the refusal points to the cooperative close: {body}"
    );
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        r1.list_channels()
            .iter()
            .any(|c| c.counterparty_node_id == lsp_id && c.is_channel_ready),
        "the live channel was closed"
    );
    assert_eq!(lsp.metric("fortis_lsp_channels"), 1);
    r1.stop().unwrap();
}
