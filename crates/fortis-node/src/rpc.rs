//! A minimal Bitcoin Core / Knots JSON-RPC client over `ureq`.
//!
//! Only what the wallet needs: cookie/basic auth, top-level calls, and
//! wallet-scoped calls (`/wallet/<name>` in the path).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

pub struct Rpc {
    base_url: String,
    auth_header: String,
    agent: ureq::Agent,
}

impl Rpc {
    pub fn new(base_url: &str, auth_userpass: &str) -> Self {
        // 600 s: `getblock` on a cold prune cache can genuinely take minutes.
        Self::new_with_timeout(base_url, auth_userpass, Duration::from_secs(600))
    }

    /// Like [`new`](Self::new) but with an explicit request timeout — a caller
    /// that only does quick calls (a reachability ping, `sendrawtransaction`)
    /// wants to fail fast, not hang for 10 minutes on a wedged node.
    pub fn new_with_timeout(base_url: &str, auth_userpass: &str, timeout: Duration) -> Self {
        let token =
            base64::engine::general_purpose::STANDARD.encode(auth_userpass.trim().as_bytes());
        Rpc {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_header: format!("Basic {token}"),
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
        }
    }

    /// Build an [`Rpc`] using the node's `.cookie` file for auth.
    pub fn new_cookie(base_url: &str, datadir: &str, network: &str) -> Result<Self> {
        Ok(Self::new(base_url, &cookie_auth(datadir, network)?))
    }

    /// A top-level RPC call (no wallet context).
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.request("", method, params)
    }

    /// A wallet-scoped RPC call, routed to `/wallet/<wallet>`.
    pub fn wallet_call(&self, wallet: &str, method: &str, params: Value) -> Result<Value> {
        self.request(&format!("/wallet/{}", urlencode(wallet)), method, params)
    }

    fn request(&self, path: &str, method: &str, params: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let payload = json!({
            "jsonrpc": "1.0",
            "id": "fortis",
            "method": method,
            "params": params,
        });

        let body = match self
            .agent
            .post(&url)
            .set("Authorization", &self.auth_header)
            .set("Content-Type", "application/json")
            .send_json(payload)
        {
            Ok(resp) => resp.into_string().context("reading RPC response body")?,
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                if text.trim_start().starts_with('{') {
                    // bitcoind returns HTTP 500 with a JSON-RPC error body.
                    text
                } else if code == 401 {
                    return Err(anyhow!(
                        "RPC auth rejected (HTTP 401) — the cookie is stale or the \
                         credentials are wrong. Restarting the node rewrites `.cookie`."
                    ));
                } else {
                    return Err(anyhow!("RPC `{method}` failed: HTTP {code} {text}"));
                }
            }
            Err(e) => {
                return Err(anyhow!(
                    "cannot reach the node at {url}: {e}\nIs bitcoind running with `server=1`?"
                ))
            }
        };

        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("RPC `{method}` returned non-JSON: {body}"))?;

        if let Some(err) = v.get("error") {
            if !err.is_null() {
                let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                return Err(anyhow!("RPC `{method}` error {code}: {msg}"));
            }
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Percent-encode a wallet name for use in the URL path.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Read the node's RPC cookie (`<datadir>[/<chain-subdir>]/.cookie`) as the
/// `user:password` string for HTTP Basic auth.
pub fn cookie_auth(datadir: &str, network: &str) -> Result<String> {
    let subdir = match network {
        "regtest" | "regtest-legacy" => "regtest",
        "testnet4" => "testnet4",
        "testnet" | "testnet3" => "testnet3",
        "signet" => "signet",
        _ => "",
    };
    let mut p = PathBuf::from(datadir);
    if !subdir.is_empty() {
        p.push(subdir);
    }
    p.push(".cookie");
    std::fs::read_to_string(&p)
        .map(|s| s.trim().to_string())
        .with_context(|| {
            format!(
                "reading RPC cookie {} — is the node running with server=1, and is the datadir right?",
                p.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::urlencode;

    #[test]
    fn wallet_names_are_path_safe() {
        assert_eq!(urlencode("fortis-xbt"), "fortis-xbt");
        assert_eq!(urlencode("Bitcoin Blake2b Wallet"), "Bitcoin%20Blake2b%20Wallet");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }
}
