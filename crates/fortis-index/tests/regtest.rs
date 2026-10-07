//! End-to-end: the `fortis-index` binary over real regtest nodes — Knots with
//! the BLAKE2b fork active from block 1 (164-byte headers) and Core (80-byte).
//!
//! Opt-in. Each test runs once per node kind whose variable is set:
//! ```sh
//! cargo build -p fortis-index
//! FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind \
//! FORTIS_BITCOIND_CORE=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-index --test regtest -- --nocapture --test-threads 1
//! ```

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn index_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fortis-index"))
}

// --------------------------------------------------------------------------
// regtest node
// --------------------------------------------------------------------------

struct Node {
    child: Child,
    datadir: PathBuf,
    cli: PathBuf,
    rpc_port: u16,
}

impl Node {
    fn cli(&self, args: &[&str]) -> String {
        let out = Command::new(&self.cli)
            .args([
                "-regtest",
                &format!("-datadir={}", self.datadir.display()),
                &format!("-rpcport={}", self.rpc_port),
            ])
            .args(args)
            .output()
            .expect("run bitcoin-cli");
        assert!(
            out.status.success(),
            "bitcoin-cli {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    fn wallet(&self, wallet: &str, args: &[&str]) -> String {
        let mut a = vec![format!("-rpcwallet={wallet}")];
        a.extend(args.iter().map(|s| s.to_string()));
        self.cli(&a.iter().map(String::as_str).collect::<Vec<_>>())
    }
    fn mine(&self, n: u32) {
        let addr = self.wallet("miner", &["getnewaddress"]);
        self.wallet("miner", &["generatetoaddress", &n.to_string(), &addr]);
    }
    fn height(&self) -> u64 {
        self.cli(&["getblockcount"]).parse().unwrap()
    }
    fn cookie(&self) -> PathBuf {
        self.datadir.join("regtest").join(".cookie")
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rpc_port)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new(&self.cli)
            .args([
                "-regtest",
                &format!("-datadir={}", self.datadir.display()),
                &format!("-rpcport={}", self.rpc_port),
                "stop",
            ])
            .output();
        std::thread::sleep(Duration::from_millis(800));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

fn start_node(bitcoind: &Path, extra: &[&str]) -> Node {
    let cli = bitcoind.with_file_name(if cfg!(windows) {
        "bitcoin-cli.exe"
    } else {
        "bitcoin-cli"
    });
    assert!(
        cli.exists(),
        "bitcoin-cli not next to bitcoind at {}",
        cli.display()
    );
    let (rpc_port, p2p_port) = (free_port(), free_port());
    let datadir = std::env::temp_dir().join(format!(
        "fortis-index-e2e-{}-{rpc_port}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&datadir).unwrap();
    let child = Command::new(bitcoind)
        .args(["-regtest", &format!("-datadir={}", datadir.display())])
        .args([
            &format!("-rpcport={rpc_port}"),
            &format!("-port={p2p_port}"),
        ])
        .args([
            "-server=1",
            "-fallbackfee=0.0002",
            "-printtoconsole=0",
            "-listen=0",
        ])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bitcoind");
    let node = Node {
        child,
        datadir,
        cli,
        rpc_port,
    };
    let deadline = Instant::now() + Duration::from_secs(40);
    while Command::new(&node.cli)
        .args([
            "-regtest",
            &format!("-datadir={}", node.datadir.display()),
            &format!("-rpcport={rpc_port}"),
            "getblockchaininfo",
        ])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        assert!(Instant::now() < deadline, "node did not come up");
        std::thread::sleep(Duration::from_millis(300));
    }
    node.cli(&["createwallet", "miner"]);
    node
}

/// Run `body` once per configured node kind.
fn for_each_node(body: impl Fn(&str, Node)) {
    let mut ran = false;
    for (var, extra) in [
        ("FORTIS_BITCOIND", &["-testactivationheight=blake2b@1"][..]),
        ("FORTIS_BITCOIND_CORE", &[][..]),
    ] {
        let Ok(bitcoind) = std::env::var(var) else {
            eprintln!("skipping {var}: not set");
            continue;
        };
        eprintln!("== {var} ({bitcoind})");
        body(var, start_node(Path::new(&bitcoind), extra));
        ran = true;
    }
    if !ran {
        eprintln!("skipping: set FORTIS_BITCOIND (Knots) and/or FORTIS_BITCOIND_CORE");
    }
}

// --------------------------------------------------------------------------
// the index process
// --------------------------------------------------------------------------

struct Index {
    child: Child,
    port: u16,
    /// stderr lines, forwarded by a reader thread.
    log: mpsc::Receiver<String>,
}

impl Drop for Index {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_index(node: &Node, db: &Path) -> Index {
    let port = free_port();
    let mut child = Command::new(index_bin())
        .args(["--network", "regtest", "--start-height", "0"])
        .args(["--rpc-url", &node.url()])
        .arg("--cookie-file")
        .arg(node.cookie())
        .arg("--db")
        .arg(db)
        .args(["--bind", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fortis-index");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("  [index] {line}");
            if tx.send(line).is_err() {
                // Keep draining so the child never blocks on a full pipe.
            }
        }
    });
    let idx = Index {
        child,
        port,
        log: rx,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while get(&idx, "/").is_none() {
        assert!(Instant::now() < deadline, "index did not come up");
        std::thread::sleep(Duration::from_millis(100));
    }
    idx
}

fn get(idx: &Index, path: &str) -> Option<String> {
    let url = format!("http://127.0.0.1:{}{path}", idx.port);
    let r = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
        .get(&url)
        .call()
        .ok()?;
    r.into_string().ok()
}

fn get_json(idx: &Index, path: &str) -> Value {
    serde_json::from_str(&get(idx, path).unwrap_or_else(|| panic!("GET {path} failed"))).unwrap()
}

fn poll<T>(within: Duration, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn tmp_db(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "fortis-index-e2e-db-{}-{tag}-{}",
        std::process::id(),
        free_port()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

// --------------------------------------------------------------------------

#[test]
fn receive_spend_and_reorg_over_real_nodes() {
    for_each_node(|_, node| {
        node.mine(101);
        node.cli(&["createwallet", "a"]);
        let x = node.wallet("a", &["getnewaddress", "", "bech32"]);
        let db = tmp_db("flow");
        let idx = start_index(&node, &db);

        // Receive: pending within 3 s, then confirmed at the tip once mined.
        let fund = node.wallet("miner", &["sendtoaddress", &x, "1.0"]);
        let utxo_path = format!("/address/{x}/utxo");
        poll(Duration::from_secs(3), "pending receive", || {
            let v = get_json(&idx, &utxo_path);
            (v[0]["txid"] == fund.as_str() && v[0]["status"]["confirmed"] == false).then_some(())
        });
        node.mine(1);
        let tip = node.height();
        poll(Duration::from_secs(10), "confirmed receive", || {
            let v = get_json(&idx, &utxo_path);
            (v.as_array()?.len() == 1
                && v[0]["status"]["confirmed"] == true
                && v[0]["status"]["block_height"] == tip)
                .then_some(())
        });
        assert_eq!(get_json(&idx, &utxo_path)[0]["value"], 100_000_000);

        // Spend X's only coin: it disappears, and the spend lists X as its input.
        let dest = node.wallet("miner", &["getnewaddress"]);
        let spend = node.wallet("a", &["sendtoaddress", &dest, "0.5"]);
        poll(Duration::from_secs(10), "spent coin hidden", || {
            get_json(&idx, &utxo_path)
                .as_array()?
                .is_empty()
                .then_some(())
        });
        let txs_path = format!("/address/{x}/txs");
        let txs = poll(Duration::from_secs(10), "two txs", || {
            let v = get_json(&idx, &txs_path);
            (v.as_array()?.len() == 2).then_some(v)
        });
        let s = txs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["txid"] == spend.as_str())
            .expect("spend listed");
        assert_eq!(s["vin"][0]["prevout"]["scriptpubkey_address"], x.as_str());
        assert_eq!(s["vin"][0]["prevout"]["value"], 100_000_000);
        assert!(s["fee"].as_u64().unwrap() > 0);
        assert_eq!(s["status"]["confirmed"], false);

        // Reorg: drop the block that confirmed the receive, mine two more.
        node.mine(1);
        let best = node.cli(&["getbestblockhash"]);
        let prev = node.cli(&["getblockhash", &(tip).to_string()]);
        node.cli(&["invalidateblock", &prev]);
        assert_ne!(best, prev);
        node.mine(2);
        let h = node.height();
        poll(Duration::from_secs(5), "reorged tip", || {
            (get(&idx, "/blocks/tip/height")? == h.to_string()).then_some(())
        });
        // The node decides what survived (a reorged-out tx may be re-mined,
        // stay pending, or be dropped): the index must agree exactly.
        poll(
            Duration::from_secs(5),
            "history matches the node after the reorg",
            || {
                let mut expected: Vec<(String, Value)> = Vec::new();
                for id in [&fund, &spend] {
                    let info: Value =
                        serde_json::from_str(&node.wallet("a", &["gettransaction", id])).unwrap();
                    let mempool: Value =
                        serde_json::from_str(&node.cli(&["getrawmempool"])).unwrap();
                    if let Some(h) = info["blockheight"].as_u64() {
                        expected.push((id.clone(), json!(h)));
                    } else if mempool.as_array()?.iter().any(|m| m == id.as_str()) {
                        expected.push((id.clone(), Value::Null));
                    }
                }
                let v = get_json(&idx, &txs_path);
                let mut got: Vec<(String, Value)> = v
                    .as_array()?
                    .iter()
                    .map(|t| {
                        (
                            t["txid"].as_str().unwrap().to_string(),
                            t["status"]["block_height"].clone(),
                        )
                    })
                    .collect();
                expected.sort_by(|a, b| a.0.cmp(&b.0));
                got.sort_by(|a, b| a.0.cmp(&b.0));
                if got != expected {
                    eprintln!("  index {got:?} ≠ node {expected:?}");
                    return None;
                }
                // The receive survives every reorg here: it must be confirmed.
                got.iter()
                    .any(|(id, h)| id == &fund && h.is_u64())
                    .then_some(())
            },
        );
        drop(idx);
        let _ = std::fs::remove_dir_all(&db);
    });
}

#[test]
fn kill_minus_nine_during_bulk_sync_recovers() {
    for_each_node(|_, node| {
        node.mine(101);
        node.cli(&["createwallet", "b"]);
        let addrs: Vec<String> = (0..20)
            .map(|_| node.wallet("b", &["getnewaddress", "", "bech32"]))
            .collect();
        let miner = node.wallet("miner", &["getnewaddress"]);
        for i in 0..600 {
            node.wallet("miner", &["sendtoaddress", &addrs[i % addrs.len()], "0.01"]);
            node.wallet("miner", &["generatetoaddress", "1", &miner]);
        }
        let tip = node.height().to_string();
        let db = tmp_db("kill");

        let mut idx = start_index(&node, &db);
        poll(Duration::from_secs(60), "first progress line", || {
            idx.log
                .recv_timeout(Duration::from_millis(100))
                .ok()
                .filter(|l| l.starts_with("index: +"))
        });
        // SIGKILL on unix: no shutdown, no flush.
        idx.child.kill().unwrap();
        idx.child.wait().unwrap();
        drop(idx);

        let idx = start_index(&node, &db);
        poll(Duration::from_secs(60), "resync to tip", || {
            (get(&idx, "/blocks/tip/height")? == tip).then_some(())
        });
        let out = Command::new(index_bin())
            .args([
                "verify",
                "--sample",
                "50",
                "--network",
                "regtest",
                "--rpc-url",
                &node.url(),
            ])
            .arg("--cookie-file")
            .arg(node.cookie())
            .arg("--db")
            .arg(&db)
            .output()
            .expect("run verify");
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "verify found mismatches");
        drop(idx);
        let _ = std::fs::remove_dir_all(&db);
    });
}
