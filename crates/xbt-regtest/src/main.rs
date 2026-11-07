//! The XBT release gate: the patched LDK (ldk-node) on a Knots regtest with
//! BLAKE2b active (height 20), syncing through fortis-index's Esplora, against
//! the LND BLAKE2b fork. Every transaction the scenarios make must be
//! confirmed by Knots, with the signature hash type bytes expected of it.
//!
//! Run it with `tools/xbt-regtest/run.sh` (starts and stops the environment).
//! `xbt-regtest all` runs every scenario in order; they build on each other.

mod check;
mod env;
mod scenarios;

use std::process::ExitCode;

use env::{Env, R};
use scenarios::{Outcome, ALL};

fn main() -> ExitCode {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.first().map(String::as_str) != Some("all") {
		eprintln!("usage: xbt-regtest all   (run it through tools/xbt-regtest/run.sh)");
		eprintln!("scenarios, in order: {}", ALL.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "));
		return ExitCode::from(2);
	}
	if let Err(e) = env::init_cfg() {
		eprintln!("{e}");
		return ExitCode::from(2);
	}

	let mut results: Vec<(String, String)> = vec![];
	let env = match setup() {
		Ok(env) => {
			results.push(("setup".into(), "PASS".into()));
			Some(env)
		},
		Err(e) => {
			results.push(("setup".into(), format!("FAIL: {e}")));
			None
		},
	};

	let mut failed = env.is_none();
	if let Some(env) = &env {
		for (name, scenario) in ALL {
			if failed {
				results.push((name.to_string(), "NOT RUN".into()));
				continue;
			}
			println!("\n== {name}");
			let r = match scenario(env) {
				Ok(Outcome::Pass) => "PASS".to_string(),
				Err(e) => {
					failed = true;
					format!("FAIL: {e}")
				},
			};
			println!("== {name}: {r}");
			results.push((name.to_string(), r));
		}
		if failed {
			if let Some(lsp) = env.lsp.borrow().as_ref() {
				println!("\nfortis-lsp's last log lines:\n{}", lsp.log_tail(30));
			}
		}
		for n in env.nodes().into_iter().chain(env.p2.get()) {
			if n.running() {
				let _ = n.stop();
			}
		}
		// Stops fortis-lsp (if a failure left it running).
		drop(env.lsp.borrow_mut().take());
	}

	println!("\n================ XBT regtest summary ================");
	for (name, r) in &results {
		println!("{name:<26}{r}");
	}
	if let Some(env) = &env {
		println!("\ntransactions confirmed by Knots (signature hash types per input):");
		for t in env.txs.borrow().iter() {
			println!("  {:<58}{}  fee {:>5} sat  {}", t.label, t.txid, t.fee_sat, check::fmt_sigs(&t.sig_bytes));
		}
	}
	let passed = results.iter().filter(|(_, r)| r == "PASS").count();
	println!("RESULT: {} ({passed} PASS of {} steps)", if failed { "FAIL" } else { "PASS" }, results.len());
	if failed {
		ExitCode::FAILURE
	} else {
		ExitCode::SUCCESS
	}
}

fn setup() -> R<Env> {
	println!("== setup");
	let env = Env::setup()?;
	scenarios::setup(&env)?;
	Ok(env)
}
