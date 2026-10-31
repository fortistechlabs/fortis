//! `/ln/info` and `/ln/vss/*` against a real `fortis-edge` process and a
//! tiny in-test VSS upstream stub. Self-contained: no bitcoind, nothing
//! outside localhost.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use common::{agent, bin, free_port, poll, spawn, wait_up, Svc, TempDir};

/// A valid compressed public key (the secp256k1 generator).
const NODE_ID: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
/// BIP173's regtest P2WPKH vector: the swap commission address for the
/// `--swap` edge in the rate-limit test.
const REGTEST_FEE_ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";

fn info_json(enabled: bool, message: Option<&str>) -> Value {
    json!({
        "node_id": NODE_ID,
        "host": "lsp.example.org",
        "port": 9735,
        "lsps2_token": "jit-token",
        "fee": { "ppm": 10000, "min_sat": 2000 },
        "min_payment_sat": 51000,
        "max_payment_sat": 250000,
        "channel_min_sat": 100000,
        "channel_max_sat": 1000000,
        "enabled": enabled,
        "message": message,
        "network": "regtest",
        "a_field_from_a_newer_operator_tool": 1,
    })
}

fn write_info(path: &Path, v: &Value) {
    std::fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

// --------------------------------------------------------------------------
// the edge
// --------------------------------------------------------------------------

struct Edge {
    url: String,
    port: u16,
    log: std::path::PathBuf,
    _svc: Svc,
}

/// The flags every test edge needs: an XBT upstream that is never asked (the
/// routes under test don't touch it), no public BTC fallback, everything on
/// disk inside `dir`.
fn base_args(dir: &TempDir, port: u16) -> Vec<String> {
    let p = |n: &str| dir.path(n).to_string_lossy().into_owned();
    vec![
        "--bind".into(), format!("127.0.0.1:{port}"),
        "--xbt-upstream".into(), "http://127.0.0.1:9".into(),
        "--btc-upstream-fallback=".into(),
        "--secret-file".into(), p("edge.secret"),
        "--cache-dir".into(), p("edge-cache"),
        "--btc-history-dir".into(), p("edge-btc-history"),
        "--btc-scan-cache-dir".into(), p("edge-btc-scan-cache"),
        "--register-per-hour".into(), "1000".into(),
        "--workers".into(), "8".into(),
    ]
}

impl Edge {
    fn start(dir: &TempDir, extra: &[&str]) -> Edge {
        let port = free_port();
        let mut args = base_args(dir, port);
        args.extend(extra.iter().map(|s| s.to_string()));
        let log = dir.path(&format!("edge-{port}.log"));
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let svc = spawn(&bin("fortis-edge"), &argv, Some(log.clone()));
        let url = format!("http://127.0.0.1:{port}");
        wait_up(&agent(), &format!("{url}/"));
        Edge { url, port, log, _svc: svc }
    }

    fn token(&self) -> String {
        let (s, _, b) = http(&format!("{}/register", self.url), "POST", &[], Some(b""));
        assert_eq!(s, 200, "register: {}", String::from_utf8_lossy(&b));
        serde_json::from_slice::<Value>(&b).unwrap()["token"].as_str().unwrap().to_string()
    }

    fn get_info(&self, tok: &str) -> (u16, Vec<(String, String)>, Value) {
        let (s, h, b) = http(&format!("{}/ln/info", self.url), "GET", &[("Authorization", &format!("Bearer {tok}"))], None);
        (s, h, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    fn vss(&self, tok: &str, op: &str, body: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        http(
            &format!("{}/ln/vss/{op}", self.url),
            "POST",
            &[("Authorization", &format!("Bearer {tok}")), ("Content-Type", "application/octet-stream")],
            Some(body),
        )
    }

    fn metric(&self, name: &str) -> u64 {
        let (_, _, b) = http(&format!("{}/metrics", self.url), "GET", &[], None);
        let text = String::from_utf8(b).unwrap();
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} not in /metrics:\n{text}"))
            .parse()
            .unwrap()
    }
}

/// `(status, headers, body)`; a transport failure is status 0.
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

fn header<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    hs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// A request line sent verbatim — an HTTP client would normalise `..` and
/// `//` before they reach the edge. Returns the status code.
fn raw_status(port: u16, method: &str, target: &str, tok: &str, body: &[u8]) -> u16 {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer {tok}\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or_else(|| panic!("no status in {text:?}"))
}

// --------------------------------------------------------------------------
// the VSS upstream stub
// --------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

type Reply = (u16, &'static str, Vec<u8>);

/// A local HTTP server that records every request and answers with
/// `handler(url, body)` after `delay`.
struct Stub {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    server: Arc<Server>,
}

impl Stub {
    fn start(delay: Duration, handler: fn(&str, &[u8]) -> Reply) -> Stub {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..4 {
            let (server, seen) = (Arc::clone(&server), Arc::clone(&seen));
            std::thread::spawn(move || {
                for mut rq in server.incoming_requests() {
                    let mut body = Vec::new();
                    let _ = rq.as_reader().read_to_end(&mut body);
                    let headers =
                        rq.headers().iter().map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.to_string())).collect();
                    let url = rq.url().to_string();
                    let (status, ct, out) = handler(&url, &body);
                    seen.lock().unwrap().push(Seen { method: rq.method().to_string(), url, headers, body });
                    std::thread::sleep(delay);
                    let resp = Response::from_data(out)
                        .with_status_code(status)
                        .with_header(Header::from_bytes("Content-Type", ct).unwrap());
                    let _ = rq.respond(resp);
                }
            });
        }
        Stub { url: format!("http://127.0.0.1:{port}/vss"), seen, server }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        for _ in 0..4 {
            self.server.unblock();
        }
    }
}

/// Every byte value, so a text conversion anywhere on the path shows.
fn binary_body(tag: u8) -> Vec<u8> {
    let mut b: Vec<u8> = (0..=255u8).collect();
    b.extend_from_slice(&[0, 0, 0xff, 0xfe, b'\r', b'\n', tag]);
    b
}

/// What vss-server would answer: the request body reversed for a success
/// (proves the edge relays bytes, not a canned reply), a protobuf-like error
/// body for the conflict and not-found cases.
fn vss_stub(url: &str, body: &[u8]) -> Reply {
    match url {
        "/vss/getObject" | "/vss/listKeyVersions" => (200, "application/octet-stream", body.iter().rev().copied().collect()),
        "/vss/putObjects" => (409, "application/octet-stream", vec![0x08, 0x02, 0x12, 0x08, b'c', b'o', b'n', b'f', b'l', 0x00, 0xff, b't']),
        "/vss/deleteObject" => (404, "application/octet-stream", vec![0x08, 0x05, 0x00, 0x80]),
        _ => (500, "text/plain", b"stub: unexpected path".to_vec()),
    }
}

fn big_response(_url: &str, _body: &[u8]) -> Reply {
    (200, "application/octet-stream", vec![7u8; 8 * 1024 * 1024 + 1])
}

// --------------------------------------------------------------------------
// /ln/info
// --------------------------------------------------------------------------

#[test]
fn info_requires_token() {
    let dir = TempDir::new("ln-info-token");
    let info = dir.path("ln-info.json");
    write_info(&info, &info_json(true, None));
    let edge = Edge::start(&dir, &["--ln-info-file", info.to_str().unwrap()]);

    let (s, _, b) = http(&format!("{}/ln/info", edge.url), "GET", &[], None);
    assert_eq!(s, 401, "{}", String::from_utf8_lossy(&b));
    assert!(serde_json::from_slice::<Value>(&b).unwrap()["error"].is_string());
    let (s, _, _) = edge.get_info("not-a-token");
    assert_eq!(s, 401);
    let (s, _, v) = edge.get_info(&edge.token());
    assert_eq!(s, 200, "{v}");
}

#[test]
fn info_404_when_unconfigured() {
    let dir = TempDir::new("ln-info-404");
    let edge = Edge::start(&dir, &[]);
    let tok = edge.token();
    let (s, _, v) = edge.get_info(&tok);
    assert_eq!(s, 404);
    assert_eq!(v["error"], "no such route");
    let (s, _, b) = edge.vss(&tok, "getObject", b"x");
    assert_eq!(s, 404, "no --vss-upstream: {}", String::from_utf8_lossy(&b));
}

#[test]
fn info_returns_file_contents() {
    let dir = TempDir::new("ln-info-contents");
    let info = dir.path("ln-info.json");
    write_info(&info, &info_json(true, Some("beta: small amounts only")));
    let edge = Edge::start(&dir, &["--ln-info-file", info.to_str().unwrap()]);

    let (s, h, v) = edge.get_info(&edge.token());
    assert_eq!(s, 200, "{v}");
    assert_eq!(header(&h, "cache-control"), Some("no-store"));
    assert!(header(&h, "content-type").unwrap().starts_with("application/json"));
    assert_eq!(
        v,
        json!({
            "node_id": NODE_ID,
            "host": "lsp.example.org",
            "port": 9735,
            "lsps2_token": "jit-token",
            "fee": { "ppm": 10000, "min_sat": 2000 },
            "min_payment_sat": 51000,
            "max_payment_sat": 250000,
            "channel_min_sat": 100000,
            "channel_max_sat": 1000000,
            "enabled": true,
            "message": "beta: small amounts only",
            "network": "regtest",
        }),
        "unknown fields are dropped, every known one served"
    );
}

#[test]
fn info_rejects_invalid_file_at_startup() {
    let dir = TempDir::new("ln-info-invalid");
    let bad = |name: &str, f: &dyn Fn(&mut Value)| {
        let mut v = info_json(true, None);
        f(&mut v);
        let p = dir.path(name);
        write_info(&p, &v);
        p
    };
    let cases = [
        (bad("short-node.json", &|v| v["node_id"] = json!("02abcd")), "node_id"),
        (bad("port-0.json", &|v| v["port"] = json!(0)), "port"),
        (bad("scheme.json", &|v| v["host"] = json!("https://lsp.example.org")), "host"),
        (bad("bounds.json", &|v| v["min_payment_sat"] = json!(300000)), "min_payment_sat"),
        (bad("net.json", &|v| v["network"] = json!("mainnet")), "network"),
        (dir.path("missing.json"), "missing.json"),
    ];
    for (path, what) in cases {
        let port = free_port();
        let mut args = base_args(&dir, port);
        args.extend(["--ln-info-file".to_string(), path.to_string_lossy().into_owned()]);
        let mut child = Command::new(bin("fortis-edge"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{}: the edge kept running with an invalid --ln-info-file", path.display());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut stderr = String::new();
        child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        assert!(!status.success(), "{}: exit {status}", path.display());
        assert!(stderr.contains("--ln-info-file"), "{}: {stderr}", path.display());
        assert!(stderr.contains(what), "{}: wanted {what:?} in {stderr}", path.display());
    }
}

#[test]
fn info_hot_reload_flips_enabled() {
    let dir = TempDir::new("ln-info-reload");
    let info = dir.path("ln-info.json");
    write_info(&info, &info_json(true, None));
    let edge = Edge::start(&dir, &["--ln-info-file", info.to_str().unwrap()]);
    let tok = edge.token();
    assert_eq!(edge.get_info(&tok).2["enabled"], true);

    std::thread::sleep(Duration::from_millis(50));
    write_info(&info, &info_json(false, Some("Lightning is paused for maintenance")));
    let t0 = Instant::now();
    let v = poll("enabled=false", 6, || Some(edge.get_info(&tok).2).filter(|v| v["enabled"] == false));
    assert!(t0.elapsed() <= Duration::from_secs(6));
    assert_eq!(v["message"], "Lightning is paused for maintenance");

    // An invalid rewrite (unparseable, then a bad port) is ignored: the last
    // good file keeps being served, and the rejection is logged.
    std::fs::write(&info, b"{ not json").unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    let mut bad = info_json(true, None);
    bad["port"] = json!(70000);
    write_info(&info, &bad);
    std::thread::sleep(Duration::from_millis(2500));
    let (s, _, v) = edge.get_info(&tok);
    assert_eq!(s, 200);
    assert_eq!(v["enabled"], false, "last good file kept: {v}");
    assert_eq!(v["message"], "Lightning is paused for maintenance");
    let log = std::fs::read_to_string(&edge.log).unwrap();
    assert!(log.contains("ln info") && log.contains("keeping the last good"), "{log}");

    // And it keeps polling: a valid file is picked up again.
    write_info(&info, &info_json(true, None));
    poll("enabled=true again", 6, || Some(edge.get_info(&tok).2).filter(|v| v["enabled"] == true));
}

// --------------------------------------------------------------------------
// /ln/vss/*
// --------------------------------------------------------------------------

#[test]
fn vss_proxy_forwards_body_and_status() {
    let dir = TempDir::new("ln-vss-forward");
    let stub = Stub::start(Duration::ZERO, vss_stub);
    let edge = Edge::start(&dir, &["--vss-upstream", &stub.url]);
    let tok = edge.token();

    // No token: refused before anything reaches the upstream.
    let (s, _, _) = http(&format!("{}/ln/vss/getObject", edge.url), "POST", &[], Some(b"x"));
    assert_eq!(s, 401);
    assert!(stub.seen().is_empty());

    for (op, tag) in [("getObject", 1u8), ("listKeyVersions", 2)] {
        let body = binary_body(tag);
        let (s, h, out) = edge.vss(&tok, op, &body);
        assert_eq!(s, 200, "{op}");
        assert_eq!(out, body.iter().rev().copied().collect::<Vec<u8>>(), "{op}: body relayed byte-exact");
        assert_eq!(header(&h, "content-type"), Some("application/octet-stream"));
    }
    let (s, _, out) = edge.vss(&tok, "putObjects", &binary_body(3));
    assert_eq!(s, 409, "a conflict is relayed unchanged");
    assert_eq!(out, vss_stub("/vss/putObjects", b"").2);
    let (s, _, out) = edge.vss(&tok, "deleteObject", &binary_body(4));
    assert_eq!(s, 404, "the upstream's 404 is relayed unchanged");
    assert_eq!(out, vss_stub("/vss/deleteObject", b"").2);

    let seen = stub.seen();
    assert_eq!(seen.len(), 4);
    for (s, (op, tag)) in seen.iter().zip([("getObject", 1u8), ("listKeyVersions", 2), ("putObjects", 3), ("deleteObject", 4)]) {
        assert_eq!(s.method, "POST");
        assert_eq!(s.url, format!("/vss/{op}"));
        assert_eq!(s.body, binary_body(tag), "{op}: request body forwarded byte-exact");
    }
    assert_eq!(edge.metric("fortis_edge_vss_requests_total"), 4);
}

#[test]
fn vss_proxy_path_allowlist() {
    let dir = TempDir::new("ln-vss-allowlist");
    let stub = Stub::start(Duration::ZERO, vss_stub);
    let edge = Edge::start(&dir, &["--vss-upstream", &stub.url]);
    let tok = edge.token();

    assert_eq!(raw_status(edge.port, "POST", "/ln/vss/getObject", &tok, b"ok"), 200);
    for target in [
        "/ln/vss/foo",
        "/ln/vss/../x",
        "/ln/vss/../../metrics",
        "/ln/vss/%2e%2e/x",
        "/ln/vss/%2E%2E/getObject",
        "/ln/vss//getObject",
        "/ln/vss/getObject/extra",
        "/ln/vss/./getObject",
        "/ln/vss/%67etObject",
        "/ln/vss/getobject",
        "/ln/vss",
        "/ln/vssgetObject",
    ] {
        let s = raw_status(edge.port, "POST", target, &tok, b"x");
        assert!(s == 404 || s == 400, "POST {target}: {s}");
    }
    for method in ["GET", "PUT", "DELETE"] {
        let s = raw_status(edge.port, method, "/ln/vss/getObject", &tok, b"");
        assert!(s == 405 || s == 404, "{method}: {s}");
    }
    let seen = stub.seen();
    assert_eq!(seen.len(), 1, "only the allowed request went upstream: {seen:?}");
    assert_eq!(seen[0].url, "/vss/getObject");
}

#[test]
fn vss_proxy_strips_sensitive_headers() {
    let dir = TempDir::new("ln-vss-headers");
    let stub = Stub::start(Duration::ZERO, vss_stub);
    let edge = Edge::start(&dir, &["--vss-upstream", &stub.url]);
    let tok = edge.token();

    let (s, _, _) = http(
        &format!("{}/ln/vss/getObject?store_id=leak", edge.url),
        "POST",
        &[
            ("Authorization", &format!("Bearer {tok}")),
            ("Content-Type", "application/x-protobuf"),
            ("Cookie", "session=secret"),
            ("X-Forwarded-For", "203.0.113.9"),
            ("X-Forwarded-Proto", "https"),
            ("X-Real-IP", "203.0.113.9"),
            ("X-Something-Else", "1"),
        ],
        Some(b"body"),
    );
    assert_eq!(s, 200);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    let names: Vec<&str> = seen[0].headers.iter().map(|(k, _)| k.as_str()).collect();
    for bad in ["authorization", "cookie", "x-forwarded-for", "x-forwarded-proto", "x-real-ip", "x-something-else"] {
        assert!(!names.contains(&bad), "{bad} reached the upstream: {names:?}");
    }
    assert!(!seen[0].headers.iter().any(|(_, v)| v.contains(&tok)), "the token reached the upstream");
    assert_eq!(header(&seen[0].headers, "content-type"), Some("application/x-protobuf"));
    assert_eq!(seen[0].url, "/vss/getObject", "no query string forwarded");
}

#[test]
fn vss_proxy_rejects_oversize() {
    let dir = TempDir::new("ln-vss-oversize");
    let stub = Stub::start(Duration::ZERO, vss_stub);
    let edge = Edge::start(&dir, &["--vss-upstream", &stub.url, "--vss-max-body", "1024"]);
    let tok = edge.token();

    let (s, _, b) = edge.vss(&tok, "putObjects", &vec![1u8; 1025]);
    assert_eq!(s, 413, "{}", String::from_utf8_lossy(&b));
    assert!(serde_json::from_slice::<Value>(&b).unwrap()["error"].is_string());
    assert!(stub.seen().is_empty(), "nothing sent upstream");
    assert_eq!(edge.metric("fortis_edge_vss_too_large_total"), 1);

    // At the cap is fine.
    let (s, _, _) = edge.vss(&tok, "getObject", &vec![1u8; 1024]);
    assert_eq!(s, 200);
    assert_eq!(stub.seen().len(), 1);

    // An upstream answer over 8 MiB is not relayed.
    let big = Stub::start(Duration::ZERO, big_response);
    let edge2 = Edge::start(&dir, &["--vss-upstream", &big.url]);
    let (s, _, b) = edge2.vss(&edge2.token(), "getObject", b"x");
    assert_eq!(s, 502, "{}", String::from_utf8_lossy(&b[..b.len().min(200)]));
    assert!(b.len() < 1024);
}

#[test]
fn vss_proxy_upstream_timeout_is_504() {
    let dir = TempDir::new("ln-vss-timeout");
    let slow = Stub::start(Duration::from_secs(4), vss_stub);
    // The hidden test-only read timeout (default 30 s) keeps this test short.
    let edge = Edge::start(&dir, &["--vss-upstream", &slow.url, "--vss-read-timeout-secs", "1"]);
    let tok = edge.token();
    let t0 = Instant::now();
    let (s, _, b) = edge.vss(&tok, "getObject", b"x");
    assert_eq!(s, 504, "{}", String::from_utf8_lossy(&b));
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
    assert!(serde_json::from_slice::<Value>(&b).unwrap()["error"].is_string());

    // An upstream that is down: 502, and the error names no internal address.
    let down = format!("http://127.0.0.1:{}/vss", free_port());
    let edge2 = Edge::start(&dir, &["--vss-upstream", &down]);
    let (s, _, b) = edge2.vss(&edge2.token(), "getObject", b"x");
    assert_eq!(s, 502);
    assert!(!String::from_utf8_lossy(&b).contains("127.0.0.1"), "{}", String::from_utf8_lossy(&b));
    assert_eq!(edge.metric("fortis_edge_vss_upstream_errors_total") + edge2.metric("fortis_edge_vss_upstream_errors_total"), 2);
}

#[test]
fn vss_proxy_rate_limited_separately_from_swaps() {
    let dir = TempDir::new("ln-vss-rate");
    let stub = Stub::start(Duration::ZERO, vss_stub);
    let swap_db = dir.path("swap-db");
    let edge = Edge::start(
        &dir,
        &[
            "--vss-upstream", &stub.url,
            "--rate-per-min", "1",
            "--rate-burst", "3",
            "--swap",
            "--swap-db", swap_db.to_str().unwrap(),
            "--network", "regtest",
            "--service-fee-address", REGTEST_FEE_ADDRESS,
            "--neoxex-url", "http://127.0.0.1:9",
        ],
    );
    let swap_book = |tok: &str| {
        http(&format!("{}/swap/book", edge.url), "GET", &[("Authorization", &format!("Bearer {tok}"))], None).0
    };

    // VSS bursts spend only the VSS bucket...
    let a = edge.token();
    for _ in 0..3 {
        assert_eq!(edge.vss(&a, "getObject", b"x").0, 200);
    }
    assert_eq!(edge.vss(&a, "getObject", b"x").0, 429);
    assert_eq!(swap_book(&a), 200, "swaps unaffected by a spent VSS bucket");

    // ...and swaps only theirs.
    let b = edge.token();
    for _ in 0..3 {
        assert_eq!(swap_book(&b), 200);
    }
    assert_eq!(swap_book(&b), 429);
    assert_eq!(edge.vss(&b, "putObjects", b"x").0, 409, "VSS unaffected by a spent swap bucket");

    assert_eq!(stub.seen().len(), 4);
    assert_eq!(edge.metric("fortis_edge_vss_rate_limited_total"), 1);
}

#[test]
fn vss_works_when_ln_disabled() {
    let dir = TempDir::new("ln-vss-disabled");
    let stub = Stub::start(Duration::ZERO, vss_stub);
    let info = dir.path("ln-info.json");
    write_info(&info, &info_json(false, Some("Lightning is switched off")));
    let edge = Edge::start(&dir, &["--ln-info-file", info.to_str().unwrap(), "--vss-upstream", &stub.url]);
    let tok = edge.token();

    let (s, _, v) = edge.get_info(&tok);
    assert_eq!(s, 200);
    assert_eq!(v["enabled"], false);
    assert_eq!(v["message"], "Lightning is switched off");
    let body = binary_body(9);
    let (s, _, out) = edge.vss(&tok, "getObject", &body);
    assert_eq!(s, 200, "backup keeps working with Lightning disabled");
    assert_eq!(out, body.iter().rev().copied().collect::<Vec<u8>>());
    assert_eq!(edge.vss(&tok, "putObjects", b"x").0, 409);
}
