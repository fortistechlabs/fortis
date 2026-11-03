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
            // as in production: `/tx/:txid…` reads any transaction from the node
            "-txindex=1",
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

/// Status code and body of a GET, whatever the status.
fn get_status(idx: &Index, path: &str) -> (u16, String) {
    let url = format!("http://127.0.0.1:{}{path}", idx.port);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    match agent.get(&url).call() {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => panic!("GET {path}: {e}"),
    }
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

/// The swap routes: `/tx/:txid`, `/tx/:txid/hex` and `/tx/:txid/outspend/:vout`
/// for any output (a swap HTLC is P2WSH, which the address index never
/// stores): unspent → spent in the mempool → spent in a block mined several
/// blocks after the funding (found by the block scan once the mempool no
/// longer has it).
#[test]
fn tx_hex_and_outspend_routes_over_real_nodes() {
    for_each_node(|_, node| {
        node.mine(101);
        node.cli(&["createwallet", "a"]);
        let x = node.wallet("a", &["getnewaddress", "", "bech32"]);
        let db = tmp_db("outspend");
        let idx = start_index(&node, &db);

        let fund = node.wallet("miner", &["sendtoaddress", &x, "1.0"]);
        node.mine(1);
        let funded_at = node.height();
        let raw: Value = serde_json::from_str(&node.cli(&["getrawtransaction", &fund, "1"])).unwrap();
        let vout = raw["vout"]
            .as_array()
            .unwrap()
            .iter()
            .position(|o| o["scriptPubKey"]["address"] == x.as_str())
            .unwrap();
        let spk = raw["vout"][vout]["scriptPubKey"]["hex"].as_str().unwrap().to_string();

        // /tx: outputs with value and script hex, confirmed at its height
        let t = poll(Duration::from_secs(10), "tx", || {
            let v = get_json(&idx, &format!("/tx/{fund}"));
            (v["status"]["confirmed"] == true).then_some(v)
        });
        assert_eq!(t["vout"][vout]["value"], 100_000_000);
        assert_eq!(t["vout"][vout]["scriptpubkey"], spk.as_str());
        assert_eq!(t["status"]["block_height"], funded_at);
        let outspend = format!("/tx/{fund}/outspend/{vout}");
        assert_eq!(get_json(&idx, &outspend), json!({ "spent": false }));
        assert_eq!(get_status(&idx, &format!("/tx/{fund}/outspend/99")).0, 404);
        assert_eq!(get_status(&idx, &format!("/tx/{}", "00".repeat(32))).0, 404);

        // A few blocks later, spend it: in the mempool first…
        node.mine(3);
        let dest = node.wallet("miner", &["getnewaddress"]);
        let spend = node.wallet("a", &["sendtoaddress", &dest, "0.5"]);
        let v = poll(Duration::from_secs(10), "mempool spend", || {
            let v = get_json(&idx, &outspend);
            (v["spent"] == true).then_some(v)
        });
        assert_eq!(v["txid"], spend.as_str());
        assert_eq!(v["status"]["confirmed"], false);
        assert_eq!(
            get(&idx, &format!("/tx/{spend}/hex")).unwrap().trim(),
            node.cli(&["getrawtransaction", &spend]).trim()
        );

        // …then mined: found in its block, with the block's height.
        node.mine(1);
        let spent_at = node.height();
        let v = poll(Duration::from_secs(10), "mined spend", || {
            let v = get_json(&idx, &outspend);
            (v["status"]["confirmed"] == true).then_some(v)
        });
        assert_eq!(v["spent"], true);
        assert_eq!(v["txid"], spend.as_str());
        assert_eq!(v["status"]["block_height"], spent_at);
        assert!(spent_at > funded_at + 3);
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

#[test]
fn txout_reports_unspent_then_404_after_spend() {
    for_each_node(|_, node| {
        node.mine(101);
        let db = tmp_db("txout");
        let idx = start_index(&node, &db);

        // A fresh funding output: unspent, in the mempool (0 confirmations).
        let addr = node.wallet("miner", &["getnewaddress", "", "bech32"]);
        let txid = node.wallet("miner", &["sendtoaddress", &addr, "0.25"]);
        let vout = {
            let raw: Value =
                serde_json::from_str(&node.cli(&["getrawtransaction", &txid, "true"])).unwrap();
            raw["vout"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["scriptPubKey"]["address"] == addr.as_str())
                .map(|o| o["n"].as_u64().unwrap())
                .expect("funding output")
        };
        let path = format!("/txout/{txid}/{vout}");
        let (code, body) = get_status(&idx, &path);
        assert_eq!(code, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["value_sat"], 25_000_000);
        assert_eq!(v["confirmations"], 0);
        let spk = node.cli(&["getaddressinfo", &addr]);
        let spk: Value = serde_json::from_str(&spk).unwrap();
        assert_eq!(v["scriptpubkey_hex"], spk["scriptPubKey"]);

        // Mined: one confirmation, same value.
        node.mine(1);
        let v: Value = serde_json::from_str(&get_status(&idx, &path).1).unwrap();
        assert_eq!(v["confirmations"], 1);
        assert_eq!(v["value_sat"], 25_000_000);

        // Spend it: gone (the miner wallet may have picked another coin, so
        // spend this one explicitly).
        let dest = node.wallet("miner", &["getnewaddress"]);
        let inputs = json!([{ "txid": txid, "vout": vout }]).to_string();
        let outputs = json!([{ dest.clone(): 0.2499 }]).to_string();
        let raw = node.wallet("miner", &["createrawtransaction", &inputs, &outputs]);
        let funded = node.wallet("miner", &["signrawtransactionwithwallet", &raw]);
        let signed: Value = serde_json::from_str(&funded).unwrap();
        node.cli(&["sendrawtransaction", signed["hex"].as_str().unwrap()]);
        let (code, body) = get_status(&idx, &path);
        assert_eq!(code, 404, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "spent or unknown");
        // Confirmed UTXO set only: still there until the spend confirms.
        let confirmed_only = format!("{path}?mempool=false");
        let (code, body) = get_status(&idx, &confirmed_only);
        assert_eq!(code, 200, "{body}");
        node.mine(1);
        assert_eq!(get_status(&idx, &confirmed_only).0, 404);

        // Malformed txid.
        assert_eq!(get_status(&idx, "/txout/zz/0").0, 400);
        drop(idx);
        let _ = std::fs::remove_dir_all(&db);
    });
}

/// Esplora's script hash of an address's script: SHA-256, not reversed.
fn script_hash(spk_hex: &str) -> String {
    use bitcoin::hashes::{sha256, Hash};
    format!("{:x}", sha256::Hash::hash(&hex::decode(spk_hex).unwrap()))
}

fn get_bytes(idx: &Index, path: &str) -> Vec<u8> {
    let url = format!("http://127.0.0.1:{}{path}", idx.port);
    let r = ureq::get(&url).call().unwrap_or_else(|e| panic!("GET {path}: {e}"));
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut out).unwrap();
    out
}

/// The Esplora routes ldk-node's chain source calls, against what the node
/// itself says: script-hash history (with paging), tip hash, hashes by
/// height, block summaries / headers / status, raw tx, tx status, merkle
/// proofs and fee estimates.
#[test]
fn esplora_routes_ldk_node_uses_over_real_nodes() {
    for_each_node(|_, node| {
        node.mine(101);
        node.cli(&["createwallet", "a"]);
        let x = node.wallet("a", &["getnewaddress", "", "bech32"]);
        let info: Value = serde_json::from_str(&node.wallet("a", &["getaddressinfo", &x])).unwrap();
        let sh = script_hash(info["scriptPubKey"].as_str().unwrap());
        let db = tmp_db("esplora");
        let idx = start_index(&node, &db);

        // Unknown script: empty, not an error.
        assert_eq!(get_json(&idx, &format!("/scripthash/{}/txs", "ab".repeat(32))), json!([]));
        assert_eq!(get_status(&idx, "/scripthash/xyz/txs").0, 400);

        // Pending, then confirmed, by script hash.
        let fund = node.wallet("miner", &["sendtoaddress", &x, "1.0"]);
        let txs_path = format!("/scripthash/{sh}/txs");
        poll(Duration::from_secs(5), "pending by script hash", || {
            let v = get_json(&idx, &txs_path);
            (v[0]["txid"] == fund.as_str() && v[0]["status"]["confirmed"] == false).then_some(())
        });
        // Unconfirmed: no merkle proof yet.
        assert_eq!(get_status(&idx, &format!("/tx/{fund}/merkleblock-proof")).0, 404);
        node.mine(1);
        let h = node.height();
        let tip = node.cli(&["getbestblockhash"]).trim().to_string();
        poll(Duration::from_secs(10), "index at the tip", || (get(&idx, "/blocks/tip/hash")? .trim() == tip).then_some(()));
        let v = poll(Duration::from_secs(10), "confirmed by script hash", || {
            let v = get_json(&idx, &txs_path);
            (v[0]["status"]["confirmed"] == true).then_some(v)
        });
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["status"]["block_height"], h);
        assert_eq!(v[0]["status"]["block_hash"], tip.as_str());
        for k in ["version", "locktime", "size", "weight", "fee"] {
            assert!(v[0][k].is_number(), "{k} in {}", v[0]);
        }
        assert!(v[0]["vin"][0]["txid"].is_string() && v[0]["vin"][0]["witness"].is_array());

        // Blocks.
        assert_eq!(get(&idx, &format!("/block-height/{h}")).unwrap().trim(), tip);
        let b5 = node.cli(&["getblockhash", "5"]).trim().to_string();
        assert_eq!(get(&idx, "/block-height/5").unwrap().trim(), b5);
        assert_eq!(get_status(&idx, &format!("/block-height/{}", h + 10)).0, 404);
        let blocks = get_json(&idx, "/blocks");
        assert_eq!(blocks.as_array().unwrap().len(), 10);
        assert_eq!(blocks[0]["id"], tip.as_str());
        assert_eq!(blocks[0]["height"], h);
        assert_eq!(blocks[1]["height"], h - 1);
        let low = get_json(&idx, "/blocks/3");
        assert_eq!(low.as_array().unwrap().len(), 4, "heights 3..=0");
        assert!(low[3]["previousblockhash"].is_null(), "genesis has no parent");
        assert_eq!(
            get(&idx, &format!("/block/{b5}/header")).unwrap().trim(),
            node.cli(&["getblockheader", &b5, "false"]).trim()
        );
        let st = get_json(&idx, &format!("/block/{b5}/status"));
        assert_eq!(st["in_best_chain"], true);
        assert_eq!(st["height"], 5);
        assert_eq!(st["next_best"], node.cli(&["getblockhash", "6"]).trim());
        let tip_st = get_json(&idx, &format!("/block/{tip}/status"));
        assert!(tip_st["next_best"].is_null());
        assert_eq!(get_json(&idx, &format!("/block/{}/status", "11".repeat(32)))["in_best_chain"], false);

        // Transactions.
        assert_eq!(
            hex::encode(get_bytes(&idx, &format!("/tx/{fund}/raw"))),
            node.cli(&["getrawtransaction", &fund]).trim()
        );
        assert_eq!(get_status(&idx, &format!("/tx/{}/raw", "00".repeat(32))).0, 404);
        let ts = get_json(&idx, &format!("/tx/{fund}/status"));
        assert_eq!(ts["confirmed"], true);
        assert_eq!(ts["block_height"], h);
        assert_eq!(
            get(&idx, &format!("/tx/{fund}/merkleblock-proof")).unwrap().trim(),
            node.cli(&["gettxoutproof", &format!("[\"{fund}\"]")]).trim()
        );
        let full = get_json(&idx, &format!("/tx/{fund}"));
        assert!(full["vin"][0]["prevout"]["value"].is_number(), "{full}");
        // ldk-node's wallet rebuilds each transaction from this JSON and files
        // it under the txid it computes: it must be the real one.
        assert_rebuilds(&full);
        for t in get_json(&idx, &txs_path).as_array().unwrap() {
            assert_rebuilds(t);
        }
        // Coinbases too (Core names their script `coinbase`, not `scriptSig`).
        let block = get_json(&idx, &format!("/block/{tip}/status"));
        assert_eq!(block["in_best_chain"], true);
        let cb: Value = serde_json::from_str(&node.cli(&["getblock", &tip, "1"])).unwrap();
        assert_rebuilds(&get_json(&idx, &format!("/tx/{}", cb["tx"][0].as_str().unwrap())));

        // Fee estimates: every target, a number.
        let fees = get_json(&idx, "/fee-estimates");
        for t in ["1", "2", "3", "6", "144", "1008"] {
            assert!(fees[t].as_f64().is_some_and(|r| r >= 1.0), "{t}: {fees}");
        }

        // Paging: 30 more payments → 31 confirmed: 25, then 6.
        let mut ids = vec![fund.clone()];
        for _ in 0..30 {
            ids.push(node.wallet("miner", &["sendtoaddress", &x, "0.01"]));
        }
        node.mine(1);
        let page1 = poll(Duration::from_secs(15), "31 confirmed", || {
            let v = get_json(&idx, &txs_path);
            let all_conf = v.as_array()?.iter().all(|t| t["status"]["confirmed"] == true);
            (all_conf && v.as_array()?.len() == 25).then_some(v)
        });
        let last = page1[24]["txid"].as_str().unwrap();
        let page2 = get_json(&idx, &format!("/scripthash/{sh}/txs/chain/{last}"));
        assert_eq!(page2.as_array().unwrap().len(), 6);
        let mut seen: Vec<String> = page1.as_array().unwrap().iter().chain(page2.as_array().unwrap())
            .map(|t| t["txid"].as_str().unwrap().to_string()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 31, "every payment once across the pages");
        let last2 = page2[5]["txid"].as_str().unwrap();
        assert_eq!(get_json(&idx, &format!("/scripthash/{sh}/txs/chain/{last2}")), json!([]));
        drop(idx);
        let _ = std::fs::remove_dir_all(&db);
    });
}

/// esplora-client's `Tx::to_tx()` (what bdk_esplora stores) gives back the
/// transaction's own txid.
fn assert_rebuilds(v: &Value) {
    let tx: esplora_client::api::Tx = serde_json::from_value(v.clone()).expect("esplora-client Tx");
    assert_eq!(tx.to_tx().compute_txid(), tx.txid, "rebuilt txid differs: {v}");
}
