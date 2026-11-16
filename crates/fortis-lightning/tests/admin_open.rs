//! `POST /admin/open`: the real `fortis-lsp` binary opens one extra channel to a peer with
//! `push_sat` handed to the peer's side, which is how the LSP gets INBOUND liquidity on a
//! channel it funded (the peer then holds balance and can pay the LSP). Regtest.
//!
//! Why explicit and not an `[[outbound]]` option: the automatic opener leaves a peer alone once
//! any channel with it exists, and a push is real money handed to someone else.
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-lightning --test admin_open -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::str::FromStr;

use common::{builder, poll, test_seed, Lsp, Regtest, TempDir};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::Node;
use serde_json::Value;

const SAT: u64 = 100_000;
const PUSH: u64 = 30_000;

/// A stock ldk-node routing peer, public (alias set), reachable at the returned address.
fn peer(rt: &Regtest, tag: u8, label: &str) -> (Node, TempDir, String) {
    let dir = TempDir::new(label);
    let (mut b, addr) = builder(rt, test_seed(tag), &dir.str(label));
    b.set_node_alias(label.into()).unwrap();
    let node = b.build_with_fs_store().unwrap();
    node.start().unwrap();
    (node, dir, addr.to_string())
}

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

fn open_body(n: &Node, addr: &str, sat: u64, push: u64) -> String {
    format!(
        "{{\"peer\":\"{}@{addr}\",\"sat\":{sat},\"push_sat\":{push}}}",
        n.node_id()
    )
}

#[test]
#[ignore = "needs fortis-lsp built and FORTIS_BTC_BITCOIND; takes ~1 minute"]
fn the_lsp_opens_a_channel_that_hands_the_peer_a_starting_balance() {
    let rt = Regtest::start();
    let (p1, _d1, a1) = peer(&rt, 0x81, "push-peer");
    let (p2, _d2, a2) = peer(&rt, 0x82, "push-peer-2");
    let lsp = Lsp::start(
        &rt,
        &[
            ("alias", "\"fortis-test\""),
            ("announce_addresses", "[\"127.0.0.2:9735\"]"),
        ],
    );
    let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();

    // Refused before anything is spent: a typo guard, a push that leaves no room for the
    // reserve, a push of everything, junk.
    for (what, body) in [
        ("channel below the minimum", open_body(&p1, &a1, 1_000, 500)),
        (
            "channel above the cap",
            open_body(&p1, &a1, 5_000_000, 1_000),
        ),
        ("no push", open_body(&p1, &a1, SAT, 0)),
        ("push of everything", open_body(&p1, &a1, SAT, SAT)),
        ("push above 90 %", open_body(&p1, &a1, SAT, 91_000)),
        (
            "peer without address",
            format!(
                "{{\"peer\":\"{}\",\"sat\":{SAT},\"push_sat\":{PUSH}}}",
                p1.node_id()
            ),
        ),
        ("not json", "nope".to_string()),
    ] {
        let (s, v) = admin(&lsp, "open", &body);
        assert_eq!(s, 400, "{what}: {v}");
    }
    assert_eq!(
        lsp.metric("fortis_lsp_channels"),
        0,
        "a refused request opens nothing"
    );

    // The open. The LSP funds SAT; PUSH of it starts on the peer's side.
    let (s, v) = admin(&lsp, "open", &open_body(&p1, &a1, SAT, PUSH));
    assert_eq!(s, 200, "{v}\n{}", lsp.stderr());
    poll("the peer sees the channel", 120, || {
        p1.list_channels()
            .iter()
            .any(|c| c.counterparty_node_id == lsp_id)
            .then_some(())
    });

    // One funding at a time (two at once spent the same coin once, 2026-11-15): refused while
    // the first funding is unconfirmed.
    let (s, v) = admin(&lsp, "open", &open_body(&p2, &a2, SAT, PUSH));
    assert_eq!(s, 400, "{v}");
    assert!(
        v["error"].as_str().unwrap_or("").contains("unconfirmed"),
        "{v}"
    );

    rt.mine(6);
    let ch = poll("the channel is ready on the peer", 120, || {
        p1.sync_wallets().ok();
        p1.list_channels()
            .into_iter()
            .find(|c| c.counterparty_node_id == lsp_id && c.is_channel_ready)
    });
    // The peer holds the pushed balance: it can pay the LSP up to about that much.
    let peer_msat = ch.outbound_capacity_msat;
    assert!(
        (PUSH * 1_000 - 2_000_000..=PUSH * 1_000).contains(&peer_msat),
        "the peer's balance is the push: {peer_msat} msat"
    );
    let inbound = lsp.metric("fortis_lsp_inbound_sat");
    assert!(
        inbound >= PUSH - 2_000 && inbound <= PUSH,
        "the LSP's inbound is the push: {inbound} sat"
    );
    p1.stop().unwrap();
    p2.stop().unwrap();
}
