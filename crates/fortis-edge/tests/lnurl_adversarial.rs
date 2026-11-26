//! Adversarial black-box tests for Lightning addresses (LUD-16) against a real `fortis-edge` process and
//! stub LSPs: hostile names, amounts, misbehaving or lying LSPs, races, rate limits, restarts. Self-contained.

#![allow(dead_code)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use common::{agent, bin, free_port, spawn, wait_up, Svc, TempDir};

const NODE_ID: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
const OTHER_NODE: &str = "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";

fn node(i: u32) -> String {
    format!("02{:064x}", i)
}

// ---------------------------------------------------------------- the edge

struct Edge {
    url: String,
    port: u16,
    _svc: Svc,
}

fn start_edge(dir: &TempDir, extra: &[&str]) -> Edge {
    let port = free_port();
    let p = |n: &str| dir.path(n).to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        "--bind".into(), format!("127.0.0.1:{port}"),
        "--xbt-upstream".into(), "http://127.0.0.1:9".into(),
        "--btc-upstream-fallback=".into(),
        "--secret-file".into(), p("edge.secret"),
        "--cache-dir".into(), p("edge-cache"),
        "--btc-history-dir".into(), p("edge-btc-history"),
        "--btc-scan-cache-dir".into(), p("edge-btc-scan-cache"),
        "--register-per-hour".into(), "1000".into(),
        "--workers".into(), "8".into(),
        // These tests change what the stub LSP answers from one request to the next.
        "--lnurl-limits-cache-secs".into(), "0".into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let svc = spawn(&bin("fortis-edge"), &argv, Some(dir.path(&format!("edge-{port}.log"))));
    let url = format!("http://127.0.0.1:{port}");
    wait_up(&agent(), &format!("{url}/"));
    Edge { url, port, _svc: svc }
}

/// An edge with Lightning addresses on, trusting `X-Forwarded-For` from loopback so a test can pick the
/// per-IP rate-limit bucket of each request.
fn names_edge(dir: &TempDir, btc: Option<&Stub>, xbt: Option<&Stub>, trust: bool) -> Edge {
    let db = dir.path("names.sqlite").to_string_lossy().into_owned();
    let mut a: Vec<&str> = vec!["--lnurl-domain", "fortis.cash", "--names-db", &db];
    if let Some(b) = btc {
        a.extend(["--lsp-admin-url", &b.url]);
    }
    if let Some(x) = xbt {
        a.extend(["--lsp-xbt-admin-url", &x.url]);
    }
    if trust {
        a.push("--trust-forwarded-for");
    }
    start_edge(dir, &a)
}

fn http(url: &str, method: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let a = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).redirects(0).build();
    let mut r = a.request(method, url);
    for (k, v) in headers {
        r = r.set(k, v);
    }
    let resp = match body {
        Some(b) => r.send_bytes(b),
        None => r.call(),
    };
    let r = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return (0, vec![], e.to_string().into_bytes()),
    };
    let status = r.status();
    let hs = r
        .headers_names()
        .into_iter()
        .map(|n| {
            let v = r.header(&n).unwrap_or_default().to_string();
            (n.to_ascii_lowercase(), v)
        })
        .collect();
    let mut out = Vec::new();
    r.into_reader().read_to_end(&mut out).unwrap();
    (status, hs, out)
}

fn token(edge: &Edge) -> String {
    let (s, _, b) = http(&format!("{}/register", edge.url), "POST", &[], Some(b""));
    assert_eq!(s, 200);
    serde_json::from_slice::<Value>(&b).unwrap()["token"].as_str().unwrap().to_string()
}

/// A request line sent verbatim (no client-side normalisation). `(status, body)`; status 0 = no answer.
fn raw(port: u16, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    s.write_all(head.as_bytes()).unwrap();
    let _ = s.write_all(body);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").map_or(out.len(), |i| i + 4);
    let status = String::from_utf8_lossy(&out[..split]).split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, out[split..].to_vec())
}

static IP: AtomicU32 = AtomicU32::new(1);

/// A fresh fake client IP (for `X-Forwarded-For`, so each request has its own rate-limit bucket).
fn fresh_ip() -> String {
    let n = IP.fetch_add(1, Ordering::Relaxed);
    format!("10.{}.{}.{}", (n >> 16) & 255, (n >> 8) & 255, n & 255)
}

/// GET with a fresh IP; `(status, json)`.
fn get(edge: &Edge, target: &str) -> (u16, Value) {
    let ip = fresh_ip();
    let (s, b) = raw(edge.port, "GET", target, &[("X-Forwarded-For", &ip)], b"");
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn is_lud06_error(v: &Value) -> bool {
    v["status"] == "ERROR" && v["reason"].is_string()
}

/// POST /ln/account with the given raw body; `(status, body bytes)`.
fn acct_raw(edge: &Edge, tok: &str, chain: &str, ip: &str, body: &[u8]) -> (u16, Vec<u8>) {
    raw(
        edge.port,
        "POST",
        &format!("/ln/account?chain={chain}"),
        &[
            ("Authorization", &format!("Bearer {tok}")),
            ("X-Fortis-Sig", "sig123abc"),
            ("Content-Type", "application/json"),
            ("X-Forwarded-For", ip),
        ],
        body,
    )
}

fn op_body(chain: &str, node: &str, op: &str, name: Option<&str>) -> Vec<u8> {
    // The chain is part of what the wallet signs (the edge refuses a name operation for another chain).
    let mut params = json!({ "chain": chain });
    if let Some(n) = name {
        params["name"] = json!(n);
    }
    json!({ "node_id": node, "op": op, "params": params, "nonce": "n".repeat(20), "expires_at": 1 }).to_string().into_bytes()
}

fn claim(edge: &Edge, tok: &str, chain: &str, node: &str, name: &str) -> (u16, Value) {
    let (s, b) = acct_raw(edge, tok, chain, &fresh_ip(), &op_body(chain, node, "claim_name", Some(name)));
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn my_name(edge: &Edge, tok: &str, chain: &str, node: &str) -> (u16, Value) {
    let (s, b) = acct_raw(edge, tok, chain, &fresh_ip(), &op_body(chain, node, "my_name", None));
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn resolves(edge: &Edge, name: &str) -> bool {
    get(edge, &format!("/.well-known/lnurlp/{name}")).1["tag"] == "payRequest"
}

// ---------------------------------------------------------------- the stub LSP

#[derive(Clone, Debug)]
struct Seen {
    url: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

type Reply = (u16, &'static str, Vec<u8>);
type Handler = Arc<dyn Fn(&str, &[u8]) -> Reply + Send + Sync>;

struct Stub {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    server: Arc<Server>,
}

impl Stub {
    fn start(handler: Handler) -> Stub {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..8 {
            let (server, seen, handler) = (Arc::clone(&server), Arc::clone(&seen), Arc::clone(&handler));
            std::thread::spawn(move || {
                for mut rq in server.incoming_requests() {
                    let mut body = Vec::new();
                    let _ = rq.as_reader().read_to_end(&mut body);
                    let headers = rq.headers().iter().map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.to_string())).collect();
                    let url = rq.url().to_string();
                    seen.lock().unwrap().push(Seen { url: url.clone(), body: body.clone(), headers });
                    let (status, ct, out) = handler(&url, &body);
                    let resp = Response::from_data(out).with_status_code(status).with_header(Header::from_bytes("Content-Type", ct).unwrap());
                    let _ = rq.respond(resp);
                }
            });
        }
        Stub { url: format!("http://127.0.0.1:{port}/vss"), seen, server }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn count(&self, url: &str) -> usize {
        self.seen().iter().filter(|r| r.url == url).count()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        for _ in 0..8 {
            self.server.unblock();
        }
    }
}

fn json_reply(v: Value) -> Reply {
    (200, "application/json", v.to_string().into_bytes())
}

/// The well-behaved fake LSP (as in `ln_routes.rs`).
fn good(url: &str, body: &[u8]) -> Reply {
    let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match url {
        "/vss/account" => json_reply(json!({ "node_id": b["node_id"], "op": b["op"], "params": b["params"] })),
        "/vss/lnurl/limits" => json_reply(json!({ "min_msat": 1_000, "max_msat": 50_000_000 })),
        "/vss/lnurl/invoice" => json_reply(json!({ "invoice": format!("lnbc-fake-{}", b["amount_msat"]) })),
        _ => (404, "application/json", b"{}".to_vec()),
    }
}

fn good_stub() -> Stub {
    Stub::start(Arc::new(good))
}

// ================================================================ lookups

#[test]
fn a_name_is_found_whatever_case_the_payer_types_it_in() {
    let dir = TempDir::new("lnadv-case");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);
    for n in ["donal", "DONAL", "Donal", "dOnAl"] {
        let (s, v) = get(&edge, &format!("/.well-known/lnurlp/{n}"));
        assert_eq!((s, v["callback"].as_str()), (200, Some("https://fortis.cash/lnurlp/callback/donal")), "{n}");
    }
}

#[test]
fn hostile_names_in_the_pay_request_url_never_resolve_and_never_reach_the_lsp() {
    let dir = TempDir::new("lnadv-names");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);
    assert_eq!(claim(&edge, &tok, "btc", OTHER_NODE, "a.b-c_d").0, 200);
    let before = btc.count("/vss/lnurl/limits");

    let ten_kb = "a".repeat(10 * 1024);
    let bad: Vec<String> = vec![
        "%44onal".into(), "%64onal".into(), "d%6fnal".into(), // percent-encoded letters are not decoded
        "donal%00".into(), "%00donal".into(), "donal%0a".into(), "donal%0d%0aX-Evil:1".into(),
        "donal%2f".into(), "donal%2F..%2F".into(), "donal/x".into(), "x/donal".into(), "donal//x".into(),
        "..".into(), "../donal".into(), "..%2fdonal".into(), "%2e%2e".into(), "donal/..".into(),
        ".donal".into(), "donal.".into(), "-donal".into(), "do..nal".into(),
        "d%C3%B6nal".into(), "dönal".into(), "%E2%80%AEdonal".into(), "do%CC%81nal".into(),
        "donal%20".into(), "do%20nal".into(), "donal+".into(), "donal%25".into(), "donal%".into(), "donal%zz".into(),
        "'%20OR%201=1--".into(), "donal'--".into(), "%27".into(), "donal;".into(), "donal@fortis.cash".into(),
        "do".into(), "ab".into(), "a".into(), "a".repeat(33), ten_kb,
        "admin".into(), "root".into(), "xbt".into(), "nobody".into(),
    ];
    for name in &bad {
        let t = format!("/.well-known/lnurlp/{name}");
        let (s, v) = get(&edge, &t);
        let shown: String = t.chars().take(80).collect();
        // tiny_http may drop a request line with raw non-ASCII bytes (status 0); the edge must survive it.
        let hangup_ok = s == 0 && !name.is_ascii();
        assert!(hangup_ok || s == 200 || s == 400 || s == 404 || s == 414 || s == 431, "{shown}: status {s}");
        assert_ne!(v["tag"], "payRequest", "{shown} resolved: {v}");
        if s == 200 {
            assert!(is_lud06_error(&v), "{shown}: {v}");
        }
        // The callback too.
        let t = format!("/lnurlp/callback/{name}?amount=5000");
        let (s, v) = get(&edge, &t);
        assert!(v["pr"].as_str().is_none(), "{shown} gave an invoice: {v}");
        assert!((s == 0 && !name.is_ascii()) || s == 200 || s == 400 || s == 404 || s == 414 || s == 431, "{shown}: status {s}");
    }
    assert_eq!(btc.count("/vss/lnurl/limits"), before, "no hostile name reached the LSP");
    assert_eq!(btc.count("/vss/lnurl/invoice"), 0);

    // The edge is still alive and the legitimate names still work, dots and dashes included.
    assert!(resolves(&edge, "donal"));
    assert!(resolves(&edge, "a.b-c_d"));
    // `?` ends the name (it is a query); a trailing slash is trimmed. Both resolve to the real name only.
    assert!(resolves(&edge, "donal?x=1"));
    assert!(resolves(&edge, "donal/"));
    // A raw `..` path (not normalised by a client) is not a path traversal.
    let (s, b) = raw(edge.port, "GET", "/.well-known/lnurlp/../lnurlp/donal", &[("X-Forwarded-For", &fresh_ip())], b"");
    assert!(s != 0 && !String::from_utf8_lossy(&b).contains("payRequest"), "{s} {}", String::from_utf8_lossy(&b));
    // The bare prefix has no name.
    for t in ["/.well-known/lnurlp/", "/.well-known/lnurlp", "/lnurlp/callback/", "/lnurlp/callback"] {
        let (s, v) = get(&edge, t);
        assert!(s == 404 || (s == 200 && is_lud06_error(&v)), "{t}: {s} {v}");
    }
}

#[test]
fn lookup_and_callback_headers_and_methods() {
    let dir = TempDir::new("lnadv-methods");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);
    for m in ["POST", "PUT", "DELETE"] {
        let (s, b) = raw(edge.port, m, "/.well-known/lnurlp/donal", &[("X-Forwarded-For", &fresh_ip())], b"{}");
        assert!(s != 200 || !String::from_utf8_lossy(&b).contains("payRequest"), "{m} answered a pay request");
    }
    let (s, _) = raw(edge.port, "OPTIONS", "/.well-known/lnurlp/donal", &[("X-Forwarded-For", &fresh_ip())], b"");
    assert!(s == 204 || s == 200, "CORS preflight: {s}");
    let (s, h, _) = http(&format!("{}/.well-known/lnurlp/donal", edge.url), "GET", &[("X-Forwarded-For", &fresh_ip())], None);
    assert_eq!(s, 200);
    assert!(h.iter().any(|(k, v)| k == "content-type" && v.starts_with("application/json")));
    assert!(h.iter().any(|(k, v)| k == "cache-control" && v.contains("no-store")), "answers must not be cached: {h:?}");
}

// ================================================================ amounts

#[test]
fn hostile_amounts_never_reach_the_lsp_as_an_invoice() {
    let dir = TempDir::new("lnadv-amount");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);

    let bad = [
        "", "amount", "amount=", "amount=0", "amount=-5000", "amount=-0", "amount=+5000", "amount=0x1388", "amount=0X1388",
        "amount=1e4", "amount=5000.0", "amount=5000.", "amount=%35000", "amount=5%20000", "amount=5000%20", "amount=%205000",
        "amount=5000%00", "amount=5000%0a", "amount=5%2C000", "amount=5,000", "amount=%EF%BC%95000", "amount=%D9%A5%D9%A0%D9%A0%D9%A0",
        "amount=99999999999999999999", "amount=999999999999999999", "amount=18446744073709551616", "amount=9223372036854775808",
        "amount=50000001", "amount=999", "amount=NaN", "amount=null", "amount=[5000]", "amount[]=5000", "Amount=5000", "AMOUNT=5000",
        "amount%20=5000", "%20amount=5000", "xamount=5000", "amount=5000x", "amount=%D9%A1%D9%A2%D9%A3", "amount=" , "amounts=5000",
        &format!("amount={}", "9".repeat(10_000)),
    ];
    for q in bad {
        let t = format!("/lnurlp/callback/donal?{q}");
        let (s, v) = get(&edge, &t);
        let shown: String = t.chars().take(80).collect();
        assert_eq!(s, 200, "{shown}");
        assert!(is_lud06_error(&v), "{shown}: {v}");
    }
    assert_eq!(btc.count("/vss/lnurl/invoice"), 0, "no bad amount produced an invoice request");

    // Boundaries are inclusive; the amount the LSP is asked for is exactly the one given.
    for (a, ok) in [(1_000u64, true), (50_000_000, true), (999, false), (50_000_001, false)] {
        let (_, v) = get(&edge, &format!("/lnurlp/callback/donal?amount={a}"));
        assert_eq!(v["pr"].as_str().is_some(), ok, "{a}: {v}");
    }
    // Twice: whatever the edge decides, it may only invoice an amount that was actually offered.
    let (_, v) = get(&edge, "/lnurlp/callback/donal?amount=5000&amount=6000");
    if let Some(pr) = v["pr"].as_str() {
        assert!(pr == "lnbc-fake-5000" || pr == "lnbc-fake-6000", "{pr}");
    } else {
        assert!(is_lud06_error(&v));
    }
    let asked: Vec<u64> = btc
        .seen()
        .iter()
        .filter(|r| r.url == "/vss/lnurl/invoice")
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["amount_msat"].as_u64().unwrap())
        .collect();
    assert!(asked.starts_with(&[1_000, 50_000_000]), "{asked:?}");
    assert!(asked.iter().all(|a| (1_000..=50_000_000).contains(a)));
    // Extra unrelated parameters are harmless.
    let (_, v) = get(&edge, "/lnurlp/callback/donal?nonce=abc&amount=7000&comment=hi&fromnodes=x");
    assert_eq!(v["pr"], "lnbc-fake-7000");
}

// ================================================================ LSP misbehaviour

#[test]
fn a_misbehaving_lsp_always_gives_lud06_error_json() {
    let dir = TempDir::new("lnadv-lspbad");
    let mode = Arc::new(Mutex::new(String::new()));
    let m = Arc::clone(&mode);
    let btc = Stub::start(Arc::new(move |url, body| {
        let mode = m.lock().unwrap().clone();
        let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        match (url, mode.as_str()) {
            ("/vss/account", _) => json_reply(json!({ "node_id": b["node_id"], "op": b["op"], "params": b["params"] })),
            ("/vss/lnurl/limits", "limits500") => (500, "text/plain", b"boom".to_vec()),
            ("/vss/lnurl/limits", "limitsgarbage") => (200, "application/json", b"\x00\xff not json".to_vec()),
            ("/vss/lnurl/limits", "limitsempty") => (200, "application/json", Vec::new()),
            ("/vss/lnurl/limits", "limitsinverted") => json_reply(json!({ "min_msat": 10, "max_msat": 1 })),
            ("/vss/lnurl/limits", "limitsstrings") => json_reply(json!({ "min_msat": "1000", "max_msat": "5000" })),
            ("/vss/lnurl/limits", "limitsnegative") => json_reply(json!({ "min_msat": -1, "max_msat": 5000 })),
            ("/vss/lnurl/limits", "limitsnull") => json_reply(json!({})),
            ("/vss/lnurl/limits", "limits404") => (404, "application/json", b"{}".to_vec()),
            ("/vss/lnurl/limits", _) => json_reply(json!({ "min_msat": 1_000, "max_msat": 50_000_000 })),
            ("/vss/lnurl/invoice", "inv500") => (500, "text/plain", b"boom".to_vec()),
            ("/vss/lnurl/invoice", "inv502") => (502, "application/json", br#"{"error":"node"}"#.to_vec()),
            ("/vss/lnurl/invoice", "invgarbage") => (200, "application/json", b"<html>".to_vec()),
            ("/vss/lnurl/invoice", "invempty") => (200, "application/json", Vec::new()),
            ("/vss/lnurl/invoice", "invnoinvoice") => json_reply(json!({ "x": 1 })),
            ("/vss/lnurl/invoice", "invnumber") => json_reply(json!({ "invoice": 5 })),
            ("/vss/lnurl/invoice", "invnull") => json_reply(json!({ "invoice": null })),
            ("/vss/lnurl/invoice", "inv409") => (409, "application/json", br#"{"error":"custody_full","min_channel_sat":1}"#.to_vec()),
            ("/vss/lnurl/invoice", "inv400") => (400, "application/json", br#"{"error":"amount_msat"}"#.to_vec()),
            ("/vss/lnurl/invoice", "inv401") => (401, "application/json", b"{}".to_vec()),
            ("/vss/lnurl/invoice", "inv200err") => (200, "application/json", br#"{"status":"ERROR","reason":"x"}"#.to_vec()),
            ("/vss/lnurl/invoice", _) => json_reply(json!({ "invoice": "lnbc-ok" })),
            _ => (404, "application/json", b"{}".to_vec()),
        }
    }));
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);

    for m in [
        "limits500", "limitsgarbage", "limitsempty", "limitsinverted", "limitsstrings", "limitsnegative", "limitsnull", "limits404",
        "inv500", "inv502", "invgarbage", "invempty", "invnoinvoice", "invnumber", "invnull", "inv409", "inv400", "inv401", "inv200err",
    ] {
        *mode.lock().unwrap() = m.to_string();
        let (s, v) = get(&edge, "/lnurlp/callback/donal?amount=5000");
        assert_eq!(s, 200, "{m}");
        assert!(is_lud06_error(&v), "{m}: {v}");
        assert!(v.get("pr").is_none(), "{m}: {v}");
        if m.starts_with("limits") {
            let (s, v) = get(&edge, "/.well-known/lnurlp/donal");
            assert_eq!(s, 200, "{m}");
            assert!(is_lud06_error(&v), "{m} pay request: {v}");
            assert!(v.get("tag").is_none());
        }
    }
    // The reason for an LSP-side refusal never leaks upstream details.
    *mode.lock().unwrap() = "inv409".into();
    let (_, v) = get(&edge, "/lnurlp/callback/donal?amount=5000");
    assert!(!v.to_string().contains("custody_full") && !v.to_string().contains("127.0.0.1"), "{v}");
    *mode.lock().unwrap() = "inv500".into();
    let (_, v) = get(&edge, "/lnurlp/callback/donal?amount=5000");
    assert!(!v.to_string().contains("boom") && !v.to_string().contains("127.0.0.1"), "{v}");
    // Recovery: the same name works as soon as the LSP does.
    *mode.lock().unwrap() = "ok".into();
    assert_eq!(get(&edge, "/lnurlp/callback/donal?amount=5000").1["pr"], "lnbc-ok");
}

#[test]
fn an_lsp_that_hangs_is_cut_off_and_does_not_block_other_requests() {
    let dir = TempDir::new("lnadv-hang");
    let hang = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hang);
    let btc = Stub::start(Arc::new(move |url, body| {
        if h.load(Ordering::SeqCst) == 1 && url != "/vss/account" {
            std::thread::sleep(Duration::from_secs(26));
        }
        if h.load(Ordering::SeqCst) == 2 && url == "/vss/account" {
            std::thread::sleep(Duration::from_secs(26));
        }
        good(url, body)
    }));
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);

    // 1. limits/invoice hang: LUD-06 error within the edge's own timeout, other routes stay up.
    hang.store(1, Ordering::SeqCst);
    let port = edge.port;
    let t0 = Instant::now();
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let ip = fresh_ip();
            std::thread::spawn(move || raw(port, "GET", "/lnurlp/callback/donal?amount=5000", &[("X-Forwarded-For", &ip)], b""))
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    let t1 = Instant::now();
    let (s, _, _) = http(&format!("{}/", edge.url), "GET", &[], None);
    assert_eq!(s, 200);
    assert!(t1.elapsed() < Duration::from_secs(3), "the root route waited behind hung lookups");
    assert!(get(&edge, "/.well-known/lnurlp/nobody").1["status"] == "ERROR", "a lookup that needs no LSP is served while others hang");
    for hd in handles {
        let (s, b) = hd.join().unwrap();
        let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
        assert_eq!(s, 200);
        assert!(is_lud06_error(&v), "{v}");
    }
    assert!(t0.elapsed() < Duration::from_secs(25), "answered after {:?}", t0.elapsed());

    // 2. /account hangs on a claim: gateway error, no name written.
    hang.store(2, Ordering::SeqCst);
    let t0 = Instant::now();
    let (s, v) = claim(&edge, &tok, "btc", OTHER_NODE, "hanger");
    assert_eq!(s, 502, "{v}");
    assert!(t0.elapsed() < Duration::from_secs(25));
    hang.store(0, Ordering::SeqCst);
    // The stub finishes its late answer afterwards; that must not create the name.
    std::thread::sleep(Duration::from_secs(8));
    assert!(!resolves(&edge, "hanger"));
    assert_eq!(my_name(&edge, &tok, "btc", OTHER_NODE).1, json!({ "name": null }));
}

#[test]
fn name_ops_for_an_unconfigured_chain_are_404_and_write_nothing() {
    let dir = TempDir::new("lnadv-down");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "donal").0, 200);
    // The XBT LSP is not configured at all.
    let (s, _) = claim(&edge, &tok, "xbt", OTHER_NODE, "xbtname");
    assert_eq!(s, 404);
    assert!(!resolves(&edge, "xbtname"));
}

// ================================================================ lying LSP on /account

fn echo_stub(echo: fn(&Value) -> (u16, Vec<u8>)) -> Stub {
    Stub::start(Arc::new(move |url, body| {
        let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        if url == "/vss/account" {
            let (s, out) = echo(&b);
            (s, "application/json", out)
        } else {
            good(url, body)
        }
    }))
}

type EchoFn = fn(&Value) -> (u16, Vec<u8>);

fn mismatch_echoes() -> Vec<(&'static str, EchoFn)> {
    fn ok(v: Value) -> (u16, Vec<u8>) {
        (200, v.to_string().into_bytes())
    }
    vec![
        ("otherop", |b| ok(json!({ "node_id": b["node_id"], "op": "my_name", "params": b["params"] }))),
        ("otherop2", |b| ok(json!({ "node_id": b["node_id"], "op": "release_name", "params": b["params"] }))),
        ("upperop", |b| ok(json!({ "node_id": b["node_id"], "op": "CLAIM_NAME", "params": b["params"] }))),
        ("noop", |b| ok(json!({ "node_id": b["node_id"], "params": b["params"] }))),
        ("numop", |b| ok(json!({ "node_id": b["node_id"], "op": 1, "params": b["params"] }))),
        ("nonode", |b| ok(json!({ "op": b["op"], "params": b["params"] }))),
        ("nullnode", |b| ok(json!({ "node_id": null, "op": b["op"], "params": b["params"] }))),
        ("numnode", |b| ok(json!({ "node_id": 5, "op": b["op"], "params": b["params"] }))),
        ("nonhex", |b| ok(json!({ "node_id": "z".repeat(66), "op": b["op"], "params": b["params"] }))),
        ("short", |b| ok(json!({ "node_id": "02ab", "op": b["op"], "params": b["params"] }))),
        ("long", |b| ok(json!({ "node_id": format!("{}00", b["node_id"].as_str().unwrap_or("")), "op": b["op"], "params": b["params"] }))),
        ("sql", |b| ok(json!({ "node_id": format!("02'; DROP TABLE names;--{}", "0".repeat(47)), "op": b["op"], "params": b["params"] }))),
        ("empty", |_| (200, Vec::new())),
        ("garbage", |_| (200, b"\x00\xff{{".to_vec())),
        ("array", |_| (200, b"[1,2,3]".to_vec())),
        ("null", |_| (200, b"null".to_vec())),
        ("201", |b| (201, json!({ "node_id": b["node_id"], "op": b["op"], "params": b["params"] }).to_string().into_bytes())),
        ("204", |_| (204, Vec::new())),
        ("500", |_| (500, b"boom".to_vec())),
        ("nameasnum", |b| ok(json!({ "node_id": b["node_id"], "op": b["op"], "params": { "name": 12345 } }))),
        ("noparams", |b| ok(json!({ "node_id": b["node_id"], "op": b["op"] }))),
        ("nullparams", |b| ok(json!({ "node_id": b["node_id"], "op": b["op"], "params": null }))),
        ("badname", |b| ok(json!({ "node_id": b["node_id"], "op": b["op"], "params": { "name": "ADMIN" } }))),
        ("dupfields", |b| {
            let raw = format!(
                r#"{{"node_id":{},"op":"claim_name","params":{{"name":"dupname"}},"node_id":"{}"}}"#,
                b["node_id"],
                "02".to_string() + &"ab".repeat(32)
            );
            (200, raw.into_bytes())
        }),
    ]
}

#[test]
fn a_mismatched_or_malformed_echo_from_the_lsp_writes_no_name() {
    let dir = TempDir::new("lnadv-echo");
    // One stub per scenario would be slow; one stub whose behaviour is picked by the node id's last byte.
    let table = mismatch_echoes();
    let names: Vec<&'static str> = table.iter().map(|(n, _)| *n).collect();
    let table = Arc::new(table);
    let t = Arc::clone(&table);
    let btc = Stub::start(Arc::new(move |url, body| {
        let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        if url != "/vss/account" {
            return good(url, body);
        }
        let idx = b["params"]["scenario"].as_u64().unwrap_or(0) as usize;
        let (s, out) = (t[idx].1)(&b);
        (s, "application/json", out)
    }));
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    let mut wrote = Vec::new();
    for (i, scenario) in names.iter().enumerate() {
        let claimed = format!("tst{i:02}x");
        let body = json!({
            "node_id": node(1000 + i as u32), "op": "claim_name",
            "params": { "name": claimed, "scenario": i }, "nonce": "n".repeat(20), "expires_at": 1,
        })
        .to_string();
        let (s, out) = acct_raw(&edge, &tok, "btc", &fresh_ip(), body.as_bytes());
        // a name that a lying echo might substitute
        let written = resolves(&edge, &claimed) || resolves(&edge, "dupname") || resolves(&edge, "12345");
        eprintln!("{scenario}: status {s} body {:?} written={written}", String::from_utf8_lossy(&out));
        if written {
            wrote.push(*scenario);
        }
        assert!(s != 200 || *scenario == "x", "{scenario}: a lying echo must not be answered 200 (got {s})");
    }
    assert!(wrote.is_empty(), "names were written from a mismatched/malformed echo: {wrote:?}");
}

/// The one deviation that is only checkable against the body: the echo names a different (valid) wallet or
/// a different name than the request asked for.
#[test]
fn an_echo_for_another_wallet_or_another_name_is_not_applied() {
    let dir = TempDir::new("lnadv-echo2");
    let btc = Stub::start(Arc::new(|url, body| {
        let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        if url != "/vss/account" {
            return good(url, body);
        }
        // The LSP "verified" some other wallet and some other name than the request carried.
        json_reply(json!({ "node_id": OTHER_NODE, "op": b["op"], "params": { "name": "swapped" } }))
    }));
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    let (s, v) = claim(&edge, &tok, "btc", NODE_ID, "wantedname");
    assert!(!resolves(&edge, "swapped") && !resolves(&edge, "wantedname"), "a claim was applied from an echo that disagrees with the request: {s} {v}");
}

#[test]
fn an_echo_with_a_differently_cased_node_id_cannot_give_one_wallet_two_names() {
    let dir = TempDir::new("lnadv-case");
    let flip = Arc::new(AtomicUsize::new(0));
    let f = Arc::clone(&flip);
    let btc = Stub::start(Arc::new(move |url, body| {
        let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        if url != "/vss/account" {
            return good(url, body);
        }
        let n = b["node_id"].as_str().unwrap_or("");
        let n = if f.load(Ordering::SeqCst) == 1 { n.to_uppercase() } else { n.to_string() };
        json_reply(json!({ "node_id": n, "op": b["op"], "params": b["params"] }))
    }));
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    let node_with_letters = "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";
    assert_eq!(claim(&edge, &tok, "btc", node_with_letters, "first1").0, 200);
    flip.store(1, Ordering::SeqCst);
    let (s, v) = claim(&edge, &tok, "btc", node_with_letters, "second1");
    assert_eq!(s, 409, "the same wallet (upper-cased id) got a second name: {v}");
    assert!(!resolves(&edge, "second1"));
}

// ================================================================ request bodies

#[test]
fn name_ops_are_recognised_exactly_and_everything_else_is_forwarded_untouched() {
    let dir = TempDir::new("lnadv-bodies");
    let btc = Stub::start(Arc::new(|url, body| {
        if url != "/vss/account" {
            return good(url, body);
        }
        let s = String::from_utf8_lossy(body);
        let status = if s.contains("STATUS402") { 402 } else if s.contains("STATUS401") { 401 } else if s.contains("STATUS409") { 409 } else { 200 };
        (status, "application/json", body.iter().rev().copied().collect())
    }));
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);

    // Not name ops (or not recognised as such): relayed byte for byte, status included, nothing written.
    let mut bodies: Vec<(Vec<u8>, u16)> = vec![
        (br#"{"op":"balance","n":1}"#.to_vec(), 200),
        (br#"{"op":"balance","n":1,"STATUS402":1}"#.to_vec(), 402),
        (br#"{"op":"invoice","STATUS401":1}"#.to_vec(), 401),
        (br#"{"op":"claim_nameSTATUS409"}"#.to_vec(), 409),
        (br#"{"op":"CLAIM_NAME","params":{"name":"upper1"}}"#.to_vec(), 200),
        (br#"{"op":"Claim_Name","params":{"name":"upper2"}}"#.to_vec(), 200),
        (br#"{"op":" claim_name","params":{"name":"upper3"}}"#.to_vec(), 200),
        (br#"{"op":"claim_name ","params":{"name":"upper4"}}"#.to_vec(), 200),
        (br#"{"op":"claim_name\u0000","params":{"name":"upper5"}}"#.to_vec(), 200),
        (br#"{"op":["claim_name"],"params":{"name":"upper6"}}"#.to_vec(), 200),
        (br#"{"op":{"x":"claim_name"},"params":{"name":"upper7"}}"#.to_vec(), 200),
        (br#"{"op":"balance","params":{"op":"claim_name","name":"nested"}}"#.to_vec(), 200),
        (br#"{"Op":"claim_name","params":{"name":"keycase"}}"#.to_vec(), 200),
        (br#"[{"op":"claim_name","params":{"name":"inarray"}}]"#.to_vec(), 200),
        (b"not json at all".to_vec(), 200),
        ((0..=255u8).collect(), 200),
        (Vec::new(), 200),
        (br#"{"op":"balance"}   "#.to_vec(), 200),
    ];
    bodies.push((format!(r#"{{"op":"balance","pad":"{}"}}"#, "x".repeat(16 * 1024 - 40)).into_bytes(), 200));
    for (body, want) in &bodies {
        let (s, out) = acct_raw(&edge, &tok, "btc", &fresh_ip(), body);
        let shown = String::from_utf8_lossy(&body[..body.len().min(60)]).to_string();
        assert_eq!(s, *want, "{shown}");
        assert_eq!(out, body.iter().rev().copied().collect::<Vec<u8>>(), "response not relayed byte-for-byte: {shown}");
    }
    let seen = btc.seen();
    let sent: Vec<&Seen> = seen.iter().filter(|r| r.url == "/vss/account").collect();
    assert_eq!(sent.len(), bodies.len());
    for (r, (b, _)) in sent.iter().zip(&bodies) {
        assert_eq!(&r.body, b, "request body altered on the way to the LSP");
        assert!(r.headers.iter().any(|(k, v)| k == "x-fortis-sig" && v == "sig123abc"));
    }
    for n in ["upper1", "upper2", "upper3", "upper4", "upper5", "upper6", "upper7", "nested", "keycase", "inarray"] {
        assert!(!resolves(&edge, n), "{n} was claimed through a non-name op");
    }

    // Over 16 KiB is refused before the LSP.
    let big = format!(r#"{{"op":"claim_name","pad":"{}"}}"#, "x".repeat(16 * 1024));
    let n = btc.count("/vss/account");
    assert_eq!(acct_raw(&edge, &tok, "btc", &fresh_ip(), big.as_bytes()).0, 413);
    assert_eq!(btc.count("/vss/account"), n);

    // A name op relayed with a non-200 (the LSP refused) is relayed as is and writes nothing.
    let refused = br#"{"node_id":"02aa","op":"claim_name","params":{"name":"STATUS409zz"}}"#;
    let (s, out) = acct_raw(&edge, &tok, "btc", &fresh_ip(), refused);
    assert_eq!(s, 409);
    assert_eq!(out, refused.iter().rev().copied().collect::<Vec<u8>>());
}

#[test]
fn a_name_op_with_the_op_key_twice_is_decided_by_one_value_the_lsp_also_sees() {
    let dir = TempDir::new("lnadv-dupop");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    let node = node(77);
    for (i, body) in [
        format!(r#"{{"node_id":"{node}","op":"balance","op":"claim_name","params":{{"name":"dupop1"}}}}"#),
        format!(r#"{{"node_id":"{node}","op":"claim_name","op":"balance","params":{{"name":"dupop2"}}}}"#),
        format!(r#"{{"node_id":"{node}","op":"claim_name","params":{{"name":"dupname1","name":"dupname2"}}}}"#),
    ]
    .iter()
    .enumerate()
    {
        let (s, out) = acct_raw(&edge, &tok, "btc", &fresh_ip(), body.as_bytes());
        eprintln!("{i}: {s} {}", String::from_utf8_lossy(&out));
        assert!(s < 500, "{i}: {s}");
    }
    // Whatever was claimed, the wallet holds at most one name, and the edge never invented one.
    let held: Vec<&str> = ["dupop1", "dupop2", "dupname1", "dupname2"].into_iter().filter(|n| resolves(&edge, n)).collect();
    assert!(held.len() <= 1, "{held:?}");
}

// ================================================================ races

#[test]
fn eight_threads_claiming_one_name_give_exactly_one_winner() {
    let dir = TempDir::new("lnadv-race1");
    let (btc, xbt) = (good_stub(), good_stub());
    let edge = Arc::new(names_edge(&dir, Some(&btc), Some(&xbt), true));
    let tok = Arc::new(token(&edge));
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let hs: Vec<_> = (0..8u32)
        .map(|i| {
            let (edge, tok, barrier) = (Arc::clone(&edge), Arc::clone(&tok), Arc::clone(&barrier));
            let ip = fresh_ip();
            std::thread::spawn(move || {
                barrier.wait();
                let chain = if i % 2 == 0 { "btc" } else { "xbt" };
                let (s, b) = acct_raw(&edge, &tok, chain, &ip, &op_body(chain, &node(i + 1), "claim_name", Some("contested")));
                (s, serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null))
            })
        })
        .collect();
    let rs: Vec<(u16, Value)> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = rs.iter().filter(|r| r.0 == 200).count();
    assert_eq!(wins, 1, "{rs:?}");
    assert!(rs.iter().filter(|r| r.0 != 200).all(|r| r.0 == 409 && r.1 == json!({ "error": "name_taken" })), "{rs:?}");
    assert!(resolves(&edge, "contested"));
}

#[test]
fn eight_threads_giving_one_wallet_eight_names_give_exactly_one_name() {
    let dir = TempDir::new("lnadv-race2");
    let btc = good_stub();
    let edge = Arc::new(names_edge(&dir, Some(&btc), None, true));
    let tok = Arc::new(token(&edge));
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let hs: Vec<_> = (0..8u32)
        .map(|i| {
            let (edge, tok, barrier) = (Arc::clone(&edge), Arc::clone(&tok), Arc::clone(&barrier));
            let ip = fresh_ip();
            std::thread::spawn(move || {
                barrier.wait();
                let (s, b) = acct_raw(&edge, &tok, "btc", &ip, &op_body("btc", NODE_ID, "claim_name", Some(&format!("multi{i}"))));
                (s, serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null))
            })
        })
        .collect();
    let rs: Vec<(u16, Value)> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(rs.iter().filter(|r| r.0 == 200).count(), 1, "{rs:?}");
    assert!(rs.iter().filter(|r| r.0 != 200).all(|r| r.0 == 409 && r.1 == json!({ "error": "has_name" })), "{rs:?}");
    let held = (0..8).filter(|i| resolves(&edge, &format!("multi{i}"))).count();
    assert_eq!(held, 1, "one wallet holds more than one name");
}

#[test]
fn claims_racing_releases_never_leave_two_names_or_a_dangling_one() {
    let dir = TempDir::new("lnadv-race3");
    let btc = good_stub();
    let edge = Arc::new(names_edge(&dir, Some(&btc), None, true));
    let tok = Arc::new(token(&edge));
    let hs: Vec<_> = (0..8u32)
        .map(|i| {
            let (edge, tok) = (Arc::clone(&edge), Arc::clone(&tok));
            std::thread::spawn(move || {
                for round in 0..6 {
                    let ip = fresh_ip();
                    let name = format!("churn{i}x{round}");
                    let (s, _) = acct_raw(&edge, &tok, "btc", &ip, &op_body("btc", &node(500 + i), "claim_name", Some(&name)));
                    assert!(s == 200 || s == 409, "{s}");
                    let (s, _) = acct_raw(&edge, &tok, "btc", &ip, &op_body("btc", &node(500 + i), "release_name", None));
                    assert_eq!(s, 200);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    for i in 0..8 {
        assert_eq!(my_name(&edge, &tok, "btc", &node(500 + i)).1, json!({ "name": null }), "wallet {i} still holds a name");
    }
}

// ================================================================ rate limits

#[test]
fn the_claim_limit_triggers_per_ip_and_recovers() {
    let dir = TempDir::new("lnadv-claimlimit");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, false); // XFF not trusted: one IP for everyone
    let tok = token(&edge);
    let ip_hdr = "1.2.3.4"; // ignored without --trust-forwarded-for
    let mut statuses = Vec::new();
    for i in 0..9u32 {
        let (s, _) = acct_raw(&edge, &tok, "btc", ip_hdr, &op_body("btc", &node(900 + i), "claim_name", Some(&format!("lim{i}x"))));
        statuses.push(s);
    }
    eprintln!("claim statuses: {statuses:?}");
    let ok = statuses.iter().filter(|s| **s == 200).count();
    let limited = statuses.iter().filter(|s| **s == 429).count();
    assert!((4..=6).contains(&ok), "burst of about 5: {statuses:?}");
    assert_eq!(ok + limited, 9, "{statuses:?}");
    assert_eq!(statuses[..ok], vec![200; ok][..], "429s only after the burst");
    assert_eq!(btc.count("/vss/account"), ok, "a limited claim never reaches the LSP");
    // Refused, not written.
    assert!(!resolves_unlimited(&edge, &format!("lim{}x", 8)));
    // my_name and release_name are not claim attempts: still served while claims are limited.
    assert_eq!(my_name(&edge, &tok, "btc", &node(900)).1, json!({ "name": "lim0x" }));
    assert_eq!(acct_raw(&edge, &tok, "btc", ip_hdr, &op_body("btc", &node(900), "release_name", None)).0, 200);
    // Recovery (one token per 6 s).
    std::thread::sleep(Duration::from_secs(7));
    let (s, _) = acct_raw(&edge, &tok, "btc", ip_hdr, &op_body("btc", &node(950), "claim_name", Some("afterwait")));
    assert_eq!(s, 200, "the limit recovers");
}

/// A lookup that does not consume this test's own limiter: the edge in the test above does not trust XFF, so
/// wait for the lookup bucket instead (20 burst, plenty here).
fn resolves_unlimited(edge: &Edge, name: &str) -> bool {
    let (_, b) = raw(edge.port, "GET", &format!("/.well-known/lnurlp/{name}"), &[], b"");
    String::from_utf8_lossy(&b).contains("payRequest")
}

#[test]
fn the_lookup_limit_triggers_with_lud06_json_ignores_spoofed_headers_and_recovers() {
    let dir = TempDir::new("lnadv-looklimit");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, false);
    let tok = token(&edge);
    assert_eq!(claim_plain(&edge, &tok, NODE_ID, "donal"), 200);
    let mut statuses = Vec::new();
    let mut last_limited: Option<(u16, Vec<u8>)> = None;
    for i in 0..26 {
        // A spoofed X-Forwarded-For / CF-Connecting-IP must not buy a fresh bucket.
        let spoof = format!("10.9.9.{i}");
        let (s, b) = raw(edge.port, "GET", "/.well-known/lnurlp/donal", &[("X-Forwarded-For", &spoof), ("CF-Connecting-IP", &spoof)], b"");
        statuses.push(s);
        if s == 429 {
            last_limited = Some((s, b));
        }
    }
    eprintln!("lookup statuses: {statuses:?}");
    let ok = statuses.iter().take_while(|s| **s == 200).count();
    assert!((18..=22).contains(&ok), "burst of about 20 (a few already used by the claim): {statuses:?}");
    assert!(statuses[ok..].iter().all(|s| *s == 429));
    let (s, b) = last_limited.expect("a 429");
    assert_eq!(s, 429);
    let v: Value = serde_json::from_slice(&b).expect("a 429 must still be LUD-06 JSON");
    assert!(is_lud06_error(&v), "{v}");
    // The callback shares the limiter.
    let (s, b) = raw(edge.port, "GET", "/lnurlp/callback/donal?amount=5000", &[], b"");
    assert_eq!(s, 429);
    assert!(is_lud06_error(&serde_json::from_slice(&b).unwrap()));
    // Other routes are unaffected.
    assert_eq!(http(&format!("{}/", edge.url), "GET", &[], None).0, 200);
    // Recovery: one token per second.
    std::thread::sleep(Duration::from_millis(2500));
    let (s, b) = raw(edge.port, "GET", "/.well-known/lnurlp/donal", &[], b"");
    assert_eq!(s, 200);
    assert!(String::from_utf8_lossy(&b).contains("payRequest"));
    // Only the LSP calls of the answered lookups happened: a limited lookup costs the LSP nothing.
    assert!(btc.count("/vss/lnurl/limits") <= ok + 1, "{} limit calls for {ok} lookups", btc.count("/vss/lnurl/limits"));
}

fn claim_plain(edge: &Edge, tok: &str, node: &str, name: &str) -> u16 {
    acct_raw(edge, tok, "btc", "1.1.1.1", &op_body("btc", node, "claim_name", Some(name))).0
}

#[test]
fn with_a_trusted_proxy_each_client_ip_has_its_own_lookup_bucket() {
    let dir = TempDir::new("lnadv-looklimit2");
    let btc = good_stub();
    let edge = names_edge(&dir, Some(&btc), None, true);
    for _ in 0..25 {
        raw(edge.port, "GET", "/.well-known/lnurlp/nobody", &[("X-Forwarded-For", "10.1.1.1")], b"");
    }
    let (s, _) = raw(edge.port, "GET", "/.well-known/lnurlp/nobody", &[("X-Forwarded-For", "10.1.1.1")], b"");
    assert_eq!(s, 429);
    let (s, b) = raw(edge.port, "GET", "/.well-known/lnurlp/nobody", &[("X-Forwarded-For", "10.2.2.2")], b"");
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
}

// ================================================================ chains and persistence

#[test]
fn a_name_survives_a_restart_and_a_missing_chain_lsp_degrades_cleanly() {
    let dir = TempDir::new("lnadv-restart");
    let names_path: PathBuf = dir.path("names.sqlite");
    {
        let (btc, xbt) = (good_stub(), good_stub());
        let edge = names_edge(&dir, Some(&btc), Some(&xbt), true);
        let tok = token(&edge);
        assert_eq!(claim(&edge, &tok, "btc", NODE_ID, "onbtc").0, 200);
        assert_eq!(claim(&edge, &tok, "xbt", NODE_ID, "onxbt").0, 200);
        assert_eq!(claim(&edge, &tok, "btc", OTHER_NODE, "gonner").0, 200);
        assert_eq!(acct_raw(&edge, &tok, "btc", &fresh_ip(), &op_body("btc", OTHER_NODE, "release_name", None)).0, 200);
    } // edge and stubs dropped: process killed

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&names_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "the names file is owner-only, got {mode:o}");
    }

    // Same database, same token secret, new process: the names are still there.
    let (btc, xbt) = (good_stub(), good_stub());
    let edge = names_edge(&dir, Some(&btc), Some(&xbt), true);
    let tok = token(&edge);
    assert!(resolves(&edge, "onbtc"));
    assert!(resolves(&edge, "onxbt"));
    assert!(!resolves(&edge, "gonner"), "a released name stays released");
    assert_eq!(my_name(&edge, &tok, "btc", NODE_ID).1, json!({ "name": "onbtc" }));
    assert_eq!(my_name(&edge, &tok, "xbt", NODE_ID).1, json!({ "name": "onxbt" }));
    assert_eq!(claim(&edge, &tok, "btc", OTHER_NODE, "onbtc").1, json!({ "error": "name_taken" }));
    assert_eq!(claim(&edge, &tok, "btc", OTHER_NODE, "gonner").0, 200, "a released name can be claimed again");
    drop(edge);

    // The XBT LSP is no longer configured: its names answer LUD-06 errors (HTTP 200), nothing panics, and the
    // BTC names still work.
    let edge = names_edge(&dir, Some(&btc), None, true);
    let tok = token(&edge);
    let (s, v) = get(&edge, "/.well-known/lnurlp/onxbt");
    assert_eq!(s, 200);
    assert!(is_lud06_error(&v), "{v}");
    let (s, v) = get(&edge, "/lnurlp/callback/onxbt?amount=5000");
    assert_eq!(s, 200);
    assert!(is_lud06_error(&v), "{v}");
    assert!(xbt.count("/vss/lnurl/invoice") == 0);
    assert!(resolves(&edge, "onbtc"));
    assert_eq!(get(&edge, "/lnurlp/callback/onbtc?amount=5000").1["pr"], "lnbc-fake-5000");
    assert_eq!(my_name(&edge, &tok, "xbt", NODE_ID).0, 404, "name ops for an unconfigured chain are 404");
    let _ = http(&format!("{}/", edge.url), "GET", &[], None);
}

#[test]
fn startup_refuses_half_configured_or_unusable_names_settings() {
    use std::process::{Command, Stdio};
    let dir = TempDir::new("lnadv-startup");
    let run = |extra: &[&str]| {
        let port = free_port();
        let p = |n: &str| dir.path(n).to_string_lossy().into_owned();
        let mut args: Vec<String> = vec![
            "--bind".into(), format!("127.0.0.1:{port}"),
            "--xbt-upstream".into(), "http://127.0.0.1:9".into(),
            "--btc-upstream-fallback=".into(),
            "--secret-file".into(), p("edge.secret"),
            "--cache-dir".into(), p("edge-cache"),
            "--btc-history-dir".into(), p("edge-btc-history"),
            "--btc-scan-cache-dir".into(), p("edge-btc-scan-cache"),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut c = Command::new(bin("fortis-edge")).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        // A refusal exits quickly; a healthy edge keeps running.
        let t0 = Instant::now();
        loop {
            if let Some(st) = c.try_wait().unwrap() {
                return !st.success();
            }
            if t0.elapsed() > Duration::from_secs(4) {
                let _ = c.kill();
                let _ = c.wait();
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let db = dir.path("n.sqlite").to_string_lossy().into_owned();
    assert!(run(&["--lnurl-domain", "fortis.cash"]), "a domain without a names db");
    assert!(run(&["--names-db", &db]), "a names db without a domain");
    for d in ["Fortis.Cash", "fortis.cash:8080", "fortis.cash/x", "fortis cash", "https://fortis.cash", "", "fortis.cash?x", "fortis.cash#x", "fortis.cash@evil.com", "förtis.cash"] {
        assert!(run(&["--lnurl-domain", d, "--names-db", &db]), "domain {d:?} must be refused");
    }
    assert!(run(&["--lnurl-domain", "fortis.cash", "--names-db", "/nonexistent-dir-xyz/names.sqlite"]), "an unwritable names path");
}

#[test]
fn a_database_that_is_not_sqlite_stops_the_edge_rather_than_losing_names() {
    use std::process::{Command, Stdio};
    let dir = TempDir::new("lnadv-corrupt");
    let db = dir.path("names.sqlite");
    std::fs::write(&db, b"this is not a database, it is long enough to look like a header but is not one at all!!!!!!!!!!!!!!!!").unwrap();
    let p = |n: &str| dir.path(n).to_string_lossy().into_owned();
    let port = free_port();
    let mut c = Command::new(bin("fortis-edge"))
        .args(["--bind", &format!("127.0.0.1:{port}"), "--xbt-upstream", "http://127.0.0.1:9", "--btc-upstream-fallback=", "--secret-file", &p("edge.secret"), "--cache-dir", &p("c"), "--btc-history-dir", &p("h"), "--btc-scan-cache-dir", &p("s"), "--lnurl-domain", "fortis.cash", "--names-db", &db.to_string_lossy()])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    let exited = loop {
        if let Some(st) = c.try_wait().unwrap() {
            break Some(st);
        }
        if t0.elapsed() > Duration::from_secs(4) {
            let _ = c.kill();
            let _ = c.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(exited.is_some_and(|s| !s.success()), "the edge must refuse to run on a corrupt names file");
    assert!(std::fs::read(&db).unwrap().starts_with(b"this is not a database"), "the file was not overwritten");
}
