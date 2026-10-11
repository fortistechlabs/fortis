//! Harness shared by the opt-in end-to-end tests: regtest nodes, child
//! services and a tiny HTTP client. Copied from `regtest_e2e.rs` and
//! generalised to run a Knots BLAKE2b node and a stock Bitcoin Core node side
//! by side (unique data dirs and ports per node).

#![allow(dead_code)]

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A workspace binary that lives next to this test's `fortis-edge` binary.
pub fn bin(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_BIN_EXE_fortis-edge"));
    p.set_file_name(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    assert!(p.exists(), "{} not built — run `cargo build --workspace` first", p.display());
    p
}

/// A fresh, empty temp directory, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fortis-edge-swap-e2e-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

// --------------------------------------------------------------------------
// regtest node
// --------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    /// Bitcoin Knots with the BLAKE2b deployment (XBT), active from height 1.
    Knots,
    /// Stock Bitcoin Core (BTC), with `-txindex=1` for the Esplora shim.
    Core,
}

pub struct Node {
    child: Child,
    pub datadir: PathBuf,
    cli: PathBuf,
    pub rpc_port: u16,
}

impl Node {
    fn base_args(&self) -> [String; 3] {
        ["-regtest".into(), format!("-datadir={}", self.datadir.display()), format!("-rpcport={}", self.rpc_port)]
    }

    pub fn cli(&self, args: &[&str]) -> String {
        let out = Command::new(&self.cli).args(self.base_args()).args(args).output().expect("run bitcoin-cli");
        assert!(out.status.success(), "bitcoin-cli {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn wallet(&self, wallet: &str, args: &[&str]) -> String {
        let mut a = vec![format!("-rpcwallet={wallet}")];
        a.extend(args.iter().map(|s| s.to_string()));
        self.cli(&a.iter().map(String::as_str).collect::<Vec<_>>())
    }

    pub fn mine(&self, n: u32) {
        let addr = self.wallet("miner", &["getnewaddress"]);
        self.wallet("miner", &["generatetoaddress", &n.to_string(), &addr]);
    }

    pub fn height(&self) -> u64 {
        self.cli(&["getblockcount"]).parse().unwrap()
    }

    /// Moves the node's clock (regtest `setmocktime`), so blocks mined after
    /// this carry `t` and the median time past catches up after 11 blocks.
    pub fn set_mock_time(&self, t: u64) {
        self.cli(&["setmocktime", &t.to_string()]);
    }

    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rpc_port)
    }

    pub fn cookie_file(&self) -> PathBuf {
        self.datadir.join("regtest").join(".cookie")
    }

    pub fn cookie(&self) -> String {
        let mut s = String::new();
        std::fs::File::open(self.cookie_file()).expect("regtest cookie").read_to_string(&mut s).unwrap();
        s.trim().to_string()
    }

    /// A JSON-RPC client for this node.
    pub fn rpc(&self) -> Rpc {
        Rpc { url: self.rpc_url(), cookie: self.cookie(), agent: agent() }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new(&self.cli).args(self.base_args()).arg("stop").output();
        std::thread::sleep(Duration::from_millis(800));
        let _ = self.child.kill();
        let _ = self.child.wait();
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.datadir).is_ok() || !self.datadir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

pub fn start_node(bitcoind: &Path, flavor: Flavor) -> Node {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let cli = bitcoind.with_file_name(if cfg!(windows) { "bitcoin-cli.exe" } else { "bitcoin-cli" });
    assert!(cli.exists(), "bitcoin-cli not next to bitcoind at {}", cli.display());

    let datadir = std::env::temp_dir().join(format!(
        "fortis-edge-swap-e2e-node-{flavor:?}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&datadir).unwrap();

    let (rpc_port, p2p_port) = (free_port(), free_port());
    let mut args = vec![
        "-regtest".to_string(),
        format!("-datadir={}", datadir.display()),
        format!("-rpcport={rpc_port}"),
        format!("-port={p2p_port}"),
        "-listen=0".into(),
        "-server=1".into(),
        "-fallbackfee=0.0002".into(),
        "-printtoconsole=0".into(),
    ];
    match flavor {
        Flavor::Knots => args.push("-testactivationheight=blake2b@1".into()),
        Flavor::Core => args.push("-txindex=1".into()),
    }
    let child = Command::new(bitcoind)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bitcoind");

    let node = Node { child, datadir, cli, rpc_port };
    let deadline = Instant::now() + Duration::from_secs(60);
    while Command::new(&node.cli)
        .args(node.base_args())
        .arg("getblockchaininfo")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        assert!(Instant::now() < deadline, "{flavor:?} node did not come up");
        std::thread::sleep(Duration::from_millis(500));
    }
    node.cli(&["createwallet", "miner"]);
    node
}

/// Bitcoin JSON-RPC over HTTP with cookie auth.
#[derive(Clone)]
pub struct Rpc {
    url: String,
    cookie: String,
    agent: ureq::Agent,
}

impl Rpc {
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = serde_json::json!({ "jsonrpc": "1.0", "id": "t", "method": method, "params": params });
        let resp = self
            .agent
            .post(&self.url)
            .set("Authorization", &format!("Basic {}", base64_lite::encode(self.cookie.as_bytes())))
            .send_json(body);
        let v: Value = match resp {
            Ok(r) => r.into_json().map_err(|e| e.to_string())?,
            // bitcoind answers RPC errors with 404/500 and a JSON body.
            Err(ureq::Error::Status(_, r)) => r.into_json().map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        };
        if !v["error"].is_null() {
            return Err(v["error"]["message"].as_str().unwrap_or("rpc error").to_string());
        }
        Ok(v["result"].clone())
    }
}

/// Just enough base64 for an HTTP Basic header.
mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(b: &[u8]) -> String {
        let mut out = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn encodes_rfc4648_vectors() {
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }
}

// --------------------------------------------------------------------------
// child services
// --------------------------------------------------------------------------

/// A child process, killed on drop. Its stderr goes to `log` when given; the
/// tail of that log is printed if the test is panicking.
pub struct Svc {
    child: Child,
    log: Option<PathBuf>,
}

impl Drop for Svc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            if let Some(log) = &self.log {
                let text = std::fs::read_to_string(log).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(40).collect();
                eprintln!("---- {} (last 40 lines) ----", log.display());
                for l in tail.iter().rev() {
                    eprintln!("{l}");
                }
            }
        }
    }
}

pub fn spawn(bin: &Path, args: &[&str], log: Option<PathBuf>) -> Svc {
    let stderr = match &log {
        Some(p) => Stdio::from(std::fs::File::create(p).unwrap()),
        None => Stdio::null(),
    };
    let child = Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    Svc { child, log }
}

// --------------------------------------------------------------------------
// tiny HTTP client
// --------------------------------------------------------------------------

/// Longer than the edge's 25 s message long-poll.
pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(40)).build()
}

/// `(status, body)`; a transport failure is status 0.
pub fn req(a: &ureq::Agent, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> (u16, String) {
    let mut r = a.request(method, url);
    for (k, v) in headers {
        r = r.set(k, v);
    }
    let resp = match body {
        Some(b) => r.set("Content-Type", "application/json").send_string(b),
        None => r.call(),
    };
    match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => (0, e.to_string()),
    }
}

pub fn wait_up(a: &ureq::Agent, url: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while req(a, "GET", url, &[], None).0 / 100 != 2 {
        assert!(Instant::now() < deadline, "{url} did not come up");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Polls `f` every 300 ms until it returns `Some`, for at most `secs`.
pub fn poll<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(300));
    }
}
