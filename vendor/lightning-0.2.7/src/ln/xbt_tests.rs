// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! FORTIS XBT: tests for the XBT (BLAKE2b chain) feature bits and channel types.
//!
//! Channels between two XBT nodes negotiate `option_unified_sigs` but, until the signing call
//! sites take the channel type (Task 4), still sign with plain sighash. Both LDK nodes agree on
//! that, so these tests do not depend on it.

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
