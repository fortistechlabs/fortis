//! Signed Fortis account requests. The phone signs
//! `SIG_PREFIX + <exact body bytes>` with its Lightning node key (LN message
//! signing, zbase32); the body names the node, the operation, its params, a
//! single-use nonce and an expiry at most `MAX_AHEAD_SECS` ahead.

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::util::message_signing;
use serde_json::Value;

use crate::account::Ledger;

pub const SIG_PREFIX: &str = "fortis-account-v1:";
/// Furthest a request's expiry may be ahead of now.
pub const MAX_AHEAD_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct AccountRequest {
    pub node_id: PublicKey,
    pub op: String,
    pub params: Value,
    pub nonce: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Malformed,
    BadSignature,
    Expired,
    TooFarAhead,
    Replayed,
}

impl AuthError {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthError::Malformed => "malformed",
            AuthError::BadSignature => "bad_signature",
            AuthError::Expired => "expired",
            AuthError::TooFarAhead => "too_far_ahead",
            AuthError::Replayed => "replayed",
        }
    }
}

fn nonce_ok(n: &str) -> bool {
    (16..=64).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse(body: &[u8]) -> Result<AccountRequest, AuthError> {
    let v: Value = serde_json::from_slice(body).map_err(|_| AuthError::Malformed)?;
    let node_id: PublicKey = v["node_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(AuthError::Malformed)?;
    let op = v["op"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 32)
        .ok_or(AuthError::Malformed)?
        .to_string();
    let nonce = v["nonce"]
        .as_str()
        .filter(|s| nonce_ok(s))
        .ok_or(AuthError::Malformed)?
        .to_string();
    let expires_at = v["expires_at"].as_u64().ok_or(AuthError::Malformed)?;
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    Ok(AccountRequest {
        node_id,
        op,
        params,
        nonce,
        expires_at,
    })
}

/// Checks `body` (as received) against `sig`, the expiry and the nonce
/// store. The nonce is spent only once the signature and expiry pass.
pub fn verify(
    body: &[u8],
    sig: &str,
    now_secs: u64,
    ledger: &Ledger,
) -> Result<AccountRequest, AuthError> {
    let req = parse(body)?;
    let mut msg = SIG_PREFIX.as_bytes().to_vec();
    msg.extend_from_slice(body);
    if !message_signing::verify(&msg, sig, &req.node_id) {
        return Err(AuthError::BadSignature);
    }
    if req.expires_at < now_secs {
        return Err(AuthError::Expired);
    }
    if req.expires_at > now_secs + MAX_AHEAD_SECS {
        return Err(AuthError::TooFarAhead);
    }
    match ledger.use_nonce(&req.node_id, &req.nonce, req.expires_at) {
        Ok(true) => Ok(req),
        Ok(false) => Err(AuthError::Replayed),
        Err(_) => Err(AuthError::Malformed),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};

    pub(crate) fn key(n: u8) -> (SecretKey, PublicKey) {
        let sk = SecretKey::from_slice(&[n; 32]).unwrap();
        (sk, PublicKey::from_secret_key(&Secp256k1::new(), &sk))
    }

    pub(crate) fn signed(sk: &SecretKey, body: &str) -> String {
        message_signing::sign(format!("{SIG_PREFIX}{body}").as_bytes(), sk)
    }

    pub(crate) fn body(
        pk: &PublicKey,
        op: &str,
        params: &str,
        nonce: &str,
        expires_at: u64,
    ) -> String {
        format!(
            r#"{{"node_id":"{pk}","op":"{op}","params":{params},"nonce":"{nonce}","expires_at":{expires_at}}}"#
        )
    }

    fn ledger(tag: &str) -> Ledger {
        let dir =
            std::env::temp_dir().join(format!("fortis-lsp-auth-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Ledger::open(&dir.join("a.sqlite")).unwrap()
    }

    const NOW: u64 = 1_800_000_000;
    const NONCE: &str = "abcdefghijklmnop";

    #[test]
    fn valid_request_is_accepted() {
        let (sk, pk) = key(1);
        let b = body(&pk, "balance", "{}", NONCE, NOW + 60);
        let r = verify(b.as_bytes(), &signed(&sk, &b), NOW, &ledger("ok")).unwrap();
        assert_eq!(
            (r.node_id, r.op.as_str(), r.expires_at),
            (pk, "balance", NOW + 60)
        );
    }

    #[test]
    fn signature_by_another_key_is_refused() {
        let (_, pk) = key(1);
        let (other, _) = key(2);
        let b = body(&pk, "balance", "{}", NONCE, NOW + 60);
        assert_eq!(
            verify(b.as_bytes(), &signed(&other, &b), NOW, &ledger("other")),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn body_changed_after_signing_is_refused() {
        let (sk, pk) = key(1);
        let b = body(&pk, "pay", r#"{"amount_msat":1000}"#, NONCE, NOW + 60);
        let sig = signed(&sk, &b);
        let tampered = b.replace("1000", "9000");
        assert_eq!(
            verify(tampered.as_bytes(), &sig, NOW, &ledger("tamper")),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn expired_and_too_far_ahead_are_refused() {
        let (sk, pk) = key(1);
        let l = ledger("time");
        let old = body(&pk, "balance", "{}", NONCE, NOW - 1);
        assert_eq!(
            verify(old.as_bytes(), &signed(&sk, &old), NOW, &l),
            Err(AuthError::Expired)
        );
        let far = body(
            &pk,
            "balance",
            "{}",
            "qrstuvwxyzabcdefgh",
            NOW + MAX_AHEAD_SECS + 1,
        );
        assert_eq!(
            verify(far.as_bytes(), &signed(&sk, &far), NOW, &l),
            Err(AuthError::TooFarAhead)
        );
    }

    #[test]
    fn replayed_nonce_is_refused() {
        let (sk, pk) = key(1);
        let l = ledger("replay");
        let b = body(&pk, "balance", "{}", NONCE, NOW + 60);
        let sig = signed(&sk, &b);
        assert!(verify(b.as_bytes(), &sig, NOW, &l).is_ok());
        assert_eq!(
            verify(b.as_bytes(), &sig, NOW, &l),
            Err(AuthError::Replayed)
        );
        assert_eq!(l.balance(&pk).unwrap().balance_msat, 0);
    }

    #[test]
    fn malformed_requests_are_refused() {
        let (sk, pk) = key(1);
        let l = ledger("bad");
        for b in [
            "not json".to_string(),
            r#"{"op":"balance","nonce":"abcdefghijklmnop","expires_at":1}"#.to_string(),
            body(&pk, "balance", "{}", "short", NOW + 60),
            body(&pk, "", "{}", NONCE, NOW + 60),
        ] {
            assert_eq!(
                verify(b.as_bytes(), &signed(&sk, &b), NOW, &l),
                Err(AuthError::Malformed),
                "{b}"
            );
        }
    }
}
