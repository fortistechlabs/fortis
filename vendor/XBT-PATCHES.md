# XBT patches to vendored crates

Every change to the crates in this directory that make LDK 0.2.7 speak
Lightning on the Bitcoin BLAKE2b chain (XBT) is listed here, and each changed
line in the source carries a `// FORTIS XBT: <why>` comment. Spec:
`docs/superpowers/specs/2026-11-06-xbt-lightning-design.md`.

Rows marked "spike" were brought over from the spike worktree unchanged and
are reworked by the task named in the last column. Sighash call sites passing
`unified: false` are placeholders until Task 4 threads the channel's
`ChannelTypeFeatures` through.

| Crate | File | Function / item | Reason | Task |
|---|---|---|---|---|
| lightning | `src/xbt.rs` | whole module | Per-call unified sighash API (`SIGHASH_UNIFIED`, `hash_type`, `p2wsh_sighash`, `sighash_with_prevouts`, `push_sig`, `unified_message`, `is_unified`); no global state | 2 |
| lightning | `src/xbt_unified_sighash.json` | test vectors | Knots' `SignatureHashUnified` vectors (142 rows) | 2 |
| lightning | `src/lib.rs` | `pub mod xbt` | Exposes the module (spike) | 2 |
| lightning | `src/ln/chan_utils.rs`, `src/ln/channel.rs` | imports | Dropped `bitcoin::sighash`, unused once digests go through `crate::xbt` | 2 |
| lightning | `src/ln/chan_utils.rs` | `build_htlc_input_witness` | `push_sig(.., unified: false)` placeholder | 4 |
| lightning | `src/ln/chan_utils.rs` | `BuiltCommitmentTransaction::get_sighash_all`, `TrustedClosingTransaction::get_sighash_all`, `HolderCommitmentTransaction::add_holder_sig` | `p2wsh_sighash` / `push_sig` with `unified: false` placeholder | 4 |
| lightning | `src/ln/chan_utils.rs` | `TrustedCommitmentTransaction::get_htlc_sigs` | `p2wsh_sighash(.., unified: false)` placeholder | 4 |
| lightning | `src/ln/channel.rs` | `validate_commitment_signed` (HTLC signature check) | `p2wsh_sighash(.., unified: false)` placeholder | 4 |
| lightning | `src/ln/channel.rs` | `build_signed_closing_transaction` | `hash_type(.., unified: false)` placeholder | 4 |
| lightning | `src/sign/mod.rs` | `sign_counterparty_commitment`, `sign_holder_htlc_transaction` | `p2wsh_sighash(.., unified: false)` placeholder | 4 |
| lightning-types | `src/features.rs` | feature bits 512-515 (`option_blake2b`, `option_unified_sigs`), `requires_unified_sigs` | XBT feature bits (spike, kept) | 2 |
| lightning | `src/util/config.rs` | `XbtConfig`, `UserConfig::xbt`, `UserConfig::is_xbt`, `Default for UserConfig`, fuzzing `Readable for UserConfig` | Per-node XBT switch (`Option<XbtConfig>`, default `None`); replaces the spike's process-wide switch. `UserConfig` has no serialization outside fuzzing, so no TLV | 3 |
| lightning | `src/ln/channel.rs` | `channel_type_from_open_channel` (new `is_xbt` param; callers `InboundV1Channel::new`, `InboundV2Channel::new` and `ChannelManager::internal_open_channel` pass `config.is_xbt()`) | XBT refuses an `open_channel` whose type lacks `option_unified_sigs` or has any anchor bit (`ChannelError::close` mentioning "XBT") | 3 |
| lightning | `src/ln/channel.rs` | `get_initial_channel_type` | XBT proposes `unified_sigs` required, and never proposes either anchor type (the same function drives the fallback after a rejected type) | 3 |
| lightning | `src/ln/channelmanager.rs` | `provided_init_features` | XBT sets `blake2b` required and `unified_sigs` optional from `config.is_xbt()`, and does not advertise either anchor feature. `provided_node_features`, `provided_bolt11_invoice_features`, `provided_bolt12_invoice_features` and `provided_channel_type_features` derive from it, so they follow with no change of their own | 3 |
| lightning | `src/ln/channelmanager.rs` | `create_bolt11_invoice` | Calls `InvoiceBuilder::require_blake2b` when `is_xbt`. Phantom invoices (`ln/invoice_utils.rs` `create_phantom_invoice*`) have no config and are not patched | 3 |
| lightning | `src/ln/channelmanager.rs` | `create_offer_builder`, `create_offer_builder_using_router`, `create_refund_builder`, `create_refund_builder_using_router` (macros), `pay_for_offer_intern` | Set `blake2b` required on the offer, refund and invoice request via `xbt_blake2b(is_xbt)`. BOLT 12 invoices built by `OffersMessageFlow` (`create_response_for_invoice_request`, `create_invoice_builder_from_refund`) and static invoices are not patched; their features field stays empty | 3 |
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
