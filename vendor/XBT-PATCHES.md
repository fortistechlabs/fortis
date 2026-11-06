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
| lightning | `src/sign/mod.rs`, `src/ln/interactivetxs.rs` | `sign_splice_shared_input`; shared input witness and peer signature checks | Splice shared input: Task 5 | 5 |
| lightning | `src/util/test_utils.rs`, `src/ln/monitor_tests.rs` | wallet test signer; a monitor test's hand-built HTLC claim | Test-only, BTC channels; plain | - |
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
| lightning-types | `src/lib.rs` | `pub mod xbt` removed | The process-wide switch is gone. `bitcoin::block::xbt_ids::set_enabled` now has no caller, so esplora's spike block-id recording stays off until Task 7 | 3 |
| lightning-invoice | `src/lib.rs` | `InvoiceBuilder::payment_secret` (global check removed), `InvoiceBuilder::require_blake2b` (new) | The invoice's `blake2b` bit is set by the caller (ChannelManager), not a global | 3 |
| lightning-transaction-sync | (crate copied, patched in) | - | Needed so the workspace resolves one set of patched crates | 7 |
| esplora-client | `src/blocking.rs`, `src/async.rs` | `xbt_header` | 164-byte BLAKE2b headers (spike) | 7 |
| bitcoin | `src/blockdata/block.rs` | `xbt_ids`, `Header::block_hash` | Block id for BLAKE2b headers (spike); temporary | 7 |

## Running the tests

`vendor/lightning-0.2.7` is a path-patched dependency, not a workspace member,
so `cargo test -p lightning` fails ("not a member of the workspace" with
dev-dependencies). The root `Cargo.toml` lists it under `workspace.exclude` so
it can be tested on its own:

```
cargo test --manifest-path vendor/lightning-0.2.7/Cargo.toml --features bitcoin/std \
  --config 'patch.crates-io.lightning-types.path="vendor/lightning-types-0.3.5"' \
  --config 'patch.crates-io.bitcoin.path="vendor/bitcoin-0.32.102"' \
  --config 'patch.crates-io.lightning-invoice.path="vendor/lightning-invoice-0.34.1"' --lib xbt::
```

- Standalone, the root `[patch.crates-io]` does not apply, so the three `--config`
  overrides point lightning at the patched `lightning-types` (it needs
  `requires_unified_sigs`), `bitcoin` and `lightning-invoice` (`require_blake2b`).
- `--features bitcoin/std` is needed because the spike's `lightning-types`
  calls `bitcoin::block::xbt_ids`, which exists only with bitcoin's `std`
  feature (the workspace build unifies it in; a standalone build does not).
- The `bitcoin` override and the `bitcoin/std` feature go away in Task 7, when
  the vendored `bitcoin` copy is removed.
