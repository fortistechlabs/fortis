// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! FORTIS XBT: tests for the XBT (BLAKE2b chain) feature bits, channel types and the unified
//! signatures (hash type 0x21) of channels whose type has `option_unified_sigs`.

use crate::ln::channelmanager::MIN_CLTV_EXPIRY_DELTA;
use crate::ln::functional_test_utils::*;
use crate::ln::msgs::{
	BaseMessageHandler, ChannelMessageHandler, MessageSendEvent, OnionMessageHandler,
};
use crate::types::features::ChannelTypeFeatures;
use crate::util::config::{UserConfig, XbtConfig};

use crate::prelude::*;

/// A test config for an XBT node. Anchors are asked for so that the tests show XBT drops them.
fn xbt_cfg() -> UserConfig {
	let mut cfg = test_default_channel_config();
	cfg.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = true;
	cfg.channel_handshake_config.negotiate_anchor_zero_fee_commitments = true;
	cfg.xbt = Some(XbtConfig::default());
	cfg
}

#[test]
fn xbt_config_defaults() {
	assert_eq!(UserConfig::default().xbt, None);
	assert!(!UserConfig::default().is_xbt());
	assert!(xbt_cfg().is_xbt());
	let x = XbtConfig::default();
	assert_eq!(x.gossip_floor_height, 961640);
	assert!(x.splice_peers.is_empty());
}

#[test]
fn xbt_init_features_set_bits() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let init = nodes[0].node.init_features();
	assert!(init.requires_blake2b());
	assert!(init.supports_unified_sigs());
	assert!(!init.requires_unified_sigs());
	assert!(!init.supports_anchors_zero_fee_htlc_tx());
	assert!(!init.supports_anchor_zero_fee_commitments());

	let node = nodes[0].node.node_features();
	assert!(node.requires_blake2b());
	assert!(node.supports_unified_sigs());
	assert!(!node.supports_anchors_zero_fee_htlc_tx());
}

#[test]
fn xbt_channel_type_unified_no_anchors() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	create_announced_chan_between_nodes(&nodes, 0, 1);

	for node in nodes.iter() {
		let chans = node.node.list_channels();
		assert_eq!(chans.len(), 1);
		let ty = chans[0].channel_type.as_ref().unwrap();
		assert!(ty.requires_unified_sigs());
		assert!(!ty.supports_anchors_zero_fee_htlc_tx());
		assert!(!ty.supports_anchors_nonzero_fee_htlc_tx());
		assert!(!ty.supports_anchor_zero_fee_commitments());
	}
}

/// Opens a channel from node 0 to node 1 (both XBT), sets the `channel_type`
/// in the `open_channel` (set to `ty`), and returns the error node 1 answers with.
fn open_with_channel_type(ty: ChannelTypeFeatures) -> String {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let node_a_id = nodes[0].node.get_our_node_id();
	let node_b_id = nodes[1].node.get_our_node_id();

	nodes[0].node.create_channel(node_b_id, 100_000, 10_001, 42, None, None).unwrap();
	let mut open = get_event_msg!(nodes[0], MessageSendEvent::SendOpenChannel, node_b_id);
	open.common_fields.channel_type = Some(ty);
	nodes[1].node.handle_open_channel(node_a_id, &open);
	get_err_msg(&nodes[1], &node_a_id).data
}

#[test]
fn xbt_refuses_open_without_unified() {
	let err = open_with_channel_type(ChannelTypeFeatures::only_static_remote_key());
	assert!(err.contains("XBT"), "unexpected error: {err}");
}

#[test]
fn xbt_refuses_open_with_anchors() {
	let mut ty = ChannelTypeFeatures::only_static_remote_key();
	ty.set_unified_sigs_required();
	ty.set_anchors_zero_fee_htlc_tx_required();
	let err = open_with_channel_type(ty);
	assert!(err.contains("XBT"), "unexpected error: {err}");

	let mut ty = ChannelTypeFeatures::empty();
	ty.set_unified_sigs_required();
	ty.set_anchor_zero_fee_commitments_required();
	let err = open_with_channel_type(ty);
	assert!(err.contains("XBT"), "unexpected error: {err}");
}

#[test]
fn btc_node_has_no_xbt_bits() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let init = nodes[0].node.init_features();
	assert!(!init.supports_blake2b());
	assert!(!init.supports_unified_sigs());
	assert!(!nodes[0].node.node_features().supports_blake2b());
	assert!(!nodes[0].node.bolt11_invoice_features().supports_blake2b());

	create_announced_chan_between_nodes(&nodes, 0, 1);
	let chans = nodes[0].node.list_channels();
	let ty = chans[0].channel_type.as_ref().unwrap();
	assert!(!ty.supports_unified_sigs());
}

#[test]
fn xbt_invoice_and_offer_carry_blake2b() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	create_announced_chan_between_nodes(&nodes, 0, 1);

	use crate::ln::channelmanager::Bolt11InvoiceParameters;
	let invoice = nodes[0]
		.node
		.create_bolt11_invoice(Bolt11InvoiceParameters {
			amount_msats: Some(10_000_000),
			min_final_cltv_expiry_delta: Some(MIN_CLTV_EXPIRY_DELTA),
			..Default::default()
		})
		.unwrap();
	assert!(invoice.features().unwrap().requires_blake2b());
	assert!(nodes[0].node.bolt11_invoice_features().requires_blake2b());

	let offer = nodes[0].node.create_offer_builder().unwrap().build().unwrap();
	assert!(offer.offer_features().requires_blake2b());

	let refund = nodes[0]
		.node
		.create_refund_builder(
			10_000_000,
			core::time::Duration::from_secs(1_000_000),
			crate::ln::channelmanager::PaymentId([1; 32]),
			crate::ln::outbound_payment::Retry::Attempts(0),
			crate::routing::router::RouteParametersConfig::default(),
		)
		.unwrap()
		.build()
		.unwrap();
	assert!(refund.features().requires_blake2b());

	// A BTC node's offer has no bit 512.
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	create_announced_chan_between_nodes(&nodes, 0, 1);
	let offer = nodes[0].node.create_offer_builder().unwrap().build().unwrap();
	assert!(!offer.offer_features().supports_blake2b());
}

/// Full offer -> invoice_request -> invoice -> payment between two nodes configured by `cfg`.
/// Returns the invoice Bob paid.
fn pay_offer(cfg: Option<UserConfig>) -> crate::offers::invoice::Bolt12Invoice {
	use super::offers_tests::{claim_bolt12_payment, extract_invoice, route_bolt12_payment};
	use crate::blinded_path::payment::{Bolt12OfferContext, PaymentContext};
	use crate::ln::channelmanager::PaymentId;
	use crate::offers::invoice_request::InvoiceRequestFields;

	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[cfg.clone(), cfg]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 10_000_000, 1_000_000_000);

	let alice = &nodes[0];
	let alice_id = alice.node.get_our_node_id();
	let bob = &nodes[1];
	let bob_id = bob.node.get_our_node_id();

	let offer =
		alice.node.create_offer_builder().unwrap().clear_paths().amount_msats(10_000_000).build().unwrap();
	let payment_id = PaymentId([1; 32]);
	bob.node.pay_for_offer(&offer, None, payment_id, Default::default()).unwrap();

	let onion_message = bob.onion_messenger.next_onion_message_for_peer(alice_id).unwrap();
	alice.onion_messenger.handle_onion_message(bob_id, &onion_message);
	let (invoice_request, _) = super::offers_tests::extract_invoice_request(alice, &onion_message);
	let _ = &invoice_request;
	let payment_context = PaymentContext::Bolt12Offer(Bolt12OfferContext {
		offer_id: offer.id(),
		invoice_request: InvoiceRequestFields {
			payer_signing_pubkey: invoice_request.payer_signing_pubkey(),
			quantity: None,
			payer_note_truncated: None,
			human_readable_name: None,
		},
	});

	let onion_message = alice.onion_messenger.next_onion_message_for_peer(bob_id).unwrap();
	bob.onion_messenger.handle_onion_message(alice_id, &onion_message);
	let (invoice, _) = extract_invoice(bob, &onion_message);
	route_bolt12_payment(bob, &[alice], &invoice);
	claim_bolt12_payment(bob, &[alice], payment_context, &invoice);
	invoice
}

#[test]
fn xbt_bolt12_offer_to_invoice_payment() {
	let invoice = pay_offer(Some(xbt_cfg()));
	assert!(invoice.invoice_features().requires_blake2b());
}

#[test]
fn btc_bolt12_offer_to_invoice_payment() {
	let invoice = pay_offer(None);
	assert!(!invoice.invoice_features().supports_blake2b());
}

#[test]
fn xbt_bolt12_refund_invoice_carries_blake2b() {
	use crate::ln::channelmanager::PaymentId;
	use crate::ln::outbound_payment::Retry;
	use crate::routing::router::RouteParametersConfig;

	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 10_000_000, 1_000_000_000);

	let refund = nodes[0]
		.node
		.create_refund_builder(
			10_000_000,
			nodes[0].node.duration_since_epoch() + core::time::Duration::from_secs(3600),
			PaymentId([1; 32]),
			Retry::Attempts(0),
			RouteParametersConfig::default(),
		)
		.unwrap()
		.clear_paths()
		.build()
		.unwrap();
	let invoice = nodes[1].node.request_refund_payment(&refund).unwrap();
	assert!(invoice.invoice_features().requires_blake2b());
}

/// The static invoice served for an async offer. The async flow tests are `ignore`d with `std`
/// upstream (they depend on mock time); this one runs the same setup and check.
#[test]
fn xbt_static_invoice_carries_blake2b() {
	use super::async_payments_tests::{
		expect_offer_paths_requests, pass_static_invoice_server_messages,
	};
	let chanmon_cfgs = create_chanmon_cfgs(3);
	let node_cfgs = create_node_cfgs(3, &chanmon_cfgs);
	let mut cfg = xbt_cfg();
	cfg.accept_forwards_to_priv_channels = true;
	let node_chanmgrs =
		create_node_chanmgrs(3, &node_cfgs, &[Some(xbt_cfg()), Some(cfg), Some(xbt_cfg())]);
	let nodes = create_network(3, &node_cfgs, &node_chanmgrs);
	create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 1_000_000, 0);
	create_unannounced_chan_between_nodes_with_value(&nodes, 1, 2, 1_000_000, 0);

	let recipient_id = vec![42; 32];
	let inv_server_paths =
		nodes[1].node.blinded_paths_for_async_recipient(recipient_id.clone(), None).unwrap();
	nodes[2].node.set_paths_to_static_invoice_server(inv_server_paths).unwrap();
	expect_offer_paths_requests(&nodes[2], &[&nodes[0], &nodes[1]]);
	let res = pass_static_invoice_server_messages(&nodes[1], &nodes[2], recipient_id);
	assert!(res.invoice.invoice_features().requires_blake2b());
}

// ---------------------------------------------------------------------------
// Task 4: per-channel unified signatures.
//
// LDK's `check_spends!` (and the helpers that call it, such as `close_channel` and
// `test_txn_broadcast`) checks scripts with libbitcoinconsensus, which knows only BIP 143. The
// XBT tests below check signatures themselves with `assert_sig_bytes` instead.

use crate::chain::channelmonitor::{CLTV_CLAIM_BUFFER, LATENCY_GRACE_PERIOD_BLOCKS};
use crate::events::ClosureReason;
use crate::ln::types::ChannelId;
use crate::util::ser::Writeable;
use bitcoin::script::Instruction;
use bitcoin::secp256k1::{ecdsa::Signature, PublicKey, Secp256k1};
use bitcoin::sighash::EcdsaSighashType;
use bitcoin::{ScriptBuf, Transaction};

/// Asserts that the signatures in input `input` of `tx` carry the hash type bytes `expected`, in
/// witness order, and that each one is valid for the digest its byte declares: the unified digest
/// when the byte has `0x20`, BIP 143 otherwise.
///
/// The input must be a 2-of-2 spend, so the witness is `[<>, sig, sig, .., script]`: the funding
/// output (`[<>, sig_a, sig_b, funding_script]`) or an HTLC output
/// (`[<>, remote_sig, local_sig, preimage-or-empty, htlc_script]`). In both scripts the 33-byte
/// pushes are the public keys in the order the signatures are checked. `spent` is the transaction
/// whose output the input spends.
fn assert_sig_bytes(tx: &Transaction, input: usize, spent: &Transaction, expected: &[u8]) {
	let txin = &tx.input[input];
	let prevout = &spent.output[txin.previous_output.vout as usize];
	assert_eq!(txin.previous_output.txid, spent.compute_txid());
	let witness: Vec<&[u8]> = txin.witness.iter().collect();
	assert!(witness.len() >= 4, "not a 2-of-2 witness: {} elements", witness.len());
	assert!(witness[0].is_empty(), "multisig dummy must be empty");
	let script = ScriptBuf::from(witness.last().unwrap().to_vec());
	assert_eq!(prevout.script_pubkey, script.to_p2wsh(), "witness script does not match prevout");
	let pubkeys: Vec<PublicKey> = script
		.instructions()
		.filter_map(|i| match i.unwrap() {
			Instruction::PushBytes(b) if b.len() == 33 => Some(PublicKey::from_slice(b.as_bytes()).unwrap()),
			_ => None,
		})
		.collect();
	assert_eq!(pubkeys.len(), 2, "expected two keys in the witness script");
	assert_eq!(expected.len(), 2);

	let secp = Secp256k1::verification_only();
	for (i, want) in expected.iter().enumerate() {
		let raw = witness[1 + i];
		let got = *raw.last().unwrap();
		assert_eq!(got, *want, "input {input} signature {i}: hash type {got:#04x}, want {want:#04x}");
		let sig = Signature::from_der(&raw[..raw.len() - 1]).unwrap();
		let unified = want & crate::xbt::SIGHASH_UNIFIED != 0;
		let base = EcdsaSighashType::from_consensus((want & !crate::xbt::SIGHASH_UNIFIED) as u32);
		let msg = crate::xbt::p2wsh_sighash(tx, input, &script, prevout.value, base, unified);
		secp.verify_ecdsa(&msg, &sig, &pubkeys[i])
			.unwrap_or_else(|_| panic!("input {input} signature {i} ({want:#04x}) does not verify"));
	}
}

/// Takes the transactions `node` broadcast, deduplicated, in order.
fn take_broadcast(node: &Node) -> Vec<Transaction> {
	let mut txn = node.tx_broadcaster.txn_broadcast();
	let mut seen = new_hash_set();
	txn.retain(|tx| seen.insert(tx.compute_txid()));
	txn
}

/// The one transaction in `txn` that spends an output of `parent`.
fn spending(txn: &[Transaction], parent: &Transaction) -> Transaction {
	let txid = parent.compute_txid();
	let found: Vec<&Transaction> =
		txn.iter().filter(|tx| tx.input.iter().any(|i| i.previous_output.txid == txid)).collect();
	assert_eq!(found.len(), 1, "expected one spend of {txid}, found {}", found.len());
	found[0].clone()
}

fn xbt_pair_with_pending_htlc<'a, 'b, 'c: 'd, 'd>(
	nodes: &'a Vec<Node<'b, 'c, 'd>>,
) -> ((crate::ln::msgs::ChannelUpdate, crate::ln::msgs::ChannelUpdate, ChannelId, Transaction),
	crate::types::payment::PaymentPreimage, crate::types::payment::PaymentHash) {
	let chan = create_announced_chan_between_nodes(nodes, 0, 1);
	assert!(nodes[0].node.list_channels()[0].channel_type.as_ref().unwrap().requires_unified_sigs());
	let (preimage, hash, ..) = route_payment(&nodes[0], &[&nodes[1]], 3_000_000);
	(chan, preimage, hash)
}

#[test]
fn xbt_holder_commitment_signed_unified() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let (chan, ..) = xbt_pair_with_pending_htlc(&nodes);

	for (i, node) in nodes.iter().enumerate() {
		let txn = get_local_commitment_txn!(node, chan.2);
		// The sender also gets its signed HTLC-timeout; the receiver has no preimage yet.
		assert_eq!(txn.len(), if i == 0 { 2 } else { 1 });
		assert_sig_bytes(&txn[0], 0, &chan.3, &[0x21, 0x21]);
		for htlc_tx in &txn[1..] {
			assert_sig_bytes(htlc_tx, 0, &txn[0], &[0x21, 0x21]);
		}
	}
}

#[test]
fn xbt_htlc_timeout_signed_unified() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let node_b_id = nodes[1].node.get_our_node_id();
	let (chan, _, hash) = xbt_pair_with_pending_htlc(&nodes);

	// Past the HTLC's expiry the sender force-closes and times the HTLC out on chain.
	connect_blocks(&nodes[0], TEST_FINAL_CLTV + LATENCY_GRACE_PERIOD_BLOCKS + 1);
	check_closed_broadcast(&nodes[0], 1, true);
	check_added_monitors(&nodes[0], 1);
	let reason = ClosureReason::HTLCsTimedOut { payment_hash: Some(hash) };
	check_closed_event(&nodes[0], 1, reason, false, &[node_b_id], 100_000);

	let txn = take_broadcast(&nodes[0]);
	let commitment = spending(&txn, &chan.3);
	assert_sig_bytes(&commitment, 0, &chan.3, &[0x21, 0x21]);
	let htlc_timeout = spending(&txn, &commitment);
	assert_ne!(htlc_timeout.lock_time, bitcoin::absolute::LockTime::ZERO, "not an HTLC-timeout");
	assert_sig_bytes(&htlc_timeout, 0, &commitment, &[0x21, 0x21]);
}

#[test]
fn xbt_htlc_success_signed_unified() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let node_a_id = nodes[0].node.get_our_node_id();
	let (chan, preimage, hash) = xbt_pair_with_pending_htlc(&nodes);

	// The receiver learns the preimage but the sender never hears of it, so near expiry the
	// receiver goes on chain with an HTLC-success.
	nodes[1].node.claim_funds(preimage);
	expect_payment_claimed!(nodes[1], hash, 3_000_000);
	check_added_monitors(&nodes[1], 1);
	let _ = get_htlc_update_msgs(&nodes[1], &node_a_id);

	connect_blocks(&nodes[1], TEST_FINAL_CLTV - CLTV_CLAIM_BUFFER + 2);
	check_closed_broadcast(&nodes[1], 1, true);
	check_added_monitors(&nodes[1], 1);
	let reason = ClosureReason::HTLCsTimedOut { payment_hash: Some(hash) };
	check_closed_event(&nodes[1], 1, reason, false, &[node_a_id], 100_000);

	let txn = take_broadcast(&nodes[1]);
	let commitment = spending(&txn, &chan.3);
	assert_sig_bytes(&commitment, 0, &chan.3, &[0x21, 0x21]);
	let htlc_success = spending(&txn, &commitment);
	assert_eq!(htlc_success.lock_time, bitcoin::absolute::LockTime::ZERO, "not an HTLC-success");
	assert_eq!(htlc_success.input[0].witness.nth(3).unwrap(), &preimage.0[..]);
	assert_sig_bytes(&htlc_success, 0, &commitment, &[0x21, 0x21]);
}

/// A cooperative close started by node 0, as `close_channel` does it but without its
/// libbitcoinconsensus check. Returns the closing transaction both nodes broadcast.
fn coop_close(nodes: &Vec<Node>, a: usize, b: usize, channel_id: &ChannelId) -> Transaction {
	let a_id = nodes[a].node.get_our_node_id();
	let b_id = nodes[b].node.get_our_node_id();
	nodes[a].node.close_channel(channel_id, &b_id).unwrap();
	let shutdown_a = get_event_msg!(nodes[a], MessageSendEvent::SendShutdown, b_id);
	nodes[b].node.handle_shutdown(a_id, &shutdown_a);
	let shutdown_b = get_event_msg!(nodes[b], MessageSendEvent::SendShutdown, a_id);
	nodes[a].node.handle_shutdown(b_id, &shutdown_b);
	let closing_a = get_event_msg!(nodes[a], MessageSendEvent::SendClosingSigned, b_id);
	nodes[b].node.handle_closing_signed(a_id, &closing_a);
	let closing_b = get_event_msg!(nodes[b], MessageSendEvent::SendClosingSigned, a_id);
	nodes[a].node.handle_closing_signed(b_id, &closing_b);
	let tx_a = take_broadcast(&nodes[a]);
	let events = nodes[a].node.get_and_clear_pending_msg_events();
	let closing_a = events
		.iter()
		.find_map(|e| match e {
			MessageSendEvent::SendClosingSigned { msg, .. } => Some(msg.clone()),
			_ => None,
		})
		.unwrap();
	nodes[b].node.handle_closing_signed(a_id, &closing_a);
	let tx_b = take_broadcast(&nodes[b]);
	let _ = nodes[b].node.get_and_clear_pending_msg_events();
	assert_eq!(tx_a.len(), 1);
	assert_eq!(tx_a, tx_b);
	tx_a[0].clone()
}

#[test]
fn xbt_coop_close_signed_unified() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let chan = create_announced_chan_between_nodes(&nodes, 0, 1);
	send_payment(&nodes[0], &[&nodes[1]], 5_000_000);

	let closing_tx = coop_close(&nodes, 0, 1, &chan.2);
	assert_sig_bytes(&closing_tx, 0, &chan.3, &[0x21, 0x21]);
	for (node, peer) in [(0, 1), (1, 0)] {
		let peer_id = nodes[peer].node.get_our_node_id();
		let reason = if node == 0 {
			ClosureReason::LocallyInitiatedCooperativeClosure
		} else {
			ClosureReason::CounterpartyInitiatedCooperativeClosure
		};
		check_closed_event(&nodes[node], 1, reason, false, &[peer_id], 100_000);
	}
}

#[test]
fn btc_and_xbt_nodes_same_process() {
	// Review Focus 1: the chain is chosen per node, so a BTC pair and an XBT pair in one process
	// each sign as their channel type says.
	let chanmon_cfgs = create_chanmon_cfgs(4);
	let node_cfgs = create_node_cfgs(4, &chanmon_cfgs);
	let node_chanmgrs = create_node_chanmgrs(
		4,
		&node_cfgs,
		&[None, None, Some(xbt_cfg()), Some(xbt_cfg())],
	);
	let nodes = create_network(4, &node_cfgs, &node_chanmgrs);
	let btc = create_announced_chan_between_nodes(&nodes, 0, 1);
	let xbt = create_announced_chan_between_nodes(&nodes, 2, 3);
	route_payment(&nodes[0], &[&nodes[1]], 3_000_000);
	route_payment(&nodes[2], &[&nodes[3]], 3_000_000);

	for (i, chan, want) in [(0, &btc, [0x01, 0x01]), (1, &btc, [0x01, 0x01]), (2, &xbt, [0x21, 0x21]), (3, &xbt, [0x21, 0x21])] {
		let txn = get_local_commitment_txn!(nodes[i], chan.2);
		assert_sig_bytes(&txn[0], 0, &chan.3, &want);
		for htlc_tx in &txn[1..] {
			assert_sig_bytes(htlc_tx, 0, &txn[0], &want);
		}
	}
	// The BTC pair's commitment also passes libbitcoinconsensus.
	let txn = get_local_commitment_txn!(nodes[0], btc.2);
	check_spends!(txn[0], btc.3);
}

#[test]
fn xbt_reloaded_node_still_unified() {
	// Review Focus 2: after a restart the signing mode comes from the channel type stored with the
	// channel and its monitor. (A node reloaded with a BTC config cannot read the channel at all:
	// `option_unified_sigs` is an unknown required feature to it, `UnknownRequiredFeature`.)
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let persister;
	let new_chain_monitor;
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let node_0_reloaded;
	let mut nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let node_b_id = nodes[1].node.get_our_node_id();
	let chan = create_announced_chan_between_nodes(&nodes, 0, 1);
	send_payment(&nodes[0], &[&nodes[1]], 5_000_000);

	nodes[1].node.peer_disconnected(nodes[0].node.get_our_node_id());
	let monitor = get_monitor!(nodes[0], chan.2).encode();
	reload_node!(
		nodes[0],
		xbt_cfg(),
		&nodes[0].node.encode(),
		&[&monitor],
		persister,
		new_chain_monitor,
		node_0_reloaded
	);

	let message = "Channel force-closed".to_owned();
	nodes[0].node.force_close_broadcasting_latest_txn(&chan.2, &node_b_id, message.clone()).unwrap();
	check_closed_broadcast(&nodes[0], 1, false); // the peer is disconnected
	check_added_monitors(&nodes[0], 1);
	let reason = ClosureReason::HolderForceClosed { broadcasted_latest_txn: Some(true), message };
	check_closed_event(&nodes[0], 1, reason, false, &[node_b_id], 100_000);

	let txn = take_broadcast(&nodes[0]);
	let commitment = spending(&txn, &chan.3);
	assert_sig_bytes(&commitment, 0, &chan.3, &[0x21, 0x21]);
}
