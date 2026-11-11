//! Waking a phone for a held payment (`held.rs`) with a push notification,
//! through Firebase Cloud Messaging (HTTP v1).
//!
//! The service-account key (`fcm_service_account` in the config) signs a
//! short-lived JWT (RS256), traded at Google's token endpoint for an OAuth
//! access token, cached until shortly before it expires. Without a key, or
//! for a phone that never registered a token (`push_register`), this only
//! logs.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::{json, Value};

use crate::account::Ledger;

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// A Firebase sender built from a service-account key.
pub struct Fcm {
    project_id: String,
    client_email: String,
    token_uri: String,
    key: RsaKeyPair,
    agent: ureq::Agent,
    /// The access token and when to stop using it.
    cached: Mutex<Option<(String, Instant)>>,
}

impl Fcm {
    /// Reads the service-account JSON at `path`.
    pub fn load(path: &std::path::Path) -> Result<Fcm, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Fcm::from_json(&text)
    }

    pub fn from_json(text: &str) -> Result<Fcm, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let field = |k: &str| {
            v[k].as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("missing {k}"))
        };
        let der = pem_to_der(&field("private_key")?)?;
        let key = RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("private_key: {e}"))?;
        Ok(Fcm {
            project_id: field("project_id")?,
            client_email: field("client_email")?,
            token_uri: field("token_uri").unwrap_or_else(|_| "https://oauth2.googleapis.com/token".into()),
            key,
            agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
            cached: Mutex::new(None),
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// The signed JWT asking for an FCM access token, valid an hour from `now`.
    fn assertion(&self, now: u64) -> Result<String, String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({
                "iss": self.client_email,
                "scope": SCOPE,
                "aud": self.token_uri,
                "iat": now,
                "exp": now + 3600,
            })
            .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut sig)
            .map_err(|_| "signing failed".to_string())?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    fn access_token(&self) -> Result<String, String> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let assertion = self.assertion(now)?;
        let resp: Value = self
            .agent
            .post(&self.token_uri)
            .send_form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .map_err(|e| format!("token: {}", ureq_error(e)))?
            .into_json()
            .map_err(|e| format!("token reply: {e}"))?;
        let token = resp["access_token"]
            .as_str()
            .ok_or("token reply without access_token")?
            .to_string();
        let ttl = resp["expires_in"].as_u64().unwrap_or(3600);
        // Renewed a minute early, so a send never carries an expired token.
        let until = Instant::now() + Duration::from_secs(ttl.saturating_sub(60));
        *cached = Some((token.clone(), until));
        Ok(token)
    }

    /// Sends a notification to `token`. `Err(Unregistered)` when Firebase
    /// says the token is no longer valid (the app was removed).
    pub fn send(&self, token: &str, title: &str, body: &str, data: Value) -> Result<(), SendError> {
        let access = self.access_token().map_err(SendError::Other)?;
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            self.project_id
        );
        let msg = json!({
            "message": {
                "token": token,
                "notification": { "title": title, "body": body },
                "data": data,
                "android": { "priority": "high", "notification": { "channel_id": "lightning" } },
            }
        });
        match self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {access}"))
            .send_json(msg)
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(404, _)) => Err(SendError::Unregistered),
            Err(e) => Err(SendError::Other(ureq_error(e))),
        }
    }
}

#[derive(Debug)]
pub enum SendError {
    Unregistered,
    Other(String),
}

fn ureq_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            format!("HTTP {code}: {}", body.chars().take(300).collect::<String>())
        }
        other => other.to_string(),
    }
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .flat_map(|l| l.trim().chars())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("private_key: {e}"))
}

/// Wakes `node`'s phone: "Incoming Lightning payment: N sat". Only logs when
/// push is not configured or the phone has no token.
pub fn wake(fcm: Option<&Fcm>, ledger: &Ledger, node: &PublicKey, amount_msat: u64, chain: &str) {
    let Some(fcm) = fcm else {
        eprintln!("fortis-lsp: {node} is offline with {amount_msat} msat held for it (push not configured)");
        return;
    };
    let token = match ledger.push_token(node) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("fortis-lsp: {node} is offline with {amount_msat} msat held for it (no push token)");
            return;
        }
        Err(e) => {
            eprintln!("fortis-lsp: push token for {node}: {e}");
            return;
        }
    };
    let sat = amount_msat / 1_000;
    let body = format!("{} sat is waiting for you. Open Fortis to receive it.", group(sat));
    let data = wake_data(chain, amount_msat);
    match fcm.send(&token, push_text(Push::Held, chain), &body, data) {
        Ok(()) => eprintln!("fortis-lsp: woke {node} by push for {amount_msat} msat"),
        Err(SendError::Unregistered) => {
            eprintln!("fortis-lsp: push token of {node} is no longer valid; dropped");
            let _ = ledger.drop_push_token(node, &token);
        }
        Err(SendError::Other(e)) => eprintln!("fortis-lsp: push to {node}: {e}"),
    }
}

/// Which push a title belongs to.
#[derive(Clone, Copy)]
enum Push {
    /// [`wake`]: a payment is held for an offline phone.
    Held,
    /// [`received`]: a payment landed on the balance.
    Received,
}

/// The notification title of a push. XBT names its chain; BTC keeps the
/// original wording.
fn push_text(kind: Push, chain: &str) -> &'static str {
    match (kind, chain) {
        (Push::Held, "xbt") => "Incoming XBT Lightning payment",
        (Push::Held, _) => "Incoming Lightning payment",
        (Push::Received, "xbt") => "XBT Lightning payment received",
        (Push::Received, _) => "Lightning payment received",
    }
}

/// The data of a [`wake`] push; `chain` (`btc`/`xbt`) tells the app which wallet's LSP woke it.
fn wake_data(chain: &str, amount_msat: u64) -> Value {
    json!({ "kind": "ln_held", "amount_msat": amount_msat.to_string(), "chain": chain })
}

/// The data of a [`received`] push.
fn received_data(chain: &str, amount_msat: u64) -> Value {
    json!({ "kind": "ln_received", "amount_msat": amount_msat.to_string(), "chain": chain })
}

/// A payment landed on `node`'s Fortis balance while its phone was away:
/// "Lightning payment received: N sat". Only logs without push or a token.
pub fn received(fcm: Option<&Fcm>, ledger: &Ledger, node: &PublicKey, amount_msat: u64, chain: &str) {
    let body = format!(
        "{} sat received. It's on your Fortis balance and moves into your wallet when you open Fortis.",
        group(amount_msat / 1_000)
    );
    let data = received_data(chain, amount_msat);
    send_to(fcm, ledger, node, push_text(Push::Received, chain), &body, data, "received");
}

/// Sends one notification to `node`'s phone; `what` names it in the log.
fn send_to(fcm: Option<&Fcm>, ledger: &Ledger, node: &PublicKey, title: &str, body: &str, data: Value, what: &str) {
    let Some(fcm) = fcm else {
        eprintln!("fortis-lsp: {what} notice for {node} not sent (push not configured)");
        return;
    };
    let token = match ledger.push_token(node) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("fortis-lsp: {what} notice for {node} not sent (no push token)");
            return;
        }
        Err(e) => {
            eprintln!("fortis-lsp: push token for {node}: {e}");
            return;
        }
    };
    match fcm.send(&token, title, body, data) {
        Ok(()) => eprintln!("fortis-lsp: sent {what} notice to {node}"),
        Err(SendError::Unregistered) => {
            eprintln!("fortis-lsp: push token of {node} is no longer valid; dropped");
            let _ = ledger.drop_push_token(node, &token);
        }
        Err(SendError::Other(e)) => eprintln!("fortis-lsp: push to {node}: {e}"),
    }
}

/// 21000 → "21,000".
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(1_234_567), "1,234,567");
    }

    #[test]
    fn wake_payload_names_chain() {
        let xbt = crate::config::LspConfig::from_toml(
            "chain = \"xbt\"\nesplora_url = \"http://127.0.0.1:8094\"",
        )
        .unwrap();
        let d = wake_data(xbt.chain.as_str(), 21_000);
        assert_eq!(d["chain"], "xbt");
        assert_eq!(d["kind"], "ln_held");
        assert_eq!(d["amount_msat"], "21000");
        let btc = crate::config::LspConfig::from_toml("").unwrap();
        assert_eq!(wake_data(btc.chain.as_str(), 1)["chain"], "btc");
    }

    #[test]
    fn received_payload_names_chain() {
        let d = received_data("xbt", 5_000);
        assert_eq!(d["chain"], "xbt");
        assert_eq!(d["kind"], "ln_received");
        assert_eq!(d["amount_msat"], "5000");
        assert_eq!(received_data("btc", 1)["chain"], "btc");
    }

    #[test]
    fn xbt_push_text() {
        assert_eq!(push_text(Push::Held, "xbt"), "Incoming XBT Lightning payment");
        assert_eq!(push_text(Push::Received, "xbt"), "XBT Lightning payment received");
    }

    #[test]
    fn btc_push_text_unchanged() {
        assert_eq!(push_text(Push::Held, "btc"), "Incoming Lightning payment");
        assert_eq!(push_text(Push::Received, "btc"), "Lightning payment received");
    }

    #[test]
    fn a_key_file_without_the_fields_is_refused() {
        assert!(Fcm::from_json("{}").is_err());
        assert!(Fcm::from_json("not json").is_err());
        let bad_key = r#"{"project_id":"p","client_email":"e","private_key":"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n"}"#;
        assert!(Fcm::from_json(bad_key).is_err());
    }
}
