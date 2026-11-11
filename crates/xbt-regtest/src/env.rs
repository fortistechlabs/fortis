//! The environment: Knots, fortis-index, LND (ports from
//! `tools/xbt-regtest/env.sh`, exported by `run.sh`) and the two ldk-node
//! nodes, P (the phone) and S (the LSP). The last two scenarios add the real
//! `fortis-lsp` binary (chain = "xbt", a child process: [`LspProcess`]) and
//! two clients of it: a second phone, P2 (ldk-node), and the app's own node
//! (`fortis_lightning::Node`, chain Xbt) over a VSS ([`crate::vss`]).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::config::{AsyncPaymentsRole, Config, XbtConfig};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::liquidity::LSPS2ServiceConfig;
use ldk_node::{Builder, Event, Node};
use serde_json::Value;

pub type R<T> = Result<T, String>;

/// The regtest gossip floor (BLAKE2b activates at height 20).
pub const GOSSIP_FLOOR: u32 = 20;

/// The environment's settings, from `env.sh`'s variables.
pub struct Cfg {
	pub xr: String,
	pub knots_bin: String,
	pub rpc_port: u16,
	pub index_port: u16,
	pub lnd_rpc_port: u16,
	pub lnd_p2p_port: u16,
	pub s_port: u16,
	/// The repository (fortis-lsp is run from its target/debug).
	pub repo: String,
	pub lsp_port: u16,
	pub lsp_admin_port: u16,
	/// The app node's VSS (vss-server, and its PostgreSQL), started by `app_node_xbt`.
	pub vss_port: u16,
	pub vss_pg_port: u16,
}

static CFG: OnceLock<Cfg> = OnceLock::new();

fn var(name: &str) -> R<String> {
	std::env::var(name).map_err(|_| {
		format!("{name} is not set: run tools/xbt-regtest/run.sh (it exports env.sh's values)")
	})
}

fn port(name: &str) -> R<u16> {
	var(name)?.parse().map_err(|e| format!("{name}: {e}"))
}

/// Reads the environment's settings; call once before anything else.
pub fn init_cfg() -> R<()> {
	let cfg = Cfg {
		xr: var("XR")?,
		knots_bin: var("XR_KNOTS_BIN")?,
		rpc_port: port("XR_RPC_PORT")?,
		index_port: port("XR_INDEX_PORT")?,
		lnd_rpc_port: port("XR_LND_RPC_PORT")?,
		lnd_p2p_port: port("XR_LND_P2P_PORT")?,
		s_port: port("XR_LDK_S_PORT")?,
		repo: var("XR_REPO")?,
		lsp_port: port("XR_LSP_PORT")?,
		lsp_admin_port: port("XR_LSP_ADMIN_PORT")?,
		vss_port: port("XR_VSS_PORT")?,
		vss_pg_port: port("XR_VSS_PG_PORT")?,
	};
	let _ = CFG.set(cfg);
	Ok(())
}

pub fn cfg() -> &'static Cfg {
	CFG.get().expect("init_cfg first")
}

/// Runs `prog`; its trimmed stdout, or an error with its stderr.
pub fn run(prog: &str, args: &[&str]) -> R<String> {
	let o = Command::new(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
	if !o.status.success() {
		return Err(format!(
			"{} {}: {}{}",
			prog.rsplit('/').next().unwrap_or(prog),
			args.join(" "),
			String::from_utf8_lossy(&o.stderr).trim(),
			String::from_utf8_lossy(&o.stdout).trim()
		));
	}
	Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// `bitcoin-cli` against the regtest Knots (wallet `lab`, made by up.sh).
pub fn btc(args: &[&str]) -> R<String> {
	let c = cfg();
	let d = format!("-datadir={}/knots", c.xr);
	let p = format!("-rpcport={}", c.rpc_port);
	let mut a =
		vec![d.as_str(), "-regtest", p.as_str(), "-rpcuser=lab", "-rpcpassword=lab", "-rpcwallet=lab"];
	a.extend_from_slice(args);
	run(&format!("{}/bitcoin-cli", c.knots_bin), &a)
}

pub fn btc_json(args: &[&str]) -> R<Value> {
	let out = btc(args)?;
	serde_json::from_str(&out).map_err(|e| format!("bitcoin-cli {args:?}: {e}: {out}"))
}

fn lncli_args(args: &[&str]) -> (String, Vec<String>) {
	let c = cfg();
	let mut a = vec![
		format!("--lnddir={}/lnd-data", c.xr),
		"--network=regtest".to_string(),
		format!("--rpcserver=127.0.0.1:{}", c.lnd_rpc_port),
	];
	a.extend(args.iter().map(|s| s.to_string()));
	(format!("{}/bin/lncli", c.xr), a)
}

/// `lncli` against the regtest LND; its JSON output (or the text as a string).
pub fn ln(args: &[&str]) -> R<Value> {
	let (prog, a) = lncli_args(args);
	let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
	let out = run(&prog, &a)?;
	Ok(serde_json::from_str(&out).unwrap_or(Value::String(out)))
}

/// `lncli` whose failure is an answer, not an error: (succeeded, stdout + stderr).
pub fn ln_try(args: &[&str]) -> (bool, String) {
	let (prog, a) = lncli_args(args);
	match Command::new(&prog).args(&a).output() {
		Ok(o) => (
			o.status.success(),
			format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
		),
		Err(e) => (false, e.to_string()),
	}
}

pub fn index_height() -> u64 {
	let url = format!("http://127.0.0.1:{}/blocks/tip/height", cfg().index_port);
	run("curl", &["-s", &url]).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

pub fn knots_height() -> R<u64> {
	btc(&["getblockcount"])?.parse().map_err(|e| format!("getblockcount: {e}"))
}

pub fn sleep_ms(ms: u64) {
	std::thread::sleep(Duration::from_millis(ms));
}

/// Polls `f` until it returns `Ok(Some(_))`, for at most `secs`.
pub fn poll<T>(what: &str, secs: u64, mut f: impl FnMut() -> R<Option<T>>) -> R<T> {
	let t = Instant::now();
	loop {
		if let Some(v) = f()? {
			return Ok(v);
		}
		if t.elapsed() > Duration::from_secs(secs) {
			return Err(format!("timed out after {secs}s waiting for {what}"));
		}
		sleep_ms(300);
	}
}

/// Which node a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Who {
	P,
	S,
	/// The second phone, client of fortis-lsp.
	P2,
	/// fortis-lsp (chain = "xbt").
	Lsp,
}

/// One ldk-node node that can be stopped, restarted and rebuilt from its
/// storage directory (the penalty scenario restores an old copy of it).
pub struct Ldk {
	pub name: &'static str,
	pub who: Who,
	pub dir: String,
	pub id: PublicKey,
	config: Box<dyn Fn() -> R<Node>>,
	node: RefCell<Option<Arc<Node>>>,
	/// Events taken off the node, not yet claimed by a `wait_for`.
	events: RefCell<Vec<Event>>,
	/// Every payment received (msat), as `PaymentReceived` reported it.
	pub received_msat: Cell<u64>,
	/// Every payment sent (msat, amount plus routing fee), as `PaymentSuccessful` reported it.
	pub sent_msat: Cell<u64>,
	/// Forwarding fees earned, including LSPS2 skims.
	pub forward_fees_msat: Cell<u64>,
	/// Payment hashes (received) and ids (sent) already counted.
	counted: RefCell<std::collections::HashSet<[u8; 32]>>,
}

impl Ldk {
	fn new(name: &'static str, who: Who, dir: String, config: Box<dyn Fn() -> R<Node>>) -> R<Self> {
		let node = config()?;
		let id = node.node_id();
		Ok(Ldk {
			name,
			who,
			dir,
			id,
			config,
			node: RefCell::new(Some(Arc::new(node))),
			events: RefCell::new(vec![]),
			received_msat: Cell::new(0),
			sent_msat: Cell::new(0),
			forward_fees_msat: Cell::new(0),
			counted: RefCell::new(Default::default()),
		})
	}

	pub fn node(&self) -> Arc<Node> {
		Arc::clone(self.node.borrow().as_ref().expect("node built"))
	}

	pub fn running(&self) -> bool {
		self.node.borrow().as_ref().map(|n| n.status().is_running).unwrap_or(false)
	}

	pub fn start(&self) -> R<()> {
		self.node().start().map_err(|e| format!("{}: start: {e}", self.name))?;
		println!("  {} started", self.name);
		Ok(())
	}

	pub fn stop(&self) -> R<()> {
		self.pump();
		self.node().stop().map_err(|e| format!("{}: stop: {e}", self.name))?;
		println!("  {} stopped", self.name);
		Ok(())
	}

	/// Drops the (stopped) node and builds it again from its directory.
	pub fn rebuild(&self) -> R<()> {
		if self.running() {
			return Err(format!("{}: rebuild while running", self.name));
		}
		self.node.borrow_mut().take();
		let node = (self.config)()?;
		if node.node_id() != self.id {
			return Err(format!("{}: rebuilt with another node id", self.name));
		}
		*self.node.borrow_mut() = Some(Arc::new(node));
		Ok(())
	}

	/// Replaces the (stopped) node's directory with `from` and builds it from there.
	pub fn restore(&self, from: &str) -> R<()> {
		if self.running() {
			return Err(format!("{}: restore while running", self.name));
		}
		self.node.borrow_mut().take();
		std::fs::remove_dir_all(&self.dir).map_err(|e| format!("rm {}: {e}", self.dir))?;
		std::fs::rename(from, &self.dir).map_err(|e| format!("mv {from}: {e}"))?;
		println!("  {} restored from {from}", self.name);
		self.rebuild()
	}

	/// Takes every ready event off the node into the buffer, accounting payments.
	pub fn pump(&self) {
		let node = self.node();
		while let Some(e) = node.next_event() {
			println!("    {} event: {}", self.name, short_event(&e));
			match &e {
				// A node restarted from an old copy (penalty) emits events again that were
				// handled before the copy; each payment counts once.
				Event::PaymentReceived { payment_hash, amount_msat, .. } => {
					if self.counted.borrow_mut().insert(payment_hash.0) {
						self.received_msat.set(self.received_msat.get() + amount_msat)
					}
				},
				Event::PaymentSuccessful { payment_id: Some(id), fee_paid_msat, .. }
					if self.counted.borrow_mut().insert(id.0) =>
				{
					let amount = node
						.payment(id)
						.and_then(|p| p.amount_msat)
						.unwrap_or_else(|| {
							println!("    {}: payment {id} has no amount", self.name);
							0
						});
					self.sent_msat.set(self.sent_msat.get() + amount + fee_paid_msat.unwrap_or(0));
				},
				Event::PaymentForwarded { total_fee_earned_msat, .. } => self
					.forward_fees_msat
					.set(self.forward_fees_msat.get() + total_fee_earned_msat.unwrap_or(0)),
				_ => {},
			}
			if let Err(err) = node.event_handled() {
				println!("    {}: event_handled: {err}", self.name);
			}
			self.events.borrow_mut().push(e);
		}
	}

	/// Waits for an event `f` accepts (taken from the buffer, in order).
	pub fn wait_for(&self, what: &str, secs: u64, f: impl Fn(&Event) -> bool) -> R<Event> {
		let t = Instant::now();
		loop {
			if self.running() {
				self.pump();
			}
			{
				let mut ev = self.events.borrow_mut();
				if let Some(i) = ev.iter().position(|e| f(e)) {
					return Ok(ev.remove(i));
				}
			}
			if t.elapsed() > Duration::from_secs(secs) {
				return Err(format!("{}: timed out after {secs}s waiting for {what}", self.name));
			}
			sleep_ms(100);
		}
	}

	/// Waits for the outcome of an outgoing payment: Ok(fee) or the failure.
	pub fn wait_payment(&self, id: ldk_node::lightning::ln::channelmanager::PaymentId, secs: u64) -> R<u64> {
		let e = self.wait_for(&format!("payment {id} to settle"), secs, |e| match e {
			Event::PaymentSuccessful { payment_id, .. } | Event::PaymentFailed { payment_id, .. } => {
				*payment_id == Some(id)
			},
			_ => false,
		})?;
		match e {
			Event::PaymentSuccessful { fee_paid_msat, .. } => Ok(fee_paid_msat.unwrap_or(0)),
			Event::PaymentFailed { reason, .. } => {
				Err(format!("{}: payment {id} failed: {reason:?}", self.name))
			},
			_ => unreachable!(),
		}
	}

	/// Waits for a payment received with this hash; its amount (msat).
	pub fn wait_received(&self, hash: ldk_node::lightning::types::payment::PaymentHash, secs: u64) -> R<u64> {
		match self.wait_for(&format!("payment {hash} received"), secs, |e| {
			matches!(e, Event::PaymentReceived { payment_hash, .. } if *payment_hash == hash)
		})? {
			Event::PaymentReceived { amount_msat, .. } => Ok(amount_msat),
			_ => unreachable!(),
		}
	}

	/// Waits for a payment received of exactly `amount_msat` (offers: no hash up front).
	pub fn wait_received_amount(&self, amount_msat: u64, secs: u64) -> R<()> {
		self.wait_for(&format!("a payment of {amount_msat} msat received"), secs, |e| {
			matches!(e, Event::PaymentReceived { amount_msat: a, .. } if *a == amount_msat)
		})?;
		Ok(())
	}

	pub fn onchain_sats(&self) -> u64 {
		self.node().list_balances().total_onchain_balance_sats
	}
}

fn short_event(e: &Event) -> String {
	match e {
		Event::PaymentForwarded { total_fee_earned_msat, skimmed_fee_msat, outbound_amount_forwarded_msat, .. } => format!(
			"PaymentForwarded {{ fee {total_fee_earned_msat:?} msat, skimmed {skimmed_fee_msat:?}, forwarded {outbound_amount_forwarded_msat:?} }}"
		),
		other => format!("{other:?}"),
	}
}

/// A confirmed transaction the suite checked, and who paid its fee.
pub struct TxRecord {
	pub label: String,
	pub txid: String,
	pub fee_sat: u64,
	pub sig_bytes: Vec<Vec<u8>>,
	pub payer: Option<Who>,
}

pub struct Env {
	pub p: Ldk,
	pub s: Ldk,
	pub lnd_id: PublicKey,
	pub lnd_addr: SocketAddress,
	pub s_addr: SocketAddress,
	/// On-chain sats sent to P and S at setup.
	pub funded: HashMap<Who, u64>,
	pub txs: RefCell<Vec<TxRecord>>,
	/// (txid, vout) -> spending txid, for every block scanned so far.
	spends: RefCell<HashMap<(String, u32), String>>,
	scanned: Cell<u64>,
	/// Values the scenarios hand on to later ones.
	pub jit_channel: RefCell<Option<ldk_node::UserChannelId>>,
	pub lnd_s_channel: RefCell<Option<ldk_node::UserChannelId>>,
	/// What P paid S in the penalty channel (P gets it back, S loses it).
	pub penalty_msat: Cell<u64>,
	/// Outputs of a force close that `who` must sweep before the suite ends: (who, label,
	/// txid, vout); `sweeps_settle` checks each has a confirmed spender.
	pub must_sweep: RefCell<Vec<(Who, String, String, u32)>>,
	/// fortis-lsp and its client P2, once `lsp_xbt_jit` has started them.
	pub lsp: RefCell<Option<LspProcess>>,
	pub p2: OnceCell<Ldk>,
	/// The app node's VSS, once `app_node_xbt` has started it.
	pub vss: RefCell<Option<crate::vss::VssProcess>>,
}

pub fn lsps2_service_config() -> LSPS2ServiceConfig {
	LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm: JIT_FEE_PPM,
		// The JIT channel is twice the payment, so P has inbound room.
		channel_over_provisioning_ppm: 1_000_000,
		min_channel_opening_fee_msat: JIT_MIN_FEE_MSAT,
		min_channel_lifetime: 100,
		max_client_to_self_delay: 1024,
		min_payment_size_msat: 1_000_000,
		max_payment_size_msat: 1_000_000_000,
		client_trusts_lsp: true,
	}
}

pub const JIT_FEE_PPM: u32 = 10_000;
pub const JIT_MIN_FEE_MSAT: u64 = 1_000_000;

fn base_config(dir: &str, splice_peers: Vec<PublicKey>) -> Config {
	let mut config = Config::default();
	config.storage_dir_path = dir.to_string();
	config.network = Network::Regtest;
	config.xbt = Some(XbtConfig { gossip_floor_height: GOSSIP_FLOOR, splice_peers, splice_all_peers: false });
	config
}

pub fn esplora_url() -> String {
	format!("http://127.0.0.1:{}", cfg().index_port)
}

/// Node S: the LSP (LSPS2 service, async payments server).
fn build_s(dir: &str, p: Option<PublicKey>) -> R<Node> {
	let mut config = base_config(dir, p.into_iter().collect());
	config.listening_addresses =
		Some(vec![SocketAddress::from_str(&format!("127.0.0.1:{}", cfg().s_port)).unwrap()]);
	let mut b = Builder::from_config(config);
	b.set_chain_source_esplora(esplora_url(), None);
	b.set_node_alias("xr-lsp".to_string()).map_err(|e| format!("S alias: {e}"))?;
	b.set_liquidity_provider_lsps2(lsps2_service_config());
	b.set_async_payments_role(Some(AsyncPaymentsRole::Server)).map_err(|e| format!("S role: {e}"))?;
	b.build().map_err(|e| format!("build S: {e}"))
}

/// Node P: the phone (LSPS2 client of S, async payments client).
fn build_p(dir: &str, s: Option<(PublicKey, SocketAddress)>) -> R<Node> {
	let config = base_config(dir, s.iter().map(|(id, _)| *id).collect());
	let mut b = Builder::from_config(config);
	b.set_chain_source_esplora(esplora_url(), None);
	if let Some((id, addr)) = s {
		b.set_liquidity_source_lsps2(id, addr, None);
	}
	b.set_async_payments_role(Some(AsyncPaymentsRole::Client)).map_err(|e| format!("P role: {e}"))?;
	b.build().map_err(|e| format!("build P: {e}"))
}

/// Node P2: a second phone, LSPS2 client of fortis-lsp (`lsp`), which may splice with it.
fn build_p2(dir: &str, lsp: PublicKey, lsp_addr: SocketAddress) -> R<Node> {
	let config = base_config(dir, vec![lsp]);
	let mut b = Builder::from_config(config);
	b.set_chain_source_esplora(esplora_url(), None);
	b.set_liquidity_source_lsps2(lsp, lsp_addr, None);
	b.build().map_err(|e| format!("build P2: {e}"))
}

impl Env {
	/// Builds and starts P2 (directory `$XR/ldk-p2`), the LSPS2 client of fortis-lsp.
	pub fn start_p2(&self, lsp: PublicKey, lsp_addr: SocketAddress) -> R<&Ldk> {
		if self.p2.get().is_some() {
			return Err("P2 already started".into());
		}
		let dir = format!("{}/ldk-p2", cfg().xr);
		let d = dir.clone();
		let p2 = Ldk::new("P2", Who::P2, dir, Box::new(move || build_p2(&d, lsp, lsp_addr.clone())))?;
		p2.start()?;
		let _ = self.p2.set(p2);
		Ok(self.p2.get().expect("set"))
	}

	/// Builds and starts P and S (each knows the other's id: probed by a
	/// first build), funds them on chain.
	pub fn setup() -> R<Env> {
		let xr = cfg().xr.clone();
		let p_dir = format!("{xr}/ldk-p");
		let s_dir = format!("{xr}/ldk-s");
		let lnd_id = ln(&["getinfo"])?["identity_pubkey"]
			.as_str()
			.ok_or("lncli getinfo: no identity_pubkey")?
			.to_string();
		let lnd_id = PublicKey::from_str(&lnd_id).map_err(|e| e.to_string())?;
		let lnd_addr = SocketAddress::from_str(&format!("127.0.0.1:{}", cfg().lnd_p2p_port)).unwrap();
		let s_addr = SocketAddress::from_str(&format!("127.0.0.1:{}", cfg().s_port)).unwrap();

		// The ids come from the seeds the first build writes into each directory.
		let p_id = build_p(&p_dir, None)?.node_id();
		let s_id = build_s(&s_dir, None)?.node_id();

		let (pd, sd, sa) = (p_dir.clone(), s_dir.clone(), s_addr.clone());
		let p = Ldk::new("P", Who::P, p_dir, Box::new(move || build_p(&pd, Some((s_id, sa.clone())))))?;
		let s = Ldk::new("S", Who::S, s_dir, Box::new(move || build_s(&sd, Some(p_id))))?;
		p.start()?;
		s.start()?;
		println!("  P {} (phone), S {} (LSP), LND {}", p.id, s.id, lnd_id);

		let mut funded = HashMap::new();
		for (who, node, sats) in [(Who::P, &p, 2_000_000u64), (Who::S, &s, 2_000_000)] {
			let a = node.node().onchain_payment().new_address().map_err(|e| e.to_string())?;
			btc(&["sendtoaddress", &a.to_string(), &btc_amount(sats)])?;
			funded.insert(who, sats);
		}
		let la = ln(&["newaddress", "p2wkh"])?["address"].as_str().ok_or("lnd address")?.to_string();
		btc(&["sendtoaddress", &la, "2"])?;

		let env = Env {
			p,
			s,
			lnd_id,
			lnd_addr,
			s_addr,
			funded,
			txs: RefCell::new(vec![]),
			spends: RefCell::new(HashMap::new()),
			scanned: Cell::new(knots_height()?),
			jit_channel: RefCell::new(None),
			lnd_s_channel: RefCell::new(None),
			penalty_msat: Cell::new(0),
			must_sweep: RefCell::new(vec![]),
			lsp: RefCell::new(None),
			p2: OnceCell::new(),
			vss: RefCell::new(None),
		};
		env.mine(6)?;
		for n in [&env.p, &env.s] {
			let want = env.funded[&n.who];
			poll(&format!("{} funds", n.name), 60, || {
				n.node().sync_wallets().map_err(|e| e.to_string())?;
				Ok((n.onchain_sats() == want).then_some(()))
			})?;
		}
		Ok(env)
	}

	pub fn nodes(&self) -> [&Ldk; 2] {
		[&self.p, &self.s]
	}

	/// Mines `n` blocks; waits for the index and LND to reach them; scans them
	/// for spends; syncs the running nodes.
	pub fn mine(&self, n: u32) -> R<()> {
		let a = btc(&["getnewaddress"])?;
		btc(&["generatetoaddress", &n.to_string(), &a])?;
		let want = knots_height()?;
		poll("the index to reach the tip", 60, || Ok((index_height() >= want).then_some(())))?;
		poll("LND to reach the tip", 60, || {
			Ok((ln(&["getinfo"])?["block_height"].as_u64().unwrap_or(0) >= want).then_some(()))
		})?;
		self.scan_blocks()?;
		for node in self.nodes().into_iter().chain(self.p2.get()) {
			if node.running() {
				node.node().sync_wallets().map_err(|e| format!("{}: sync_wallets: {e}", node.name))?;
				node.pump();
			}
		}
		Ok(())
	}

	fn scan_blocks(&self) -> R<()> {
		let tip = knots_height()?;
		for h in self.scanned.get() + 1..=tip {
			let hash = btc(&["getblockhash", &h.to_string()])?;
			let block = btc_json(&["getblock", &hash, "2"])?;
			for tx in block["tx"].as_array().into_iter().flatten() {
				record_spends(&mut self.spends.borrow_mut(), tx);
			}
			self.scanned.set(h);
		}
		Ok(())
	}

	/// The transaction spending `txid:vout`, confirmed or in the mempool.
	pub fn spender(&self, txid: &str, vout: u32) -> R<Option<String>> {
		if let Some(s) = self.spends.borrow().get(&(txid.to_string(), vout)) {
			return Ok(Some(s.clone()));
		}
		let mempool: Vec<String> = serde_json::from_value(btc_json(&["getrawmempool"])?)
			.map_err(|e| e.to_string())?;
		let mut found = HashMap::new();
		for t in mempool {
			if let Ok(tx) = btc_json(&["getrawtransaction", &t, "true"]) {
				record_spends(&mut found, &tx);
			}
		}
		Ok(found.remove(&(txid.to_string(), vout)))
	}

	/// Waits (mining `step` blocks at a time, up to `rounds`) for `txid:vout`'s spender.
	pub fn wait_spender(&self, what: &str, txid: &str, vout: u32, step: u32, rounds: u32) -> R<String> {
		for round in 0..=rounds {
			// Give the broadcaster a moment after each sync.
			for _ in 0..10 {
				if let Some(s) = self.spender(txid, vout)? {
					show_sighash_types(&format!("{what} (after {} blocks):", round * step), &s)?;
					return Ok(s);
				}
				sleep_ms(300);
			}
			if round < rounds {
				self.mine(step)?;
			}
		}
		Err(format!("{what}: nothing spent {txid}:{vout} after {} blocks", rounds * step))
	}

	/// Records a checked transaction; `payer` is charged its fee.
	pub fn record(&self, label: &str, info: &crate::check::TxInfo, payer: Option<Who>) {
		self.txs.borrow_mut().push(TxRecord {
			label: label.to_string(),
			txid: info.txid.clone(),
			fee_sat: info.fee_sat,
			sig_bytes: info.sig_bytes.clone(),
			payer,
		});
	}

	pub fn fees_of(&self, who: Who) -> u64 {
		self.txs.borrow().iter().filter(|t| t.payer == Some(who)).map(|t| t.fee_sat).sum()
	}

	pub fn recorded(&self, txid: &str) -> bool {
		self.txs.borrow().iter().any(|t| t.txid == txid)
	}

	pub fn lnd_active_channels(&self) -> R<usize> {
		Ok(ln(&["listchannels", "--active_only"])?["channels"].as_array().map(|a| a.len()).unwrap_or(0))
	}

	/// Waits until LND has `n` active channels.
	pub fn wait_lnd_active(&self, n: usize) -> R<()> {
		poll(&format!("LND to have {n} active channels"), 90, || {
			Ok((self.lnd_active_channels()? >= n).then_some(()))
		})
	}
}

fn record_spends(map: &mut HashMap<(String, u32), String>, tx: &Value) {
	let Some(txid) = tx["txid"].as_str() else { return };
	for vin in tx["vin"].as_array().into_iter().flatten() {
		if let (Some(prev), Some(vout)) = (vin["txid"].as_str(), vin["vout"].as_u64()) {
			map.insert((prev.to_string(), vout as u32), txid.to_string());
		}
	}
}

pub fn btc_amount(sats: u64) -> String {
	format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000)
}

/// Pays with `send`, retrying while a fresh channel settles (spike lesson:
/// RouteNotFound / insufficient balance for a few seconds after it opens).
pub fn send_retry(
	node: &Ldk, what: &str, secs: u64,
	send: impl Fn(&Node) -> Result<ldk_node::lightning::ln::channelmanager::PaymentId, ldk_node::NodeError>,
) -> R<u64> {
	let t = Instant::now();
	loop {
		let res = match send(&node.node()) {
			Ok(id) => node.wait_payment(id, 60),
			Err(e) => Err(format!("send: {e:?}")),
		};
		match res {
			Ok(fee) => {
				println!("  {what}: paid (routing fee {fee} msat) after {:?}", t.elapsed());
				return Ok(fee);
			},
			Err(e) if t.elapsed() < Duration::from_secs(secs) => {
				println!("  {what}: {e}, retrying");
				sleep_ms(3000);
			},
			Err(e) => return Err(format!("{what}: {e}")),
		}
	}
}

/// LND pays a BOLT 11 invoice, retrying while routes settle.
pub fn lnd_pay_retry(what: &str, invoice: &Bolt11Invoice, secs: u64) -> R<()> {
	let t = Instant::now();
	let inv = invoice.to_string();
	loop {
		let (_, out) = ln_try(&["payinvoice", "--force", "--json", "--timeout", "60s", &inv]);
		if out.contains("\"SUCCEEDED\"") {
			println!("  {what}: LND paid after {:?}", t.elapsed());
			return Ok(());
		}
		let why: String = out
			.lines()
			.filter(|l| l.contains("failure_reason") || l.contains("rpc error") || l.contains("[lncli]"))
			.collect::<Vec<_>>()
			.join(" ");
		if t.elapsed() > Duration::from_secs(secs) {
			return Err(format!("{what}: LND could not pay: {why}"));
		}
		println!("  {what}: LND try failed ({}), retrying", why.trim());
		sleep_ms(5000);
	}
}

/// Prints the hash type byte of every DER signature in `txid`'s witnesses.
pub fn show_sighash_types(label: &str, txid: &str) -> R<()> {
	let info = crate::check::tx(txid)?;
	println!("  {label} {txid}: signature hash types {}", crate::check::fmt_sigs(&info.sig_bytes));
	Ok(())
}

/// The real `fortis-lsp` binary (target/debug), `chain = "xbt"`, as a child
/// process: data in `$XR/lsp`, stdout in `$XR/logs/lsp.out`, stderr in
/// `$XR/logs/lsp.log`. Its PID goes in `$XR/pids` (down.sh stops it if the
/// suite does not); dropping it stops it (SIGTERM, then KILL after 30 s).
pub struct LspProcess {
	child: Child,
	pub id: PublicKey,
	pub addr: SocketAddress,
	/// A fresh address of its on-chain wallet (its startup line).
	pub onchain_address: String,
	pub data_dir: String,
}

/// The config the suite runs fortis-lsp with (regtest; JIT fee 1 %, min 1,000 sat, as S).
pub fn lsp_config_toml(data_dir: &str) -> String {
	let c = cfg();
	format!(
		r#"chain = "xbt"
network = "regtest"
esplora_url = "{esplora}"
xbt_gossip_floor_height = {GOSSIP_FLOOR}
listen = "127.0.0.1:{lsp_port}"
metrics_listen = "127.0.0.1:{admin_port}"
data_dir = "{data_dir}"
alias = "xr-fortis-lsp"
open_fee_ppm = {JIT_FEE_PPM}
open_fee_min_sat = {open_fee_min_sat}
min_payment_size_msat = 10000000
max_payment_size_msat = 500000000
channel_size_factor = 2.0
channel_min_sat = 100000
channel_max_sat = 500000
low_liquidity_sat = 0
custody_max_sat = 0
"#,
		esplora = esplora_url(),
		lsp_port = c.lsp_port,
		admin_port = c.lsp_admin_port,
		open_fee_min_sat = JIT_MIN_FEE_MSAT / 1000,
	)
}

fn port_free(port: u16) -> bool {
	std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The last `n` lines of a file ("" if it cannot be read).
pub fn tail(path: &str, n: usize) -> String {
	let text = std::fs::read_to_string(path).unwrap_or_default();
	let lines: Vec<&str> = text.lines().collect();
	lines[lines.len().saturating_sub(n)..].join("\n")
}

impl LspProcess {
	/// Writes the config, starts fortis-lsp and waits for its `ready` line.
	pub fn start() -> R<LspProcess> {
		use std::os::unix::fs::DirBuilderExt;
		let c = cfg();
		for p in [c.lsp_port, c.lsp_admin_port] {
			if !port_free(p) {
				return Err(format!("port {p} is in use by a process the suite did not start"));
			}
		}
		let bin = format!("{}/target/debug/fortis-lsp", c.repo);
		if !std::path::Path::new(&bin).exists() {
			return Err(format!("{bin} missing: cargo build -p fortis-lsp"));
		}
		let data_dir = format!("{}/lsp", c.xr);
		// fortis-lsp refuses a data_dir group or others can read.
		std::fs::DirBuilder::new()
			.recursive(true)
			.mode(0o700)
			.create(&data_dir)
			.map_err(|e| format!("{data_dir}: {e}"))?;
		let conf = format!("{data_dir}/lsp.toml");
		std::fs::write(&conf, lsp_config_toml(&data_dir)).map_err(|e| format!("{conf}: {e}"))?;
		let logs = format!("{}/logs", c.xr);
		let out_path = format!("{logs}/lsp.out");
		let open = |p: &str| std::fs::File::create(p).map_err(|e| format!("{p}: {e}"));
		let mut child = Command::new(&bin)
			.args(["--config", &conf])
			.stdin(Stdio::null())
			.stdout(open(&out_path)?)
			.stderr(open(&format!("{logs}/lsp.log"))?)
			.spawn()
			.map_err(|e| format!("{bin}: {e}"))?;
		let pid = child.id();
		// down.sh stops it from here if the suite cannot (its command line names $XR).
		let pids = format!("{}/pids", c.xr);
		let recorded = std::fs::OpenOptions::new()
			.append(true)
			.create(true)
			.open(&pids)
			.and_then(|mut f| std::io::Write::write_all(&mut f, format!("lsp {pid}\n").as_bytes()))
			.map_err(|e| format!("{pids}: {e}"));
		let ready = recorded.and_then(|()| poll("fortis-lsp to print ready", 120, || {
			if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
				return Err(format!("fortis-lsp exited ({status}) before ready"));
			}
			let out = std::fs::read_to_string(&out_path).unwrap_or_default();
			Ok(out.lines().any(|l| l == "ready").then_some(out))
		}));
		let started = ready.and_then(|out| {
			let field = |k: &str| {
				out.lines()
					.find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix(' ')).map(str::to_string))
					.ok_or_else(|| format!("fortis-lsp printed no {k} line"))
			};
			let id = PublicKey::from_str(&field("node_id")?).map_err(|e| format!("node_id: {e}"))?;
			Ok((id, field("onchain_address")?))
		});
		let (id, onchain_address) = match started {
			Ok(v) => v,
			Err(e) => {
				let _ = child.kill();
				let _ = child.wait();
				return Err(format!("{e}\n--- {logs}/lsp.log (last 30) ---\n{}", tail(&format!("{logs}/lsp.log"), 30)));
			},
		};
		let addr = SocketAddress::from_str(&format!("127.0.0.1:{}", c.lsp_port)).unwrap();
		println!("  fortis-lsp (pid {pid}) ready: node {id} at {addr}");
		Ok(LspProcess { child, id, addr, onchain_address, data_dir })
	}

	/// A gauge of its `GET /metrics` (e.g. `fortis_lsp_onchain_sat`).
	pub fn metric(&self, name: &str) -> R<u64> {
		let url = format!("http://127.0.0.1:{}/metrics", cfg().lsp_admin_port);
		let text = run("curl", &["-sf", &url])?;
		text.lines()
			.find_map(|l| l.strip_prefix(name).and_then(|v| v.strip_prefix(' ')).map(str::trim))
			.ok_or_else(|| format!("fortis-lsp metrics: no {name}"))?
			.parse()
			.map_err(|e| format!("{name}: {e}"))
	}

	/// Its last `n` log lines: ldk-node's log, then its stderr.
	pub fn log_tail(&self, n: usize) -> String {
		let xr = &cfg().xr;
		format!(
			"--- {d}/ldk_node.log (last {n}) ---\n{}\n--- {xr}/logs/lsp.log (last {n}) ---\n{}",
			tail(&format!("{d}/ldk_node.log", d = self.data_dir), n),
			tail(&format!("{xr}/logs/lsp.log"), n),
			d = self.data_dir,
		)
	}

	/// SIGTERM (fortis-lsp stops its node cleanly), KILL after 30 s.
	pub fn stop(&mut self) {
		if let Ok(Some(_)) = self.child.try_wait() {
			return;
		}
		let pid = self.child.id().to_string();
		let _ = Command::new("kill").args(["-TERM", &pid]).status();
		let t = Instant::now();
		while t.elapsed() < Duration::from_secs(30) {
			if let Ok(Some(status)) = self.child.try_wait() {
				println!("  fortis-lsp stopped ({status})");
				return;
			}
			sleep_ms(200);
		}
		println!("  fortis-lsp ignored SIGTERM for 30 s: KILL");
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

impl Drop for LspProcess {
	fn drop(&mut self) {
		self.stop();
	}
}
