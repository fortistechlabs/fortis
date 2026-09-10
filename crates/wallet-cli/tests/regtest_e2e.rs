//! End-to-end test against a real regtest node with the BLAKE2b deployment active.
//!
//! Proves the whole `send` path — coin selection, `SIGHASH_UNIFIED` signing, fee
//! estimation, `testmempoolaccept`, broadcast — against a node that enforces the
//! fork's consensus rules.
//!
//! Opt-in: set `FORTIS_BITCOIND` to a `bitcoind` from a Knots BLAKE2b build
//! (`bitcoin-cli` must sit next to it). Without it the test is a no-op.
//!
//! ```sh
//! FORTIS_BITCOIND="/c/Program Files/Bitcoin Knots/daemon/bitcoind.exe" \
//!   cargo test -p wallet-cli --test regtest_e2e -- --nocapture
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FORTIS: &str = env!("CARGO_BIN_EXE_fortis");
const PHRASE: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const RPC_PORT: u16 = 19443;
const P2P_PORT: u16 = 19444;

struct Node {
    child: Child,
    datadir: PathBuf,
    cli: PathBuf,
}

impl Node {
    fn cli(&self, args: &[&str]) -> String {
        let out = Command::new(&self.cli)
            .args([
                "-regtest",
                &format!("-datadir={}", self.datadir.display()),
                &format!("-rpcport={RPC_PORT}"),
            ])
            .args(args)
            .output()
            .expect("run bitcoin-cli");
        assert!(
            out.status.success(),
            "bitcoin-cli {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn wallet(&self, wallet: &str, args: &[&str]) -> String {
        let mut a = vec![format!("-rpcwallet={wallet}")];
        a.extend(args.iter().map(|s| s.to_string()));
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        self.cli(&refs)
    }

    fn mine(&self, n: u32) {
        let addr = self.wallet("miner", &["getnewaddress"]);
        self.wallet("miner", &["generatetoaddress", &n.to_string(), &addr]);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new(&self.cli)
            .args([
                "-regtest",
                &format!("-datadir={}", self.datadir.display()),
                &format!("-rpcport={RPC_PORT}"),
                "stop",
            ])
            .output();
        std::thread::sleep(Duration::from_millis(800));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

fn start_node(bitcoind: &Path) -> Node {
    let cli = bitcoind.with_file_name(if cfg!(windows) {
        "bitcoin-cli.exe"
    } else {
        "bitcoin-cli"
    });
    assert!(cli.exists(), "bitcoin-cli not found next to bitcoind at {}", cli.display());

    let datadir = std::env::temp_dir().join(format!("fortis-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&datadir).unwrap();

    let child = Command::new(bitcoind)
        .args([
            "-regtest",
            "-testactivationheight=blake2b@1",
            &format!("-datadir={}", datadir.display()),
            &format!("-rpcport={RPC_PORT}"),
            &format!("-port={P2P_PORT}"),
            "-server=1",
            "-fallbackfee=0.0002",
            "-printtoconsole=0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bitcoind");

    let node = Node { child, datadir, cli };

    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let ok = Command::new(&node.cli)
            .args([
                "-regtest",
                &format!("-datadir={}", node.datadir.display()),
                &format!("-rpcport={RPC_PORT}"),
                "getblockchaininfo",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            break;
        }
        assert!(Instant::now() < deadline, "node did not come up");
        std::thread::sleep(Duration::from_millis(500));
    }
    node
}

fn fortis(home: &Path, node: &Node, args: &[&str], stdin: Option<&str>) -> String {
    let mut cmd = Command::new(FORTIS);
    cmd.arg("--home")
        .arg(home)
        .args(args)
        .env("FORTIS_SEED_PASSWORD", "regtest-e2e-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn fortis");
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "fortis {:?} failed\nstdout: {stdout}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = node; // keep the node alive for the call's duration
    stdout
}

fn btc(s: &str) -> f64 {
    s.trim().parse().unwrap_or(0.0)
}

#[test]
fn send_over_regtest_blake2b() {
    let Ok(bitcoind) = std::env::var("FORTIS_BITCOIND") else {
        eprintln!("skipping: set FORTIS_BITCOIND to a Knots BLAKE2b bitcoind");
        return;
    };
    let bitcoind = PathBuf::from(bitcoind);
    let node = start_node(&bitcoind);
    let home = node.datadir.join("wallet");
    let rpc_url = format!("http://127.0.0.1:{RPC_PORT}");

    // deployment must really be active, else the whole test proves nothing
    let dep = node.cli(&["getdeploymentinfo"]);
    assert!(
        dep.contains("\"blake2b\"") && dep.contains("\"active\": true"),
        "blake2b deployment not active on the test node"
    );

    node.cli(&["createwallet", "miner"]);
    node.mine(110);

    let datadir = node.datadir.to_string_lossy().into_owned();
    fortis(
        &home,
        &node,
        &[
            "init", "--restore", "--chain", "xbt", "--network", "regtest",
            "--datadir", &datadir, "--rpc-url", &rpc_url,
        ],
        Some(&format!("{PHRASE}\n")),
    );

    fortis(&home, &node, &["connect"], None);

    // fund a fortis receive address with 4 XBT
    let addr = fortis(&home, &node, &["address"], None);
    let addr = addr.lines().next().unwrap().trim();
    node.wallet("miner", &["sendtoaddress", addr, "4"]);
    node.mine(1);

    let bal = fortis(&home, &node, &["balance"], None);
    assert!(bal.contains("4.00000000"), "unexpected balance:\n{bal}");

    // send 1 XBT, phrase piped
    let dest = node.wallet("miner", &["getnewaddress"]);
    let summary = fortis(
        &home,
        &node,
        &["send", &dest, "1", "--phrase-stdin", "--yes"],
        Some(&format!("{PHRASE}\n")),
    );
    assert!(summary.contains("broadcast"), "send did not broadcast:\n{summary}");
    node.mine(1);

    assert!((btc(&node.wallet("miner", &["getreceivedbyaddress", &dest])) - 1.0).abs() < 1e-8);
    let utxos = fortis(&home, &node, &["utxos"], None);
    assert!(utxos.contains("2.999"), "expected ~3 XBT change:\n{utxos}");

    // seal the seed, then send again using the sealed seed (no --phrase-stdin)
    fortis(&home, &node, &["import-seed", "--phrase-stdin"], Some(&format!("{PHRASE}\n\n")));
    assert!(home.join("seed.enc").exists(), "import-seed did not write seed.enc");

    let dest2 = node.wallet("miner", &["getnewaddress"]);
    fortis(&home, &node, &["send", &dest2, "--sweep", "--yes"], None);
    node.mine(1);

    // check the node's own view of the fortis wallet, not fortis's formatting
    let swept = btc(&node.wallet("fortis-xbt", &["getbalance"]));
    assert!(swept.abs() < 1e-8, "wallet not swept, node still sees {swept} XBT");
}
