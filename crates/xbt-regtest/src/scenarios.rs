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
use fortis_lightning::LnEvent;

use crate::env::{
	self, btc, btc_amount, ln, ln_try, lnd_pay_retry, poll, send_retry, sleep_ms, Env, Ldk, LspProcess, Who,
	JIT_FEE_PPM, JIT_MIN_FEE_MSAT, R,
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
	("force_close_after_splice", force_close_after_splice),
	("force_close_htlcs", force_close_htlcs),
	("penalty", penalty),
	("sweeps_settle", sweeps_settle),
	("lsp_xbt_jit", lsp_xbt_jit),
	("app_node_xbt", app_node_xbt),
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

/// Waits for a splice of `node`'s channel `ucid` to lock; returns the new funding txid.
fn splice_and_lock(env: &Env, node: &Ldk, ucid: UserChannelId, what: &str) -> R<String> {
	let Event::SplicePending { new_funding_txo, .. } = node.wait_for(&format!("{what} pending"), 60, |e| {
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
		node.pump();
		env.s.pump();
		let c = channel_by_id(node, ucid)?;
		Ok((c.funding_txo == Some(new_funding_txo) && c.is_usable).then_some(()))
	})
	.or_else(|_| {
		// One more block and sync, in case a node missed the depth.
		env.mine(1)?;
		poll(&format!("{what} to lock"), 60, || {
			let c = channel_by_id(node, ucid)?;
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
	let txid = splice_and_lock(env, &env.p, ucid, "splice-in")?;
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
	let txid = splice_and_lock(env, &env.p, ucid, "splice-out")?;
	let exp = splice_expectation(&txid, &old)?;
	let info = confirmed_with(&txid, &exp)?;
	if !info.outputs.iter().any(|(v, a)| *v == 30_000 && *a == addr.to_string()) {
		return Err(format!("splice-out {txid} pays no 30,000 sat to {addr}: {:?}", info.outputs));
	}
	record(env, "splice-out (shared input)", &info, Some(Who::P));
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 5

/// P and S pay each other's offers, P pays an LND offer and LND pays S's offer; all must pass.
///
/// P -> LND goes P -> S -> LND into the offer's blinded path, which LND starts at itself and
/// pads with dummy hops. LND caps the CLTV of that path at its creation height + 12 blocks
/// + the payinfo delta; P adds no shadow CLTV offset to a path LND made (vendor patch, see
/// XBT-PATCHES.md), otherwise LND refuses the HTLC ("insufficient record with type 4").
fn bolt12_both_ways(env: &Env) -> R<Outcome> {
	let s_offer = env.s.node().bolt12_payment().receive(10_000_000, "xr S offer", None, None).map_err(|e| format!("S offer: {e}"))?;
	send_retry(&env.p, "P pays S's offer", 90, |n| n.bolt12_payment().send(&s_offer, None, None, None))?;
	env.s.wait_received_amount(10_000_000, 30)?;
	let p_offer = env.p.node().bolt12_payment().receive(12_000_000, "xr P offer", None, None).map_err(|e| format!("P offer: {e}"))?;
	send_retry(&env.s, "S pays P's offer", 90, |n| n.bolt12_payment().send(&p_offer, None, None, None))?;
	env.p.wait_received_amount(12_000_000, 30)?;

	p_pays_lnd_offer(env).map_err(|e| format!("P -> LND offer: {e}{}", lnd_htlc_error()))?;
	println!("  P -> LND offer: paid");
	lnd_pays_s_offer(env).map_err(|e| format!("LND -> S offer: {e}"))?;
	println!("  LND -> S offer: paid");
	Ok(Outcome::Pass)
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

// ------------------------------------------------------------------ 7b

/// A force close after a splice (spec §4): P opens a channel to S, pays S 30,000 sat over
/// it, splices 50,000 sat in, then force-closes. P's commitment spends the splice's funding
/// output and is [0x21, 0x21]; its to_local (P, after the delay) and to_remote (S) outputs
/// must be swept and confirmed by `sweeps_settle`, and count in its final accounting.
fn force_close_after_splice(env: &Env) -> R<Outcome> {
	let ucid = env
		.p
		.node()
		.open_channel(env.s.id, env.s_addr.clone(), 200_000, None, None)
		.map_err(|e| format!("P open to S: {e}"))?;
	let Event::ChannelPending { funding_txo, .. } = env.p.wait_for("channel pending", 30, |e| {
		matches!(e, Event::ChannelPending { user_channel_id, .. } if *user_channel_id == ucid)
	})?
	else {
		unreachable!()
	};
	env.mine(6)?;
	env.p.wait_for("channel ready", 60, |e| matches!(e, Event::ChannelReady { user_channel_id, .. } if *user_channel_id == ucid))?;
	let info = confirmed_all(&funding_txo.txid.to_string(), U)?;
	record(env, "P->S splice channel funding (P wallet inputs)", &info, Some(Who::P));

	// S gets a balance, so the commitment has a to_remote output.
	let inv = env.s.node().bolt11_payment().receive(30_000_000, &desc("xr splice-close"), 3600).map_err(|e| e.to_string())?;
	send_retry(&env.p, "P pays S 30,000 sat", 90, |n| n.bolt11_payment().send(&inv, None))?;
	env.s.wait_received(hash_of(&inv), 30)?;

	let old = funding_of(&channel_by_id(&env.p, ucid)?)?;
	env.p.node().splice_in(&ucid, env.s.id, 50_000).map_err(|e| format!("splice_in: {e}"))?;
	let splice = splice_and_lock(env, &env.p, ucid, "splice-in (before force close)")?;
	let exp = splice_expectation(&splice, &old)?;
	let info = confirmed_with(&splice, &exp)?;
	record(env, "splice-in before force close (shared + P wallet)", &info, Some(Who::P));
	let (fund, vout) = funding_of(&channel_by_id(&env.p, ucid)?)?;
	if fund != splice {
		return Err(format!("channel funding {fund} is not the splice {splice}"));
	}

	env.p.node().force_close_channel(&ucid, env.s.id, None).map_err(|e| format!("force close: {e}"))?;
	let commit = env.wait_spender("P's commitment (spliced channel)", &fund, vout, 1, 0)?;
	confirm(env, &commit)?;
	let info = confirmed_with(&commit, &[UU])?;
	record(env, "P commitment after splice", &info, Some(Who::P));
	let to_remote = output_with_value(&info, 30_000)?;
	let to_local: Vec<u32> = (0..info.outputs.len() as u32).filter(|i| *i != to_remote).collect();
	let [to_local] = to_local[..] else {
		return Err(format!("P's commitment {commit}: expected to_local and to_remote: {:?}", info.outputs));
	};
	println!("  to_local {commit}:{to_local} ({} sat), to_remote {commit}:{to_remote} (30,000 sat)", info.outputs[to_local as usize].0);
	env.must_sweep.borrow_mut().push((Who::P, "P to_local after splice".into(), commit.clone(), to_local));
	env.must_sweep.borrow_mut().push((Who::S, "S to_remote after splice".into(), commit.clone(), to_remote));

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
	// The outputs of the force close after the splice: each swept by its owner, confirmed.
	for (who, label, txid, vout) in env.must_sweep.borrow().iter() {
		match env.spender(txid, *vout)? {
			Some(s) if check::tx(&s).map(|t| t.confirmations).unwrap_or(0) >= 1 => {
				if !env.recorded(&s) {
					let info = confirmed_all(&s, &[])?;
					record(env, &format!("{who:?} sweep"), &info, Some(*who));
				}
				println!("  {label} {txid}:{vout} swept by {s}");
			},
			other => errors.push(format!("{label} {txid}:{vout} not swept and confirmed ({other:?})")),
		}
	}
	for n in env.nodes() {
		let adj = match n.who {
			Who::P => env.penalty_msat.get() as i64,
			Who::S => -(env.penalty_msat.get() as i64),
			Who::P2 | Who::Lsp => 0,
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

// ------------------------------------------------------------------ 11

/// What fortis-lsp's wallet is funded with, and P2's.
const LSP_FUNDED_SAT: u64 = 3_000_000;
const P2_FUNDED_SAT: u64 = 500_000;

/// The real fortis-lsp binary (`chain = "xbt"`) as the XBT LSP of a new phone, P2.
///
/// fortis-lsp is funded from Knots and LND opens a channel to it. P2 (splice peer: the
/// LSP) gets a JIT invoice from it, which LND pays: the LSP opens the channel ([0x21]
/// wallet inputs). P2 pays an LND invoice through it, splices 20,000 sat in (shared input
/// [0x21, 0x21]) and closes cooperatively ([0x21, 0x21]). Then P2's and the LSP's on-chain
/// wallets must hold what the payments and the recorded fees say. fortis-lsp is left
/// running for `app_node_xbt`; on a failure main prints its last log lines, then stops it.
fn lsp_xbt_jit(env: &Env) -> R<Outcome> {
	let lsp = LspProcess::start()?;
	let (lsp_id, lsp_addr, lsp_wallet) = (lsp.id, lsp.addr.clone(), lsp.onchain_address.clone());
	*env.lsp.borrow_mut() = Some(lsp);
	let lsp_metric = |name: &str| env.lsp.borrow().as_ref().ok_or("fortis-lsp not running")?.metric(name);

	btc(&["sendtoaddress", &lsp_wallet, &btc_amount(LSP_FUNDED_SAT)])?;
	let p2 = env.start_p2(lsp_id, lsp_addr.clone())?;
	let a = p2.node().onchain_payment().new_address().map_err(|e| e.to_string())?;
	btc(&["sendtoaddress", &a.to_string(), &btc_amount(P2_FUNDED_SAT)])?;

	ln(&["connect", &format!("{lsp_id}@{lsp_addr}")])?;
	let open = ln(&["openchannel", "--node_key", &lsp_id.to_string(), "--local_amt", "1000000"])?;
	let lnd_funding =
		open["funding_txid"].as_str().ok_or(format!("LND refused to open to fortis-lsp: {open}"))?.to_string();
	println!("  LND -> fortis-lsp funding {lnd_funding}");
	poll("the LND channel funding in the mempool", 30, || {
		Ok(btc(&["getrawtransaction", &lnd_funding]).is_ok().then_some(()))
	})?;
	env.mine(6)?;
	// fortis-lsp syncs on its own schedule (ldk-node: lightning 30 s, on-chain 80 s).
	poll("LND's channel to fortis-lsp to be active", 240, || Ok(lnd_channel_to(&lsp_id, true)?.map(|_| ())))?;
	// LND's wallet, not ours: by now it spends its own taproot change (Schnorr, no DER
	// signature), so its signatures are not checked; confirmed is enough.
	let info = confirmed_all(&lnd_funding, &[])?;
	record(env, "LND->fortis-lsp funding (LND wallet, not checked)", &info, None);
	poll("P2 funds", 60, || {
		p2.node().sync_wallets().map_err(|e| e.to_string())?;
		Ok((p2.onchain_sats() == P2_FUNDED_SAT).then_some(()))
	})?;
	poll("fortis-lsp funds", 240, || Ok((lsp_metric("fortis_lsp_onchain_sat")? == LSP_FUNDED_SAT).then_some(())))?;
	println!("  fortis-lsp on-chain {LSP_FUNDED_SAT} sat; P2 {}", p2.id);

	p2.node().connect(lsp_id, lsp_addr.clone(), true).map_err(|e| format!("P2 connect fortis-lsp: {e}"))?;
	let lnd_node = ldk_node::lightning::routing::gossip::NodeId::from_pubkey(&env.lnd_id);
	let lsp_node = ldk_node::lightning::routing::gossip::NodeId::from_pubkey(&lsp_id);
	poll("P2 to learn the LND-fortis-lsp channel by gossip", 240, || {
		let g = p2.node().network_graph();
		Ok(g.list_channels().into_iter().filter_map(|scid| g.channel(scid)).any(|c| {
			(c.node_one == lnd_node && c.node_two == lsp_node) || (c.node_one == lsp_node && c.node_two == lnd_node)
		})
		.then_some(()))
	})?;

	// 1. JIT channel: LND pays P2's invoice; fortis-lsp opens the channel, skims its fee.
	let amount_msat = 200_000_000;
	let inv = p2
		.node()
		.bolt11_payment()
		.receive_via_jit_channel(amount_msat, &desc("xr fortis-lsp jit"), 3600, None)
		.map_err(|e| format!("P2 receive_via_jit_channel: {e}"))?;
	lnd_pay_retry("fortis-lsp JIT invoice", &inv, 180)?;
	let got = p2.wait_received(hash_of(&inv), 60)?;
	let jit_fee = JIT_MIN_FEE_MSAT.max(amount_msat * JIT_FEE_PPM as u64 / 1_000_000);
	println!("  P2 received {got} msat of {amount_msat} (fortis-lsp fee {jit_fee} msat)");
	if got != amount_msat - jit_fee {
		return Err(format!("P2 received {got} msat, expected {} (amount minus the LSPS2 fee)", amount_msat - jit_fee));
	}
	let c = channel_with(p2, &lsp_id)?;
	let ucid = c.user_channel_id;
	let (funding, _) = funding_of(&c)?;
	println!("  JIT channel {} sat, funding {funding}", c.channel_value_sats);
	poll("fortis-lsp to broadcast the JIT funding", 60, || {
		Ok(btc(&["getrawtransaction", &funding]).is_ok().then_some(()))
	})?;
	env.mine(6)?;
	let info = confirmed_all(&funding, U)?;
	record(env, "fortis-lsp JIT channel funding (LSP wallet inputs)", &info, Some(Who::Lsp));

	// 2. P2 pays LND through fortis-lsp.
	let inv = lnd_invoice(20_000, "xr p2->lnd")?;
	let routing_fee_msat = send_retry(p2, "P2 pays LND 20,000 sat", 120, |n| n.bolt11_payment().send(&inv, None))?;

	// 3. Splice-in with fortis-lsp (it splices with every peer).
	let before = channel_by_id(p2, ucid)?;
	let old = funding_of(&before)?;
	p2.node().splice_in(&ucid, lsp_id, 20_000).map_err(|e| format!("P2 splice_in: {e}"))?;
	let txid = splice_and_lock(env, p2, ucid, "P2 splice-in with fortis-lsp")?;
	let exp = splice_expectation(&txid, &old)?;
	if exp.len() < 2 {
		return Err(format!("splice-in {txid} has no wallet input"));
	}
	let info = confirmed_with(&txid, &exp)?;
	record(env, "P2 splice-in with fortis-lsp (shared + P2 wallet)", &info, Some(Who::P2));
	let after = channel_by_id(p2, ucid)?;
	if after.channel_value_sats != before.channel_value_sats + 20_000 {
		return Err(format!("channel {} sat after splice-in, was {}", after.channel_value_sats, before.channel_value_sats));
	}

	// 4. Cooperative close (P2 asks; fortis-lsp opened the channel, so it pays the fee).
	let (fund, vout) = funding_of(&after)?;
	p2.node().close_channel(&ucid, lsp_id).map_err(|e| format!("P2 close: {e}"))?;
	let closing = env.wait_spender("P2-fortis-lsp cooperative close", &fund, vout, 1, 3)?;
	confirm(env, &closing)?;
	let info = confirmed_with(&closing, &[UU])?;
	record(env, "P2-fortis-lsp cooperative close (LSP opened: LSP pays)", &info, Some(Who::Lsp));
	env.mine(5)?;
	p2.wait_for("P2 channel closed", 60, |e| matches!(e, Event::ChannelClosed { user_channel_id, .. } if *user_channel_id == ucid))?;

	// On-chain accounting. P2: funded + received - sent (amount and routing fee) - its fees.
	let p2_expected = ((P2_FUNDED_SAT * 1000 + p2.received_msat.get() - p2.sent_msat.get()) / 1000) as i64
		- env.fees_of(Who::P2) as i64;
	let p2_actual = poll("P2's wallet to hold its close output", 60, || {
		p2.node().sync_wallets().map_err(|e| e.to_string())?;
		let a = p2.onchain_sats() as i64;
		Ok(((a - p2_expected).abs() <= 1).then_some(a))
	})
	.map_err(|e| format!("{e}: P2 on-chain {} sat, expected {p2_expected}", p2.onchain_sats()))?;
	println!(
		"  P2: on-chain {p2_actual} sat, expected {p2_expected} (funded {P2_FUNDED_SAT}, received {} msat, sent {} msat, fees {} sat)",
		p2.received_msat.get(), p2.sent_msat.get(), env.fees_of(Who::P2)
	);
	// fortis-lsp: funded + its JIT fee + P2's routing fee - its fees - what it still holds in
	// LND's channel (LND's remote balance: what it was paid, less what it forwarded on).
	let in_lnd_channel = lnd_channel_to(&lsp_id, false)?.ok_or("LND has no channel to fortis-lsp")?["remote_balance"]
		.as_str()
		.and_then(|v| v.parse::<u64>().ok())
		.ok_or("LND channel: no remote_balance")?;
	let lsp_expected = ((LSP_FUNDED_SAT * 1000 + jit_fee + routing_fee_msat) / 1000) as i64
		- in_lnd_channel as i64
		- env.fees_of(Who::Lsp) as i64;
	let mut lsp_actual = 0;
	poll("fortis-lsp's wallet to hold its close output", 240, || {
		lsp_actual = lsp_metric("fortis_lsp_onchain_sat")? as i64;
		Ok(((lsp_actual - lsp_expected).abs() <= 1).then_some(()))
	})
	.map_err(|e| format!("{e}: fortis-lsp on-chain {lsp_actual} sat, expected {lsp_expected}"))?;
	println!(
		"  fortis-lsp: on-chain {lsp_actual} sat, expected {lsp_expected} (funded {LSP_FUNDED_SAT}, JIT fee {jit_fee} msat, routing fee {routing_fee_msat} msat, {in_lnd_channel} sat in LND's channel, fees {} sat)",
		env.fees_of(Who::Lsp)
	);

	// fortis-lsp stays up for `app_node_xbt`, which stops it.
	p2.stop()?;
	Ok(Outcome::Pass)
}

// ------------------------------------------------------------------ 12

/// The app node's BIP-39 seed (fixed: the suite's own wallet, not a secret).
const APP_SEED: [u8; 64] = [0x5a; 64];
/// What LND pays the app node's JIT invoice, and what the app pays LND.
const APP_JIT_MSAT: u64 = 200_000_000;
const APP_PAY_SAT: u64 = 20_000;

/// The app's node (`fortis_lightning::Node`), stopped when dropped.
struct App(fortis_lightning::Node);

impl Drop for App {
	fn drop(&mut self) {
		self.0.stop();
	}
}

/// The node the phone runs: chain Xbt, the suite's Esplora (fortis-index), fortis-lsp as its
/// LSP (limits as `lsp_config_toml`), state in the VSS; `dir` holds only its local files.
fn app_node(lsp_id: &PublicKey, vss_url: &str, dir: &str) -> R<App> {
	use fortis_lightning::{ChainSource, LnChain, LspInfo, LspLimits, NodeConfig, VssConfig};
	let cfg = NodeConfig {
		bip39_seed: APP_SEED.to_vec(),
		network: ldk_node::bitcoin::Network::Regtest,
		data_dir: dir.into(),
		chain_source: ChainSource::Esplora { url: env::esplora_url(), auth_header: None },
		lsp: LspInfo { node_id: lsp_id.to_string(), host: "127.0.0.1".into(), port: env::cfg().lsp_port, token: None },
		vss: VssConfig { url: vss_url.to_string(), auth_header: String::new() },
		limits: LspLimits {
			min_payment_msat: 10_000_000,
			max_payment_msat: 500_000_000,
			channel_min_sat: 100_000,
			channel_max_sat: 500_000,
		},
		chain: LnChain::Xbt,
	};
	let node = fortis_lightning::Node::new(cfg).map_err(|e| format!("app Node::new: {e}"))?;
	println!("  app node {} (data {dir})", node.node_id());
	Ok(App(node))
}

/// Takes the app node's events (printing them) until one `f` accepts.
fn app_wait(app: &App, what: &str, secs: u64, f: impl Fn(&LnEvent) -> bool) -> R<LnEvent> {
	let t = std::time::Instant::now();
	while t.elapsed().as_secs() < secs {
		if let Some(e) = app.0.next_event(std::time::Duration::from_millis(500)) {
			println!("    app event: {e:?}");
			if f(&e) {
				return Ok(e);
			}
		}
	}
	Err(format!("app: timed out after {secs}s waiting for {what}"))
}

/// The app pays a new LND invoice of `sat` (retrying while the app learns the LSP-LND
/// channel by gossip); (payment id, routing fee msat).
fn app_pays_lnd(app: &App, sat: u64, secs: u64) -> R<(String, u64)> {
	let t = std::time::Instant::now();
	loop {
		let inv = lnd_invoice(sat, "xr app->lnd")?;
		let target = fortis_lightning::parse(&inv.to_string(), ldk_node::bitcoin::Network::Regtest)
			.map_err(|e| format!("parse LND invoice: {e}"))?;
		let res = match app.0.pay(&target, None) {
			Ok(id) => match app_wait(app, &format!("payment {id} to settle"), 60, |e| {
				matches!(e, LnEvent::PaymentSent { id: i, .. } | LnEvent::PaymentFailed { id: i, .. } if *i == id)
			})? {
				LnEvent::PaymentSent { fee_msat, .. } => Ok((id, fee_msat)),
				other => Err(format!("{other:?}")),
			},
			Err(e) => Err(format!("pay: {e:?}")),
		};
		match res {
			Ok(v) => {
				println!("  app paid LND {sat} sat (routing fee {} msat) after {:?}", v.1, t.elapsed());
				return Ok(v);
			},
			Err(e) if t.elapsed().as_secs() < secs => {
				println!("  app pays LND: {e}, retrying with a new invoice");
				sleep_ms(5000);
			},
			Err(e) => return Err(format!("app pays LND: {e}")),
		}
	}
}

/// A BOLT 11 invoice as a BTC node makes it: regtest, no `option_blake2b` (bit 512), signed by
/// a key of its own (no node of this chain could be paid with it).
fn btc_style_invoice(amount_msat: u64) -> R<Bolt11Invoice> {
	use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};
	use ldk_node::lightning_invoice::{Currency, InvoiceBuilder};
	use ldk_node::lightning_types::payment::PaymentSecret;
	let secp = Secp256k1::new();
	let key = SecretKey::from_slice(&sha256::Hash::hash(&preimage(31)).to_byte_array()).map_err(|e| e.to_string())?;
	let inv = InvoiceBuilder::new(Currency::Regtest)
		.description("xr btc-style".into())
		.payment_hash(sha256::Hash::hash(&preimage(32)))
		.payment_secret(PaymentSecret(preimage(33)))
		.amount_milli_satoshis(amount_msat)
		.current_timestamp()
		.min_final_cltv_expiry_delta(18)
		.build_signed(|h| secp.sign_ecdsa_recoverable(h, &key))
		.map_err(|e| format!("build BTC-style invoice: {e:?}"))?;
	if inv.features().map_or(false, |f| f.supports_blake2b()) {
		return Err("the BTC-style invoice carries option_blake2b".into());
	}
	Ok(inv)
}

/// The JIT channel's funding: the one new transaction in the mempool (not in `before`)
/// with a P2WSH output; (txid, vout).
fn new_funding_in_mempool(before: &[String]) -> R<Option<(String, u32)>> {
	let mempool: Vec<String> = serde_json::from_value(env::btc_json(&["getrawmempool"])?).map_err(|e| e.to_string())?;
	let mut found = vec![];
	for t in mempool.iter().filter(|t| !before.contains(t)) {
		let tx = env::btc_json(&["getrawtransaction", t, "true"])?;
		for o in tx["vout"].as_array().into_iter().flatten() {
			if o["scriptPubKey"]["type"] == "witness_v0_scripthash" {
				found.push((t.clone(), o["n"].as_u64().unwrap_or(0) as u32));
			}
		}
	}
	match found.len() {
		0 => Ok(None),
		1 => Ok(found.pop()),
		_ => Err(format!("more than one new P2WSH output in the mempool: {found:?}")),
	}
}

/// The app's node layer on XBT: `fortis_lightning::Node` (chain Xbt, the code the phone runs)
/// with its state in a VSS (LDK vss-server on its own PostgreSQL), against the running
/// fortis-lsp (chain = "xbt") of `lsp_xbt_jit`.
///
/// 1. It starts (LSP reachable, device lock and state in VSS).
/// 2. A JIT receive paid by LND: fortis-lsp opens the channel ([0x21] on its wallet inputs);
///    the app gets the amount less the quoted fee.
/// 3. It pays an LND invoice through fortis-lsp.
/// 4. It refuses a BTC-style invoice (no bit 512) with `LnError::WrongChain`, and no
///    payment is recorded.
/// 5. Stopped and built again from the same seed on a new data dir (a new phone): from VSS,
///    the balances and the payments are the same.
/// 6. It closes cooperatively ([0x21, 0x21]; fortis-lsp opened the channel, so it pays the
///    fee); its close output is what it received less what it sent, and its transit wallet
///    holds it. Then fortis-lsp and the VSS are stopped.
fn app_node_xbt(env: &Env) -> R<Outcome> {
	let (lsp_id, lsp_addr) = {
		let lsp = env.lsp.borrow();
		let lsp = lsp.as_ref().ok_or("fortis-lsp is not running (lsp_xbt_jit)")?;
		(lsp.id, lsp.addr.clone())
	};
	let vss = crate::vss::VssProcess::start()?;
	let vss_url = vss.url.clone();
	*env.vss.borrow_mut() = Some(vss);
	let xr = env::cfg().xr.clone();

	// 1. Start.
	let app = app_node(&lsp_id, &vss_url, &format!("{xr}/app-a"))?;
	app.0.start().map_err(|e| format!("app start: {e:?}"))?;
	println!("  app node started, LSP {lsp_id} at {lsp_addr}");

	// 2. JIT receive, paid by LND.
	let quote = app.0.receive(APP_JIT_MSAT, "xr app jit").map_err(|e| format!("app receive: {e:?}"))?;
	let jit_fee = JIT_MIN_FEE_MSAT.max(APP_JIT_MSAT * JIT_FEE_PPM as u64 / 1_000_000);
	println!("  app JIT quote: fee {} msat, needs channel {}", quote.fee_msat, quote.needs_channel);
	if !quote.needs_channel || quote.fee_msat != jit_fee {
		return Err(format!("app JIT quote {quote:?}, expected a channel and fee {jit_fee} msat"));
	}
	let inv = Bolt11Invoice::from_str(&quote.invoice).map_err(|e| format!("app invoice: {e:?}"))?;
	if !inv.features().map_or(false, |f| f.supports_blake2b()) {
		return Err("the app's invoice lacks option_blake2b (bit 512)".into());
	}
	let mempool_before: Vec<String> =
		serde_json::from_value(env::btc_json(&["getrawmempool"])?).map_err(|e| e.to_string())?;
	lnd_pay_retry("app JIT invoice", &inv, 180)?;
	let received_msat = match app_wait(&app, "the JIT payment", 60, |e| matches!(e, LnEvent::PaymentReceived { .. }))? {
		LnEvent::PaymentReceived { amount_msat, .. } => amount_msat,
		_ => unreachable!(),
	};
	println!("  app received {received_msat} msat of {APP_JIT_MSAT} (fortis-lsp fee {jit_fee} msat)");
	if received_msat != APP_JIT_MSAT - jit_fee {
		return Err(format!("app received {received_msat} msat, expected {}", APP_JIT_MSAT - jit_fee));
	}
	let (funding, funding_vout) =
		poll("fortis-lsp to broadcast the app's JIT funding", 60, || new_funding_in_mempool(&mempool_before))?;
	println!("  app JIT channel funding {funding}:{funding_vout}");
	env.mine(6)?;
	let info = confirmed_all(&funding, U)?;
	record(env, "app JIT channel funding (LSP wallet inputs)", &info, Some(Who::Lsp));
	let channel_sat = info.outputs[funding_vout as usize].0;

	// 3. Pay an LND invoice.
	let (paid_id, routing_fee_msat) = app_pays_lnd(&app, APP_PAY_SAT, 240)?;
	let sent_msat = APP_PAY_SAT * 1000 + routing_fee_msat;

	// 4. A BTC-style invoice is refused before any HTLC.
	let payments_before = app.0.payments();
	let btc_inv = btc_style_invoice(10_000_000)?;
	let target = fortis_lightning::parse(&btc_inv.to_string(), ldk_node::bitcoin::Network::Regtest)
		.map_err(|e| format!("parse BTC-style invoice: {e}"))?;
	match app.0.pay(&target, None) {
		Err(fortis_lightning::LnError::WrongChain) => println!("  BTC-style invoice: refused, WrongChain"),
		other => return Err(format!("paying a BTC-style invoice: {other:?}, expected WrongChain")),
	}
	let btc_id = hex(&btc_inv.payment_hash().to_byte_array());
	let payments_after = app.0.payments();
	if payments_after.len() != payments_before.len() || payments_after.iter().any(|p| p.id == btc_id) {
		return Err(format!("a payment was recorded for the BTC-style invoice: {payments_after:?}"));
	}
	if app.0.next_event(std::time::Duration::from_secs(2)).is_some_and(|e| matches!(e, LnEvent::PaymentFailed { .. } | LnEvent::PaymentSent { .. })) {
		return Err("the BTC-style invoice produced a payment event".into());
	}

	// 5. Stop; a new node from the same seed and VSS, on a fresh data dir.
	let before = app.0.balances();
	println!("  app balances before stop: {before:?}");
	if before.lightning_sat == 0 {
		return Err(format!("app has no Lightning balance: {before:?}"));
	}
	drop(app);
	let app = app_node(&lsp_id, &vss_url, &format!("{xr}/app-b"))?;
	app.0.start().map_err(|e| format!("app restart from VSS: {e:?}"))?;
	let after = poll("the restarted app's balances to match", 240, || {
		let b = app.0.balances();
		Ok((b.lightning_sat == before.lightning_sat && b.onchain_transit_sat == before.onchain_transit_sat).then_some(b))
	})
	.map_err(|e| format!("{e}: before {before:?}, after {:?}", app.0.balances()))?;
	println!("  app balances after restart from VSS: {after:?}");
	let ids: Vec<String> = app.0.payments().into_iter().map(|p| p.id).collect();
	if !ids.contains(&paid_id) || ids.len() != payments_after.len() {
		return Err(format!("payments after restart {ids:?}, expected {} including {paid_id}", payments_after.len()));
	}

	// 6. Cooperative close.
	poll("the restarted app's channel to be ready", 120, || {
		Ok((app.0.channel_state() == fortis_lightning::ChannelState::Ready).then_some(()))
	})?;
	poll("the app to close cooperatively", 60, || match app.0.close_channel() {
		Ok(()) => Ok(Some(())),
		Err(e) => {
			println!("  app close: {e:?}, retrying");
			Ok(None)
		},
	})?;
	let closing = env.wait_spender("app-fortis-lsp cooperative close", &funding, funding_vout, 1, 3)?;
	confirm(env, &closing)?;
	let info = confirmed_with(&closing, &[UU])?;
	record(env, "app-fortis-lsp cooperative close (LSP opened: LSP pays)", &info, Some(Who::Lsp));
	env.mine(5)?;
	app_wait(&app, "the app's channel closed", 120, |e| matches!(e, LnEvent::ChannelClosed { .. }))?;
	let expected_sat = (received_msat - sent_msat) / 1000;
	let out = check::output_with_value(&info, expected_sat)
		.map_err(|e| format!("app close output (received {received_msat} - sent {sent_msat} msat): {e}"))?;
	println!("  app close output {closing}:{out} = {expected_sat} sat (channel {channel_sat} sat)");
	let transit = poll("the app's transit wallet to hold its close output", 240, || {
		let b = app.0.balances();
		Ok((b.onchain_transit_sat == expected_sat && b.lightning_sat == 0).then_some(b))
	})
	.map_err(|e| format!("{e}: app balances {:?}, expected {expected_sat} sat on chain", app.0.balances()))?;
	println!("  app balances after close: {transit:?}");

	drop(app);
	if let Some(mut lsp) = env.lsp.borrow_mut().take() {
		lsp.stop();
	}
	if let Some(mut vss) = env.vss.borrow_mut().take() {
		vss.stop();
	}
	Ok(Outcome::Pass)
}

/// LND's channel to `peer` (only an active one if `active`), as `lncli listchannels` reports it.
fn lnd_channel_to(peer: &PublicKey, active: bool) -> R<Option<serde_json::Value>> {
	let args: &[&str] = if active { &["listchannels", "--active_only"] } else { &["listchannels"] };
	let v = ln(args)?;
	Ok(v["channels"].as_array().into_iter().flatten().find(|c| c["remote_pubkey"] == peer.to_string()).cloned())
}

fn short_balance(b: &LightningBalance) -> String {
	let s = format!("{b:?}");
	s.split(" {").next().unwrap_or(&s).to_string()
}
