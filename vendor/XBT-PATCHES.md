# XBT patches to vendored crates

Every change to the crates in this directory that make LDK 0.2.7 speak
Lightning on the Bitcoin BLAKE2b chain (XBT) is listed here, and each changed
line in the source carries a `// FORTIS XBT: <why>` comment. Spec:
`docs/superpowers/specs/2026-11-06-xbt-lightning-design.md`.

Rows marked "spike" were brought over from the spike worktree unchanged and
are reworked by the task named in the last column. Every bilateral signature
(commitment, closing, HTLC) is unified exactly when the channel's
`ChannelTypeFeatures::requires_unified_sigs()` is true (`xbt::is_unified`),
whether we create it or check it; no call site reads config (Task 4).

| Crate | File | Function / item | Reason | Task |
|---|---|---|---|---|
| lightning | `src/xbt.rs` | whole module | Per-call unified sighash API (`SIGHASH_UNIFIED`, `hash_type`, `p2wsh_sighash`, `sighash_with_prevouts`, `push_sig`, `unified_message`, `is_unified`); no global state | 2 |
| lightning | `src/xbt_unified_sighash.json` | test vectors | Knots' `SignatureHashUnified` vectors (142 rows) | 2 |
| lightning | `src/lib.rs` | `pub mod xbt` | Exposes the module (spike) | 2 |
| lightning | `src/ln/chan_utils.rs`, `src/ln/channel.rs` | imports | Dropped `bitcoin::sighash`, unused once digests go through `crate::xbt` | 2 |
| lightning | `src/ln/chan_utils.rs` | `build_htlc_input_witness` | Both HTLC signature bytes (remote and local) from `is_unified(channel_type_features)` | 4 |
| lightning | `src/ln/chan_utils.rs` | `HolderCommitmentTransaction::add_holder_sig` | Both commitment signature bytes from the commitment's own `channel_type_features` (no params reachable; stored in the monitor's commitment, survives reload) | 4 |
| lightning | `src/ln/chan_utils.rs` | `BuiltCommitmentTransaction::get_sighash_all`, `sign_counterparty_commitment`, `sign_holder_commitment` | New `unified: bool` parameter (the built transaction has no channel type); every caller passes `is_unified` of the channel type. A public API change, so a missed caller fails to compile | 4 |
| lightning | `src/ln/chan_utils.rs` | `TrustedClosingTransaction::get_sighash_all`, `TrustedClosingTransaction::sign` | New `unified: bool` parameter, as above (`ClosingTransaction` has no channel type) | 4 |
| lightning | `src/ln/chan_utils.rs` | `TrustedCommitmentTransaction::get_htlc_sigs` | Digest from `is_unified(self.channel_type_features)` | 4 |
| lightning | `src/sign/mod.rs` | `InMemorySigner::sign_counterparty_commitment` | Commitment signature and HTLC signatures from `channel_parameters.channel_type_features` | 4 |
| lightning | `src/sign/mod.rs` | `InMemorySigner::sign_holder_commitment`, `unsafe_sign_holder_commitment` | From `channel_parameters.channel_type_features` (debug-asserted equal to the commitment's) | 4 |
| lightning | `src/sign/mod.rs` | `InMemorySigner::sign_holder_htlc_transaction` | From the descriptor's `channel_parameters.channel_type_features` (the spike's missed site) | 4 |
| lightning | `src/sign/mod.rs` | `InMemorySigner::sign_closing_transaction` | From `channel_parameters.channel_type_features` | 4 |
| lightning | `src/ln/channel.rs` | `check_counterparty_commitment_signature` (funding_created / funding_signed), `validate_commitment_signed` (commitment and HTLC signatures) | Peer signatures checked against the digest `funding.get_channel_type()` names | 4 |
| lightning | `src/ln/channel.rs` | `signer_maybe_unblocked` (pending closing_signed debug check), `closing_signed` (both checks), `build_signed_closing_transaction` | Closing signature checks and both witness bytes from `funding.get_channel_type()` | 4 |
| lightning | `src/ln/channel.rs` | `tests::outbound_commitment_test` (BOLT 3 vectors) | Passes `unified: false`: the vectors are BTC channels | 4 |
| lightning | `src/util/test_channel_signer.rs` | `TestChannelSigner::sign_holder_htlc_transaction`, imports | The test signer's check of the counterparty HTLC signature uses `xbt::p2wsh_sighash` with the channel type (it panicked on unified channels) | 4 |
| lightning | `src/ln/xbt_tests.rs` | `assert_sig_bytes` and six tests | Hash type bytes and signature validity of commitment, HTLC-timeout, HTLC-success and closing transactions; BTC and XBT pairs in one process; after reload | 4 |
| lightning | `src/sign/mod.rs` | `sign_counterparty_payment_input` (to_remote sweep), `sign_dynamic_p2wsh_input` (to_local sweep), `sign_justice_revoked_output`, `sign_justice_revoked_htlc`, `sign_counterparty_htlc_transaction` (claim of an HTLC on the counterparty's commitment), `KeysManager::sign_spendable_outputs_psbt` (static output sweep) | Plain spend, allowed: only we sign | - |
| lightning | `src/chain/package.rs` | `PackageSolvingData::finalize_input` (`RevokedOutput`, `RevokedHTLCOutput`, `CounterpartyOfferedHTLCOutput`, `CounterpartyReceivedHTLCOutput`: `EcdsaSighashType::All as u8`) | Plain spend, allowed: penalty and counterparty-HTLC claims only we sign. The holder HTLC path (`HolderHTLCOutput::get_maybe_signed_htlc_tx`) and holder commitment path go through `build_htlc_input_witness` / `add_holder_sig` and are unified | - |
| lightning | `src/chain/channelmonitor.rs` | `sign_to_local_justice_tx` (`BitcoinSignature::sighash_all`) | Plain spend, allowed: justice transaction only we sign | - |
| lightning | `src/chain/channelmonitor.rs` | `tests::test_claim_txn_weight_computation` `sign_input!` (about line 7287) | Test-only weight check with dummy keys; plain spend, allowed. Builds no counterparty-checked signature | - |
| lightning | `src/sign/mod.rs`, `src/ln/chan_utils.rs`, `src/events/bump_transaction/mod.rs` | `sign_holder_keyed_anchor_input`, `build_keyed_anchor_input_witness`; the `SinglePlusAnyoneCanPay` (0x83) HTLC branches | Anchors are off on XBT (never proposed, refused), so these never run on a unified channel. Behaviour for BTC unchanged | - |
| lightning | `src/sign/ecdsa.rs` | `EcdsaChannelSigner::sign_splice_shared_input` | New `prevouts: &[TxOut]` parameter (every input's prevout, in order). A public trait change, so a signer that misses it fails to compile | 5 |
| lightning | `src/sign/mod.rs` | `InMemorySigner::sign_splice_shared_input` | Digest from `xbt::sighash_with_prevouts(.., is_unified(channel_type))`: unified over every prevout on XBT, BIP 143 (same digest as before) on BTC. Asserts one prevout per input and that the shared one is the funding output | 5 |
| lightning | `src/util/test_channel_signer.rs`, `src/util/dyn_signer.rs` | `TestChannelSigner::sign_splice_shared_input`, `DynSigner` delegate | Pass `prevouts` through | 5 |
| lightning | `src/ln/interactivetxs.rs` | `ConstructedTransaction::prevouts` (new) | Every input's prevout from the `tx_add_input` metadata, in input order | 5 |
| lightning | `src/ln/interactivetxs.rs` | `ConstructedTransaction::finalize`, `InteractiveTxSigningSession::maybe_finalize_funding_tx`, `received_tx_signatures`, `provide_holder_witnesses` | New `unified` parameter; both shared input witness signatures pushed with `xbt::push_sig` (0x21 on unified channels). Unused `bitcoin::ecdsa::Signature` import dropped | 5 |
| lightning | `src/ln/interactivetxs.rs` | `InteractiveTxSigningSession::verify_interactive_tx_signatures` (P2WPKH branch); `tests::do_verify_tx_signatures` | On a unified channel our own wallet inputs may carry 0x21 (Task 6 signs them so): checked against the unified digest over every prevout. 0x01 still takes the BIP 143 check. The test helper passes `false` | 5 |
| lightning | `src/ln/interactivetxs.rs` | `AbortReason::XbtSpliceNotAllowed` (new) | tx_abort text "splicing not allowed with this peer on XBT" | 5 |
| lightning | `src/ln/channel.rs` | `FundedChannel::funding_transaction_signed` | Passes the prevouts to `sign_splice_shared_input` and `unified` (from `funding.get_channel_type()`) to `provide_holder_witnesses` | 5 |
| lightning | `src/ln/channel.rs` | `FundedChannel::tx_signatures` | New check: the counterparty's shared input signature is verified against the digest the channel type names (over every prevout) with their funding key of the spent funding; failure is `ChannelError::Warn`. LDK 0.2.7 did not check it; BTC peers sending valid signatures are unaffected. Passes `unified` to `received_tx_signatures` | 5 |
| lightning | `src/ln/channel.rs` | `FundedChannel::refuse_xbt_splice_init` (new) | Ends the quiescence entered for the refused splice and returns `ChannelError::Abort(XbtSpliceNotAllowed)` (sent as tx_abort) | 5 |
| lightning | `src/ln/channelmanager.rs` | `xbt_splice_allowed` (new), `internal_splice_channel`, `internal_splice_init` | With `config.xbt` set, splicing only with peers in `splice_peers`: outbound returns `APIMisuseError` "splicing not allowed with this peer on XBT", inbound `splice_init` is answered with tx_abort. `xbt: None` unchanged. Splicing is still advertised (`provided_init_features`) | 5 |
| lightning | `src/ln/channelmanager.rs`, `src/events/mod.rs` | `Event::FundingTransactionReadyForSigning::prevouts` (new field), emitted in `handle_channel_resumption` | Every input's prevout, in order, for the wallet's 0x21 signatures. Not serialized: the event type (49) is never written and is regenerated after a restart, so there is no TLV | 5 |
| lightning | `src/ln/xbt_tests.rs` | splice tests and helpers (`xbt_splice`, `assert_shared_input_sigs`, `assert_wallet_input_unified`, `sign_wallet_inputs_unified`) | Shared input 0x21/0x21 verified over all prevouts (2 and 3 inputs, each node initiating), event prevouts, allow-list refusal both ways, bad counterparty signature refused | 5 |
| lightning | `src/util/test_utils.rs`, `src/ln/monitor_tests.rs` | wallet test signer; a monitor test's hand-built HTLC claim | Test-only, BTC channels; plain | - |
| ldk-node | `src/config.rs` | `XbtConfig` (new), `Config::xbt`, `Default for Config`, `default_user_config` | Per-node XBT switch (`Option<XbtConfig>`, default `None`; `XbtConfig` defaults 961640 / no splice peers), mapped onto `UserConfig::xbt` | 6 |
| ldk-node | `src/builder.rs` | `build_with_store_internal` | With `xbt` set, `anchor_channels_config` is cleared (no anchor reserve, `negotiate_anchors_zero_fee_htlc_tx` off); passes the BIP 84 master `xprv` to `Wallet::new` | 6 |
| ldk-node | `src/wallet/xbt_sign.rs` | `sign_xbt_p2wpkh` (new), tests | Signs our P2WPKH inputs `[sig \|\| 0x21, pubkey]` over the unified digest of every prevout (`xbt::sighash_with_prevouts`, P2PKH script code); byte-identical to wallet-core's `MasterKey::sign_p2wpkh_tx` | 6 |
| ldk-node | `src/wallet/mod.rs` | `Wallet::xprv`, `Wallet::new`, `sign_xbt_wallet_spend` (new), `sign_xbt_inputs` (new) | On XBT the wallet signs with the descriptors' master key instead of BDK. Fails (signs nothing) unless there is one prevout per input, our prevouts match our UTXOs, and every input of ours gets signed | 6 |
| ldk-node | `src/wallet/mod.rs` | `create_funding_transaction`, `send_to_address` (sends and drains/sweeps of the wallet) | XBT: `sign_xbt_wallet_spend` (prevouts from our UTXOs) instead of `locked_wallet.sign` | 6 |
| ldk-node | `src/wallet/mod.rs`, `src/event.rs` | `sign_owned_inputs` (new `prevouts` parameter), `FundingTransactionReadyForSigning` handler | XBT: splice/dual-funded contributions signed over the event's `prevouts`; BTC ignores them | 6 |
| ldk-node | `src/wallet/mod.rs` | `sign_psbt_inner` (`WalletSource::sign_psbt`) | XBT: prevouts from every input's `witness_utxo` (refused if one is missing). Only LDK's anchor bumping calls it, which never runs on XBT | 6 |
| ldk-node | `src/wallet/mod.rs` | `WalletKeysManager::spend_spendable_outputs` | Not patched: sweeps of channel outputs (`OutputSweeper`) exist only on XBT and only we sign them, so they keep plain hash types (allowed) | - |
| ldk-node | `bindings/ldk_node.udl`, `src/ffi/types.rs` | `Config.xbt`, `dictionary XbtConfig`, re-export | The `uniffi` feature's `Config` record must list every field | 6 |
| ldk-node | `Cargo.toml` | `[dev-dependencies.wallet-core]` (path) | `xbt_sign_matches_wallet_core` compares against the app's XBT signer | 6 |
| lightning-types | `src/features.rs` | feature bits 512-515 (`option_blake2b`, `option_unified_sigs`), `requires_unified_sigs` | XBT feature bits (spike, kept) | 2 |
| lightning | `src/util/config.rs` | `XbtConfig`, `UserConfig::xbt`, `UserConfig::is_xbt`, `Default for UserConfig`, fuzzing `Readable for UserConfig` | Per-node XBT switch (`Option<XbtConfig>`, default `None`); replaces the spike's process-wide switch. `UserConfig` has no serialization outside fuzzing, so no TLV | 3 |
| lightning | `src/ln/channel.rs` | `channel_type_from_open_channel` (new `is_xbt` param; callers `InboundV1Channel::new`, `InboundV2Channel::new` and `ChannelManager::internal_open_channel` pass `config.is_xbt()`) | XBT refuses an `open_channel` whose type lacks `option_unified_sigs` or has any anchor bit (`ChannelError::close` mentioning "XBT") | 3 |
| lightning | `src/ln/channel.rs` | `get_initial_channel_type` | XBT proposes `unified_sigs` required, and never proposes either anchor type (the same function drives the fallback after a rejected type) | 3 |
| lightning | `src/ln/channelmanager.rs` | `provided_init_features` | XBT sets `blake2b` required and `unified_sigs` optional from `config.is_xbt()`, and does not advertise either anchor feature. `provided_node_features`, `provided_bolt11_invoice_features`, `provided_bolt12_invoice_features` and `provided_channel_type_features` derive from it, so they follow with no change of their own | 3 |
| lightning | `src/ln/channelmanager.rs` | `create_bolt11_invoice` | Calls `InvoiceBuilder::require_blake2b` when `is_xbt`. Phantom invoices (`ln/invoice_utils.rs` `create_phantom_invoice*`) have no config and are not patched | 3 |
| lightning | `src/ln/channelmanager.rs` | `create_offer_builder`, `create_offer_builder_using_router`, `create_refund_builder`, `create_refund_builder_using_router` (macros), `pay_for_offer_intern` | Set `blake2b` required on the offer, refund and invoice request via `xbt_blake2b(is_xbt)` | 3 |
| lightning | `src/ln/channelmanager.rs` | `ChannelManager::new`, `ChannelManager::read` | `OffersMessageFlow::with_xbt(config.is_xbt())` | 3 |
| lightning | `src/offers/flow.rs` | `OffersMessageFlow::xbt` field, `with_xbt`, `create_static_invoice_builder`, `create_invoice_builder_from_refund`, `create_response_for_invoice_request` | Every BOLT 12 invoice and static invoice the flow builds requires `blake2b` when the flow is XBT. Fixed at construction, as `is_xbt` is never changed at runtime | 3 |
| lightning | `src/offers/invoice.rs`, `src/offers/static_invoice.rs` | `InvoiceBuilder::xbt_blake2b`, `StaticInvoiceBuilder::xbt_blake2b` | New `pub(crate)` setters | 3 |
| lightning | `src/ln/invoice_utils.rs` | `create_phantom_invoice*` | Not used by Fortis, not patched (no config in hand; phantom invoices have no `blake2b`) | - |
| lightning | `src/ln/offers_tests.rs`, `src/ln/async_payments_tests.rs` | `route_bolt12_payment`, `claim_bolt12_payment`, `extract_invoice`, `pass_static_invoice_server_messages`, `expect_offer_paths_requests`, `StaticInvoiceServerFlowResult` | Visibility only (`pub(super)`), so `xbt_tests` can reuse the helpers | 3 |
| lightning | `src/offers/offer.rs`, `src/offers/refund.rs`, `src/offers/invoice_request.rs` | `OfferBuilder::xbt_blake2b`, `RefundBuilder::xbt_blake2b`, `InvoiceRequestBuilder::xbt_blake2b` | New `pub(crate)` setters; the `c_bindings` builder variants are not patched | 3 |
| lightning | `src/chain/package.rs` | `verify_channel_type_features` | Adds `unified_sigs` to the permitted set, else a monitor of an XBT channel fails to deserialize (`UnknownRequiredFeature`) | 3 |
| lightning | `src/ln/xbt_tests.rs`, `src/ln/mod.rs` | `mod xbt_tests` | Functional tests for the bits and channel types | 3 |
| lightning-types | `src/features.rs` | `Blake2b` added to `OfferContext`, `InvoiceRequestContext`, `Bolt12InvoiceContext` (byte 64) | `option_blake2b` is a known feature in offers, invoice requests and BOLT 12 invoices | 3 |
| lightning-types | `src/lib.rs` | `pub mod xbt` removed | The process-wide switch is gone (the spike's `bitcoin::block::xbt_ids` map went with the vendored `bitcoin` in Task 7) | 3 |
| lightning-invoice | `src/lib.rs` | `InvoiceBuilder::payment_secret` (global check removed), `InvoiceBuilder::require_blake2b` (new) | The invoice's `blake2b` bit is set by the caller (ChannelManager), not a global | 3 |
| lightning | `src/chain/mod.rs` | `Confirm::transactions_confirmed_with_hash`, `Confirm::best_block_updated_with_hash` (new provided methods) | Take the block's id from the chain source: an XBT block's id is not `Header::block_hash()` (the double SHA-256 of the 80 bytes kept of a 164-byte header). Defaults ignore the hash and call the original methods; an implementor whose originals delegate to them must override both (else they recurse) | 7 |
| lightning | `src/ln/channelmanager.rs` | `ChannelManager` `Confirm::transactions_confirmed`, `best_block_updated` (now delegate with `header.block_hash()`), `transactions_confirmed_with_hash`, `best_block_updated_with_hash` (the former bodies) | The given id goes to the channels, `best_block` and the offers flow. `Listen::filtered_block_connected` is not patched (full-block sources, not used on XBT) | 7 |
| lightning | `src/offers/flow.rs` | `OffersMessageFlow::best_block_updated` (delegates), `best_block_updated_with_hash` (new); `BlockHash` import | The flow's best block takes the given id | 7 |
| lightning | `src/chain/chainmonitor.rs` | `process_chain_data`, `update_monitor_with_chain_data` (take `block_hash` instead of `header`, its only use); `Listen::filtered_block_connected` (passes `header.block_hash()`); `Confirm::transactions_confirmed`, `best_block_updated` (delegate), `transactions_confirmed_with_hash`, `best_block_updated_with_hash` | The given id reaches every monitor and the outputs registered with the `Filter` | 7 |
| lightning | `src/chain/channelmonitor.rs` | `ChannelMonitor::transactions_confirmed`, `best_block_updated` (delegate), `transactions_confirmed_with_hash`, `best_block_updated_with_hash` (new, take the id, no header); `ChannelMonitorImpl::block_connected`, `best_block_updated`, `transactions_confirmed` (take `block_hash` instead of `header`, its only use, incl. the re-confirmation assert); `Confirm for (M, T, F, L)` `_with_hash` overrides | The monitor's best block and its confirmation entries store the given id | 7 |
| lightning | `src/util/sweep.rs` | `OutputSweeper::transactions_confirmed_internal`, `best_block_updated_internal` (take the id); `Listen::filtered_block_connected` (passes `header.block_hash()`); `OutputSweeper` and `OutputSweeperSync` `Confirm` `_with_hash` overrides (originals delegate / forward) | The sweeper's best block and spend confirmations store the given id | 7 |
| lightning | `src/util/sweep.rs`, `src/ln/xbt_tests.rs` | `tests::confirm_with_hash_uses_given_hash`; `xbt_tests::confirm_with_hash_uses_given_hash` and helpers (`xbt_block_id`, `xbt_chain`, `connect_with_id`, `connect_chain_with_ids`, `assert_best_block`) | Ids that differ from `header.block_hash()` are what the manager, monitors and sweeper store; funding reorged out by the stored id and reconfirmed, channel ready as usual | 7 |
| lightning-transaction-sync | (crate copied, patched in) | - | Needed so the workspace resolves one set of patched crates | 7 |
| lightning-transaction-sync | `src/esplora.rs` | `sync_best_block_updated` | `best_block_updated_with_hash` with the tip hash from `/blocks/tip/hash` | 7 |
| lightning-transaction-sync | `src/esplora.rs` | `get_confirmed_tx` | The block id is the output status's `block_hash`, else `get_tx_status(txid).block_hash` (unconfirmed: inconsistency, restart). If the proof's header does not hash to it, the index's header for that id (`get_header_by_hash`) must equal the proof's header, else inconsistency. BTC: same results, one extra `/tx/:txid/status` request per newly confirmed watched transaction | 7 |
| lightning-transaction-sync | `src/common.rs` | `ConfirmedTx::block_hash` (new), `SyncState::sync_confirmed_transactions` | Carries the id and calls `transactions_confirmed_with_hash` | 7 |
| lightning-transaction-sync | `src/electrum.rs` | `get_confirmed_transactions` (`ConfirmedTx` construction) | `block_hash: block_header.block_hash()`: Electrum is not used on XBT, behaviour unchanged | 7 |
| lightning-transaction-sync | `src/lib.rs`, `src/xbt_tests.rs` | `xbt_sync_uses_index_hashes` | A mock Esplora serves an 80-byte tip, then a 164-byte tip with a watched transaction confirmed in it (regtest Knots fixtures); the sync reports the served ids and never calls the methods without a hash | 7 |
| esplora-client | `src/xbt.rs` (new), `src/lib.rs` (`mod xbt`) | `header_from_bytes`, `merkle_block_from_bytes`, tests `parses_80_and_164_byte_headers`, `merkle_block_164_byte_header` | Headers of 80 or 164 bytes (the `Header` is bytes 0..80; any other length is `Error::InvalidResponse`). A merkle block proof is read with whichever header length makes its partial merkle tree commit to the header's merkle root (else the 80-byte reading's result, as upstream). Fixtures from regtest Knots. Replaces the spike's `xbt_header` / `xbt_merkle_block`, which recorded ids in `bitcoin::block::xbt_ids` | 7 |
| esplora-client | `src/blocking.rs`, `src/async.rs` | `get_header_by_hash`, `get_merkle_block`; `get_response_hex`, `get_opt_response_hex` (`#[allow(dead_code)]`) | Parse through `crate::xbt`. Signatures unchanged; callers take the id from the hash they asked for or from the transaction's status. The two hex helpers are now unused | 7 |
| (workspace) | `Cargo.toml` | `[patch.crates-io] bitcoin` removed; `workspace.exclude` adds `vendor/esplora-client-0.12.3`, `vendor/lightning-transaction-sync-0.2.7` | The vendored `bitcoin` (spike's `xbt_ids` map and `Header::block_hash` override) is deleted; crates.io `bitcoin` 0.32.102 (same version) is used. The two crates are excluded so their tests can run with `--manifest-path` | 7 |
| ldk-node | `src/payment/mod.rs` | `check_chain_bolt11`, `check_chain_offer` (new), `tests::xbt_chain_tests` | XBT keeps BTC's chain hash and `lnbc`, so `option_blake2b` is the only mark. An XBT node refuses (`Error::WrongChain`) an invoice or offer without it, a BTC node one with it; "supports" (bit 512 or 513) counts as marked. A `Bolt11Invoice` without field 9 cannot exist (`from_signed` needs the features), so only the missing-bit case is reachable | 8 |
| ldk-node | `src/payment/bolt11.rs` | `send`, `send_using_amount`, `send_probes`, `send_probes_using_amount` | Call `check_chain_bolt11` first, before any HTLC or probe. `UnifiedQrPayment::send` pays through these and `Bolt12Payment::send`, so it is covered | 8 |
| ldk-node | `src/payment/bolt12.rs` | `send`, `send_using_amount` | Call `check_chain_offer` first | 8 |
| ldk-node | `src/error.rs`, `bindings/ldk_node.udl` | `Error::WrongChain` (new), `Display`, `NodeError` enum | "This invoice or offer is for the other Bitcoin chain." | 8 |
| ldk-node | `src/builder.rs` | `build_with_store_internal` | With `xbt` set, `NetworkGraph::set_min_announcement_height(xbt.gossip_floor_height)` after the graph is created or read | 8 |
| lightning | `src/routing/gossip.rs` | imports (`AtomicU32`), `NetworkGraph::min_announcement_height` (new field, both constructors), `set_min_announcement_height` (new), `update_channel_from_unsigned_announcement_intern`, `tests::gossip_floor_ignores_old_channels` | `channel_announcement`s whose SCID block is below the floor are ignored (`IgnoreAndLog(Debug)`); default 0. Rapid gossip sync bypasses announcements and is not filtered | 8 |
| lightning | `src/ln/channelmanager.rs` | `OffersMessageHandler::handle_message` (`OffersMessage::Invoice` and `OffersMessage::StaticInvoice` arms) | On an XBT node an invoice without `option_blake2b` (either bit) abandons the payment (`PaymentFailureReason::UnexpectedError`, log "XBT: invoice without option_blake2b") before any HTLC and before `Event::InvoiceReceived` when invoices are handled manually. A BTC node refuses the mirror case, see the I3 row below | 8 |
| lightning | `src/ln/xbt_tests.rs` | `xbt_abandons_bolt12_invoice_without_blake2b` | An intercepted reply without the bit: `Event::PaymentFailed(UnexpectedError)`, no message events, no monitor update | 8 |
| fortis-lightning (not vendor) | `crates/fortis-lightning/src/pay.rs` | `map_send_error` | `WrongChain` shown as its own message instead of "pay: ..." | 8 |
| ldk-node | `src/liquidity.rs` | `build_jit_invoice` (new, from `LiquiditySource::lsps2_create_jit_invoice`), `tests xbt_jit_invoice_tests` | The LSPS2 JIT invoice is built by ldk-node, not `ChannelManager::create_bolt11_invoice`, so it lacked `option_blake2b` on XBT and the LND fork refused it ("invoice does not set option_blake2b"). Now `require_blake2b` when `config.xbt` is set. Found by the release gate (`jit_open`) | 10 |
| ldk-node | `src/payment/bolt11.rs` | `build_public_hints_invoice` (new, from `Bolt11Payment::invoice_with_public_hints`), `tests xbt_public_hints_invoice_tests` | Same for the invoice with public route hints (FORTIS PATCH), used by any node with a public channel (an LSP): `require_blake2b` when `config.xbt` is set. Found by the release gate | 10 |
| lightning | `src/offers/flow.rs` | `OffersMessageFlow::create_async_receive_offer_builder` | The async receive offer (`ChannelManager::get_async_receive_offer`, ldk-node `receive_async`) is built here from the static invoice server's paths, not by the patched builder macros, so it lacked `option_blake2b` and XBT payers refused it (`WrongChain`). Now `xbt_blake2b(self.xbt)`. Found by the release gate (`offer_while_stopped`) | 10 |
| lightning | `src/ln/xbt_tests.rs` | `xbt_static_invoice_carries_blake2b` | Also asserts the async receive offer requires `option_blake2b` | 10 |
| lightning | `src/ln/channel.rs`, `src/ln/xbt_tests.rs` | `ChannelContext::maybe_downgrade_channel_features` (V1 and V2 opens); `xbt_refused_open_closes_without_retry` | On XBT the last channel type is static_remote_key + unified_sigs (`get_initial_channel_type` re-adds unified_sigs), so "exhausted" compares against that. Before, every peer `error` re-sent the same `open_channel` and the channel never closed. Found by the final review (I1) | final |
| lightning | `src/ln/channelmanager.rs` | `OffersMessageHandler::handle_message` (`OffersMessage::Invoice` and `OffersMessage::StaticInvoice` arms) | The mirror of the XBT check: a BTC node abandons a payment whose invoice carries `option_blake2b` (either bit; `PaymentFailureReason::UnexpectedError`, log "BTC: invoice with option_blake2b") before any HTLC and before `Event::InvoiceReceived`. Upstream only refused bit 512 as an unknown required feature (and paid an invoice with only bit 513). Found by the final review (I3) | final |
| lightning | `src/ln/xbt_tests.rs` | `btc_abandons_offer_invoice_with_blake2b`, `btc_abandons_refund_invoice_with_blake2b`, `assert_invoice_abandoned` (shared with `xbt_abandons_bolt12_invoice_without_blake2b`) | BTC payer in the offer flow and in the refund flow receives an invoice with bit 512: `PaymentFailed(UnexpectedError)`, no HTLC | final |
| ldk-node | `src/builder.rs`, `bindings/ldk_node.udl` | `BuildError::InvalidChainSource` (new variant, `Display`, UDL enum), `build_with_store_internal` | With `xbt` set, a bitcoind (RPC or REST) or Electrum chain source is refused at build: they report SHA-256d block hashes, XBT needs the BLAKE2b index (Esplora API). No chain source (the Esplora default) is allowed. Found by the final review (I4) | final |
| ldk-node | `src/builder.rs` | `tests::xbt_refuses_electrum_chain_source`, `tests::xbt_refuses_bitcoind_chain_source`, `tests::xbt_accepts_esplora_chain_source` | Build fails with `InvalidChainSource` for Electrum and bitcoind; Esplora gets past the check | final |
| lightning | `src/ln/xbt_tests.rs` | `btc_splice_in_shared_input_plain` | A BTC pair's splice-in: the shared input's signatures are 0x01, 0x01 and verify against BIP 143 over all prevouts (the BTC counterpart of `xbt_splice_in_shared_input_unified`). Final review (M8) | final |

## Running the tests

The vendored crates are path-patched dependencies, not workspace members, so
`cargo test -p <crate>` fails ("not a member of the workspace", or "cannot
specify features for packages outside of workspace"). The root `Cargo.toml`
lists them under `workspace.exclude` so each can be tested on its own. Standalone,
the root `[patch.crates-io]` does not apply, so each command names the patched
crates it needs with `--config` overrides. `bitcoin` is the crates.io crate
(the vendored copy was removed in Task 7), so it needs no override.

lightning (needs `requires_unified_sigs` from `lightning-types` and
`require_blake2b` from `lightning-invoice`):

```
cargo test --manifest-path vendor/lightning-0.2.7/Cargo.toml \
  --config 'patch.crates-io.lightning-types.path="vendor/lightning-types-0.3.5"' \
  --config 'patch.crates-io.lightning-invoice.path="vendor/lightning-invoice-0.34.1"' --lib
```

ldk-node:

```
cargo test --manifest-path vendor/ldk-node/Cargo.toml \
  --config 'patch.crates-io.lightning.path="vendor/lightning-0.2.7"' \
  --config 'patch.crates-io.lightning-types.path="vendor/lightning-types-0.3.5"' \
  --config 'patch.crates-io.lightning-invoice.path="vendor/lightning-invoice-0.34.1"' \
  --config 'patch.crates-io.lightning-transaction-sync.path="vendor/lightning-transaction-sync-0.2.7"' \
  --config 'patch.crates-io.esplora-client.path="vendor/esplora-client-0.12.3"' --lib
```

esplora-client (the XBT tests need no server; the crate's other tests need
electrsd):

```
cargo test --manifest-path vendor/esplora-client-0.12.3/Cargo.toml --features blocking,async --lib xbt::
```

lightning-transaction-sync (the XBT test runs its own mock Esplora; it is
built only with `esplora-blocking`):

```
cargo test --manifest-path vendor/lightning-transaction-sync-0.2.7/Cargo.toml --features esplora-blocking \
  --config 'patch.crates-io.lightning.path="vendor/lightning-0.2.7"' \
  --config 'patch.crates-io.lightning-types.path="vendor/lightning-types-0.3.5"' \
  --config 'patch.crates-io.lightning-invoice.path="vendor/lightning-invoice-0.34.1"' \
  --config 'patch.crates-io.esplora-client.path="vendor/esplora-client-0.12.3"' --lib
```

## Release gate

`tools/xbt-regtest/run.sh` must pass for any change under `vendor/`, before
it is merged or released. It is the only check that the patched crates
produce transactions Knots accepts (BLAKE2b active at height 20) and that
they interoperate with paulscode's LND fork; the unit and functional tests
above do not touch a real chain.

```
cargo build -p fortis-index            # run.sh also builds it
tools/xbt-regtest/run.sh               # exit code 0 = pass
```

It builds the LND fork once (`build-lnd.sh`, needs `mise` with go 1.27.3,
`offersrpc` tag), wipes the regtest state under `$XR`
(`~/.cache/fortis-xbt-regtest`), starts Knots, fortis-index, the fee server
and LND (`up.sh`), runs `cargo run -p xbt-regtest -- all` and stops the
environment (`down.sh`) on any exit. Only the regtest ports in
`tools/xbt-regtest/env.sh` are used; nothing else on the host is touched.

The suite (`crates/xbt-regtest`) runs two ldk-node nodes, P (phone: LSPS2
client, async payments client) and S (LSP: LSPS2 service, async payments
server), both with `xbt: Some(XbtConfig { gossip_floor_height: 20,
splice_peers: [the other] })`, against LND, through the scenarios `jit_open`,
`pay_both_ways`, `splice_in`, `splice_out`, `bolt12_both_ways`,
`offer_while_stopped`, `coop_close`, `force_close_after_splice`,
`force_close_htlcs`, `penalty` and `sweeps_settle`. `force_close_after_splice`
(spec §4) splices a P-S channel and then force-closes it: the commitment
spends the splice output with [0x21, 0x21], and `sweeps_settle` checks that its
to_local (P) and to_remote (S) outputs are swept and confirmed and count in
the final accounting. Every transaction they make is checked as confirmed by
Knots with the expected signature hash type bytes per input (0x21 for every
signature on a unified channel and for the wallet's inputs; plain allowed
only where one side alone signs). The summary prints each scenario's result,
`BOLT12_INTEROP=pass|fail (...)` (LND offers; reported, does not fail the
gate) and every confirmed txid with its signature bytes. Every scenario must
be `PASS`; only `BOLT12_INTEROP` may be `fail`.

What `offer_while_stopped` proves, and what it does not: a held
`invoice_request` (the onion message S keeps for an offline P) is answered
after P restarts, and the LND payer then pays P. It is not a held HTLC: LND
pays only once P is back. Single-LSP held-HTLC async payment (S holding the
payer's HTLC until P comes online) is blocked in upstream LDK 0.2.7:
`ChannelManager::process_pending_update_add_htlcs` calls `can_forward_htlc`
before it holds the HTLC, and `can_forward_htlc_to_outgoing_channel` fails a
forward to a channel that is not live (`ChannelDisabled`).

### Known interop gap (Plan B)

`BOLT12_INTEROP=fail`: P paying an LND-fork offer fails. P routes
P -> S -> LND into the offer's blinded path (LND's path starts at LND itself,
padded with dummy hops), and LND refuses the HTLC: "unable
to decode forwarding instructions: onion payload for intermediate hop
insufficient record with type 4" (from LND's log, `[ERR] HSWC`, shown in the
summary); P ends with `RouteNotFound`. LND paying S's offer works. This does
not fail the gate; it is a known interop gap left for Plan B.
