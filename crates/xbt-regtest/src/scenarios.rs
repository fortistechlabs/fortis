//! The scenarios, run in order; each leaves the state the next one needs.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::offers::offer::Offer;
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::lightning_types::payment::PaymentHash;
use ldk_node::payment::{PaymentDirection, PaymentKind};
use ldk_node::{ChannelDetails, Event, LightningBalance, PendingSweepBalance, UserChannelId};

use crate::check::{self, confirmed_all, confirmed_with, output_with_value, TxInfo};
use crate::env::{
	btc, ln, ln_try, lnd_pay_retry, poll, send_retry, sleep_ms, Env, Ldk, Who, JIT_FEE_PPM,
	JIT_MIN_FEE_MSAT, R,
};

const U: &[u8] = &[0x21];
const UU: &[u8] = &[0x21, 0x21];

pub enum Outcome {
	Pass,
}

pub type Scenario = fn(&Env) -> R<Outcome>;

pub const ALL: &[(&str, Scenario)] = &[
	("jit_open", jit_open),
	("pay_both_ways", pay_both_ways),
	("splice_in", splice_in),
	("splice_out", splice_out),
	("bolt12_both_ways", bolt12_both_ways),
	("offer_while_stopped", offer_while_stopped),
	("coop_close", coop_close),
	("force_close_htlcs", force_close_htlcs),
	("penalty", penalty),
	("sweeps_settle", sweeps_settle),
];

// ------------------------------------------------------------------ helpers

fn desc(s: &str) -> Bolt11InvoiceDescription {
	Bolt11InvoiceDescription::Direct(Description::new(s.to_string()).unwrap())
}

fn hash_of(inv: &Bolt11Invoice) -> PaymentHash {
	PaymentHash(inv.payment_hash().to_byte_array())
}

fn channel_with(node: &Ldk, peer: &PublicKey) -> R<ChannelDetails> {
	node.node()
		.list_channels()
		.into_iter()
		.find(|c| c.counterparty_node_id == *peer)
		.ok_or_else(|| format!("{} has no channel with {peer}", node.name))
}

fn channel_by_id(node: &Ldk, ucid: UserChannelId) -> R<ChannelDetails> {
	node.node()
		.list_channels()
		.into_iter()
		.find(|c| c.user_channel_id == ucid)
		.ok_or_else(|| format!("{} has no channel {}", node.name, ucid.0))
}

fn funding_of(c: &ChannelDetails) -> R<(String, u32)> {
	let f = c.funding_txo.ok_or("channel has no funding outpoint")?;
	Ok((f.txid.to_string(), f.vout))
}

/// Mines until `txid` has a confirmation (it must already be broadcast).
fn confirm(env: &Env, txid: &str) -> R<()> {
	for _ in 0..6 {
		if check::tx(txid).map(|t| t.confirmations).unwrap_or(0) >= 1 {
			return Ok(());
		}
		env.mine(1)?;
	}
	Err(format!("{txid} did not confirm"))
}

fn record(env: &Env, label: &str, info: &TxInfo, payer: Option<Who>) {
	env.record(label, info, payer);
}

/// A preimage nobody else will make.
fn preimage(n: u8) -> [u8; 32] {
	let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
	let mut p = [0u8; 32];
	p[..16].copy_from_slice(&nanos.to_le_bytes());
	p[31] = n;
	p
}

fn hex(b: &[u8]) -> String {
	b.iter().map(|x| format!("{x:02x}")).collect()
}

fn lnd_invoice(amt_sat: u64, memo: &str) -> R<Bolt11Invoice> {
	let v = ln(&["addinvoice", "--amt", &amt_sat.to_string(), "--memo", memo])?;
	let pr = v["payment_request"].as_str().ok_or(format!("addinvoice: {v}"))?;
	Bolt11Invoice::from_str(pr).map_err(|e| format!("LND invoice: {e:?}"))
}

/// A hold invoice at LND: (invoice, preimage hex, hash hex).
fn lnd_hold_invoice(amt_sat: u64, n: u8) -> R<(Bolt11Invoice, String, String)> {
	let pre = preimage(n);
	let hash = sha256::Hash::hash(&pre).to_string();
	let v = ln(&["addholdinvoice", &hash, "--amt", &amt_sat.to_string(), "--expiry", "3600"])?;
	let pr = v["payment_request"].as_str().ok_or(format!("addholdinvoice: {v}"))?;
	let inv = Bolt11Invoice::from_str(pr).map_err(|e| format!("LND hold invoice: {e:?}"))?;
	Ok((inv, hex(&pre), hash))
}

/// P pays a hold invoice; returns once LND holds the HTLC (ACCEPTED).
fn pay_hold(env: &Env, inv: &Bolt11Invoice, hash: &str) -> R<()> {
	let t = std::time::Instant::now();
	loop {
		match env.p.node().bolt11_payment().send(inv, None) {
			Ok(id) => {
				let held = poll("LND to hold the HTLC", 30, || {
					env.p.pump();
					if ln(&["lookupinvoice", hash])?["state"] == "ACCEPTED" {
						return Ok(Some(true));
					}
					let failed = env.p.node().payment(&id).map(|p| {
						p.status == ldk_node::payment::PaymentStatus::Failed
					});
					Ok((failed == Some(true)).then_some(false))
				})?;
				if held {
					println!("  hold invoice {}: HTLC held by LND", &hash[..16]);
					return Ok(());
				}
				println!("  hold invoice {}: payment failed, retrying", &hash[..16]);
			},
			Err(e) => println!("  hold invoice: send: {e:?}, retrying"),
		}
		if t.elapsed().as_secs() > 60 {
			return Err("the hold invoice payment never reached LND".into());
		}
		sleep_ms(3000);
	}
}

/// P opens a channel to LND; its funding is checked (P's wallet inputs 0x21).
fn open_p_to_lnd(env: &Env, sats: u64, lnd_channels: usize) -> R<(UserChannelId, (String, u32))> {
	env.p
		.node()
		.connect(env.lnd_id, env.lnd_addr.clone(), true)
		.map_err(|e| format!("P connect LND: {e}"))?;
	let ucid = env
		.p
		.node()
		.open_channel(env.lnd_id, env.lnd_addr.clone(), sats, None, None)
		.map_err(|e| format!("P open to LND: {e}"))?;
	let Event::ChannelPending { funding_txo, .. } = env.p.wait_for("channel pending", 30, |e| {
		matches!(e, Event::ChannelPending { user_channel_id, .. } if *user_channel_id == ucid)
	})?
	else {
		unreachable!()
	};
	env.mine(6)?;
	env.p.wait_for("channel ready", 60, |e| {
		matches!(e, Event::ChannelReady { user_channel_id, .. } if *user_channel_id == ucid)
	})?;
	env.wait_lnd_active(lnd_channels)?;
	let funding = funding_txo.txid.to_string();
	let info = confirmed_all(&funding, U)?;
	record(env, &format!("P->LND {sats} funding (P wallet inputs)"), &info, Some(Who::P));
	let lch = ln(&["listchannels"])?;
	let ct = lch["channels"]
		.as_array()
		.into_iter()
		.flatten()
		.find(|c| c["channel_point"] == format!("{}:{}", funding, funding_txo.vout))
		.map(|c| c["commitment_type"].clone())
		.unwrap_or_default();
	println!("  LND commitment type: {ct}");
	if !ct.as_str().unwrap_or("").contains("STATIC_REMOTE_KEY") {
		return Err(format!("LND reports commitment type {ct}, expected STATIC_REMOTE_KEY"));
	}
	Ok((ucid, (funding, funding_txo.vout)))
}

// ------------------------------------------------------------------ setup

/// LND opens a public 1,000,000 sat channel to S; P connects to S and learns
/// the channel by gossip (so P can route to LND and S is in P's graph).
pub fn setup(env: &Env) -> R<()> {
	let s_uri = format!("{}@{}", env.s.id, env.s_addr);
	ln(&["connect", &s_uri])?;
	let open = ln(&["openchannel", "--node_key", &env.s.id.to_string(), "--local_amt", "1000000"])?;
	let funding = open["funding_txid"].as_str().ok_or(format!("LND refused to open to S: {open}"))?.to_string();
	println!("  LND -> S funding {funding}");
	let Event::ChannelPending { user_channel_id, .. } =
		env.s.wait_for("LND channel pending", 30, |e| matches!(e, Event::ChannelPending { .. }))?
	else {
		unreachable!()
	};
	env.mine(6)?;
	env.s.wait_for("LND channel ready", 60, |e| matches!(e, Event::ChannelReady { .. }))?;
	env.wait_lnd_active(1)?;
	*env.lnd_s_channel.borrow_mut() = Some(user_channel_id);
	let info = confirmed_all(&funding, U)?;
	record(env, "LND->S funding (LND wallet inputs)", &info, None);

	env.p.node().connect(env.s.id, env.s_addr.clone(), true).map_err(|e| format!("P connect S: {e}"))?;
	poll("P to learn the LND-S channel by gossip", 180, || {
		Ok((!env.p.node().network_graph().list_channels().is_empty()).then_some(()))
	})?;
	println!("  P's graph: {} channel(s), {} node(s)", env.p.node().network_graph().list_channels().len(), env.p.node().network_graph().list_nodes().len());
	Ok(())
}

// ------------------------------------------------------------------ 1

/// P creates an LSPS2 JIT invoice; LND pays it through S, which opens the channel.
fn jit_open(env: &Env) -> R<Outcome> {
	let amount_msat = 200_000_000;
	let inv = env
		.p
		.node()
		.bolt11_payment()
		.receive_via_jit_channel(amount_msat, &desc("xr jit"), 3600, None)
		.map_err(|e| format!("receive_via_jit_channel: {e}"))?;
	println!("  JIT invoice route hints: {:?}", inv.route_hints().iter().map(|h| h.0.iter().map(|x| (x.src_node_id, x.short_channel_id)).collect::<Vec<_>>()).collect::<Vec<_>>());
	lnd_pay_retry("JIT invoice", &inv, 120)?;
	let got = env.p.wait_received(hash_of(&inv), 60)?;
	let fee = JIT_MIN_FEE_MSAT.max(amount_msat * JIT_FEE_PPM as u64 / 1_000_000);
	println!("  P received {got} msat of {amount_msat} (LSPS2 fee {fee} msat)");
	if got != amount_msat - fee {
		return Err(format!("P received {got} msat, expected {} (amount minus the LSPS2 fee)", amount_msat - fee));
	}
	let c = channel_with(&env.p, &env.s.id)?;
	*env.jit_channel.borrow_mut() = Some(c.user_channel_id);
	let (funding, _) = funding_of(&c)?;
	println!("  JIT channel {} sat, funding {funding}", c.channel_value_sats);
	poll("S to broadcast the JIT funding", 60, || {
		Ok(btc(&["getrawtransaction", &funding]).is_ok().then_some(()))
	})?;
	env.mine(6)?;
	let info = confirmed_all(&funding, U)?;
	record(env, "JIT channel funding (S wallet inputs)", &info, Some(Who::S));
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 2

/// P pays an LND invoice (through S); LND pays a P invoice through S (its
/// route hint names S and the JIT channel's alias SCID).
fn pay_both_ways(env: &Env) -> R<Outcome> {
	let inv = lnd_invoice(20_000, "xr p->lnd")?;
	send_retry(&env.p, "P pays LND 20,000 sat", 90, |n| n.bolt11_payment().send(&inv, None))?;

	let inv = env
		.p
		.node()
		.bolt11_payment()
		.receive(30_000_000, &desc("xr lnd->p"), 3600)
		.map_err(|e| format!("P receive: {e}"))?;
	let hints: Vec<_> = inv.route_hints().iter().flat_map(|h| h.0.clone()).collect();
	println!("  P invoice route hints: {:?}", hints.iter().map(|h| (h.src_node_id, h.short_channel_id)).collect::<Vec<_>>());
	if !hints.iter().any(|h| h.src_node_id == env.s.id) {
		return Err("P's invoice has no route hint through S".into());
	}
	lnd_pay_retry("LND pays P 30,000 sat through S", &inv, 90)?;
	let got = env.p.wait_received(hash_of(&inv), 60)?;
	if got != 30_000_000 {
		return Err(format!("P received {got} msat, expected 30,000,000"));
	}
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 3, 4

/// Waits for a splice of the JIT channel to lock; returns the new funding txid.
fn splice_and_lock(env: &Env, what: &str) -> R<String> {
	let ucid = env.jit_channel.borrow().ok_or("no JIT channel")?;
	let Event::SplicePending { new_funding_txo, .. } = env.p.wait_for(&format!("{what} pending"), 60, |e| {
		matches!(e, Event::SplicePending { user_channel_id, .. } if *user_channel_id == ucid)
	})?
	else {
		unreachable!()
	};
	let txid = new_funding_txo.txid.to_string();
	println!("  {what} transaction {txid}");
	poll(&format!("{what} in the mempool"), 30, || Ok(btc(&["getrawtransaction", &txid]).is_ok().then_some(())))?;
	env.mine(6)?;
	poll(&format!("{what} to lock"), 90, || {
		env.p.pump();
		env.s.pump();
		let c = channel_by_id(&env.p, ucid)?;
		Ok((c.funding_txo == Some(new_funding_txo) && c.is_usable).then_some(()))
	})
	.or_else(|_| {
		// One more block and sync, in case a node missed the depth.
		env.mine(1)?;
		poll(&format!("{what} to lock"), 60, || {
			let c = channel_by_id(&env.p, ucid)?;
			Ok((c.funding_txo == Some(new_funding_txo) && c.is_usable).then_some(()))
		})
	})?;
	Ok(txid)
}

/// The expected bytes of a splice: the shared input (spending `old`) is
/// [0x21, 0x21], every other input (P's wallet) is [0x21].
fn splice_expectation(txid: &str, old: &(String, u32)) -> R<Vec<&'static [u8]>> {
	let t = check::tx(txid)?;
	let exp: Vec<&[u8]> = t.prevouts.iter().map(|p| if p == old { UU } else { U }).collect();
	if !t.prevouts.contains(old) {
		return Err(format!("{txid} does not spend the old funding output {}:{}", old.0, old.1));
	}
	Ok(exp)
}

/// P splices 50,000 sat in from its wallet.
fn splice_in(env: &Env) -> R<Outcome> {
	let ucid = env.jit_channel.borrow().ok_or("no JIT channel")?;
	let before = channel_by_id(&env.p, ucid)?;
	let old = funding_of(&before)?;
	env.p.node().splice_in(&ucid, env.s.id, 50_000).map_err(|e| format!("splice_in: {e}"))?;
	let txid = splice_and_lock(env, "splice-in")?;
	let exp = splice_expectation(&txid, &old)?;
	if exp.len() < 2 {
		return Err(format!("splice-in {txid} has no wallet input"));
	}
	let info = confirmed_with(&txid, &exp)?;
	record(env, "splice-in (shared + P wallet inputs)", &info, Some(Who::P));
	let after = channel_by_id(&env.p, ucid)?;
	if after.channel_value_sats != before.channel_value_sats + 50_000 {
		return Err(format!("channel {} sat after splice-in, was {}", after.channel_value_sats, before.channel_value_sats));
	}
	Ok(Outcome::Pass)
}

/// P splices 30,000 sat out to its own wallet.
fn splice_out(env: &Env) -> R<Outcome> {
	let ucid = env.jit_channel.borrow().ok_or("no JIT channel")?;
	let old = funding_of(&channel_by_id(&env.p, ucid)?)?;
	let addr = env.p.node().onchain_payment().new_address().map_err(|e| e.to_string())?;
	env.p.node().splice_out(&ucid, env.s.id, &addr, 30_000).map_err(|e| format!("splice_out: {e}"))?;
	let txid = splice_and_lock(env, "splice-out")?;
	let exp = splice_expectation(&txid, &old)?;
	let info = confirmed_with(&txid, &exp)?;
	if !info.outputs.iter().any(|(v, a)| *v == 30_000 && *a == addr.to_string()) {
		return Err(format!("splice-out {txid} pays no 30,000 sat to {addr}: {:?}", info.outputs));
	}
	record(env, "splice-out (shared input)", &info, Some(Who::P));
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 5

/// P and S pay each other's offers (must pass); P pays an LND offer and LND
/// pays S's offer (interop: recorded as BOLT12_INTEROP, does not fail the suite).
fn bolt12_both_ways(env: &Env) -> R<Outcome> {
	let s_offer = env.s.node().bolt12_payment().receive(10_000_000, "xr S offer", None, None).map_err(|e| format!("S offer: {e}"))?;
	send_retry(&env.p, "P pays S's offer", 90, |n| n.bolt12_payment().send(&s_offer, None, None, None))?;
	env.s.wait_received_amount(10_000_000, 30)?;
	let p_offer = env.p.node().bolt12_payment().receive(12_000_000, "xr P offer", None, None).map_err(|e| format!("P offer: {e}"))?;
	send_retry(&env.s, "S pays P's offer", 90, |n| n.bolt12_payment().send(&p_offer, None, None, None))?;
	env.p.wait_received_amount(12_000_000, 30)?;

	let interop = bolt12_interop(env);
	println!("  BOLT12_INTEROP: {interop:?}");
	*env.bolt12_interop.borrow_mut() = match interop {
		Ok(()) => "pass".into(),
		Err(e) => format!("fail ({e})"),
	};
	Ok(Outcome::Pass)
}

/// Both interop directions, each tried whatever the other did.
fn bolt12_interop(env: &Env) -> R<()> {
	let p_to_lnd = p_pays_lnd_offer(env).map_err(|e| format!("P -> LND offer: {e}{}", lnd_htlc_error()));
	println!("  P -> LND offer: {p_to_lnd:?}");
	let lnd_to_s = lnd_pays_s_offer(env).map_err(|e| format!("LND -> S offer: {e}"));
	println!("  LND -> S offer: {lnd_to_s:?}");
	match (p_to_lnd, lnd_to_s) {
		(Ok(()), Ok(())) => Ok(()),
		(a, b) => Err([a.err(), b.err()].into_iter().flatten().collect::<Vec<_>>().join("; ")),
	}
}

/// LND's last refusal of an HTLC, from its log (why a payment to it failed).
fn lnd_htlc_error() -> String {
	let log = format!("{}/logs/lnd.log", crate::env::cfg().xr);
	std::fs::read_to_string(log)
		.ok()
		.and_then(|l| l.lines().rev().find(|x| x.contains("[ERR] HSWC")).map(|x| format!("; LND: {}", x.trim())))
		.unwrap_or_default()
}

fn p_pays_lnd_offer(env: &Env) -> R<()> {
	let v = ln(&["offer", "create", "--description", "xr LND offer", "--amount_msat", "15000000"])?;
	let lno = v["offer"]["bolt12"].as_str().ok_or(format!("lncli offer create: {v}"))?;
	let offer = Offer::from_str(lno).map_err(|e| format!("LND offer: {e:?}"))?;
	send_retry(&env.p, "P pays LND's offer", 30, |n| n.bolt12_payment().send(&offer, None, None, None))?;
	Ok(())
}

fn lnd_pays_s_offer(env: &Env) -> R<()> {
	let s_offer = env
		.s
		.node()
		.bolt12_payment()
		.receive(16_000_000, "xr S offer for LND", None, None)
		.map_err(|e| format!("S offer: {e}"))?;
	let (ok, out) = ln_try(&["offer", "pay", "--timeout", "60", &s_offer.to_string()]);
	println!("  lncli offer pay: {}", out.trim());
	if !ok {
		return Err(out.trim().to_string());
	}
	env.s.wait_received_amount(16_000_000, 60)?;
	Ok(())
}

// ------------------------------------------------------------------ 6

/// LND pays P's async offer while P is stopped; P starts and claims.
///
/// P registers with S (static invoice server) for an async offer, then stops. LND sends its
/// invoice request over the offer's path (introduced at S). S answers with P's static invoice
/// and forwards the request to P; S holds the forwarded request while P is offline (its onion
/// message mailbox). When P starts it answers LND with an invoice, LND pays it through S and P
/// claims. (S cannot be the payer: LDK 0.2 cannot send into a blinded path introduced at
/// itself, and S is P's static invoice server.)
///
/// What this proves: a held `invoice_request` (an onion message S keeps for offline P) is
/// answered after P restarts, and the LND payer then pays P. It does not prove a held HTLC: LND
/// pays only after P is back online. Single-LSP held-HTLC async payment (S holding the payer's
/// HTLC until P is online) is blocked in upstream LDK 0.2.7: `process_pending_update_add_htlcs`
/// calls `can_forward_htlc` (whose `can_forward_htlc_to_outgoing_channel` fails a forward to a
/// channel that is not live with `ChannelDisabled`) before the HTLC is held.
fn offer_while_stopped(env: &Env) -> R<Outcome> {
	let paths = env
		.s
		.node()
		.bolt12_payment()
		.blinded_paths_for_async_recipient(env.p.id.serialize().to_vec())
		.map_err(|e| format!("S cannot make async paths for P: {e}"))?;
	env.p
		.node()
		.bolt12_payment()
		.set_paths_to_static_invoice_server(paths)
		.map_err(|e| format!("P cannot take S's async paths: {e}"))?;
	let offer = poll("P's async offer", 180, || {
		env.p.pump();
		env.s.pump();
		Ok(env.p.node().bolt12_payment().receive_async().ok())
	})?;
	println!("  P's async offer: {offer}");
	if !offer.offer_features().requires_blake2b() {
		return Err("P's async offer does not require option_blake2b".into());
	}

	env.p.stop()?;
	let amount_msat = 8_000_000u64;
	let lno = offer.to_string();
	let amt = amount_msat.to_string();
	let payer = std::thread::spawn(move || {
		ln_try(&["offer", "pay", "--amount_msat", &amt, "--timeout", "120", &lno])
	});
	// LND's request is on its way while P is offline.
	sleep_ms(5000);
	if payer.is_finished() {
		let (_, out) = payer.join().map_err(|_| "lncli thread panicked")?;
		return Err(format!("LND finished paying before P started: {}", out.trim()));
	}
	env.p.start()?;
	env.p.node().connect(env.s.id, env.s_addr.clone(), true).map_err(|e| format!("P reconnect: {e}"))?;
	let (ok, out) = payer.join().map_err(|_| "lncli thread panicked")?;
	println!("  lncli offer pay: {}", out.trim());
	if !ok {
		return Err(format!("LND could not pay P's async offer: {}", out.trim()));
	}
	let v: serde_json::Value =
		serde_json::from_str(out.trim()).map_err(|e| format!("lncli offer pay output: {e}: {out}"))?;
	let paid = v["payment_preimage"].as_str().map_or(false, |p| !p.is_empty())
		&& v["amount_msat"] == serde_json::Value::String(amount_msat.to_string());
	if !paid {
		return Err(format!("LND's payment did not succeed for {amount_msat} msat: {v}"));
	}
	env.p.wait_received_amount(amount_msat, 60)?;
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 7

/// P closes the JIT channel with S cooperatively.
fn coop_close(env: &Env) -> R<Outcome> {
	let ucid = env.jit_channel.borrow().ok_or("no JIT channel")?;
	let (funding, vout) = funding_of(&channel_by_id(&env.p, ucid)?)?;
	env.p.node().close_channel(&ucid, env.s.id).map_err(|e| format!("close: {e}"))?;
	let closing = env.wait_spender("cooperative close", &funding, vout, 1, 3)?;
	confirm(env, &closing)?;
	let info = confirmed_with(&closing, &[UU])?;
	record(env, "P-S cooperative close", &info, Some(Who::S));
	env.mine(5)?;
	env.p.wait_for("P channel closed", 60, |e| matches!(e, Event::ChannelClosed { user_channel_id, .. } if *user_channel_id == ucid))?;
	env.s.wait_for("S channel closed", 60, |e| matches!(e, Event::ChannelClosed { counterparty_node_id, .. } if *counterparty_node_id == Some(env.p.id)))?;
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 8

/// Force closes with HTLCs in flight, both ways:
/// - channel A: P force-closes holding two HTLCs at LND. P's commitment
///   and its HTLC-timeout are [0x21, 0x21]; the second HTLC is settled after
///   the close and LND claims it from P's commitment with the preimage.
/// - channel B: LND force-closes holding P's HTLC, then settles it: LND's
///   commitment and its HTLC-success transaction (P's signature and LND's)
///   are [0x21, 0x21]. On P's commitment LND's preimage claim is a one-key
///   spend; the two-signature HTLC-success exists only on LND's commitment.
fn force_close_htlcs(env: &Env) -> R<Outcome> {
	let (ucid_a, (fund_a, vout_a)) = open_p_to_lnd(env, 400_000, 2)?;
	let (inv1, _pre1, hash1) = lnd_hold_invoice(30_000, 1)?;
	let (inv2, pre2, hash2) = lnd_hold_invoice(20_000, 2)?;
	pay_hold(env, &inv1, &hash1)?;
	pay_hold(env, &inv2, &hash2)?;

	env.p.node().force_close_channel(&ucid_a, env.lnd_id, None).map_err(|e| format!("force close: {e}"))?;
	let commit = env.wait_spender("P's commitment (channel A)", &fund_a, vout_a, 1, 0)?;
	confirm(env, &commit)?;
	let info = confirmed_with(&commit, &[UU])?;
	record(env, "P commitment, channel A", &info, Some(Who::P));
	let h1 = output_with_value(&info, 30_000)?;
	let h2 = output_with_value(&info, 20_000)?;

	ln(&["settleinvoice", &pre2])?;
	let claim = env.wait_spender("LND's preimage claim of HTLC 2 on P's commitment", &commit, h2, 1, 20)?;
	confirm(env, &claim)?;
	let info = confirmed_all(&claim, &[])?;
	record(env, "LND preimage claim on P commitment (LND only signs)", &info, None);

	let timeout = env.wait_spender("P's HTLC-timeout of HTLC 1", &commit, h1, 10, 30)?;
	confirm(env, &timeout)?;
	let info = confirmed_with(&timeout, &[UU])?;
	record(env, "P HTLC-timeout, channel A", &info, Some(Who::P));
	let _ = ln(&["cancelinvoice", &hash1]);

	let (_ucid_b, (fund_b, vout_b)) = open_p_to_lnd(env, 200_000, 2)?;
	let (inv3, pre3, hash3) = lnd_hold_invoice(25_000, 3)?;
	pay_hold(env, &inv3, &hash3)?;
	ln(&["closechannel", "--force", "--funding_txid", &fund_b, "--output_index", &vout_b.to_string()])?;
	let commit_b = env.wait_spender("LND's commitment (channel B)", &fund_b, vout_b, 1, 0)?;
	confirm(env, &commit_b)?;
	let info = confirmed_with(&commit_b, &[UU])?;
	record(env, "LND commitment, channel B", &info, Some(Who::P));
	let h3 = output_with_value(&info, 25_000)?;
	ln(&["settleinvoice", &pre3])?;
	let success = env.wait_spender("LND's HTLC-success of HTLC 3", &commit_b, h3, 1, 20)?;
	confirm(env, &success)?;
	let info = confirmed_with(&success, &[UU])?;
	record(env, "LND HTLC-success, channel B (P and LND signatures)", &info, None);
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 9

/// S broadcasts a revoked commitment (restored from an old copy of its
/// directory); P's justice transaction takes S's balance.
fn penalty(env: &Env) -> R<Outcome> {
	let ucid = env
		.p
		.node()
		.open_channel(env.s.id, env.s_addr.clone(), 300_000, None, None)
		.map_err(|e| format!("P open to S: {e}"))?;
	let Event::ChannelPending { funding_txo, .. } = env.p.wait_for("channel pending", 30, |e| {
		matches!(e, Event::ChannelPending { user_channel_id, .. } if *user_channel_id == ucid)
	})?
	else {
		unreachable!()
	};
	env.mine(6)?;
	env.p.wait_for("channel ready", 60, |e| matches!(e, Event::ChannelReady { user_channel_id, .. } if *user_channel_id == ucid))?;
	let (fund, vout) = (funding_txo.txid.to_string(), funding_txo.vout);
	let info = confirmed_all(&fund, U)?;
	record(env, "P->S penalty channel funding (P wallet inputs)", &info, Some(Who::P));

	let pay_s = |amt: u64, label: &str| -> R<()> {
		let inv = env.s.node().bolt11_payment().receive(amt * 1000, &desc(label), 3600).map_err(|e| e.to_string())?;
		send_retry(&env.p, label, 90, |n| n.bolt11_payment().send(&inv, None))?;
		env.s.wait_received(hash_of(&inv), 30)?;
		env.penalty_msat.set(env.penalty_msat.get() + amt * 1000);
		Ok(())
	};
	// S needs a balance in the state it will later publish.
	pay_s(40_000, "xr penalty 1")?;

	let xr = &crate::env::cfg().xr;
	let stale = format!("{xr}/ldk-s.stale");
	env.s.stop()?;
	let _ = std::fs::remove_dir_all(&stale);
	crate::env::run("cp", &["-a", &env.s.dir, &stale])?;
	println!("  copied S's directory to {stale}");
	env.s.start()?;
	env.p.node().connect(env.s.id, env.s_addr.clone(), true).map_err(|e| format!("P reconnect: {e}"))?;
	poll("the penalty channel to be usable again", 60, || Ok(channel_by_id(&env.p, ucid)?.is_usable.then_some(())))?;
	pay_s(25_000, "xr penalty 2")?;
	pay_s(25_000, "xr penalty 3")?;

	// P stays offline while S publishes, so the channel is not re-established
	// (S would learn it is stale and refuse to broadcast).
	env.p.stop()?;
	env.s.stop()?;
	env.s.restore(&stale)?;
	env.s.start()?;
	let s_chan = channel_with(&env.s, &env.p.id)?;
	println!("  restored S sees the channel at {} msat outbound (stale)", s_chan.outbound_capacity_msat);
	env.s
		.node()
		.force_close_channel(&s_chan.user_channel_id, env.p.id, Some("xr penalty".into()))
		.map_err(|e| format!("stale S force close: {e}"))?;
	let revoked = env.wait_spender("S's revoked commitment", &fund, vout, 1, 0)?;
	confirm(env, &revoked)?;
	let info = confirmed_with(&revoked, &[UU])?;
	record(env, "S revoked commitment (P opened: P pays its fee)", &info, Some(Who::P));
	let s_out = output_with_value(&info, 40_000)?;

	env.p.start()?;
	let justice = env.wait_spender("P's justice transaction", &revoked, s_out, 1, 20)?;
	confirm(env, &justice)?;
	let info = confirmed_all(&justice, &[])?;
	record(env, "P justice", &info, Some(Who::P));
	// The justice transaction pays P's wallet directly (ldk-node's destination script is a
	// wallet address): P's wallet records it as an inbound on-chain payment, and what reaches P
	// is S's whole revoked balance less the justice fee.
	let to_p = poll("P's wallet to record the justice transaction", 60, || {
		env.p.node().sync_wallets().map_err(|e| e.to_string())?;
		Ok(env.p.node().list_payments().into_iter().find_map(|p| match p.kind {
			PaymentKind::Onchain { txid, .. }
				if txid.to_string() == justice && p.direction == PaymentDirection::Inbound =>
			{
				Some(p.amount_msat.unwrap_or(0) / 1000)
			},
			_ => None,
		}))
	})?;
	println!("  justice pays P's wallet {to_p} sat (fee {} sat)", info.fee_sat);
	if to_p + info.fee_sat < 40_000 {
		return Err(format!(
			"the justice transaction pays P's wallet {to_p} sat + fee {} sat, less than S's revoked balance of 40,000",
			info.fee_sat
		));
	}
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 10

/// Closes S's channel with LND, then mines (10 blocks a round, up to 60
/// rounds) until P and S have no lightning balances and every sweep they
/// reported is confirmed by Knots; then checks the on-chain totals.
fn sweeps_settle(env: &Env) -> R<Outcome> {
	if let Some(ucid) = *env.lnd_s_channel.borrow() {
		let (fund, vout) = funding_of(&channel_by_id(&env.s, ucid)?)?;
		// S was restarted from its old copy in `penalty`: be connected to LND again first.
		env.s.node().connect(env.lnd_id, env.lnd_addr.clone(), true).map_err(|e| format!("S connect LND: {e}"))?;
		poll("S's channel with LND to be usable", 60, || Ok(channel_by_id(&env.s, ucid)?.is_usable.then_some(())))?;
		env.s.node().close_channel(&ucid, env.lnd_id).map_err(|e| format!("S close LND: {e}"))?;
		let closing = env.wait_spender("S-LND cooperative close", &fund, vout, 1, 3)?;
		confirm(env, &closing)?;
		let info = confirmed_with(&closing, &[UU])?;
		record(env, "S-LND cooperative close (LND opened: LND pays)", &info, None);
	}

	let mut sweeps: Vec<(Who, String)> = vec![];
	let mut settled = false;
	'rounds: for round in 0..60 {
		for _ in 0..10 {
			env.mine(1)?;
			settled = true;
			for n in env.nodes() {
				let b = n.node().list_balances();
				for s in &b.pending_balances_from_channel_closures {
					let txid = match s {
						PendingSweepBalance::PendingBroadcast { .. } => None,
						PendingSweepBalance::BroadcastAwaitingConfirmation { latest_spending_txid, .. }
						| PendingSweepBalance::AwaitingThresholdConfirmations { latest_spending_txid, .. } => {
							Some(latest_spending_txid.to_string())
						},
					};
					match txid {
						Some(t) => {
							if !sweeps.contains(&(n.who, t.clone())) {
								println!("  {} sweep {t}", n.name);
								sweeps.push((n.who, t.clone()));
							}
							if check::tx(&t).map(|i| i.confirmations).unwrap_or(0) == 0 {
								settled = false;
							}
						},
						None => settled = false,
					}
				}
				if !b.lightning_balances.is_empty() {
					settled = false;
				}
			}
			if settled {
				println!("  settled after {} rounds", round + 1);
				break 'rounds;
			}
		}
		for n in env.nodes() {
			let b = n.node().list_balances();
			println!("  round {}: {} lightning {:?}", round + 1, n.name, b.lightning_balances.iter().map(short_balance).collect::<Vec<_>>());
		}
	}
	if !settled {
		return Err("P or S still has lightning balances after 600 blocks".into());
	}
	env.mine(6)?;

	// Sweeps count by confirmed txid (ldk-node's list lags behind).
	for (who, txid) in &sweeps {
		match check::tx(txid) {
			Ok(t) if t.confirmations >= 1 => {
				if !env.recorded(txid) {
					let info = confirmed_all(txid, &[])?;
					record(env, &format!("{who:?} sweep"), &info, Some(*who));
				}
			},
			_ => println!("  {who:?} sweep {txid} not confirmed (replaced)"),
		}
	}

	let mut errors = vec![];
	for n in env.nodes() {
		let adj = match n.who {
			Who::P => env.penalty_msat.get() as i64,
			Who::S => -(env.penalty_msat.get() as i64),
		};
		let expected = env.funded[&n.who] as i64
			+ (n.received_msat.get() as i64 - n.sent_msat.get() as i64 + n.forward_fees_msat.get() as i64 + adj) / 1000;
		let actual = n.onchain_sats() as i64;
		let fees = env.fees_of(n.who) as i64;
		let b = n.node().list_balances();
		println!(
			"  {}: on-chain {actual} sat, expected {expected} (funded {}, received {} msat, sent {} msat, forwarding fees {} msat, penalty {adj} msat), fees {fees} sat, diff {} sat, lightning {:?}",
			n.name, env.funded[&n.who], n.received_msat.get(), n.sent_msat.get(), n.forward_fees_msat.get(), actual - expected, b.lightning_balances.len()
		);
		if !b.lightning_balances.is_empty() {
			errors.push(format!("{} still has lightning balances", n.name));
		}
		if (actual - expected).abs() > fees + 1_000 {
			errors.push(format!("{} on-chain {actual} sat, expected {expected} within {} sat", n.name, fees + 1_000));
		}
	}
	if errors.is_empty() {
		Ok(Outcome::Pass)
	} else {
		Err(errors.join("; "))
	}
}

fn short_balance(b: &LightningBalance) -> String {
	let s = format!("{b:?}");
	s.split(" {").next().unwrap_or(&s).to_string()
}
