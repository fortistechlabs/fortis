//! End-to-end: a wallet client's full flow through `fortis-edge` → `fortis-index`
//! → a regtest Knots BLAKE2b node.
//!
//! Proves, together: the index syncs blocks and serves the Esplora shape the app
//! expects, the mempool overlay surfaces an unconfirmed change output, the edge
//! mints a token and gates + rate-limits + proxies the chain routes, and the
//! `wallet-core` send path (derive → coin-select → `SIGHASH_UNIFIED` sign →
//! broadcast) works across the whole chain.
//!
//! Opt-in. Build the workspace first, then:
//! ```sh
//! cargo build --workspace
//! FORTIS_BITCOIND="/c/Program Files/Bitcoin Knots/daemon/bitcoind.exe" \
//!   cargo test -p fortis-edge --test regtest_e2e -- --nocapture
//! ```

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use wallet_core::bitcoin::address::NetworkUnchecked;
use wallet_core::bitcoin::{consensus, Address, Amount, OutPoint, TxOut, Txid};
use wallet_core::{Chain, ChainParams, MasterKey, Utxo, WalletView};

const PHRASE: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A workspace binary that lives next to this test's `fortis-edge` binary.
fn bin(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_BIN_EXE_fortis-edge"));
    p.set_file_name(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    assert!(p.exists(), "{} not built — run `cargo build --workspace` first", p.display());
    p
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
            .args(["-regtest", &format!("-datadir={}", self.datadir.display()), &format!("-rpcport={}", self.rpc_port)])
            .args(args)
            .output()
            .expect("run bitcoin-cli");
        assert!(out.status.success(), "bitcoin-cli {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
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
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new(&self.cli)
            .args(["-regtest", &format!("-datadir={}", self.datadir.display()), &format!("-rpcport={}", self.rpc_port), "stop"])
            .output();
        std::thread::sleep(Duration::from_millis(800));
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Windows can hold file handles briefly after the process exits.
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.datadir).is_ok() || !self.datadir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

fn start_node(bitcoind: &Path, rpc_port: u16, p2p_port: u16) -> Node {
    let cli = bitcoind.with_file_name(if cfg!(windows) { "bitcoin-cli.exe" } else { "bitcoin-cli" });
    assert!(cli.exists(), "bitcoin-cli not next to bitcoind at {}", cli.display());

    let datadir = std::env::temp_dir().join(format!("fortis-edge-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&datadir).unwrap();

    let child = Command::new(bitcoind)
        .args([
            "-regtest",
            "-testactivationheight=blake2b@1",
            &format!("-datadir={}", datadir.display()),
            &format!("-rpcport={rpc_port}"),
            &format!("-port={p2p_port}"),
            "-server=1",
            "-fallbackfee=0.0002",
            "-printtoconsole=0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bitcoind");

    let node = Node { child, datadir, cli, rpc_port };
    let deadline = Instant::now() + Duration::from_secs(40);
    while Command::new(&node.cli)
        .args(["-regtest", &format!("-datadir={}", node.datadir.display()), &format!("-rpcport={rpc_port}"), "getblockchaininfo"])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        assert!(Instant::now() < deadline, "node did not come up");
        std::thread::sleep(Duration::from_millis(500));
    }
    node
}

// --------------------------------------------------------------------------
// child services
// --------------------------------------------------------------------------

struct Svc(Child);
impl Drop for Svc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn spawn(bin: &Path, args: &[&str]) -> Svc {
    Svc(Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display())))
}

// --------------------------------------------------------------------------
// tiny HTTP client
// --------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build()
}

fn req(a: &ureq::Agent, method: &str, url: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    let mut r = a.request(method, url);
    if let Some(t) = token {
        r = r.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = match body {
        Some(b) => r.set("Content-Type", "text/plain").send_string(b),
        None => r.call(),
    };
    match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => (0, e.to_string()),
    }
}

fn wait_up(a: &ureq::Agent, url: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while req(a, "GET", url, None, None).0 / 100 != 2 {
        assert!(Instant::now() < deadline, "{url} did not come up");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn poll_json<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "condition not met in time");
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn cookie_of(node: &Node) -> String {
    let mut s = String::new();
    std::fs::File::open(node.datadir.join("regtest").join(".cookie"))
        .expect("regtest cookie")
        .read_to_string(&mut s)
        .unwrap();
    s.trim().to_string()
}

// --------------------------------------------------------------------------

#[test]
fn wallet_flow_through_the_edge() {
    let Ok(bitcoind) = std::env::var("FORTIS_BITCOIND") else {
        eprintln!("skipping: set FORTIS_BITCOIND to a Knots BLAKE2b bitcoind");
        return;
    };
    let idx_bin = bin("fortis-index");
    let edge_bin = bin("fortis-edge");

    let (rpc_port, p2p_port) = (free_port(), free_port());
    let node = start_node(Path::new(&bitcoind), rpc_port, p2p_port);

    let dep = node.cli(&["getdeploymentinfo"]);
    assert!(
        dep.contains("\"blake2b\"") && dep.contains("\"active\": true"),
        "blake2b deployment not active on the test node"
    );
    node.cli(&["createwallet", "miner"]);
    node.mine(110);

    // --- fortis-index against the node ---
    let idx_port = free_port();
    let rpc_url = format!("http://127.0.0.1:{rpc_port}");
    let cookie_file = node.datadir.join("regtest").join(".cookie");
    let _idx = spawn(
        &idx_bin,
        &[
            "--network", "regtest",
            "--rpc-url", &rpc_url,
            "--cookie-file", cookie_file.to_str().unwrap(),
            "--db", node.datadir.join("idx.sqlite").to_str().unwrap(),
            "--start-height", "0",
            "--bind", &format!("127.0.0.1:{idx_port}"),
            "--poll", "1",
        ],
    );
    let _ = cookie_of(&node); // sanity: the cookie exists

    // --- fortis-edge in front of it ---
    let edge_port = free_port();
    let _edge = spawn(
        &edge_bin,
        &[
            "--bind", &format!("127.0.0.1:{edge_port}"),
            "--xbt-upstream", &format!("http://127.0.0.1:{idx_port}"),
            "--secret-file", node.datadir.join("edge.secret").to_str().unwrap(),
            "--require-token",
            "--rate-per-min", "6000",
            "--rate-burst", "100",
        ],
    );

    let a = agent();
    wait_up(&a, &format!("http://127.0.0.1:{idx_port}/"));
    wait_up(&a, &format!("http://127.0.0.1:{edge_port}/"));
    let edge = format!("http://127.0.0.1:{edge_port}");

    // --- token gate ---
    assert_eq!(req(&a, "GET", &format!("{edge}/xbt/blocks/tip/height"), None, None).0, 401, "no token must 401");
    let (s, body) = req(&a, "POST", &format!("{edge}/register"), None, Some(""));
    assert_eq!(s, 200, "register failed: {body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    // --- wallet from the phrase ---
    let params = ChainParams::resolve(Chain::Xbt, "regtest").unwrap();
    let key = MasterKey::from_phrase(PHRASE, "").unwrap();
    let xpub = key.account_xpub(&params, 0).unwrap();
    let mut view = WalletView::new(params.clone(), xpub);
    let recv0 = view.address_at(0, 0).unwrap();

    // fund receive address 0 with 4 XBT, confirm
    node.wallet("miner", &["sendtoaddress", &recv0.to_string(), "4"]);
    node.mine(1);

    // --- the index sees the confirmed UTXO through the edge ---
    let utxo_url = format!("{edge}/xbt/address/{recv0}/utxo");
    let utxos: serde_json::Value = poll_json(|| {
        let (s, b) = req(&a, "GET", &utxo_url, Some(&token), None);
        if s != 200 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&b).ok()?;
        let non_empty = v.as_array().is_some_and(|arr| !arr.is_empty());
        non_empty.then_some(v)
    });
    assert_eq!(utxos[0]["value"].as_u64().unwrap(), 400_000_000);
    assert_eq!(utxos[0]["status"]["confirmed"], serde_json::Value::Bool(true));

    // --- build + sign a 1 XBT payment (wallet-core, exactly as the app does) ---
    let dest = node.wallet("miner", &["getnewaddress", "", "bech32"]);
    let dest_spk = Address::<NetworkUnchecked>::from_str(&dest)
        .unwrap()
        .require_network(params.network)
        .unwrap()
        .script_pubkey();

    let coins: Vec<Utxo> = utxos
        .as_array()
        .unwrap()
        .iter()
        .map(|u| Utxo {
            outpoint: OutPoint::new(Txid::from_str(u["txid"].as_str().unwrap()).unwrap(), u["vout"].as_u64().unwrap() as u32),
            value: Amount::from_sat(u["value"].as_u64().unwrap()),
            script_pubkey: recv0.script_pubkey(),
            confirmations: 1,
            derivation_index: 0,
            is_change: false,
        })
        .collect();

    let plan = view
        .plan_payment(&coins, vec![TxOut { value: Amount::from_sat(100_000_000), script_pubkey: dest_spk }], 2, 1, None, false)
        .unwrap();
    let (_, next_change) = view.next_indices();
    let change_addr = view.address_at(1, next_change - 1).unwrap();

    let mut tx = plan.tx.clone();
    let prevouts: Vec<TxOut> =
        plan.selected.iter().map(|u| TxOut { value: u.value, script_pubkey: u.script_pubkey.clone() }).collect();
    let paths: Vec<(bool, u32)> = plan.selected.iter().map(|u| (u.is_change, u.derivation_index)).collect();
    key.sign_p2wpkh_tx(&params, 0, &mut tx, &prevouts, &paths).unwrap();
    let raw_hex = hex::encode(consensus::serialize(&tx));

    // --- service fee: a second edge configured with a fee address rejects this
    //     transaction (it pays nothing to the fee address) and advertises /pricing ---
    {
        let fee_addr = node.wallet("miner", &["getnewaddress"]);
        let fee_port = free_port();
        let _fee_edge = spawn(
            &edge_bin,
            &[
                "--bind", &format!("127.0.0.1:{fee_port}"),
                "--xbt-upstream", &format!("http://127.0.0.1:{idx_port}"),
                "--secret-file", node.datadir.join("edge2.secret").to_str().unwrap(),
                "--network", "regtest",
                "--service-fee-address", &fee_addr,
                "--service-fee-floor-sat", "200",
            ],
        );
        wait_up(&a, &format!("http://127.0.0.1:{fee_port}/"));
        let (ps, pb) = req(&a, "GET", &format!("http://127.0.0.1:{fee_port}/pricing"), None, None);
        assert_eq!(ps, 200, "pricing: {pb}");
        assert!(pb.contains("\"floor_sat\":200") && pb.contains(&fee_addr), "pricing shape: {pb}");
        let (fs, fb) = req(&a, "POST", &format!("http://127.0.0.1:{fee_port}/xbt/tx"), None, Some(&raw_hex));
        assert_eq!(fs, 402, "a tx that doesn't pay the fee must be rejected: {fb}");
    }

    // --- broadcast through the edge ---
    let (s, txid_body) = req(&a, "POST", &format!("{edge}/xbt/tx"), Some(&token), Some(&raw_hex));
    assert_eq!(s, 200, "broadcast failed: {txid_body}");
    assert_eq!(txid_body.trim().len(), 64, "not a txid: {txid_body}");

    // --- mempool overlay: the change output is visible, unconfirmed ---
    let change_url = format!("{edge}/xbt/address/{change_addr}/utxo");
    let mp: serde_json::Value = poll_json(|| {
        let (_, b) = req(&a, "GET", &change_url, Some(&token), None);
        let v: serde_json::Value = serde_json::from_str(&b).ok()?;
        let non_empty = v.as_array().is_some_and(|arr| !arr.is_empty());
        non_empty.then_some(v)
    });
    assert_eq!(mp[0]["status"]["confirmed"], serde_json::Value::Bool(false), "change should be unconfirmed in the mempool");

    // --- confirm; the same output flips to confirmed ---
    node.mine(1);
    let confirmed: serde_json::Value = poll_json(|| {
        let (_, b) = req(&a, "GET", &change_url, Some(&token), None);
        let v: serde_json::Value = serde_json::from_str(&b).ok()?;
        let first = v.as_array()?.first()?.clone();
        (first["status"]["confirmed"] == serde_json::Value::Bool(true)).then_some(v)
    });
    assert!(confirmed[0]["value"].as_u64().unwrap() > 250_000_000, "change ~3 XBT expected");

    // node's own view: the destination received 1 XBT
    let got: f64 = node.wallet("miner", &["getreceivedbyaddress", &dest]).parse().unwrap();
    assert!((got - 1.0).abs() < 1e-8, "dest got {got} XBT");

    // --- /txs shape the client parses ---
    let (_, txs_b) = req(&a, "GET", &format!("{edge}/xbt/address/{recv0}/txs"), Some(&token), None);
    let txs: serde_json::Value = serde_json::from_str(&txs_b).unwrap();
    let spend = txs
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["vin"].as_array().is_some_and(|v| !v.is_empty()))
        .expect("a spend tx in history");
    assert!(spend["vin"][0]["prevout"]["value"].is_u64());
    assert!(spend["vout"][0]["value"].is_u64());
    assert!(spend["fee"].as_u64().unwrap() > 0);

    // --- cache hits are free: hammering one cached URL never trips the limiter ---
    let tip_url = format!("{edge}/xbt/blocks/tip/height");
    let limited_cached = (0..300).filter(|_| req(&a, "GET", &tip_url, Some(&token), None).0 == 429).count();
    assert_eq!(limited_cached, 0, "cached responses must not spend rate budget");

    // --- distinct uncached requests do: burst 100, fire 160 -> some 429s ---
    let limited = (0..160)
        .filter(|i| req(&a, "GET", &format!("{edge}/xbt/address/{recv0}/txs?x={i}"), Some(&token), None).0 == 429)
        .count();
    assert!(limited > 0, "expected the limiter to reject some of a 160-request burst of distinct queries");
}
