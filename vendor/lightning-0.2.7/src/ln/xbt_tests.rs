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
use crate::ln::msgs::{BaseMessageHandler, ChannelMessageHandler, MessageSendEvent};
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
