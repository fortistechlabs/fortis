//! A minimal Bitcoin Core / Knots JSON-RPC client over `ureq`.
//!
//! Only what the wallet needs: cookie/basic auth, top-level calls, and
//! wallet-scoped calls (`/wallet/<name>` in the path).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Rpc {
    base_url: String,
    /// Shared across `fresh()` clones so a cookie re-read by one is seen by all.
    auth_header: Arc<RwLock<String>>,
    /// Set by `from_cookie_file`: re-read on HTTP 401. `None` = static creds.
    cookie_path: Option<PathBuf>,
    timeout: Duration,
    agent: ureq::Agent,
}

fn basic_header(userpass: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(userpass.trim().as_bytes());
    format!("Basic {token}")
}

/// Outcome of one HTTP attempt that reached the node.
enum Attempt {
    Body(String),
    Unauthorized,
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
        Rpc {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_header: Arc::new(RwLock::new(basic_header(auth_userpass))),
            cookie_path: None,
            timeout,
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
        }
    }

    /// Build an [`Rpc`] using the node's `.cookie` file for auth.
    pub fn new_cookie(base_url: &str, datadir: &str, network: &str) -> Result<Self> {
        Ok(Self::new(base_url, &cookie_auth(datadir, network)?))
    }

    /// Build an [`Rpc`] from an explicit cookie file. On HTTP 401 (the node
    /// rewrites `.cookie` on every restart) the file is re-read once and the
    /// request retried once.
    pub fn from_cookie_file(base_url: &str, path: &Path) -> Result<Self> {
        let mut rpc = Self::new(base_url, &read_cookie(path)?);
        rpc.cookie_path = Some(path.to_path_buf());
        Ok(rpc)
    }

    /// Same url and (shared) auth, but its own `ureq::Agent` — one per fetch
    /// worker, so workers don't contend on a single connection pool.
    pub fn fresh(&self) -> Rpc {
        Rpc {
            agent: ureq::AgentBuilder::new().timeout(self.timeout).build(),
            ..self.clone()
        }
    }

    /// Same url and (shared, re-readable) auth with a different request
    /// timeout — e.g. a short one for calls made on behalf of an API client.
    pub fn with_timeout(&self, timeout: Duration) -> Rpc {
        Rpc {
            timeout,
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
            ..self.clone()
        }
    }

    /// A top-level RPC call (no wallet context).
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.request("", method, params)
    }

    /// A wallet-scoped RPC call, routed to `/wallet/<wallet>`.
    pub fn wallet_call(&self, wallet: &str, method: &str, params: Value) -> Result<Value> {
        self.request(&format!("/wallet/{}", urlencode(wallet)), method, params)
    }

    /// Many calls in one HTTP POST (JSON-RPC batch). Results are in request
    /// order (matched by id); a per-call error is an `Err` entry, while a
    /// transport/HTTP failure fails the whole batch.
    pub fn call_batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Result<Value>>> {
        if calls.is_empty() {
            return Ok(vec![]);
        }
        let payload: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| {
                json!({ "jsonrpc": "1.0", "id": i, "method": method, "params": params })
            })
            .collect();
        let body = self.send("", Value::Array(payload), "batch")?;
        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("RPC batch returned non-JSON: {body}"))?;
        let Value::Array(items) = v else {
            return Err(anyhow!("RPC batch: expected a JSON array, got: {body}"));
        };
        let mut out: Vec<Option<Result<Value>>> = calls.iter().map(|_| None).collect();
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_u64).map(|i| i as usize) else {
                continue;
            };
            if id < out.len() {
                out[id] = Some(parse_response(&item, calls[id].0));
            }
        }
        Ok(out
            .into_iter()
            .zip(calls)
            .map(|(r, (method, _))| {
                r.unwrap_or_else(|| Err(anyhow!("RPC `{method}`: no response in batch")))
            })
            .collect())
    }

    fn request(&self, path: &str, method: &str, params: Value) -> Result<Value> {
        let payload = json!({
            "jsonrpc": "1.0",
            "id": "fortis",
            "method": method,
            "params": params,
        });
        let body = self.send(path, payload, method)?;
        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("RPC `{method}` returned non-JSON: {body}"))?;
        parse_response(&v, method)
    }

    /// POST `payload`, re-reading the cookie file and retrying once on 401
    /// (only when built with `from_cookie_file`). Returns the response body.
    fn send(&self, path: &str, payload: Value, label: &str) -> Result<String> {
        if let Attempt::Body(b) = self.attempt(path, &payload, label)? {
            return Ok(b);
        }
        if let Some(p) = &self.cookie_path {
            if let Ok(creds) = read_cookie(p) {
                *self.auth_header.write().unwrap() = basic_header(&creds);
                if let Attempt::Body(b) = self.attempt(path, &payload, label)? {
                    return Ok(b);
                }
            }
        }
        Err(anyhow!(
            "RPC auth rejected (HTTP 401) — the cookie is stale or the \
             credentials are wrong. Restarting the node rewrites `.cookie`."
        ))
    }

    fn attempt(&self, path: &str, payload: &Value, label: &str) -> Result<Attempt> {
        let url = format!("{}{}", self.base_url, path);
        let auth = self.auth_header.read().unwrap().clone();
        let body = match self
            .agent
            .post(&url)
            .set("Authorization", &auth)
            .set("Content-Type", "application/json")
            .send_json(payload)
        {
            // Not `resp.into_string()`: ureq hard-caps that at 10MB and
            // errors with "response too big for into_string" past it — found
            // live indexing fortis-index into modern (2019+) blocks, whose
            // full-verbosity `getblock <hash> 2` JSON (every tx, every
            // input's prevout) routinely exceeds that on a busy block,
            // silently stalling the sync on the exact same block forever.
            // This is a trusted local node's response, not an untrusted
            // third party's — read it unbounded instead.
            Ok(resp) => {
                let mut body = String::new();
                resp.into_reader()
                    .read_to_string(&mut body)
                    .context("reading RPC response body")?;
                body
            }
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                let t = text.trim_start();
                if t.starts_with('{') || t.starts_with('[') {
                    // bitcoind returns HTTP 500 with a JSON-RPC error body.
                    text
                } else if code == 401 {
                    return Ok(Attempt::Unauthorized);
                } else {
                    return Err(anyhow!("RPC `{label}` failed: HTTP {code} {text}"));
                }
            }
            Err(e) => {
                return Err(anyhow!(
                    "cannot reach the node at {url}: {e}\nIs bitcoind running with `server=1`?"
                ))
            }
        };
        Ok(Attempt::Body(body))
    }
}

/// Extract `result` from one JSON-RPC response object, or its `error`.
fn parse_response(v: &Value, method: &str) -> Result<Value> {
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            return Err(anyhow!("RPC `{method}` error {code}: {msg}"));
        }
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

fn read_cookie(p: &Path) -> Result<String> {
    std::fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .with_context(|| {
            format!(
                "reading RPC cookie {} — is the node running with server=1, and is the datadir right?",
                p.display()
            )
        })
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
    use super::{urlencode, Rpc};
    use base64::Engine as _;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn wallet_names_are_path_safe() {
        assert_eq!(urlencode("fortis-xbt"), "fortis-xbt");
        assert_eq!(
            urlencode("Bitcoin Blake2b Wallet"),
            "Bitcoin%20Blake2b%20Wallet"
        );
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }

    type Seen = Arc<Mutex<Vec<String>>>;

    /// Tiny HTTP stub: for each connection, read one request (headers plus
    /// Content-Length body), record it, answer `handler(request)` as
    /// `(status, body)` with `Connection: close`.
    fn stub(handler: impl Fn(&str) -> (u16, String) + Send + 'static) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut c = conn.unwrap();
                let mut buf = Vec::new();
                let mut b = [0u8; 4096];
                let req = loop {
                    let n = c.read(&mut b).unwrap();
                    buf.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len || n == 0 {
                            break text;
                        }
                    } else if n == 0 {
                        break text;
                    }
                };
                log.lock().unwrap().push(req.clone());
                let (code, body) = handler(&req);
                let _ = write!(
                    c,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, seen)
    }

    #[test]
    fn with_timeout_bounds_a_slow_node_and_keeps_the_credentials() {
        let (url, seen) = stub(|_| {
            std::thread::sleep(std::time::Duration::from_millis(400));
            (200, r#"{"result":1,"error":null,"id":"fortis"}"#.into())
        });
        let slow = Rpc::new(&url, "u:p");
        let quick = slow.with_timeout(std::time::Duration::from_millis(100));
        let t = std::time::Instant::now();
        assert!(quick.call("getblockcount", json!([])).is_err());
        assert!(t.elapsed() < std::time::Duration::from_millis(350));
        assert_eq!(slow.call("getblockcount", json!([])).unwrap(), json!(1));
        let auth = base64::engine::general_purpose::STANDARD.encode("u:p");
        assert!(seen.lock().unwrap().iter().all(|r| r.contains(&auth)));
    }

    #[test]
    fn batch_results_come_back_in_request_order() {
        let (url, seen) = stub(|_| {
            (
                200,
                r#"[{"id":1,"result":"b","error":null},{"id":0,"result":"a","error":null}]"#.into(),
            )
        });
        let rpc = Rpc::new(&url, "u:p");
        let out = rpc
            .call_batch(&[("getblockhash", json!([0])), ("getblockhash", json!([1]))])
            .unwrap();
        assert_eq!(out[0].as_ref().unwrap(), &json!("a"));
        assert_eq!(out[1].as_ref().unwrap(), &json!("b"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(rpc.call_batch(&[]).unwrap().is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_call_inside_a_batch_is_an_err_entry_not_a_failed_batch() {
        let (url, _) = stub(|_| {
            (
                200,
                r#"[{"id":0,"result":7,"error":null},{"id":1,"result":null,"error":{"code":-8,"message":"Block height out of range"}}]"#.into(),
            )
        });
        let out = Rpc::new(&url, "u:p")
            .call_batch(&[("getblockhash", json!([0])), ("getblockhash", json!([9]))])
            .unwrap();
        assert!(out[0].is_ok());
        assert!(out[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("out of range"));
    }

    #[test]
    fn cookie_is_reread_after_401() {
        let want = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u:new")
        );
        let (url, seen) = stub(move |req| {
            if req.contains(&want) {
                (200, r#"{"result":42,"error":null,"id":"fortis"}"#.into())
            } else {
                (401, String::new())
            }
        });
        let dir = std::env::temp_dir().join(format!("fortis-rpc-cookie-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".cookie");
        std::fs::write(&path, "u:old\n").unwrap();
        let rpc = Rpc::from_cookie_file(&url, &path).unwrap();
        std::fs::write(&path, "u:new\n").unwrap();
        assert_eq!(rpc.call("getblockcount", json!([])).unwrap(), json!(42));
        assert_eq!(seen.lock().unwrap().len(), 2);
        // A fresh clone shares the refreshed credentials: no further 401.
        assert_eq!(
            rpc.fresh().call("getblockcount", json!([])).unwrap(),
            json!(42)
        );
        assert_eq!(seen.lock().unwrap().len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
