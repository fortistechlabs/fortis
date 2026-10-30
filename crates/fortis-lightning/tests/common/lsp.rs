//! The real `fortis-lsp` binary as the tests' LSP.
//!
//! [`Lsp::start`] writes an `lsp.toml` into a `fortis-ln-test-fortis-lsp-<pid>-<n>`
//! temp dir (regtest, bitcoind RPC of the given [`Regtest`], free ports,
//! the RPC password in a 0600 file), spawns the binary built next to the
//! test binary (`cargo build -p fortis-lsp` first), waits for its `ready`
//! line, reads `GET /info`, funds its on-chain wallet and waits until the
//! LSP sees the funds. The process is held by a guard from the moment it
//! spawns, so a panic anywhere (in `start` or in the test) still stops it:
//! SIGTERM, then SIGKILL after 15 s. The temp dir goes after the process.
//!
//! The binary's command line names the temp dir (`--config <dir>/lsp.toml`),
//! so [`super::sweep_dead_runs`] kills one left over from a killed test run
//! and removes its dir, like bitcoind/postgres/vss-server.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fortis_lightning::{LspInfo, LspLimits};
use serde_json::Value;

use super::{free_port, poll, Regtest, TempDir, RPC_PASS, RPC_USER};

/// What [`Lsp::start`] puts in the LSP's on-chain wallet (0.1 BTC).
pub const LSP_FUNDING_SAT: u64 = 10_000_000;

/// The `fortis-lsp` binary next to this test binary (`target/<profile>/`).
pub fn lsp_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/<test>-<hash> -> target/<profile>/fortis-lsp
    let p = exe
        .parent()
        .and_then(|d| d.parent())
        .unwrap()
        .join("fortis-lsp");
    assert!(
        p.exists(),
        "{} not built — run `cargo build -p fortis-lsp` first",
        p.display()
    );
    p
}

/// A running `fortis-lsp`; stopped (SIGTERM, then SIGKILL) on drop.
struct LspProcess {
    child: Child,
}

impl LspProcess {
    /// SIGTERM, wait up to `secs`, then SIGKILL. Returns the exit status.
    fn stop(&mut self, secs: u64) -> Option<ExitStatus> {
        if let Ok(Some(s)) = self.child.try_wait() {
            return Some(s);
        }
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(Some(s)) = self.child.try_wait() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        self.stop(15);
    }
}

/// `fortis-lsp` on regtest. Fields drop in order: process, then dir.
pub struct Lsp {
    pub node_id: String,
    pub host: String,
    /// Lightning P2P port.
    pub port: u16,
    pub token: Option<String>,
    /// `http://127.0.0.1:<port>/metrics`.
    pub metrics_url: String,
    /// The funding address it printed at (first) start.
    pub onchain_address: String,
    /// `GET /info` as read at start.
    pub info: Value,
    proc: Option<LspProcess>,
    dir: TempDir,
}

impl Lsp {
    /// Starts `fortis-lsp` against `rt` and funds it with
    /// [`LSP_FUNDING_SAT`]. `overrides` are `lsp.toml` lines as
    /// `(key, toml value)`, e.g. `("token", "\"t0k\"")` or
    /// `("channel_max_sat", "1000000")`; they replace the harness defaults.
    pub fn start(rt: &Regtest, overrides: &[(&str, &str)]) -> Lsp {
        let dir = TempDir::new("fortis-lsp");
        let pw = dir.path("rpc-password");
        std::fs::write(&pw, format!("{RPC_PASS}\n")).unwrap();
        std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o600)).unwrap();

        let port = free_port();
        let q = |s: &str| serde_json::to_string(s).unwrap();
        let mut cfg: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in [
            ("network", q("regtest")),
            ("listen", q(&format!("127.0.0.1:{port}"))),
            ("bitcoind_rpc", q(&format!("127.0.0.1:{}", rt.rpc_port))),
            ("rpc_user", q(RPC_USER)),
            ("rpc_password_file", q(pw.to_str().unwrap())),
            ("data_dir", q(&dir.str("data"))),
            ("metrics_listen", q(&format!("127.0.0.1:{}", free_port()))),
        ] {
            cfg.insert(k.into(), v);
        }
        for (k, v) in overrides {
            cfg.insert(k.to_string(), v.to_string());
        }
        let token = cfg
            .get("token")
            .map(|t| serde_json::from_str::<String>(t).expect("token override: a quoted string"));
        let toml: String = cfg.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
        std::fs::write(dir.path("lsp.toml"), toml).unwrap();

        let mut lsp = Lsp {
            node_id: String::new(),
            host: "127.0.0.1".into(),
            port,
            token,
            metrics_url: String::new(),
            onchain_address: String::new(),
            info: Value::Null,
            proc: None,
            dir,
        };
        let lines = lsp.spawn();
        lsp.node_id = lines["node_id"].clone();
        lsp.onchain_address = lines["onchain_address"].clone();
        lsp.metrics_url = format!("http://{}/metrics", lines["metrics"]);
        let info_url = lsp.metrics_url.replace("/metrics", "/info");
        lsp.info = serde_json::from_str(
            &ureq::get(&info_url)
                .call()
                .expect("GET /info")
                .into_string()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            lsp.info["node_id"].as_str(),
            Some(lsp.node_id.as_str()),
            "/info node id"
        );
        assert_eq!(lines["listen"], format!("127.0.0.1:{port}"));

        lsp.fund(rt, LSP_FUNDING_SAT);
        lsp
    }

    /// Spawns the binary on this dir's `lsp.toml`; returns its startup
    /// `key value` lines once it printed `ready`.
    fn spawn(&mut self) -> BTreeMap<String, String> {
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.path("stderr.log"))
            .unwrap();
        let child = Command::new(lsp_bin())
            .arg("--config")
            .arg(self.dir.path("lsp.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("spawn fortis-lsp");
        // Guarded before anything else can panic.
        let proc = self.proc.insert(LspProcess { child });
        let stdout = proc.child.stdout.take().unwrap();
        let pid = proc.child.id();
        let (tx, rx) = mpsc::channel::<String>();
        // Keeps draining stdout for the life of the process.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                eprintln!("[fortis-lsp] {line}");
                let _ = tx.send(line);
            }
        });
        let mut lines = BTreeMap::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) if l == "ready" => break,
                Ok(l) => {
                    if let Some((k, v)) = l.split_once(' ') {
                        lines.insert(k.to_string(), v.to_string());
                    }
                }
                Err(_) => {
                    let exited = self
                        .proc
                        .as_mut()
                        .and_then(|p| p.child.try_wait().ok().flatten());
                    if let Some(status) = exited {
                        panic!(
                            "fortis-lsp (pid {pid}) exited during startup ({status}): {}",
                            self.stderr()
                        );
                    }
                    assert!(
                        Instant::now() < deadline,
                        "fortis-lsp not ready after 60 s: {}",
                        self.stderr()
                    );
                }
            }
        }
        for k in ["node_id", "listen", "metrics", "onchain_address"] {
            assert!(
                lines.contains_key(k),
                "fortis-lsp did not print {k}: {lines:?}"
            );
        }
        lines
    }

    /// Sends `sat` to the LSP's on-chain wallet, mines 6 blocks and waits
    /// until its `fortis_lsp_onchain_sat` grows by `sat`.
    pub fn fund(&self, rt: &Regtest, sat: u64) {
        let before = self.metric("fortis_lsp_onchain_sat");
        rt.fund(&self.onchain_address, sat);
        rt.mine(6);
        poll("fortis-lsp sees its funding", 90, || {
            (self.metric("fortis_lsp_onchain_sat") >= before + sat).then_some(())
        });
    }

    /// Stops the process (SIGTERM); `Some(status)` when it exited by itself
    /// within 15 s.
    pub fn stop(&mut self) -> Option<ExitStatus> {
        self.proc.take().and_then(|mut p| p.stop(15))
    }

    /// Stops and starts it again on the same config and data dir; returns the
    /// node id it printed.
    pub fn restart(&mut self) -> String {
        let status = self.stop();
        assert!(
            status.is_some_and(|s| s.success()),
            "fortis-lsp did not stop cleanly: {status:?}"
        );
        let lines = self.spawn();
        self.metrics_url = format!("http://{}/metrics", lines["metrics"]);
        lines["node_id"].clone()
    }

    pub fn pid(&self) -> Option<u32> {
        self.proc.as_ref().map(|p| p.child.id())
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.0.clone()
    }

    /// Everything the binary wrote to stderr so far.
    pub fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path("stderr.log")).unwrap_or_default()
    }

    /// `GET /metrics` as text.
    pub fn metrics(&self) -> String {
        ureq::get(&self.metrics_url)
            .call()
            .expect("GET /metrics")
            .into_string()
            .unwrap()
    }

    /// One metric's value (panics when absent).
    pub fn metric(&self, name: &str) -> u64 {
        let text = self.metrics();
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {name} in /metrics:\n{text}"))
    }

    /// Where a client `NodeConfig` finds this LSP.
    pub fn lsp_info(&self) -> LspInfo {
        LspInfo {
            node_id: self.node_id.clone(),
            host: self.host.clone(),
            port: self.port,
            token: self.token.clone(),
        }
    }

    /// The limits it serves (from `GET /info`).
    pub fn limits(&self) -> LspLimits {
        let n = |k: &str| self.info[k].as_u64().unwrap_or_else(|| panic!("/info {k}"));
        LspLimits {
            min_payment_msat: n("min_payment_size_msat"),
            max_payment_msat: n("max_payment_size_msat"),
            channel_min_sat: n("channel_min_sat"),
            channel_max_sat: n("channel_max_sat"),
        }
    }
}
