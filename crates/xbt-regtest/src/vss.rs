//! LDK's `vss-server` for the app's node (`app_node_xbt`): the launcher of
//! `crates/fortis-lightning/tests/common/mod.rs` (`Vss`), lifted here with
//! the regtest's fixed ports and directories instead of free ports and a
//! temp dir. A private PostgreSQL cluster in `$XR/vss/pg` (port
//! `XR_VSS_PG_PORT`, socket in `$XR/vss`), then the no-op-auth vss-server
//! that `run.sh` builds and copies to `$XR/bin/vss-server` (port
//! `XR_VSS_PORT`; log `$XR/vss/vss.log`). Both PIDs go in `$XR/pids`
//! (down.sh stops them if the suite does not); dropping it stops both. Never
//! the production fortis-vss or its PostgreSQL.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::env::{cfg, poll, run, sleep_ms, tail, R};

pub struct VssProcess {
	server: Option<Child>,
	pg_data: String,
	pg_up: bool,
	/// The client base URL (`…/vss`).
	pub url: String,
}

fn port_free(port: u16) -> bool {
	std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn record_pid(name: &str, pid: u32) -> R<()> {
	let pids = format!("{}/pids", cfg().xr);
	std::fs::OpenOptions::new()
		.append(true)
		.create(true)
		.open(&pids)
		.and_then(|mut f| std::io::Write::write_all(&mut f, format!("{name} {pid}\n").as_bytes()))
		.map_err(|e| format!("{pids}: {e}"))
}

impl VssProcess {
	/// Starts PostgreSQL (a new cluster) and vss-server; waits until it answers.
	pub fn start() -> R<VssProcess> {
		let c = cfg();
		for p in [c.vss_port, c.vss_pg_port] {
			if !port_free(p) {
				return Err(format!("port {p} is in use by a process the suite did not start"));
			}
		}
		let bin = format!("{}/bin/vss-server", c.xr);
		if !std::path::Path::new(&bin).exists() {
			return Err(format!("{bin} missing: run tools/xbt-regtest/run.sh (it builds and copies it)"));
		}
		let dir = format!("{}/vss", c.xr);
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
		let pg_data = format!("{dir}/pg");
		run("initdb", &["-D", &pg_data, "-U", "postgres", "--auth=trust", "-E", "UTF8"])?;
		// From here on, drop stops whatever is running.
		let mut vss =
			VssProcess { server: None, pg_data: pg_data.clone(), pg_up: true, url: format!("http://127.0.0.1:{}/vss", c.vss_port) };
		let opts = format!("-p {} -k {dir} -c listen_addresses=127.0.0.1", c.vss_pg_port);
		run("pg_ctl", &["-D", &pg_data, "-o", &opts, "-l", &format!("{dir}/pg.log"), "-w", "start"])?;
		let pg_pid = std::fs::read_to_string(format!("{pg_data}/postmaster.pid"))
			.ok()
			.and_then(|s| s.lines().next().and_then(|l| l.trim().parse::<u32>().ok()))
			.ok_or("postgres: no postmaster.pid")?;
		record_pid("vss-pg", pg_pid)?;

		let log = format!("{dir}/vss.log");
		let child = Command::new(&bin)
			.current_dir(&dir)
			.env("VSS_BIND_ADDRESS", format!("127.0.0.1:{}", c.vss_port))
			.env("VSS_PSQL_USERNAME", "postgres")
			.env("VSS_PSQL_PASSWORD", "unused")
			.env("VSS_PSQL_ADDRESS", format!("127.0.0.1:{}", c.vss_pg_port))
			.env("VSS_PSQL_DEFAULT_DB", "postgres")
			.env("VSS_PSQL_VSS_DB", "vss")
			.env("VSS_LOG_FILE", &log)
			.env("VSS_LOG_LEVEL", "debug")
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.map_err(|e| format!("{bin}: {e}"))?;
		let pid = child.id();
		vss.server = Some(child);
		record_pid("vss", pid)?;
		let metrics = format!("http://127.0.0.1:{}/metrics", c.vss_port);
		let server = vss.server.as_mut().expect("set");
		poll("vss-server to answer", 30, || {
			if let Some(status) = server.try_wait().map_err(|e| e.to_string())? {
				return Err(format!("vss-server exited ({status}) during startup:\n{}", tail(&log, 20)));
			}
			Ok(run("curl", &["-sf", "-o", "/dev/null", &metrics]).is_ok().then_some(()))
		})?;
		println!("  vss-server (pid {pid}, PostgreSQL pid {pg_pid}) at {}", vss.url);
		Ok(vss)
	}

	/// Stops vss-server (SIGTERM, KILL after 10 s), then PostgreSQL (fast shutdown).
	pub fn stop(&mut self) {
		if let Some(mut child) = self.server.take() {
			if let Ok(None) = child.try_wait() {
				let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
				let t = Instant::now();
				while t.elapsed() < Duration::from_secs(10) {
					if let Ok(Some(_)) = child.try_wait() {
						break;
					}
					sleep_ms(100);
				}
				let _ = child.kill();
				let _ = child.wait();
				println!("  vss-server stopped");
			}
		}
		if self.pg_up {
			self.pg_up = false;
			match run("pg_ctl", &["-D", &self.pg_data, "-m", "fast", "-w", "stop"]) {
				Ok(_) => println!("  vss PostgreSQL stopped"),
				Err(e) => println!("  vss PostgreSQL: {e}"),
			}
		}
	}
}

impl Drop for VssProcess {
	fn drop(&mut self) {
		self.stop();
	}
}
