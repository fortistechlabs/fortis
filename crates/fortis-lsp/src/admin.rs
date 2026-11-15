//! Operator commands on the loopback listener, for managing the LSP's own
//! funds (e.g. paying an invoice out once to gain inbound liquidity).
//! `POST /admin/pay {"invoice": "...", "amount_msat": optional}` with
//! `Authorization: Bearer <token>`; `POST /admin/close {"peer": "<node id>"}`
//! cooperatively closes every channel with that peer (funds return to the
//! on-chain wallet); `POST /admin/abandon {"peer": "<node id>"}` force-closes only the
//! channels we funded whose funding transaction never confirmed (a dead channel, e.g. its
//! funding was rejected as a double spend), and refuses a confirmed one. The token is `<data_dir>/admin.token`
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
        .filter(|c| crate::outbound::funding_unconfirmed(c.is_outbound, c.is_channel_ready, c.confirmations))
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

/// What `POST /admin/pay` needs from the node.
pub trait AdminNode {
    fn pay_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<String, String>;
    /// Cooperatively closes every channel with `peer`; how many.
    fn close_peer(&self, peer: &str) -> Result<usize, String>;
    /// Force-closes the channels with `peer` that [`abandon_ids`] allows; how many.
    fn abandon_peer(&self, peer: &str) -> Result<usize, String>;
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
        let pk: ldk_node::bitcoin::secp256k1::PublicKey =
            peer.trim().parse().map_err(|_| "peer must be a node id".to_string())?;
        let channels: Vec<_> = self
            .list_channels()
            .into_iter()
            .filter(|c| c.counterparty_node_id == pk)
            .collect();
        if channels.is_empty() {
            return Err("no channel with that peer".into());
        }
        for c in &channels {
            self.close_channel(&c.user_channel_id, pk).map_err(|e| e.to_string())?;
        }
        Ok(channels.len())
    }

    fn abandon_peer(&self, peer: &str) -> Result<usize, String> {
        let pk: ldk_node::bitcoin::secp256k1::PublicKey =
            peer.trim().parse().map_err(|_| "peer must be a node id".to_string())?;
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
    }

    fn chan(id: u128, peer: &str, outbound: bool, ready: bool, confs: Option<u32>) -> ChanView {
        ChanView { id, peer: peer.into(), is_outbound: outbound, is_channel_ready: ready, confirmations: confs }
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
            chan(1, "02aa", true, false, None),     // ours, funding unconfirmed: yes
            chan(2, "02aa", true, true, Some(6)),   // ours, ready: never
            chan(3, "02bb", true, false, Some(0)),  // another peer: never
            chan(4, "02aa", false, false, None),    // the peer funded it: not ours to abandon
            chan(5, "02aa", true, false, Some(2)),  // funding confirmed, waiting for depth: no
        ];
        assert_eq!(abandon_ids(&chans, "02aa"), Ok(vec![1]));
    }

    #[test]
    fn abandon_refuses_with_a_reason_when_nothing_qualifies() {
        let ready = vec![chan(1, "02aa", true, true, Some(6))];
        let e = abandon_ids(&ready, "02aa").unwrap_err();
        assert!(e.contains("/admin/close"), "points a live channel to the cooperative close: {e}");
        let e = abandon_ids(&ready, "02ff").unwrap_err();
        assert!(e.contains("no channel"), "{e}");
        assert!(abandon_ids(&[], "02aa").is_err());
    }

    #[test]
    fn abandon_needs_the_token_and_a_peer() {
        let node = Fake(RefCell::new(vec![]));
        assert_eq!(abandon(&node, None, "t", br#"{"peer":"02aa"}"#).0, 401);
        assert_eq!(abandon(&node, Some("Bearer wrong"), "t", br#"{"peer":"02aa"}"#).0, 401);
        assert_eq!(abandon(&node, Some("Bearer t"), "t", b"{}").0, 400);
        assert!(node.0.borrow().is_empty(), "nothing was attempted without auth or a peer");
        let (s, v) = abandon(&node, Some("Bearer t"), "t", br#"{"peer":"02aa"}"#);
        assert_eq!((s, v["abandoned"].as_u64()), (200, Some(1)));
        assert_eq!(node.0.borrow()[0].0, "abandon 02aa");
    }
}
