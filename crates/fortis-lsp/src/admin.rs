//! Operator commands on the loopback listener, for managing the LSP's own
//! funds (e.g. paying an invoice out once to gain inbound liquidity).
//! `POST /admin/pay {"invoice": "...", "amount_msat": optional}` with
//! `Authorization: Bearer <token>`; `POST /admin/close {"peer": "<node id>"}`
//! cooperatively closes every channel with that peer (funds return to the
//! on-chain wallet); `POST /admin/abandon {"peer": "<node id>"}` force-closes only the
//! channels we funded whose funding transaction never confirmed (a dead channel, e.g. its
//! funding was rejected as a double spend), and refuses a confirmed one;
//! `POST /admin/send {"address": "...", "amount_sat": N}` sends exactly N sat on-chain from
//! the LSP's own wallet (N within `MIN_ADMIN_SEND_SAT..=MAX_ADMIN_SEND_SAT`, address for the
//! node's own network, the anchor reserve of open channels kept);
//! `POST /admin/open {"peer": "<node id>@<host>:<port>", "sat": N, "push_sat": M}` opens one
//! extra announced channel of N sat with M sat handed to the peer's side (inbound liquidity for
//! the LSP: the peer can then pay it), one funding at a time. The token is `<data_dir>/admin.token`
//! (random, created owner-only on first start). Never forwarded by the edge.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde_json::{json, Value};

pub const TOKEN_FILE: &str = "admin.token";

/// Reads the admin token, creating a random one (0600) if there is none.
pub fn load_or_create_token(data_dir: &Path) -> std::io::Result<String> {
    let path = data_dir.join(TOKEN_FILE);
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 32 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            return Ok(t);
        }
    }
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let token = hex::encode(bytes);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(token.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(token)
}

/// Constant-time comparison of the presented bearer token.
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
        return false;
    };
    let (a, b) = (given.trim().as_bytes(), token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A channel as the abandon rules see it (see [`abandon_ids`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChanView {
    pub id: u128,
    pub peer: String,
    pub is_outbound: bool,
    pub is_channel_ready: bool,
    pub confirmations: Option<u32>,
}

/// Which of `chans` `POST /admin/abandon` may force-close for `peer`: only channels WE funded
/// whose funding transaction has not confirmed. A confirmed channel is never abandoned here
/// (that would force-close live funds); close those cooperatively with `/admin/close`.
pub fn abandon_ids(chans: &[ChanView], peer: &str) -> Result<Vec<u128>, String> {
    let with_peer: Vec<&ChanView> = chans.iter().filter(|c| c.peer == peer).collect();
    if with_peer.is_empty() {
        return Err("no channel with that peer".into());
    }
    let ids: Vec<u128> = with_peer
        .iter()
        .filter(|c| {
            crate::outbound::funding_unconfirmed(c.is_outbound, c.is_channel_ready, c.confirmations)
        })
        .map(|c| c.id)
        .collect();
    if ids.is_empty() {
        return Err(
            "no channel with that peer is one we funded with an unconfirmed funding transaction; \
             close a live channel cooperatively with /admin/close"
                .into(),
        );
    }
    Ok(ids)
}

/// Bounds for `POST /admin/send` (sat): a typo guard, not a spending policy.
pub const MIN_ADMIN_SEND_SAT: u64 = 1_000;
pub const MAX_ADMIN_SEND_SAT: u64 = 1_000_000;

/// Bounds for `POST /admin/open` (sat): a typo guard, not a spending policy.
pub const MIN_ADMIN_OPEN_SAT: u64 = 20_000;
pub const MAX_ADMIN_OPEN_SAT: u64 = 1_000_000;

/// A validated `POST /admin/open` request.
#[derive(Debug, PartialEq, Eq)]
pub struct OpenRequest {
    pub node_id: ldk_node::bitcoin::secp256k1::PublicKey,
    pub address: ldk_node::lightning::ln::msgs::SocketAddress,
    pub sat: u64,
    pub push_sat: u64,
}

/// Parses and checks the body of `POST /admin/open`. The push must leave room for both channel
/// reserves: at most 90 % of the channel, and at least 1 sat (a channel with no push is what
/// `[[outbound]]` is for).
pub fn parse_open(body: &[u8]) -> Result<OpenRequest, String> {
    use std::str::FromStr;
    let v: Value = serde_json::from_slice(body).map_err(|_| "body must be JSON".to_string())?;
    let peer = v["peer"].as_str().map(str::trim).unwrap_or_default();
    let (id, addr) = peer
        .split_once('@')
        .ok_or_else(|| "peer must be <node id>@<host>:<port>".to_string())?;
    let node_id = ldk_node::bitcoin::secp256k1::PublicKey::from_str(id.trim())
        .map_err(|_| "peer: node id must be a 66-hex public key".to_string())?;
    let address = ldk_node::lightning::ln::msgs::SocketAddress::from_str(addr.trim())
        .map_err(|e| format!("peer: address: {e:?}"))?;
    let sat = v["sat"]
        .as_u64()
        .ok_or("sat must be a whole number of sat")?;
    if !(MIN_ADMIN_OPEN_SAT..=MAX_ADMIN_OPEN_SAT).contains(&sat) {
        return Err(format!(
            "sat must be {MIN_ADMIN_OPEN_SAT}..={MAX_ADMIN_OPEN_SAT}"
        ));
    }
    let push_sat = v["push_sat"]
        .as_u64()
        .ok_or("push_sat must be a whole number of sat")?;
    if push_sat == 0 || push_sat > sat / 10 * 9 {
        return Err(format!(
            "push_sat must be 1..={} (at most 90 % of sat)",
            sat / 10 * 9
        ));
    }
    Ok(OpenRequest {
        node_id,
        address,
        sat,
        push_sat,
    })
}

/// What `POST /admin/pay` needs from the node.
pub trait AdminNode {
    fn pay_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<String, String>;
    /// Cooperatively closes every channel with `peer`; how many.
    fn close_peer(&self, peer: &str) -> Result<usize, String>;
    /// Force-closes the channels with `peer` that [`abandon_ids`] allows; how many.
    fn abandon_peer(&self, peer: &str) -> Result<usize, String>;
    /// Sends exactly `amount_sat` on-chain to `address` (which must be for this node's network);
    /// returns the txid.
    fn send_onchain(&self, address: &str, amount_sat: u64) -> Result<String, String>;
    /// Opens one announced channel as [`parse_open`] describes; the user channel id.
    fn open_with_push(&self, req: &OpenRequest) -> Result<String, String>;
}

impl AdminNode for ldk_node::Node {
    fn pay_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<String, String> {
        let inv = invoice
            .trim()
            .parse::<ldk_node::lightning_invoice::Bolt11Invoice>()
            .map_err(|e| e.to_string())?;
        let p = self.bolt11_payment();
        let id = match (inv.amount_milli_satoshis(), amount_msat) {
            (Some(_), _) => p.send(&inv, None),
            (None, Some(a)) => p.send_using_amount(&inv, a, None),
            (None, None) => return Err("amount_msat is required for an amountless invoice".into()),
        }
        .map_err(|e| e.to_string())?;
        Ok(hex::encode(id.0))
    }

    fn close_peer(&self, peer: &str) -> Result<usize, String> {
        let pk: ldk_node::bitcoin::secp256k1::PublicKey = peer
            .trim()
            .parse()
            .map_err(|_| "peer must be a node id".to_string())?;
        let channels: Vec<_> = self
            .list_channels()
            .into_iter()
            .filter(|c| c.counterparty_node_id == pk)
            .collect();
        if channels.is_empty() {
            return Err("no channel with that peer".into());
        }
        for c in &channels {
            self.close_channel(&c.user_channel_id, pk)
                .map_err(|e| e.to_string())?;
        }
        Ok(channels.len())
    }

    fn send_onchain(&self, address: &str, amount_sat: u64) -> Result<String, String> {
        // The address must belong to this node's network (a testnet or other-chain address is
        // refused here, before any transaction is built).
        let addr = address
            .trim()
            .parse::<ldk_node::bitcoin::Address<_>>()
            .map_err(|_| "not a valid address".to_string())?
            .require_network(self.config().network)
            .map_err(|_| "address is for another network".to_string())?;
        // Exactly `amount_sat`, keeping the anchor reserve of open channels (ldk-node refuses
        // a send that would dip into it).
        let txid = self
            .onchain_payment()
            .send_to_address(&addr, amount_sat, None)
            .map_err(|e| e.to_string())?;
        Ok(txid.to_string())
    }

    fn open_with_push(&self, req: &OpenRequest) -> Result<String, String> {
        // One funding at a time: two opened together spent the same coin once (2026-11-15) and the
        // second never confirmed.
        if self.list_channels().iter().any(|c| {
            c.is_outbound && (c.funding_txo.is_none() || c.confirmations.unwrap_or(0) == 0)
        }) {
            return Err(
                "an earlier channel's funding is unconfirmed: wait for it to confirm".into(),
            );
        }
        let id = self
            .open_announced_channel(
                req.node_id,
                req.address.clone(),
                req.sat,
                Some(req.push_sat * 1_000),
                None,
            )
            .map_err(|e| e.to_string())?;
        Ok(id.0.to_string())
    }

    fn abandon_peer(&self, peer: &str) -> Result<usize, String> {
        let pk: ldk_node::bitcoin::secp256k1::PublicKey = peer
            .trim()
            .parse()
            .map_err(|_| "peer must be a node id".to_string())?;
        let views: Vec<ChanView> = self
            .list_channels()
            .iter()
            .map(|c| ChanView {
                id: c.user_channel_id.0,
                peer: c.counterparty_node_id.to_string(),
                is_outbound: c.is_outbound,
                is_channel_ready: c.is_channel_ready,
                confirmations: c.confirmations,
            })
            .collect();
        let ids = abandon_ids(&views, &pk.to_string())?;
        for id in &ids {
            self.force_close_channel(
                &ldk_node::UserChannelId(*id),
                pk,
                Some("abandoned: funding transaction never confirmed".into()),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(ids.len())
    }
}

/// Handles `POST /admin/pay`. Status and JSON body.
pub fn pay(node: &dyn AdminNode, auth: Option<&str>, token: &str, body: &[u8]) -> (u16, Value) {
    if !authorized(auth, token) {
        return (401, json!({ "error": "unauthorized" }));
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (400, json!({ "error": "body must be JSON" }));
    };
    let Some(invoice) = v["invoice"].as_str() else {
        return (400, json!({ "error": "invoice" }));
    };
    match node.pay_bolt11(invoice, v["amount_msat"].as_u64()) {
        Ok(id) => {
            eprintln!("fortis-lsp: admin pay started (payment {id})");
            (200, json!({ "payment_id": id }))
        }
        Err(e) => (400, json!({ "error": e })),
    }
}

/// Handles `POST /admin/send`. Status and JSON body.
pub fn send(node: &dyn AdminNode, auth: Option<&str>, token: &str, body: &[u8]) -> (u16, Value) {
    if !authorized(auth, token) {
        return (401, json!({ "error": "unauthorized" }));
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (400, json!({ "error": "body must be JSON" }));
    };
    let Some(address) = v["address"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    else {
        return (400, json!({ "error": "address" }));
    };
    // An exact whole number of sat: no "all", no fractions, no negatives.
    let Some(amount_sat) = v["amount_sat"].as_u64() else {
        return (
            400,
            json!({ "error": "amount_sat must be a whole number of sat" }),
        );
    };
    if !(MIN_ADMIN_SEND_SAT..=MAX_ADMIN_SEND_SAT).contains(&amount_sat) {
        return (
            400,
            json!({ "error": format!("amount_sat must be {MIN_ADMIN_SEND_SAT}..={MAX_ADMIN_SEND_SAT} sat") }),
        );
    }
    match node.send_onchain(address, amount_sat) {
        Ok(txid) => {
            eprintln!("fortis-lsp: admin send of {amount_sat} sat to {address} (txid {txid})");
            (200, json!({ "txid": txid }))
        }
        Err(e) => (400, json!({ "error": e })),
    }
}

/// Handles `POST /admin/open`. Status and JSON body.
pub fn open(node: &dyn AdminNode, auth: Option<&str>, token: &str, body: &[u8]) -> (u16, Value) {
    if !authorized(auth, token) {
        return (401, json!({ "error": "unauthorized" }));
    }
    let req = match parse_open(body) {
        Ok(r) => r,
        Err(e) => return (400, json!({ "error": e })),
    };
    match node.open_with_push(&req) {
        Ok(id) => {
            eprintln!(
                "fortis-lsp: admin open of {} sat to {} with {} sat pushed (user_channel_id {id})",
                req.sat, req.node_id, req.push_sat
            );
            (200, json!({ "user_channel_id": id }))
        }
        Err(e) => (400, json!({ "error": e })),
    }
}

/// Handles `POST /admin/abandon`. Status and JSON body.
pub fn abandon(node: &dyn AdminNode, auth: Option<&str>, token: &str, body: &[u8]) -> (u16, Value) {
    if !authorized(auth, token) {
        return (401, json!({ "error": "unauthorized" }));
    }
    let Some(peer) = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["peer"].as_str().map(str::to_string))
    else {
        return (400, json!({ "error": "peer" }));
    };
    match node.abandon_peer(&peer) {
        Ok(n) => {
            eprintln!("fortis-lsp: admin abandon of {n} unconfirmed channel(s) with {peer}");
            (200, json!({ "abandoned": n }))
        }
        Err(e) => (400, json!({ "error": e })),
    }
}

/// Handles `POST /admin/close`. Status and JSON body.
pub fn close(node: &dyn AdminNode, auth: Option<&str>, token: &str, body: &[u8]) -> (u16, Value) {
    if !authorized(auth, token) {
        return (401, json!({ "error": "unauthorized" }));
    }
    let Some(peer) = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["peer"].as_str().map(str::to_string))
    else {
        return (400, json!({ "error": "peer" }));
    };
    match node.close_peer(&peer) {
        Ok(n) => {
            eprintln!("fortis-lsp: admin close of {n} channel(s) with {peer}");
            (200, json!({ "closing": n }))
        }
        Err(e) => (400, json!({ "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake(RefCell<Vec<(String, Option<u64>)>>);

    impl AdminNode for Fake {
        fn pay_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<String, String> {
            self.0.borrow_mut().push((invoice.into(), amount_msat));
            Ok("ab".repeat(32))
        }
        fn close_peer(&self, peer: &str) -> Result<usize, String> {
            self.0.borrow_mut().push((format!("close {peer}"), None));
            Ok(1)
        }
        fn abandon_peer(&self, peer: &str) -> Result<usize, String> {
            self.0.borrow_mut().push((format!("abandon {peer}"), None));
            Ok(1)
        }
        fn open_with_push(&self, r: &OpenRequest) -> Result<String, String> {
            self.0.borrow_mut().push((
                format!("open {} push {}", r.node_id, r.push_sat),
                Some(r.sat),
            ));
            Ok("77".into())
        }
        fn send_onchain(&self, address: &str, amount_sat: u64) -> Result<String, String> {
            self.0
                .borrow_mut()
                .push((format!("send {address}"), Some(amount_sat)));
            if address.starts_with("bad") {
                return Err("address is for another network".into());
            }
            Ok("cd".repeat(32))
        }
    }

    fn chan(id: u128, peer: &str, outbound: bool, ready: bool, confs: Option<u32>) -> ChanView {
        ChanView {
            id,
            peer: peer.into(),
            is_outbound: outbound,
            is_channel_ready: ready,
            confirmations: confs,
        }
    }

    const PEER: &str =
        "02aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@1.2.3.4:9735";

    fn open_json(sat: u64, push: u64) -> String {
        format!("{{\"peer\":\"{PEER}\",\"sat\":{sat},\"push_sat\":{push}}}")
    }

    #[test]
    fn open_accepts_a_sane_request() {
        let r = parse_open(open_json(100_000, 30_000).as_bytes()).unwrap();
        assert_eq!((r.sat, r.push_sat), (100_000, 30_000));
        // The edges: 90 % exactly, and 1 sat.
        assert!(parse_open(open_json(100_000, 90_000).as_bytes()).is_ok());
        assert!(parse_open(open_json(20_000, 1).as_bytes()).is_ok());
    }

    #[test]
    fn open_refuses_what_could_lose_money_by_mistake() {
        for (sat, push) in [
            (19_999, 1_000),
            (1_000_001, 1_000),
            (100_000, 0),
            (100_000, 90_001),
            (100_000, 100_000),
        ] {
            assert!(
                parse_open(open_json(sat, push).as_bytes()).is_err(),
                "{sat} {push}"
            );
        }
        for body in [
            "nope".to_string(),
            "{}".to_string(),
            format!(
                "{{\"peer\":\"{}\",\"sat\":100000,\"push_sat\":1000}}",
                &PEER[..66]
            ), // no address
            "{\"peer\":\"zz@1.2.3.4:9735\",\"sat\":100000,\"push_sat\":1000}".to_string(),
            format!("{{\"peer\":\"{PEER}\",\"sat\":\"all\",\"push_sat\":1000}}"),
            format!("{{\"peer\":\"{PEER}\",\"sat\":100000.5,\"push_sat\":1000}}"),
        ] {
            assert!(parse_open(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn open_needs_the_token_and_never_calls_the_node_when_the_request_is_bad() {
        let node = Fake(RefCell::new(vec![]));
        assert_eq!(
            open(&node, None, "t", open_json(100_000, 30_000).as_bytes()).0,
            401
        );
        assert_eq!(
            open(
                &node,
                Some("Bearer wrong"),
                "t",
                open_json(100_000, 30_000).as_bytes()
            )
            .0,
            401
        );
        assert_eq!(
            open(
                &node,
                Some("Bearer t"),
                "t",
                open_json(100_000, 0).as_bytes()
            )
            .0,
            400
        );
        assert!(node.0.borrow().is_empty());
        let (s, v) = open(
            &node,
            Some("Bearer t"),
            "t",
            open_json(100_000, 30_000).as_bytes(),
        );
        assert_eq!((s, v["user_channel_id"].as_str()), (200, Some("77")));
        assert_eq!(node.0.borrow().len(), 1);
    }

    #[test]
    fn close_needs_the_token_and_a_peer() {
        let node = Fake(RefCell::new(vec![]));
        assert_eq!(close(&node, None, "t", br#"{"peer":"02aa"}"#).0, 401);
        assert_eq!(close(&node, Some("Bearer t"), "t", b"{}").0, 400);
        let (s, v) = close(&node, Some("Bearer t"), "t", br#"{"peer":"02aa"}"#);
        assert_eq!((s, v["closing"].as_u64()), (200, Some(1)));
        assert_eq!(node.0.borrow()[0].0, "close 02aa");
    }

    #[test]
    fn only_the_right_bearer_token_is_accepted() {
        assert!(authorized(Some("Bearer secret"), "secret"));
        assert!(!authorized(Some("Bearer secreT"), "secret"));
        assert!(!authorized(Some("secret"), "secret"));
        assert!(!authorized(None, "secret"));
        assert!(!authorized(Some("Bearer "), "secret"));
    }

    #[test]
    fn pay_needs_the_token_and_an_invoice() {
        let node = Fake(RefCell::new(vec![]));
        assert_eq!(pay(&node, None, "t0k", br#"{"invoice":"lnbc1"}"#).0, 401);
        assert_eq!(pay(&node, Some("Bearer t0k"), "t0k", b"{}").0, 400);
        let (s, v) = pay(
            &node,
            Some("Bearer t0k"),
            "t0k",
            br#"{"invoice":"lnbc1","amount_msat":5000}"#,
        );
        assert_eq!((s, v["payment_id"].as_str().map(str::len)), (200, Some(64)));
        assert_eq!(
            node.0.borrow().as_slice(),
            &[("lnbc1".to_string(), Some(5_000))]
        );
    }

    #[test]
    fn the_token_file_is_created_once_and_owner_only() {
        let dir = std::env::temp_dir().join(format!("fortis-lsp-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create_token(&dir).unwrap();
        let b = load_or_create_token(&dir).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let mode = std::fs::metadata(dir.join(TOKEN_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    // --- /admin/abandon: force-close a channel whose funding never confirmed -------------

    #[test]
    fn abandon_picks_only_our_unconfirmed_channels_with_that_peer() {
        let chans = vec![
            chan(1, "02aa", true, false, None), // ours, funding unconfirmed: yes
            chan(2, "02aa", true, true, Some(6)), // ours, ready: never
            chan(3, "02bb", true, false, Some(0)), // another peer: never
            chan(4, "02aa", false, false, None), // the peer funded it: not ours to abandon
            chan(5, "02aa", true, false, Some(2)), // funding confirmed, waiting for depth: no
        ];
        assert_eq!(abandon_ids(&chans, "02aa"), Ok(vec![1]));
    }

    #[test]
    fn abandon_refuses_with_a_reason_when_nothing_qualifies() {
        let ready = vec![chan(1, "02aa", true, true, Some(6))];
        let e = abandon_ids(&ready, "02aa").unwrap_err();
        assert!(
            e.contains("/admin/close"),
            "points a live channel to the cooperative close: {e}"
        );
        let e = abandon_ids(&ready, "02ff").unwrap_err();
        assert!(e.contains("no channel"), "{e}");
        assert!(abandon_ids(&[], "02aa").is_err());
    }

    #[test]
    fn abandon_needs_the_token_and_a_peer() {
        let node = Fake(RefCell::new(vec![]));
        assert_eq!(abandon(&node, None, "t", br#"{"peer":"02aa"}"#).0, 401);
        assert_eq!(
            abandon(&node, Some("Bearer wrong"), "t", br#"{"peer":"02aa"}"#).0,
            401
        );
        assert_eq!(abandon(&node, Some("Bearer t"), "t", b"{}").0, 400);
        assert!(
            node.0.borrow().is_empty(),
            "nothing was attempted without auth or a peer"
        );
        let (s, v) = abandon(&node, Some("Bearer t"), "t", br#"{"peer":"02aa"}"#);
        assert_eq!((s, v["abandoned"].as_u64()), (200, Some(1)));
        assert_eq!(node.0.borrow()[0].0, "abandon 02aa");
    }

    // --- /admin/send: an exact on-chain payment from the LSP's wallet ----------------------

    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";

    fn send_body(address: &str, amount: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({ "address": address, "amount_sat": amount })).unwrap()
    }

    #[test]
    fn send_needs_the_token() {
        let node = Fake(RefCell::new(vec![]));
        assert_eq!(send(&node, None, "t", &send_body(ADDR, 50_000)).0, 401);
        assert_eq!(
            send(&node, Some("Bearer wrong"), "t", &send_body(ADDR, 50_000)).0,
            401
        );
        assert!(
            node.0.borrow().is_empty(),
            "nothing is sent without the token"
        );
    }

    #[test]
    fn send_needs_an_address_and_an_exact_amount() {
        let node = Fake(RefCell::new(vec![]));
        let t = Some("Bearer t");
        for body in [
            b"{}".to_vec(),
            br#"{"address":"bc1q"}"#.to_vec(),   // no amount
            br#"{"amount_sat":50000}"#.to_vec(), // no address
            br#"{"address":"","amount_sat":50000}"#.to_vec(), // empty address
            br#"{"address":"bc1q","amount_sat":"all"}"#.to_vec(), // not a number: no "send all"
            br#"{"address":"bc1q","amount_sat":-5}"#.to_vec(),
            br#"{"address":"bc1q","amount_sat":1.5}"#.to_vec(),
            b"not json".to_vec(),
        ] {
            let (s, v) = send(&node, t, "t", &body);
            assert_eq!(s, 400, "{} -> {v}", String::from_utf8_lossy(&body));
        }
        assert!(
            node.0.borrow().is_empty(),
            "no bad request reaches the wallet"
        );
    }

    #[test]
    fn send_refuses_amounts_outside_the_guard_rails() {
        let node = Fake(RefCell::new(vec![]));
        for amount in [0, MIN_ADMIN_SEND_SAT - 1, MAX_ADMIN_SEND_SAT + 1, u64::MAX] {
            let (s, v) = send(&node, Some("Bearer t"), "t", &send_body(ADDR, amount));
            assert_eq!(s, 400, "{amount}: {v}");
            assert!(v["error"].as_str().unwrap().contains("sat"), "{v}");
        }
        assert!(node.0.borrow().is_empty());
        // The bounds themselves are allowed.
        for amount in [MIN_ADMIN_SEND_SAT, MAX_ADMIN_SEND_SAT] {
            assert_eq!(
                send(&node, Some("Bearer t"), "t", &send_body(ADDR, amount)).0,
                200,
                "{amount}"
            );
        }
    }

    #[test]
    fn send_passes_the_address_and_amount_and_returns_the_txid() {
        let node = Fake(RefCell::new(vec![]));
        let (s, v) = send(&node, Some("Bearer t"), "t", &send_body(ADDR, 50_000));
        assert_eq!((s, v["txid"].as_str().map(str::len)), (200, Some(64)));
        assert_eq!(
            node.0.borrow().as_slice(),
            &[(format!("send {ADDR}"), Some(50_000))]
        );
    }

    #[test]
    fn send_reports_a_wallet_error_and_nothing_else() {
        let node = Fake(RefCell::new(vec![]));
        let (s, v) = send(
            &node,
            Some("Bearer t"),
            "t",
            &send_body("bad-address", 50_000),
        );
        assert_eq!(s, 400);
        assert_eq!(v["error"], "address is for another network");
        assert!(v.get("txid").is_none());
    }
}
