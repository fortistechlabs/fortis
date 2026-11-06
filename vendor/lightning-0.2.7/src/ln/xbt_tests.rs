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

/// A BOLT 12 invoice that lacks `option_blake2b`, answering an XBT node's request, is abandoned
/// before any HTLC: XBT shares BTC's chain hash, so the bit is all that tells the chains apart.
#[test]
fn xbt_abandons_bolt12_invoice_without_blake2b() {
	use crate::events::{Event, PaymentFailureReason};
	use crate::ln::channelmanager::PaymentId;
	use crate::offers::offer::OfferBuilder;
	use crate::offers::test_utils::payment_paths;
	use crate::onion_message::messenger::{Destination, MessageSendInstructions};

	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 10_000_000, 1_000_000_000);

	let alice = &nodes[0];
	let alice_id = alice.node.get_our_node_id();
	let bob = &nodes[1];
	let bob_id = bob.node.get_our_node_id();

	// Bob (XBT) pays an XBT offer whose signing key is alice's node key, so the test can sign a
	// replacement invoice with it.
	let offer = OfferBuilder::new(alice_id)
		.chain(bitcoin::Network::Testnet)
		.xbt_blake2b(true)
		.description("x".into())
		.amount_msats(10_000_000)
		.build()
		.unwrap();
	assert!(offer.offer_features().requires_blake2b());
	let payment_id = PaymentId([7; 32]);
	bob.node.pay_for_offer(&offer, None, payment_id, Default::default()).unwrap();

	let onion_message = bob.onion_messenger.next_onion_message_for_peer(alice_id).unwrap();
	let (invoice_request, reply_path) =
		super::offers_tests::extract_invoice_request(alice, &onion_message);

	// The intercepted reply: an invoice for the request, signed by the offer key, with no blake2b.
	let invoice = invoice_request
		.respond_with(payment_paths(), crate::types::payment::PaymentHash([42; 32]))
		.unwrap()
		.build()
		.unwrap()
		.sign(|i: &crate::offers::invoice::UnsignedBolt12Invoice| {
			use crate::sign::NodeSigner;
			alice.keys_manager.sign_bolt12_invoice(i)
		})
		.unwrap();
	assert!(!invoice.invoice_features().supports_blake2b());

	alice
		.onion_messenger
		.send_onion_message(
			crate::onion_message::offers::OffersMessage::Invoice(invoice),
			MessageSendInstructions::WithoutReplyPath {
				destination: Destination::BlindedPath(reply_path),
			},
		)
		.unwrap();
	let onion_message = alice.onion_messenger.next_onion_message_for_peer(bob_id).unwrap();
	bob.onion_messenger.handle_onion_message(alice_id, &onion_message);

	// No HTLC left bob, and the payment was abandoned.
	check_added_monitors(bob, 0);
	assert!(bob.node.get_and_clear_pending_msg_events().is_empty());
	let events = bob.node.get_and_clear_pending_events();
	assert_eq!(events.len(), 1, "{:?}", events);
	match &events[0] {
		Event::PaymentFailed { payment_id: id, reason, .. } => {
			assert_eq!(*id, payment_id);
			assert_eq!(*reason, Some(PaymentFailureReason::UnexpectedError));
		},
		e => panic!("unexpected event {:?}", e),
	}
	bob.logger.assert_log_contains(
		"lightning::ln::channelmanager",
		"XBT: invoice without option_blake2b",
		1,
	);
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

// Task 5: splicing on XBT.
//
// The splice transaction spends the old funding output (the shared input, a 2-of-2 both nodes
// sign) next to wallet inputs. On a unified channel both shared-input signatures are 0x21 over the
// unified digest, which without ANYONECANPAY commits to the prevout of every input. LDK 0.2.7's
// splice acceptor cannot contribute inputs (`internal_splice_init`), so "both sides contribute"
// is two splices, one started by each node.

use super::splicing_tests::{complete_interactive_funding_negotiation, complete_splice_handshake};
use crate::chain::chaininterface::FEERATE_FLOOR_SATS_PER_KW;
use crate::chain::channelmonitor::ANTI_REORG_DELAY;
use crate::events::Event;
use crate::ln::funding::{FundingTxInput, SpliceContribution};
use crate::sign::{NodeSigner, Recipient};
use crate::util::errors::APIError;
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Amount, TxIn, TxOut, Witness};

/// An XBT config that allows splicing with `peers`.
fn xbt_splice_cfg(peers: Vec<PublicKey>) -> UserConfig {
	let mut cfg = xbt_cfg();
	cfg.xbt.as_mut().unwrap().splice_peers = peers;
	cfg
}

fn cfg_node_id(cfg: &NodeCfg) -> PublicKey {
	cfg.keys_manager.get_node_id(Recipient::Node).unwrap()
}

/// A wallet key of the test (the test wallet of `functional_test_utils` signs only BIP 143).
fn wallet_key(i: u8) -> SecretKey {
	SecretKey::from_slice(&[0x40 + i; 32]).unwrap()
}

fn wallet_spk(key: &SecretKey) -> ScriptBuf {
	let pk = bitcoin::PublicKey::new(key.public_key(&Secp256k1::new()));
	ScriptBuf::new_p2wpkh(&pk.wpubkey_hash().unwrap())
}

/// A coin of `value` sat paying to `key`, as a splice input. `salt` makes the transaction unique.
fn wallet_coin(key: &SecretKey, value: u64, salt: u32) -> (FundingTxInput, Transaction) {
	let tx = Transaction {
		version: bitcoin::transaction::Version::TWO,
		lock_time: bitcoin::absolute::LockTime::from_consensus(salt),
		input: vec![TxIn::default()],
		output: vec![TxOut { value: Amount::from_sat(value), script_pubkey: wallet_spk(key) }],
	};
	(FundingTxInput::new_p2wpkh(tx.clone(), 0).unwrap(), tx)
}

/// Signs the inputs of `tx` that pay to `key` as an XBT wallet does: P2WPKH, hash type 0x21, over
/// the unified digest of every prevout in `prevouts`.
fn sign_wallet_inputs_unified(tx: &mut Transaction, prevouts: &[TxOut], key: &SecretKey) {
	let secp = Secp256k1::new();
	let pk = bitcoin::PublicKey::new(key.public_key(&secp));
	let spk = wallet_spk(key);
	let code = spk.p2wpkh_script_code().unwrap();
	let unsigned = tx.clone();
	for i in 0..tx.input.len() {
		if prevouts[i].script_pubkey != spk {
			continue;
		}
		let msg =
			crate::xbt::sighash_with_prevouts(&unsigned, prevouts, i, &code, EcdsaSighashType::All, true);
		let mut w = Witness::new();
		crate::xbt::push_sig(&mut w, &secp.sign_ecdsa(&msg, key), EcdsaSighashType::All, true);
		w.push(pk.to_bytes());
		tx.input[i].witness = w;
	}
}

/// The output every input of `tx` spends, looked up in `sources` (the old funding transaction and
/// the wallet coins), in input order.
fn prevouts_from(tx: &Transaction, sources: &[&Transaction]) -> Vec<TxOut> {
	tx.input
		.iter()
		.map(|i| {
			let src = sources
				.iter()
				.find(|s| s.compute_txid() == i.previous_output.txid)
				.expect("input from an unknown transaction");
			src.output[i.previous_output.vout as usize].clone()
		})
		.collect()
}

/// Asserts that the shared input `idx` of the splice `tx` carries two signatures with hash type
/// bytes `expected` and that each verifies against the digest over all of `prevouts`.
fn assert_shared_input_sigs(tx: &Transaction, prevouts: &[TxOut], idx: usize, expected: &[u8]) {
	let witness: Vec<&[u8]> = tx.input[idx].witness.iter().collect();
	assert_eq!(witness.len(), 4, "not a 2-of-2 witness");
	assert!(witness[0].is_empty());
	let script = ScriptBuf::from(witness[3].to_vec());
	assert_eq!(prevouts[idx].script_pubkey, script.to_p2wsh());
	let pubkeys: Vec<PublicKey> = script
		.instructions()
		.filter_map(|i| match i.unwrap() {
			Instruction::PushBytes(b) if b.len() == 33 => Some(PublicKey::from_slice(b.as_bytes()).unwrap()),
			_ => None,
		})
		.collect();
	assert_eq!(pubkeys.len(), 2);
	let secp = Secp256k1::verification_only();
	for (i, want) in expected.iter().enumerate() {
		let raw = witness[1 + i];
		assert_eq!(*raw.last().unwrap(), *want, "shared input signature {i}: hash type");
		let sig = Signature::from_der(&raw[..raw.len() - 1]).unwrap();
		let unified = want & crate::xbt::SIGHASH_UNIFIED != 0;
		let base = EcdsaSighashType::from_consensus((want & !crate::xbt::SIGHASH_UNIFIED) as u32);
		let msg = crate::xbt::sighash_with_prevouts(tx, prevouts, idx, &script, base, unified);
		secp.verify_ecdsa(&msg, &sig, &pubkeys[i])
			.unwrap_or_else(|_| panic!("shared input signature {i} does not verify over all prevouts"));
	}
}

/// Asserts that wallet input `idx` of `tx` is a P2WPKH spend with hash type 0x21 that verifies
/// against the unified digest over all of `prevouts`.
fn assert_wallet_input_unified(tx: &Transaction, prevouts: &[TxOut], idx: usize) {
	let witness: Vec<&[u8]> = tx.input[idx].witness.iter().collect();
	assert_eq!(witness.len(), 2);
	assert_eq!(*witness[0].last().unwrap(), 0x21);
	let sig = Signature::from_der(&witness[0][..witness[0].len() - 1]).unwrap();
	let pk = PublicKey::from_slice(witness[1]).unwrap();
	let code = prevouts[idx].script_pubkey.p2wpkh_script_code().unwrap();
	let msg = crate::xbt::sighash_with_prevouts(tx, prevouts, idx, &code, EcdsaSighashType::All, true);
	Secp256k1::verification_only().verify_ecdsa(&msg, &sig, &pk).unwrap();
}

/// Runs a splice `initiator` starts with `contribution` and signs its wallet inputs with `key`
/// (0x21, using the prevouts of `Event::FundingTransactionReadyForSigning`). Returns the splice
/// transaction both nodes broadcast and the event's prevouts.
fn xbt_splice<'a, 'b, 'c, 'd>(
	initiator: &'a Node<'b, 'c, 'd>, acceptor: &'a Node<'b, 'c, 'd>, channel_id: ChannelId,
	contribution: SpliceContribution, key: &SecretKey,
) -> (Transaction, Vec<TxOut>) {
	let node_id_initiator = initiator.node.get_our_node_id();
	let node_id_acceptor = acceptor.node.get_our_node_id();
	let script = complete_splice_handshake(initiator, acceptor, channel_id, contribution.clone());
	let commit_sig =
		complete_interactive_funding_negotiation(initiator, acceptor, channel_id, contribution, script);

	acceptor.node.handle_commitment_signed(node_id_initiator, &commit_sig);
	let msg_events = acceptor.node.get_and_clear_pending_msg_events();
	assert_eq!(msg_events.len(), 2, "{msg_events:?}");
	if let MessageSendEvent::UpdateHTLCs { ref updates, .. } = &msg_events[0] {
		initiator.node.handle_commitment_signed(node_id_acceptor, &updates.commitment_signed[0]);
	} else {
		panic!();
	}
	if let MessageSendEvent::SendTxSignatures { ref msg, .. } = &msg_events[1] {
		initiator.node.handle_tx_signatures(node_id_acceptor, msg);
	} else {
		panic!();
	}

	let event = get_event!(initiator, Event::FundingTransactionReadyForSigning);
	let prevouts = if let Event::FundingTransactionReadyForSigning {
		channel_id,
		counterparty_node_id,
		unsigned_transaction,
		prevouts,
		..
	} = event
	{
		assert_eq!(prevouts.len(), unsigned_transaction.input.len());
		let mut tx = unsigned_transaction;
		sign_wallet_inputs_unified(&mut tx, &prevouts, key);
		initiator.node.funding_transaction_signed(&channel_id, &counterparty_node_id, tx).unwrap();
		prevouts
	} else {
		unreachable!()
	};
	let mut msg_events = initiator.node.get_and_clear_pending_msg_events();
	assert_eq!(msg_events.len(), 1, "{msg_events:?}");
	if let MessageSendEvent::SendTxSignatures { ref msg, .. } = msg_events.remove(0) {
		acceptor.node.handle_tx_signatures(node_id_initiator, msg);
	} else {
		panic!();
	}
	check_added_monitors(initiator, 1);
	check_added_monitors(acceptor, 1);

	let mut initiator_txn = initiator.tx_broadcaster.txn_broadcast();
	assert_eq!(initiator_txn.len(), 1);
	assert_eq!(initiator_txn, acceptor.tx_broadcaster.txn_broadcast());
	expect_splice_pending_event(initiator, &node_id_acceptor);
	expect_splice_pending_event(acceptor, &node_id_initiator);
	(initiator_txn.remove(0), prevouts)
}

fn shared_input_index(tx: &Transaction, old_funding: &Transaction) -> usize {
	let txid = old_funding.compute_txid();
	let found: Vec<usize> =
		(0..tx.input.len()).filter(|i| tx.input[*i].previous_output.txid == txid).collect();
	assert_eq!(found.len(), 1);
	found[0]
}

/// Two XBT nodes that list each other in `splice_peers`, with a 100k sat channel.
macro_rules! xbt_splice_pair {
	($chanmon_cfgs: ident, $node_cfgs: ident, $node_chanmgrs: ident, $nodes: ident, $chan: ident) => {
		let $chanmon_cfgs = create_chanmon_cfgs(2);
		let $node_cfgs = create_node_cfgs(2, &$chanmon_cfgs);
		let ids = [cfg_node_id(&$node_cfgs[0]), cfg_node_id(&$node_cfgs[1])];
		let $node_chanmgrs = create_node_chanmgrs(
			2,
			&$node_cfgs,
			&[Some(xbt_splice_cfg(vec![ids[1]])), Some(xbt_splice_cfg(vec![ids[0]]))],
		);
		let $nodes = create_network(2, &$node_cfgs, &$node_chanmgrs);
		let $chan = create_announced_chan_between_nodes_with_value(&$nodes, 0, 1, 100_000, 0);
		assert!($nodes[0].node.list_channels()[0].channel_type.as_ref().unwrap().requires_unified_sigs());
	};
}

#[test]
fn xbt_splice_in_shared_input_unified() {
	xbt_splice_pair!(chanmon_cfgs, node_cfgs, node_chanmgrs, nodes, chan);
	let key = wallet_key(0);
	let (input, coin) = wallet_coin(&key, 200_000, 1);
	let contribution = SpliceContribution::SpliceIn {
		value: Amount::from_sat(50_000),
		inputs: vec![input],
		change_script: Some(wallet_spk(&key)),
	};
	let (tx, _) = xbt_splice(&nodes[0], &nodes[1], chan.2, contribution, &key);
	assert_eq!(tx.input.len(), 2);

	let prevouts = prevouts_from(&tx, &[&chan.3, &coin]);
	let shared = shared_input_index(&tx, &chan.3);
	assert_shared_input_sigs(&tx, &prevouts, shared, &[0x21, 0x21]);
	assert_wallet_input_unified(&tx, &prevouts, 1 - shared);
}

#[test]
fn xbt_splice_both_sides_contribute() {
	// Review Focus 4: each node signs the shared input over prevouts the other node contributed.
	xbt_splice_pair!(chanmon_cfgs, node_cfgs, node_chanmgrs, nodes, chan);

	// Node 0 starts a splice with two wallet inputs: three inputs in all.
	let key0 = wallet_key(0);
	let (in_a, coin_a) = wallet_coin(&key0, 60_000, 1);
	let (in_b, coin_b) = wallet_coin(&key0, 70_000, 2);
	let contribution = SpliceContribution::SpliceIn {
		value: Amount::from_sat(100_000),
		inputs: vec![in_a, in_b],
		change_script: Some(wallet_spk(&key0)),
	};
	let (tx1, _) = xbt_splice(&nodes[0], &nodes[1], chan.2, contribution, &key0);
	assert_eq!(tx1.input.len(), 3);
	let prevouts1 = prevouts_from(&tx1, &[&chan.3, &coin_a, &coin_b]);
	let shared1 = shared_input_index(&tx1, &chan.3);
	assert_shared_input_sigs(&tx1, &prevouts1, shared1, &[0x21, 0x21]);
	for i in (0..3).filter(|i| *i != shared1) {
		assert_wallet_input_unified(&tx1, &prevouts1, i);
	}

	mine_transaction(&nodes[0], &tx1);
	mine_transaction(&nodes[1], &tx1);
	super::splicing_tests::lock_splice_after_blocks(&nodes[0], &nodes[1], ANTI_REORG_DELAY - 1);

	// Node 1 starts the next splice with its own input, which node 0 signs over.
	let key1 = wallet_key(1);
	let (in_c, coin_c) = wallet_coin(&key1, 80_000, 3);
	let contribution = SpliceContribution::SpliceIn {
		value: Amount::from_sat(40_000),
		inputs: vec![in_c],
		change_script: Some(wallet_spk(&key1)),
	};
	let (tx2, _) = xbt_splice(&nodes[1], &nodes[0], chan.2, contribution, &key1);
	assert_eq!(tx2.input.len(), 2);
	let prevouts2 = prevouts_from(&tx2, &[&tx1, &coin_c]);
	let shared2 = shared_input_index(&tx2, &tx1);
	assert_shared_input_sigs(&tx2, &prevouts2, shared2, &[0x21, 0x21]);
	assert_wallet_input_unified(&tx2, &prevouts2, 1 - shared2);
}

#[test]
fn xbt_splice_event_carries_prevouts() {
	xbt_splice_pair!(chanmon_cfgs, node_cfgs, node_chanmgrs, nodes, chan);
	let key = wallet_key(0);
	let (in_a, coin_a) = wallet_coin(&key, 60_000, 1);
	let (in_b, coin_b) = wallet_coin(&key, 70_000, 2);
	let contribution = SpliceContribution::SpliceIn {
		value: Amount::from_sat(100_000),
		inputs: vec![in_a, in_b],
		change_script: Some(wallet_spk(&key)),
	};
	let (tx, event_prevouts) = xbt_splice(&nodes[0], &nodes[1], chan.2, contribution, &key);
	assert_eq!(event_prevouts.len(), tx.input.len());
	assert_eq!(event_prevouts, prevouts_from(&tx, &[&chan.3, &coin_a, &coin_b]));
}

#[test]
fn xbt_splice_refused_for_unlisted_peer() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let id1 = cfg_node_id(&node_cfgs[1]);
	// Node 0 lists node 1; node 1 lists no one.
	let node_chanmgrs = create_node_chanmgrs(
		2,
		&node_cfgs,
		&[Some(xbt_splice_cfg(vec![id1])), Some(xbt_splice_cfg(vec![]))],
	);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let node_id_0 = nodes[0].node.get_our_node_id();
	let node_id_1 = nodes[1].node.get_our_node_id();
	let chan = create_announced_chan_between_nodes_with_value(&nodes, 0, 1, 100_000, 50_000_000);

	let contribution = SpliceContribution::SpliceOut {
		outputs: vec![TxOut { value: Amount::from_sat(1_000), script_pubkey: wallet_spk(&wallet_key(0)) }],
	};

	// Outbound: node 1 does not list node 0.
	let res = nodes[1].node.splice_channel(
		&chan.2,
		&node_id_0,
		contribution.clone(),
		FEERATE_FLOOR_SATS_PER_KW,
		None,
	);
	match res {
		Err(APIError::APIMisuseError { err }) => {
			assert_eq!(err, "splicing not allowed with this peer on XBT")
		},
		_ => panic!("Wrong result {:?}", res),
	}
	assert!(nodes[1].node.get_and_clear_pending_msg_events().is_empty());

	// Inbound: node 0 lists node 1 and starts; node 1 answers its splice_init with tx_abort.
	nodes[0]
		.node
		.splice_channel(&chan.2, &node_id_1, contribution, FEERATE_FLOOR_SATS_PER_KW, None)
		.unwrap();
	let stfu = get_event_msg!(nodes[0], MessageSendEvent::SendStfu, node_id_1);
	nodes[1].node.handle_stfu(node_id_0, &stfu);
	let stfu = get_event_msg!(nodes[1], MessageSendEvent::SendStfu, node_id_0);
	nodes[0].node.handle_stfu(node_id_1, &stfu);
	let splice_init = get_event_msg!(nodes[0], MessageSendEvent::SendSpliceInit, node_id_1);
	nodes[1].node.handle_splice_init(node_id_0, &splice_init);

	let tx_abort = get_event_msg!(nodes[1], MessageSendEvent::SendTxAbort, node_id_0);
	let reason = String::from_utf8(tx_abort.data.clone()).unwrap();
	assert!(reason.contains("splicing not allowed with this peer on XBT"), "{reason}");
	nodes[0].node.handle_tx_abort(node_id_1, &tx_abort);
	let _ = get_event!(nodes[0], Event::SpliceFailed);
	let ack = get_event_msg!(nodes[0], MessageSendEvent::SendTxAbort, node_id_1);
	nodes[1].node.handle_tx_abort(node_id_0, &ack);
	assert!(nodes[1].node.get_and_clear_pending_msg_events().is_empty());

	// Neither node is left quiescent: payments still flow both ways.
	send_payment(&nodes[0], &[&nodes[1]], 1_000_000);
	send_payment(&nodes[1], &[&nodes[0]], 1_000_000);
}

#[test]
fn xbt_splice_rejects_bad_shared_input_signature() {
	// The counterparty's shared input signature is checked against the digest over all prevouts.
	xbt_splice_pair!(chanmon_cfgs, node_cfgs, node_chanmgrs, nodes, chan);
	let node_id_0 = nodes[0].node.get_our_node_id();
	let node_id_1 = nodes[1].node.get_our_node_id();
	let key = wallet_key(0);
	let (input, coin) = wallet_coin(&key, 200_000, 1);
	let contribution = SpliceContribution::SpliceIn {
		value: Amount::from_sat(50_000),
		inputs: vec![input],
		change_script: Some(wallet_spk(&key)),
	};
	let script = complete_splice_handshake(&nodes[0], &nodes[1], chan.2, contribution.clone());
	let commit_sig =
		complete_interactive_funding_negotiation(&nodes[0], &nodes[1], chan.2, contribution, script);
	nodes[1].node.handle_commitment_signed(node_id_0, &commit_sig);
	let msg_events = nodes[1].node.get_and_clear_pending_msg_events();
	assert_eq!(msg_events.len(), 2, "{msg_events:?}");
	if let MessageSendEvent::UpdateHTLCs { ref updates, .. } = &msg_events[0] {
		nodes[0].node.handle_commitment_signed(node_id_1, &updates.commitment_signed[0]);
	} else {
		panic!();
	}
	check_added_monitors(&nodes[0], 1);
	check_added_monitors(&nodes[1], 1);
	let mut tx_signatures = if let MessageSendEvent::SendTxSignatures { msg, .. } = &msg_events[1] {
		msg.clone()
	} else {
		panic!();
	};

	// Replace node 1's shared input signature with one by a key that is not its funding key.
	let event = get_event!(nodes[0], Event::FundingTransactionReadyForSigning);
	let unsigned = match event {
		Event::FundingTransactionReadyForSigning { unsigned_transaction, .. } => unsigned_transaction,
		_ => unreachable!(),
	};
	let prevouts = prevouts_from(&unsigned, &[&chan.3, &coin]);
	let shared = shared_input_index(&unsigned, &chan.3);
	let digest = crate::xbt::sighash_with_prevouts(
		&unsigned,
		&prevouts,
		shared,
		&prevouts[shared].script_pubkey,
		EcdsaSighashType::All,
		true,
	);
	let other_key = SecretKey::from_slice(&[0x77; 32]).unwrap();
	tx_signatures.shared_input_signature =
		Some(Secp256k1::new().sign_ecdsa(&digest, &other_key));
	nodes[0].node.handle_tx_signatures(node_id_1, &tx_signatures);

	let msg_events = nodes[0].node.get_and_clear_pending_msg_events();
	assert_eq!(msg_events.len(), 1, "{msg_events:?}");
	match &msg_events[0] {
		MessageSendEvent::HandleError { action, .. } => {
			let s = format!("{action:?}");
			assert!(s.contains("shared input signature"), "{s}");
		},
		e => panic!("{e:?}"),
	}
	assert!(nodes[0].tx_broadcaster.txn_broadcast().is_empty());
}

// FORTIS XBT: block ids from the chain source (Task 7). An XBT block's id is not the double
// SHA-256 of its 80-byte `Header`, so the Esplora sync passes the index's id through
// `Confirm::*_with_hash`, and everything LDK stores must carry that id.

use crate::chain::{BestBlock, Confirm};
use bitcoin::block::Header;
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::BlockHash;

/// An id for `header` that is not its `block_hash()`, as an XBT block's is not.
fn xbt_block_id(header: &Header) -> BlockHash {
	let mut data = bitcoin::consensus::serialize(header);
	data.extend_from_slice(b"blake2b");
	let id = BlockHash::from_byte_array(sha256d::Hash::hash(&data).to_byte_array());
	assert_ne!(id, header.block_hash());
	id
}

/// `count` headers from `height` on, each linked to the previous one's id; `salt` makes forks.
fn xbt_chain(mut prev: BlockHash, height: u32, count: u32, salt: u32) -> Vec<(Header, BlockHash)> {
	(height..height + count)
		.map(|h| {
			let header = create_dummy_header(prev, h + salt);
			let id = xbt_block_id(&header);
			prev = id;
			(header, id)
		})
		.collect()
}

/// Connects a block to `node` only through the `_with_hash` methods, as the Esplora sync does.
fn connect_with_id(node: &Node, header: &Header, id: BlockHash, txn: &[&Transaction], height: u32) {
	let txdata: Vec<_> = txn.iter().enumerate().map(|(i, tx)| (i + 1, *tx)).collect();
	let chain_monitor = &node.chain_monitor.chain_monitor;
	chain_monitor.transactions_confirmed_with_hash(header, id, &txdata, height);
	chain_monitor.best_block_updated_with_hash(header, id, height);
	node.node.transactions_confirmed_with_hash(header, id, &txdata, height);
	node.node.best_block_updated_with_hash(header, id, height);
}

fn connect_chain_with_ids(node: &Node, chain: &[(Header, BlockHash)], first: u32, tx: &Transaction) {
	for (i, (header, id)) in chain.iter().enumerate() {
		let txn: &[&Transaction] = if i == 0 { &[tx] } else { &[] };
		connect_with_id(node, header, *id, txn, first + i as u32);
	}
}

fn assert_best_block(node: &Node, best: BestBlock) {
	assert_eq!(node.node.current_best_block(), best);
	let chain_monitor = &node.chain_monitor.chain_monitor;
	for channel_id in chain_monitor.list_monitors() {
		assert_eq!(chain_monitor.get_monitor(channel_id).unwrap().current_best_block(), best);
	}
}

#[test]
fn confirm_with_hash_uses_given_hash() {
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(xbt_cfg()), Some(xbt_cfg())]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
	let node_a_id = nodes[0].node.get_our_node_id();
	let node_b_id = nodes[1].node.get_our_node_id();

	let funding_tx = create_chan_between_nodes_with_value_init(&nodes[0], &nodes[1], 100_000, 0);
	let funding_txid = funding_tx.compute_txid();
	let (tip, tip_height) = nodes[0].best_block_info();
	assert_eq!(nodes[1].best_block_info(), (tip, tip_height));
	let conf_height = tip_height + 1;

	// The funding transaction confirms in a block that is then reorged out, as the sync does it:
	// the stored id shows the block is stale, the transaction is unconfirmed, the new tip given.
	let stale = xbt_chain(tip, conf_height, 1, 0);
	let chain = xbt_chain(tip, conf_height, CHAN_CONFIRM_DEPTH, 1_000);
	for node in nodes.iter() {
		let (header, id) = stale[0];
		connect_with_id(node, &header, id, &[&funding_tx], conf_height);
		assert_best_block(node, BestBlock::new(id, conf_height));
		assert!(node.node.get_relevant_txids().contains(&(funding_txid, conf_height, Some(id))));

		node.chain_monitor.chain_monitor.transaction_unconfirmed(&funding_txid);
		node.node.transaction_unconfirmed(&funding_txid);
		assert!(node.node.get_relevant_txids().is_empty());
	}

	// It confirms again on the new chain, first at node 1 and then at node 0, and the channel
	// becomes ready as usual.
	connect_chain_with_ids(&nodes[1], &chain, conf_height, &funding_tx);
	let channel_ready =
		get_event_msg!(nodes[1], MessageSendEvent::SendChannelReady, node_a_id);
	nodes[0].node.handle_channel_ready(node_b_id, &channel_ready);
	connect_chain_with_ids(&nodes[0], &chain, conf_height, &funding_tx);
	expect_channel_ready_event(&nodes[0], &node_b_id);
	let (as_funding_msgs, _) = create_chan_between_nodes_with_value_confirm_second(&nodes[1], &nodes[0]);
	create_chan_between_nodes_with_value_b(&nodes[0], &nodes[1], &as_funding_msgs);

	let (_, last_id) = *chain.last().unwrap();
	let best = BestBlock::new(last_id, conf_height + CHAN_CONFIRM_DEPTH - 1);
	for node in nodes.iter() {
		assert_best_block(node, best);
		let txids = node.node.get_relevant_txids();
		assert_eq!(txids, vec![(funding_txid, conf_height, Some(chain[0].1))]);
		let chans = node.node.list_channels();
		assert_eq!(chans.len(), 1);
		assert!(chans[0].is_channel_ready && chans[0].is_usable);
	}
}
