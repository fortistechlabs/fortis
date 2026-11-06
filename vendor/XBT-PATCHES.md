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
| lightning | `src/ln/channel.rs` | `channel_type_from_open_channel` | Refuses opens whose channel_type lacks unified sigs; uses global `lightning_types::xbt::enabled()` (was `crate::xbt::enabled`, re-export removed) | 3 |
| lightning | `src/ln/channel.rs` | `get_initial_channel_type` | Proposes unified sigs; uses global `lightning_types::xbt::enabled()` | 3 |
| lightning | `src/ln/channelmanager.rs` | `provided_init_features` | Sets `option_blake2b` from the global switch (spike) | 3 |
| lightning-types | `src/features.rs` | feature bits 512-515 (`option_blake2b`, `option_unified_sigs`), `requires_unified_sigs` | XBT feature bits (spike) | 3 |
| lightning-types | `src/lib.rs` | `pub mod xbt` | Process-wide switch (spike) | 3 |
| lightning-invoice | `src/lib.rs` | `InvoiceBuilder::payment_secret` | Sets `option_blake2b` in BOLT 11 (spike) | 3 |
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
  --config 'patch.crates-io.bitcoin.path="vendor/bitcoin-0.32.102"' --lib xbt::
```

- Standalone, the root `[patch.crates-io]` does not apply, so the two `--config`
  overrides point lightning at the patched `lightning-types` (it needs
  `requires_unified_sigs`) and `bitcoin`.
- `--features bitcoin/std` is needed because the spike's `lightning-types`
  calls `bitcoin::block::xbt_ids`, which exists only with bitcoin's `std`
  feature (the workspace build unifies it in; a standalone build does not).
- The `bitcoin` override and the `bitcoin/std` feature go away in Task 7, when
  the vendored `bitcoin` copy is removed.
