//! Real-node regtest harness for the `fortis-lightning` integration tests:
//! a Bitcoin Core regtest node, a throwaway PostgreSQL cluster and LDK's
//! `vss-server`, plus helpers to build `ldk_node` nodes against them. Helper
//! style copied from `crates/fortis-edge/tests/common/mod.rs`.
//!
//! External binaries (tests that need them are `#[ignore]`d):
//! - `FORTIS_BTC_BITCOIND` — stock Bitcoin Core `bitcoind` (`bitcoin-cli` next to it).
//! - `FORTIS_VSS_SERVER` — LDK `vss-server` built with `--cfg noop_authorizer`
//!   (`crates/fortis-lightning/scripts/build-vss-server.sh` builds it and prints the path).
//! - `initdb` / `pg_ctl` on `PATH` (or `FORTIS_PG_BIN` = their directory) for VSS's PostgreSQL.
//!
//! **Chain source.** No Esplora/electrs is installed on the dev host (and
//! running a downloaded electrs binary is not an option here), so the
//! harness points ldk-node at bitcoind over RPC
//! ([`Regtest::chain_source`]) — tests only. Production uses Esplora (edge
//! BTC or the user's own server). `Regtest::esplora_url` is therefore empty;
//! it stays in the struct so a later Esplora-backed harness can fill it
//! without changing callers. A bitcoind-RPC node starts syncing at the
//! current tip (no history rescan), unlike an Esplora full scan — restore
//! tests must create the restored node *before* the funds they expect land.
//!
//! Every temp dir is `fortis-ln-test-<label>-<pid>-<n>`; on start the
//! harness sweeps dirs left by dead test processes (and stops any bitcoind
//! or postgres still running from them), so killed runs cannot pile up in
//! /tmp. That covers `fortis-lsp` too ([`Lsp`], `tests/common/lsp.rs`):
//! its command line names its temp dir.

#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Event, Node};
use serde_json::Value;

mod lsp;
mod payer;
#[allow(unused_imports)]
pub use lsp::*;
#[allow(unused_imports)]
pub use payer::*;

const PREFIX: &str = "fortis-ln-test-";

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The path in env var `var`, or `None` (with a "skipping" note) when unset.
pub fn env_bin(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(p) => {
            let p = PathBuf::from(p);
            assert!(p.exists(), "{var}={} does not exist", p.display());
            Some(p)
        }
        None => {
            eprintln!("skipping: set {var}");
            None
        }
    }
}

// --------------------------------------------------------------------------
// temp dirs + sweeping dead runs
// --------------------------------------------------------------------------

/// A fresh, empty temp directory, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        sweep_dead_runs();
        let dir = std::env::temp_dir().join(format!(
            "{PREFIX}{label}-{}-{}",
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

    pub fn str(&self, name: &str) -> String {
        self.path(name).to_str().unwrap().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove_dir(&self.0);
    }
}

fn remove_dir(dir: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Removes `fortis-ln-test-*-<pid>-*` dirs whose test process is gone, after
/// killing any process still running out of them: bitcoind/postgres by
/// pidfile, and anything (vss-server, postgres, fortis-lsp) whose cwd or command line
/// names the dir.
pub fn sweep_dead_runs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(PREFIX) else {
            continue;
        };
        // <label>-<pid>-<n>; label may contain '-'.
        let parts: Vec<&str> = rest.rsplitn(3, '-').collect();
        let Some(pid) = parts.get(1).and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid_alive(pid) {
            continue;
        }
        let dir = e.path();
        let dir_str = dir.to_str().unwrap_or("\u{0}").to_string();
        for pidfile in [
            dir.join("bitcoind/regtest/bitcoind.pid"),
            dir.join("pg/postmaster.pid"),
        ] {
            if let Some(child) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.lines().next().and_then(|l| l.trim().parse::<u32>().ok()))
            {
                let cmdline = std::fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
                if String::from_utf8_lossy(&cmdline).contains(&dir_str) {
                    kill9(child);
                }
            }
        }
        for child in procs_referencing(&dir_str) {
            kill9(child);
        }
        remove_dir(&dir);
    }
}

fn kill9(pid: u32) {
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

/// Pids (other than ours) whose cwd or command line contains `needle`.
pub fn procs_referencing(needle: &str) -> Vec<u32> {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != me)
        .filter(|pid| {
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                .map(|p| p.to_string_lossy().to_string());
            let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            cwd.map(|c| c.contains(needle)).unwrap_or(false)
                || String::from_utf8_lossy(&cmd).contains(needle)
        })
        .collect()
}

// --------------------------------------------------------------------------
// regtest bitcoind
// --------------------------------------------------------------------------

pub const RPC_USER: &str = "fortis";
pub const RPC_PASS: &str = "fortis";

/// A Bitcoin Core regtest node with a funded `miner` wallet (101 blocks).
pub struct Regtest {
    pub rpc_url: String,
    /// Empty: no Esplora on the dev host — see the module docs and
    /// [`Regtest::chain_source`].
    pub esplora_url: String,
    pub rpc_port: u16,
    p2p_port: u16,
    bitcoind: PathBuf,
    child: Child,
    cli: PathBuf,
    dir: TempDir,
}

impl Regtest {
    /// Starts bitcoind from `FORTIS_BTC_BITCOIND` (panics when unset — gate
    /// callers with [`env_bin`]).
    pub fn start() -> Regtest {
        let bitcoind = env_bin("FORTIS_BTC_BITCOIND").expect("FORTIS_BTC_BITCOIND");
        let cli = bitcoind.with_file_name("bitcoin-cli");
        assert!(
            cli.exists(),
            "bitcoin-cli not next to bitcoind at {}",
            cli.display()
        );
        let dir = TempDir::new("btc");
        let datadir = dir.path("bitcoind");
        std::fs::create_dir_all(&datadir).unwrap();
        let (rpc_port, p2p_port) = (free_port(), free_port());
        let child = spawn_bitcoind(&bitcoind, &datadir, rpc_port, p2p_port, &[]);
        let rt = Regtest {
            rpc_url: format!("http://127.0.0.1:{rpc_port}"),
            esplora_url: String::new(),
            rpc_port,
            p2p_port,
            bitcoind,
            child,
            cli,
            dir,
        };
        rt.wait_up();
        rt.cli(&["createwallet", "miner"]);
        rt.mine(101);
        rt
    }

    fn wait_up(&self) {
        poll("bitcoind up", 60, || {
            self.try_cli(&["getblockchaininfo"]).ok()
        });
    }

    /// Stops bitcoind and starts it again on the same data dir and ports
    /// with `-persistmempool=0`: every unconfirmed transaction is gone (as
    /// when a transaction falls out of the network's mempools). Reloads the
    /// miner wallet.
    pub fn restart_dropping_mempool(&mut self) {
        let _ = self.try_cli(&["stop"]);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(self.child.try_wait(), Ok(Some(_))) {
            assert!(Instant::now() < deadline, "bitcoind did not stop");
            std::thread::sleep(Duration::from_millis(200));
        }
        let datadir = self.dir.path("bitcoind");
        self.child = spawn_bitcoind(
            &self.bitcoind,
            &datadir,
            self.rpc_port,
            self.p2p_port,
            &["-persistmempool=0"],
        );
        self.wait_up();
        let _ = self.try_cli(&["loadwallet", "miner"]);
        assert_eq!(self.json(&["getrawmempool"]).as_array().unwrap().len(), 0);
    }

    fn base_args(&self) -> Vec<String> {
        vec![
            "-regtest".into(),
            format!("-datadir={}", self.dir.path("bitcoind").display()),
            format!("-rpcport={}", self.rpc_port),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASS}"),
        ]
    }

    pub fn try_cli(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(&self.cli)
            .args(self.base_args())
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    pub fn cli(&self, args: &[&str]) -> String {
        self.try_cli(args)
            .unwrap_or_else(|e| panic!("bitcoin-cli {args:?}: {e}"))
    }

    pub fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.cli(args)).unwrap()
    }

    pub fn mine(&self, n: u32) {
        let addr = self.cli(&["-rpcwallet=miner", "getnewaddress"]);
        self.cli(&["generatetoaddress", &n.to_string(), &addr]);
    }

    /// Sends `sat` from the miner wallet to `address`; returns the txid.
    /// Does not mine.
    pub fn fund(&self, address: &str, sat: u64) -> String {
        let btc = format!("{}.{:08}", sat / 100_000_000, sat % 100_000_000);
        self.cli(&["-rpcwallet=miner", "sendtoaddress", address, &btc])
    }

    pub fn height(&self) -> u64 {
        self.cli(&["getblockcount"]).parse().unwrap()
    }

    /// Points `builder` at this node: bitcoind RPC chain source, regtest.
    pub fn chain_source(&self, builder: &mut Builder) {
        builder.set_network(Network::Regtest);
        builder.set_chain_source_bitcoind_rpc(
            "127.0.0.1".into(),
            self.rpc_port,
            RPC_USER.into(),
            RPC_PASS.into(),
        );
    }
}

fn spawn_bitcoind(
    bitcoind: &Path,
    datadir: &Path,
    rpc_port: u16,
    p2p_port: u16,
    extra: &[&str],
) -> Child {
    Command::new(bitcoind)
        .args([
            "-regtest".to_string(),
            format!("-datadir={}", datadir.display()),
            format!("-rpcport={rpc_port}"),
            format!("-port={p2p_port}"),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASS}"),
            "-listen=0".into(),
            "-server=1".into(),
            "-txindex=1".into(),
            "-fallbackfee=0.0002".into(),
            "-printtoconsole=0".into(),
        ])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bitcoind")
}

impl Drop for Regtest {
    fn drop(&mut self) {
        let _ = self.try_cli(&["stop"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --------------------------------------------------------------------------
// VSS: throwaway PostgreSQL + vss-server
// --------------------------------------------------------------------------

fn pg_bin(name: &str) -> PathBuf {
    match std::env::var_os("FORTIS_PG_BIN") {
        Some(d) => PathBuf::from(d).join(name),
        None => PathBuf::from(name),
    }
}

/// A PostgreSQL cluster started with `pg_ctl`; stopped on drop.
struct PgGuard {
    data: PathBuf,
}

impl Drop for PgGuard {
    fn drop(&mut self) {
        let _ = Command::new(pg_bin("pg_ctl"))
            .args([
                "-D",
                self.data.to_str().unwrap(),
                "-m",
                "immediate",
                "-w",
                "stop",
            ])
            .output();
    }
}

/// A child process, killed on drop.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// LDK `vss-server` (no-op auth) on a private PostgreSQL cluster in a temp
/// dir. `url` is the client base URL (`…/vss`).
///
/// Each process is held by its own guard the moment it starts, so a panic
/// anywhere in `start` (failed spawn, server exiting, timeout) still stops
/// what was already running. Fields drop in order: server, postgres, dir.
pub struct Vss {
    pub url: String,
    server: ChildGuard,
    pg: PgGuard,
    dir: TempDir,
    bin: PathBuf,
    port: u16,
    pg_port: u16,
}

impl Vss {
    /// Starts from `FORTIS_VSS_SERVER` (panics when unset).
    pub fn start() -> Vss {
        Vss::start_with(&env_bin("FORTIS_VSS_SERVER").expect("FORTIS_VSS_SERVER"))
    }

    /// Starts PostgreSQL, then the vss-server binary `bin`.
    pub fn start_with(bin: &Path) -> Vss {
        let dir = TempDir::new("vss");
        let pg_data = dir.path("pg");
        let out = Command::new(pg_bin("initdb"))
            .args([
                "-D",
                pg_data.to_str().unwrap(),
                "-U",
                "postgres",
                "--auth=trust",
                "-E",
                "UTF8",
            ])
            .output()
            .expect("run initdb (set FORTIS_PG_BIN or put it on PATH)");
        assert!(
            out.status.success(),
            "initdb: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pg_port = free_port();
        let opts = format!(
            "-p {pg_port} -k {} -c listen_addresses=127.0.0.1",
            dir.0.display()
        );
        // Guard before pg_ctl: `pg_ctl -w start` can fail after postgres is up.
        let pg = PgGuard {
            data: pg_data.clone(),
        };
        let out = Command::new(pg_bin("pg_ctl"))
            .args([
                "-D",
                pg_data.to_str().unwrap(),
                "-o",
                &opts,
                "-l",
                &dir.str("pg.log"),
                "-w",
                "start",
            ])
            .output()
            .expect("run pg_ctl");
        assert!(
            out.status.success(),
            "pg_ctl start: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let port = free_port();
        let server = spawn_vss_server(bin, &dir, port, pg_port);
        Vss {
            url: format!("http://127.0.0.1:{port}/vss"),
            server,
            pg,
            dir,
            bin: bin.to_path_buf(),
            port,
            pg_port,
        }
    }

    /// Kills vss-server (PostgreSQL and its data stay): connections are
    /// refused, as with a VSS host that is down. [`Vss::up`] starts it
    /// again on the same port. Unlike [`Vss::pause`], nothing sent while it
    /// is down is executed later.
    pub fn down(&mut self) {
        let _ = self.server.0.kill();
        let _ = self.server.0.wait();
    }

    pub fn up(&mut self) {
        self.server = spawn_vss_server(&self.bin, &self.dir, self.port, self.pg_port);
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path("vss.log")).unwrap_or_default()
    }

    /// Freezes vss-server (SIGSTOP): connections are accepted by the kernel
    /// but never answered, as with a VSS host that went away. Its state is
    /// kept; [`Vss::resume`] (or drop) lets it go on.
    pub fn pause(&self) {
        self.signal("-STOP");
    }

    pub fn resume(&self) {
        self.signal("-CONT");
    }

    fn signal(&self, sig: &str) {
        let ok = Command::new("kill")
            .arg(sig)
            .arg(self.server.0.id().to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "kill {sig} vss-server");
    }
}

/// Starts vss-server on `port` against the PostgreSQL on `pg_port` and
/// waits until it answers.
fn spawn_vss_server(bin: &Path, dir: &TempDir, port: u16, pg_port: u16) -> ChildGuard {
    let mut server = ChildGuard(
        Command::new(bin)
            .current_dir(&dir.0)
            .env("VSS_BIND_ADDRESS", format!("127.0.0.1:{port}"))
            .env("VSS_PSQL_USERNAME", "postgres")
            .env("VSS_PSQL_PASSWORD", "unused")
            .env("VSS_PSQL_ADDRESS", format!("127.0.0.1:{pg_port}"))
            .env("VSS_PSQL_DEFAULT_DB", "postgres")
            .env("VSS_PSQL_VSS_DB", "vss")
            .env("VSS_LOG_FILE", dir.path("vss.log"))
            .env("VSS_LOG_LEVEL", "debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn vss-server {}: {e}", bin.display())),
    );
    let metrics = format!("http://127.0.0.1:{port}/metrics");
    poll("vss-server up", 30, || {
        if let Ok(Some(status)) = server.0.try_wait() {
            panic!("vss-server exited during startup: {status}");
        }
        ureq::get(&metrics).call().ok()
    });
    server
}

// --------------------------------------------------------------------------
// ldk-node helpers
// --------------------------------------------------------------------------

/// A builder for a regtest node: given 64-byte seed, storage dir, a free
/// listening port, and the regtest chain source.
pub fn builder(rt: &Regtest, seed: [u8; 64], storage_dir: &str) -> (Builder, SocketAddress) {
    let mut b = Builder::new();
    rt.chain_source(&mut b);
    b.set_entropy_seed_bytes(seed);
    b.set_storage_dir_path(storage_dir.to_string());
    let addr: SocketAddress = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    b.set_listening_addresses(vec![addr.clone()]).unwrap();
    (b, addr)
}

/// Waits up to `secs` for an event matching `pred`, acknowledging every
/// event seen on the way. Returns the match.
pub fn wait_event(
    node: &Node,
    what: &str,
    secs: u64,
    mut pred: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(e) = node.next_event() {
            node.event_handled().unwrap();
            if pred(&e) {
                return e;
            }
            eprintln!("({what}: skipping {e:?})");
            continue;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
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

/// A random 64-byte seed (not secret: tests only).
pub fn test_seed(tag: u8) -> [u8; 64] {
    let mut s = [0u8; 64];
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for (i, b) in s.iter_mut().enumerate() {
        *b = (t >> ((i % 16) * 8)) as u8 ^ tag ^ (i as u8).wrapping_mul(31);
    }
    s
}
