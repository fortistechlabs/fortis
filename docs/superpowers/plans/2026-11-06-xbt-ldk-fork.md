# XBT Lightning, Plan A: the LDK fork and its regtest gate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** LDK and ldk-node that run Lightning on XBT per lightning-blake2b/bolts.
That means per-channel unified signatures, the chain bits, splicing between our
own nodes, wallet inputs signed 0x21, and XBT block ids. The fork is proven by
an on-chain regtest suite against Knots and paulscode's LND fork.

**Architecture:**
- **Vendored crates:** the patched crates live in `vendor/` and are wired in
  through `[patch.crates-io]`, as `vendor/ldk-node` is today.
- **What decides XBT behaviour:**
  - the channel's `channel_type` (bit 514) decides how each signature is
    hashed;
  - an `Option<XbtConfig>` in LDK's `UserConfig` (mirrored in ldk-node's
    `Config`) decides everything else.
- **No global state:** a BTC node and an XBT node can share a process.
- **Starting point:** the spike's patches (worktree `~/Work/fortis-spike-xbt`,
  branch `spike/ldk-xbt`). This plan replaces their process-wide switch.

**Tech Stack:** Rust; LDK 0.2.7, lightning-types 0.3.5, lightning-invoice
0.34.1, lightning-transaction-sync 0.2.7, esplora-client 0.12.3, ldk-node
0.7.1 (already vendored); Knots 29.4.3 (`/opt/bitcoin-knots/current`);
paulscode/lightning-fork `v0.21.6-beta-blake2b.23` built with Go 1.27.3
(mise).

**Spec:** `docs/superpowers/specs/2026-11-06-xbt-lightning-design.md` (§1, the
fortis-index fix from §2, and the regtest part of §4).

**Follow-on plans:**
- **Plan B (written after this one lands):** the server side, meaning
  `fortis-lsp-xbt`, the edge's `chain` routing and the relay port.
- **Plan C:** the app.

## Global Constraints

- Work in a new worktree `~/Work/fortis-xbt` on branch `feat/xbt-ldk`, made
  from `main`. Do not touch `main` or the spike worktree.
- Every changed line in a vendored crate sits under a `// FORTIS XBT: <why>`
  comment. Every patched site is listed in `vendor/XBT-PATCHES.md`, with file,
  function and reason.
- **Constants:**
  - `option_blake2b` = bits 512/513, and the even bit (512) is always set.
  - `option_unified_sigs` = bits 514/515; set 514 in `channel_type`, and 515
    (optional) in init and node features.
  - `SIGHASH_UNIFIED` = `0x20`.
  - The unified ALL byte is `0x21`.
  - The mainnet gossip floor is `961640`; regtest uses `20`.
- **A signature is unified if and only if** the channel's
  `ChannelTypeFeatures::requires_unified_sigs()` is true. There are no global
  switches anywhere; `lightning_types::xbt` and `bitcoin::block::xbt_ids` from
  the spike are not carried over.
- Spends that only we sign (penalty claims, sweeps, our closing outputs) keep
  plain hash types.
- **On an XBT node, anchor channel types are never proposed and are refused.**
- Do not commit to `main`, push, or publish. Commit only on `feat/xbt-ldk`.
- The spike's `vendor/lightning-0.2.7/src/xbt_unified_sighash.json` (Knots'
  vectors) is the reference for every digest test.

## Review Focus

1. **Two nodes in one process:** an XBT node and a BTC node run in one
   process, and each must keep signing its own way (0x21 vs 0x01). The test
   lives in Task 4.
2. **A restarted node:** a node reloaded from disk after the channel opened
   must still sign 0x21 for that channel. The test lives in Task 4 (reload,
   then force close).
3. **BOLT11 invoices with no features field (no field 9) at all,**
   amountless ones included: an XBT node must refuse them as WrongChain. The
   test lives in Task 8.
4. **A splice where the counterparty also contributes inputs:** the digest must
   cover every prevout, both ours and theirs. The test lives in Task 5.
5. **Chain sync across the activation height**, where 80-byte headers come
   before it and 164-byte headers after: block ids must stay consistent. The
   test lives in Task 7.

---

### Task 1: fortis-index reports BLAKE2b difficulty

**Files:**
- Modify: `crates/fortis-index/src/api/esplora.rs` (`block_info_json`, about
  line 206; tests module about line 539)

**Interfaces:**
- Produces: `/blocks` and `/block/:hash` return `"difficulty"` as a number:
  - Knots' `difficulty` when it's a number;
  - otherwise `difficulty_blake2b`;
  - otherwise `0.0`.

- [ ] **Step 1: Write the failing tests**

  - `block_info_uses_blake2b_difficulty`: input JSON with
    `"difficulty": null, "difficulty_blake2b": 1.25e9` gives output
    `["difficulty"] == 1.25e9`.
  - `block_info_difficulty_defaults_to_zero`: with neither field, the output
    is `0.0`.

  Model the input on the fixture at about line 597.

- [ ] **Step 2: Run them and see them fail**

  Run: `cargo test -p fortis-index block_info_ -- --nocapture`
  Expected: both FAIL (`null` != number).

- [ ] **Step 3: Implement**

  Make the fallback described in Interfaces. The spike's version is in
  `~/Work/fortis-spike-xbt` (`git diff crates/fortis-index`) and can be copied
  as is.

- [ ] **Step 4: Run and see them pass**

  Run: `cargo test -p fortis-index`
  Expected: all PASS.

- [ ] **Step 5: Commit**

  `git commit -m "fortis-index: BLAKE2b blocks report difficulty_blake2b as difficulty"`
  (only `crates/fortis-index`). It must be a standalone commit, so it can be
  cherry-picked to `main` on its own.

### Task 2: Vendor the crates; a per-call unified sighash module

**Files:**
- Copy the following from `~/Work/fortis-spike-xbt/vendor/`:
  `lightning-0.2.7`, `lightning-types-0.3.5`, `lightning-invoice-0.34.1`,
  `lightning-transaction-sync-0.2.7`, `esplora-client-0.12.3`,
  `bitcoin-0.32.102`.
  - The `bitcoin` copy is temporary; Task 7 removes it.
  - Bring the spike's edits along; the following tasks rework them.
- Modify: `Cargo.toml` `[patch.crates-io]`: add the six crates (the spike's
  `Cargo.toml` has the exact lines).
- Rewrite: `vendor/lightning-0.2.7/src/xbt.rs`
- Create: `vendor/XBT-PATCHES.md`

**Interfaces (all in `lightning::xbt`; none reads global state):**
- `pub const SIGHASH_UNIFIED: u8 = 0x20;`
- `pub fn hash_type(base: EcdsaSighashType, unified: bool) -> u8`
- `pub fn p2wsh_sighash(tx: &Transaction, idx: usize, script_code: &Script, value: Amount, base: EcdsaSighashType, unified: bool) -> Message`:
  - `unified == false`: BIP143.
  - `unified == true`: the unified digest, with the single prevout
    `TxOut{value, script_code.to_p2wsh()}`.
  - It panics when `unified && tx.input.len() != 1 && base` lacks
    ANYONECANPAY.
- `pub fn sighash_with_prevouts(tx: &Transaction, prevouts: &[TxOut], idx: usize, script_code: &Script, base: EcdsaSighashType, unified: bool) -> Message`:
  the multi-input form, for splices and wallet inputs.
- `pub fn push_sig(w: &mut Witness, sig: &Signature, base: EcdsaSighashType, unified: bool)`
- `pub fn unified_message(tx, prevouts, idx, script_code, script_type: u8, hash_type: u8) -> [u8; 32]`:
  unchanged from the spike.
- `pub fn is_unified(ct: &ChannelTypeFeatures) -> bool`, which is
  `ct.requires_unified_sigs()`.

- [ ] **Step 1: Write the failing tests** (in `xbt.rs`)

  - `unified_message_matches_knots_vectors`: the spike's test. 142 vectors,
    `checked == 142`.
  - `p2wsh_sighash_bip143_when_not_unified`: equals
    `SighashCache::p2wsh_signature_hash` for a one-input transaction.
  - `sighash_with_prevouts_matches_vector_multi_input`: pick the first vector
    row with more than one input and script type 1. The result must equal
    that row's sighash.
  - `hash_type_bytes`: `hash_type(All, true) == 0x21`, and
    `hash_type(All, false) == 0x01`.

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p lightning --lib xbt::`
  Expected: compile errors, because of the new signatures.

- [ ] **Step 3: Implement xbt.rs to the Interfaces above**

  - Remove `pub use lightning_types::xbt::{enabled, set_enabled}`.
  - Leave the spike's call sites failing to compile.

  **Do not** delete `lightning_types::xbt` yet; Task 3 removes it.

- [ ] **Step 4: Make the crate build, then run the tests**

  Make the crate build by passing `unified: false` at every existing call
  site, each with a `// FORTIS XBT: TODO Task 4` comment.

  Run: `cargo test -p lightning --lib xbt::`
  Expected: PASS (4 tests).

- [ ] **Step 5: Write XBT-PATCHES.md and commit**

  Write `vendor/XBT-PATCHES.md`:
  - a header that names the spec;
  - a table with the columns crate, file, function, reason and task.

  Fill in the rows so far. Commit:
  `vendor: XBT LDK crates (LDK 0.2.7) and per-call unified sighash`

### Task 3: XbtConfig; feature bits; channel types without anchors

**Files:**
- Modify: `vendor/lightning-types-0.3.5/src/features.rs`
  - Keep the spike's `Blake2b` (513) and `UnifiedSigs` (515) definitions.
  - Add `Blake2b` to `OfferContext`, `InvoiceRequestContext` and
    `Bolt12InvoiceContext` (byte 64).
- Modify: `vendor/lightning-types-0.3.5/src/lib.rs`: delete `pub mod xbt`.
- Modify: `vendor/lightning-0.2.7/src/util/config.rs`
- Modify: `vendor/lightning-0.2.7/src/ln/channelmanager.rs`:
  - `provided_init_features`
  - `provided_node_features`
  - `provided_bolt11_invoice_features`
  - `provided_bolt12_invoice_features`
  - the offer and invoice_request builders' features
- Modify: `vendor/lightning-0.2.7/src/ln/channel.rs`:
  `get_initial_channel_type` and `channel_type_from_open_channel`.
- Modify: `vendor/lightning-invoice-0.34.1/src/lib.rs`: remove the spike's
  global check. The invoice's features come from ChannelManager.
- Modify: `vendor/lightning-0.2.7/src/offers/{offer,invoice_request,invoice}.rs`
  as needed, to set `Blake2b` from the builder.

**Interfaces:**
- `pub struct XbtConfig { pub gossip_floor_height: u32, pub splice_peers: Vec<PublicKey> }`
  in `lightning::util::config`. It needs `Clone`, `Debug`, `PartialEq` and
  `Default`. The default is `gossip_floor_height: 961640` and
  `splice_peers: vec![]`.
- `UserConfig.xbt: Option<XbtConfig>` (default `None`). If `UserConfig` is
  serialized, add it as an odd TLV.
- `pub fn UserConfig::is_xbt(&self) -> bool`
- **When `is_xbt`:**
  - init and node features: `blake2b` required, `unified_sigs` optional;
  - BOLT11 and BOLT12 invoice features: `blake2b` required;
  - offer and invoice request features: `blake2b` required;
  - proposed channel_type includes `unified_sigs` required and **no anchors**;
  - an `open_channel` whose type lacks 514, or has any anchors bit, is
    refused with a `ChannelError::close` mentioning "XBT".

- [ ] **Step 1: Write the failing tests**

  Put them in a new `vendor/lightning-0.2.7/src/ln/xbt_tests.rs`, registered
  under `#[cfg(test)]` in `ln/mod.rs`. Use LDK's functional test harness
  (`create_chanmon_cfgs`, `create_node_cfgs`,
  `create_node_chanmgrs(2, ..., &[Some(xbt_cfg()), Some(xbt_cfg())])`).

  - `xbt_init_features_set_bits`: `nodes[0].node.init_features()` requires
    blake2b and supports unified_sigs.
  - `xbt_channel_type_unified_no_anchors`: after
    `create_announced_chan_between_nodes`, the channel's type
    `requires_unified_sigs() && !supports_anchors_zero_fee_htlc_tx() && !supports_anchors_nonzero_fee_htlc_tx()`.
  - `xbt_refuses_open_without_unified`: two XBT nodes. Take node 0's
    `open_channel` message and set its `channel_type` to static_remote_key
    only before delivering it to node 1. (A BTC node can't be used here: it
    disconnects on the unknown even bit 512.) Node 1 must answer with an
    error containing "XBT".
  - `xbt_refuses_open_with_anchors`: as above, with the type set to
    `unified_sigs + anchors_zero_fee_htlc_tx`. The error must contain "XBT".
  - `btc_node_has_no_xbt_bits`: a default config has no bit 512 in init, and
    no bit 514 in channel_type.
  - `xbt_invoice_and_offer_carry_blake2b`:
    - `create_bolt11_invoice` features require blake2b;
    - `create_offer_builder().build()` offer features require blake2b.

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p lightning --lib ln::xbt_tests`
  Expected: FAIL or compile errors.

- [ ] **Step 3: Implement the Interfaces above**

  Make every bit decision read `self.config.is_xbt()` (or the passed-in
  `&UserConfig`) instead of the spike's global.

- [ ] **Step 4: Run the tests, then the whole crate**

  Run: `cargo test -p lightning --lib ln::xbt_tests` (expected PASS).
  Then run: `cargo test -p lightning --lib`. Expected: no new failures
  compared with an unpatched run. Record the baseline count in the commit
  message if any tests fail upstream.

- [ ] **Step 5: Update XBT-PATCHES.md and commit**

  `lightning: XbtConfig, chain feature bits, XBT channel types without anchors`

### Task 4: Per-channel unified signatures (create and verify)

**Files (every `// FORTIS XBT: TODO Task 4` site from Task 2):**
- `vendor/lightning-0.2.7/src/ln/chan_utils.rs`:
  - `build_htlc_input_witness`
  - the commitment `add_holder_sig`
  - both `get_sighash_all`
  - `get_htlc_sigs`
- `vendor/lightning-0.2.7/src/sign/mod.rs`:
  - `sign_counterparty_commitment` (HTLC sigs)
  - `sign_holder_htlc_transaction`
  - the holder commitment and closing signing
- `vendor/lightning-0.2.7/src/ln/channel.rs`:
  - HTLC signature verification (about line 16986)
  - commitment signature verification
  - the closing signature and witness
- `vendor/lightning-0.2.7/src/chain/channelmonitor.rs` (about line 7287): only
  if it builds a counterparty-checked signature. It's a plain spend otherwise;
  record the decision in XBT-PATCHES.md.

**Interfaces:**
- Consumes: `xbt::{p2wsh_sighash, push_sig, hash_type, is_unified}` (Task 2).
- **Rule:** each site gets `unified` from the `ChannelTypeFeatures` it already
  has, or can reach through `ChannelTransactionParameters` or
  `DirectedChannelTransactionParameters`. Thread it as a parameter where none
  is reachable. Never read config.

- [ ] **Step 1: Write the failing tests (append to `ln/xbt_tests.rs`)**

  Use a helper
  `fn assert_sig_bytes(tx: &Transaction, input: usize, expected: &[u8])`:
  it asserts that the last byte of each DER signature in the witness equals
  `expected[i]`. It must also check each signature with secp256k1 against
  `xbt::p2wsh_sighash(..., unified = expected[i] & 0x20 != 0)`, using the
  witness script and the funding or HTLC value.

  - `xbt_holder_commitment_signed_unified`:
    - route a payment and leave it pending;
    - get the holder commitment with `get_local_commitment_txn`;
    - check that input 0's two signatures are `[0x21, 0x21]`.
  - `xbt_htlc_timeout_signed_unified`:
    - force-close with the HTLC pending;
    - connect blocks past the CLTV;
    - in the broadcast HTLC-timeout transaction the two signatures are
      `[0x21, 0x21]` (anchors are off, so remote is ALL).
  - `xbt_htlc_success_signed_unified`: the same, on the receiving side after
    the preimage.
  - `xbt_coop_close_signed_unified`: the closing transaction's signatures are
    `[0x21, 0x21]`.
  - `btc_and_xbt_nodes_same_process`: four nodes in one test, two BTC and two
    XBT, with a channel in each pair. Commitments are `[0x01, 0x01]` and
    `[0x21, 0x21]` respectively. (Review Focus 1.)
  - `xbt_reloaded_node_still_unified`:
    - use `reload_node!` after the channel opens;
    - force close;
    - the commitment is `[0x21, 0x21]`. (Review Focus 2.)

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p lightning --lib ln::xbt_tests`
  Expected: the new tests FAIL with `0x01`.

- [ ] **Step 3: Implement the rule at every site**

  Grep for leftovers:
  - `grep -rn "p2wsh_signature_hash\|EcdsaSighashType::All as u8\|sighash_type: EcdsaSighashType" vendor/lightning-0.2.7/src`
  - every hit outside tests and `xbt.rs` must be either patched or listed in
    XBT-PATCHES.md as "plain spend, allowed".

- [ ] **Step 4: Run the tests and the crate suite**

  Run: `cargo test -p lightning --lib ln::xbt_tests` (PASS), then
  `cargo test -p lightning --lib`. Expected: no new failures against the Task 3
  baseline.

- [ ] **Step 5: Commit**

  `lightning: unified signatures decided by channel type`

### Task 5: Splicing on XBT: shared input, peer allow-list, prevouts for wallet inputs

**Files:**
- `vendor/lightning-0.2.7/src/sign/ecdsa.rs`: the trait
  `EcdsaChannelSigner::sign_splice_shared_input`.
- `vendor/lightning-0.2.7/src/sign/mod.rs`: `InMemorySigner` (about line
  1849), plus the test signers that implement the trait.
- `vendor/lightning-0.2.7/src/ln/channel.rs` (about line 9047): the call
  site, and the check of the counterparty's shared input signature.
- `vendor/lightning-0.2.7/src/ln/interactivetxs.rs`:
  - expose the prevouts;
  - shared-input witness hash-type bytes in `finalize`.
- `vendor/lightning-0.2.7/src/ln/channelmanager.rs`:
  - `splice_channel` (about line 4753)
  - `internal_splice_init` (about line 11872)
- `vendor/lightning-0.2.7/src/events/mod.rs`: `FundingTransactionReadyForSigning`.

**Interfaces:**
- `fn sign_splice_shared_input(&self, channel_parameters: &ChannelTransactionParameters, tx: &Transaction, input_index: usize, prevouts: &[TxOut], secp_ctx: &Secp256k1<All>) -> Signature`
  - `prevouts` lists every input of `tx`, in order, taken from the
    interactive transaction's input metadata.
  - It signs `xbt::sighash_with_prevouts(..., unified = is_unified(channel_type))`.
- `Event::FundingTransactionReadyForSigning` gains `prevouts: Vec<TxOut>` (every
  input, in order), serialized as the next free odd TLV.
- **Allow-list:** when `config.xbt` is `Some`, both `splice_channel` and
  `internal_splice_init` refuse a counterparty that is not in
  `xbt.splice_peers`:
  - `APIError::APIMisuseError` / tx_abort, with the message
    "splicing not allowed with this peer on XBT".
  - When `xbt` is `None`, behaviour is unchanged.

- [ ] **Step 1: Write the failing tests (append to `ln/xbt_tests.rs`)**

  Model them on `ln/splicing_tests.rs`, using two XBT nodes that list each
  other in `splice_peers`.

  - `xbt_splice_in_shared_input_unified`:
    - splice in with node 0 contributing a wallet input;
    - in the final splice transaction, the shared input's witness has two
      signatures, both `0x21`;
    - both verify against
      `sighash_with_prevouts(tx, all_prevouts, shared_idx, funding_script, All, true)`.
  - `xbt_splice_both_sides_contribute`:
    - both nodes contribute an input;
    - the same check holds, with all prevouts (three inputs). (Review Focus
      4.)
  - `xbt_splice_event_carries_prevouts`: in `FundingTransactionReadyForSigning`,
    `prevouts.len() == unsigned_transaction.input.len()`, and each value
    matches the input's source.
  - `xbt_splice_refused_for_unlisted_peer`:
    - with an empty `splice_peers`, `splice_channel` returns an error
      containing "splicing not allowed";
    - an inbound `splice_init` from the unlisted peer is answered with
      tx_abort.
  - `btc_splice_unchanged`: the existing `splicing_tests` still pass (this is
    run in Step 4).

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p lightning --lib ln::xbt_tests::xbt_splice`
  Expected: FAIL or compile errors.

- [ ] **Step 3: Implement the Interfaces above**

- [ ] **Step 4: Run the tests**

  Run: `cargo test -p lightning --lib ln::xbt_tests` and
  `cargo test -p lightning --lib ln::splicing_tests`.
  Expected: all PASS.

- [ ] **Step 5: Commit**

  `lightning: XBT splicing (shared input over all prevouts, peer allow-list)`

### Task 6: ldk-node: XBT config; wallet inputs signed 0x21

**Files:**
- `vendor/ldk-node/src/config.rs`: `Config`.
- `vendor/ldk-node/src/builder.rs`: pass the config into `UserConfig`.
- `vendor/ldk-node/src/wallet/mod.rs`:
  - `sign_owned_inputs` (about line 735)
  - `sign_psbt_inner` (about line 772)
  - the two `locked_wallet.sign` calls (about lines 253 and 519)
- `vendor/ldk-node/src/event.rs` (about line 1705): pass `prevouts`.
- Create: `vendor/ldk-node/src/wallet/xbt_sign.rs`

**Interfaces:**
- `ldk_node::config::XbtConfig { pub gossip_floor_height: u32, pub splice_peers: Vec<PublicKey> }`.
- `Config.xbt: Option<XbtConfig>` (default `None`). The builder maps it to
  `UserConfig.xbt`. It also sets `anchor_channels_config = None` when `Some`.
- `pub(crate) fn sign_xbt_p2wpkh(psbt: &mut Psbt, prevouts: &[TxOut], keys: &impl Fn(&bip32::DerivationPath) -> Option<SecretKey>, secp: &Secp256k1<All>) -> Result<usize, ()>`:
  - **Which inputs:** every P2WPKH input that has `bip32_derivation` and that
    `keys` can derive (that is, ours).
  - **Signature:** the witness is `[sig || 0x21, pubkey]`, with the digest
    from `lightning::xbt::sighash_with_prevouts(tx, prevouts, i, p2pkh_script_code(pubkey), All, true)`.
  - **Returns** the number of inputs it signed.
- **When `config.xbt` is `Some`,** every wallet spend uses `sign_xbt_p2wpkh`
  instead of BDK's signer: channel funding, splice contributions, sends to an
  address, and sweeps.
  - The prevouts come from the event for splices.
  - Otherwise every input belongs to the wallet, so they come from the
    wallet's UTXOs.
  - Keys come from the wallet's descriptor xprv (the same seed BDK uses).

- [ ] **Step 1: Write the failing tests (in `xbt_sign.rs`)**

  - `xbt_sign_p2wpkh_unified`:
    - build a two-input transaction spending wallet-owned P2WPKH outputs,
      from a test xprv and a BIP84 path;
    - after signing, each witness has two items, and the signature's last
      byte is `0x21`;
    - the signature verifies against the unified digest over both prevouts.
  - `xbt_sign_skips_foreign_inputs`: one input is not ours. The function
    returns `1`, and the foreign input's witness stays empty.
  - `btc_wallet_still_bdk`: with `xbt: None`, `sign_psbt_inner` gives
    signatures ending `0x01`. Use an existing ldk-node test wallet fixture if
    there is one; otherwise drive `Wallet` directly.

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p ldk-node --lib wallet::xbt_sign`
  Expected: FAIL or compile errors.

- [ ] **Step 3: Implement the Interfaces above**

  Cross-check the P2WPKH script code and the unified layout against
  `crates/wallet-core/src/sighash.rs` (`sighash_all`, `SighashVariant::Unified`).
  The resulting signature must match wallet-core's for the same transaction.
  Add the test `xbt_sign_matches_wallet_core` if `wallet-core` can be a
  dev-dependency of the vendored ldk-node through a path. Otherwise compare
  against a hex vector produced by `wallet-core` in a one-off test, and pin
  that vector.

- [ ] **Step 4: Run the tests**

  Run: `cargo test -p ldk-node --lib`
  Expected: no new failures.

- [ ] **Step 5: Commit**

  `ldk-node: XbtConfig; wallet spends signed with SIGHASH_UNIFIED on XBT`

### Task 7: XBT block ids from the index (removes the bitcoin crate patch)

**Files:**
- `vendor/esplora-client-0.12.3/src/{async.rs,blocking.rs}`: the spike's
  `xbt_header` and `xbt_merkle_block`.
- `vendor/lightning-transaction-sync-0.2.7/src/esplora.rs` (about line 354).
- `vendor/ldk-node/src/chain/esplora.rs`, and any other path that calls
  `Header::block_hash()` on an Esplora-sourced header (audit with
  `grep -rn "block_hash()" vendor/ldk-node/src vendor/lightning-transaction-sync-0.2.7/src`).
- Delete: `vendor/bitcoin-0.32.102`, and its `[patch.crates-io]` line.

**Interfaces:**
- **Header parsing:** `esplora_client` parses headers of 80 or 164 bytes from
  `/block/:hash/header`.
  - For 164 bytes, the `Header` is built from bytes 0..80.
  - Any other length is an error.
- **Hashes:**
  - `get_header_by_hash(hash)` keeps its signature.
  - Callers never recompute the hash of a header they got from it; they use
    the `hash` they asked for.
  - The tip hash comes from `/blocks/tip/hash`.
  - The hash of a confirmed transaction's block comes from
    `get_tx_status().block_hash`.
- **Merkle proofs:** `get_merkle_block` handles both header lengths (from the
  spike).

- [ ] **Step 1: Write the failing tests**

  - In esplora-client:
    - `parses_80_and_164_byte_headers`: both hex fixtures parse, and a
      100-byte one gives an error. Take the 164-byte fixture from regtest
      Knots (Task 10's environment, or a header from spike `run9.log`), and
      pin it as a constant.
    - `merkle_block_164_byte_header`: a pinned fixture, made the same way.
  - In lightning-transaction-sync: `xbt_sync_uses_index_hashes`. Use a mock
    `EsploraClient` (or the crate's test utilities) serving a 164-byte header
    whose `Header::block_hash()` differs from the served hash. The sync must
    report the served hash to `Confirm::best_block_updated`. (Review Focus 5:
    serve one 80-byte and then one 164-byte header in sequence.)

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p esplora-client` and
  `cargo test -p lightning-transaction-sync --features esplora-blocking`.

- [ ] **Step 3: Implement, then delete the bitcoin vendor copy**

  Implement the Interfaces above, then delete `vendor/bitcoin-0.32.102` and
  its patch line. Then run `cargo update -p bitcoin --precise 0.32.102`, only
  if the lock file changes version.

- [ ] **Step 4: Run the tests and build the workspace**

  Run the Step 2 commands (PASS), then `cargo build --workspace` (succeeds).

- [ ] **Step 5: Commit**

  `esplora/tx-sync: XBT block ids from the index; drop the bitcoin crate patch`

### Task 8: Wrong-chain refusals and the gossip floor

**Files:**
- `vendor/ldk-node/src/error.rs`
- `vendor/ldk-node/src/payment/bolt11.rs`: `send`, `send_using_amount`,
  `send_probes*`.
- `vendor/ldk-node/src/payment/bolt12.rs`: `send`, `send_using_amount`.
- `vendor/lightning-0.2.7/src/ln/outbound_payment.rs`: or the offers flow
  where a received `Bolt12Invoice` is accepted for payment.
- `vendor/lightning-0.2.7/src/routing/gossip.rs`:
  `update_channel_from_unsigned_announcement_intern` (about line 2164), plus
  a setter.
- `vendor/ldk-node/src/builder.rs`: call the setter.
- `vendor/ldk-node/src/lib.rs` or `vendor/ldk-node/bindings/ldk_node.udl`: if
  the Error enum is exported there, add the variant there too.

**Interfaces:**
- `ldk_node::Error::WrongChain`, displayed as "This invoice or offer is for the
  other Bitcoin chain."
- **The check:** an XBT node (`config.xbt.is_some()`) refuses with
  `WrongChain` any BOLT11 invoice or BOLT12 offer whose features lack bit 512.
  A BTC node refuses any whose features *have* bit 512. In both cases the
  refusal comes before any HTLC is sent.
- **BOLT12 invoices:** in LDK, a `Bolt12Invoice` received for an XBT node's
  invoice request without bit 512 in `invoice_features` abandons the payment
  with `PaymentFailureReason::UnexpectedError`. Log "XBT: invoice without
  option_blake2b".
- **Gossip floor:**
  - `NetworkGraph::set_min_announcement_height(&self, height: u32)`, default
    0;
  - `channel_announcement`s whose SCID block height is below it are ignored
    with `LightningError { action: IgnoreAndLog(Debug) }`;
  - ldk-node sets it to `xbt.gossip_floor_height` when `xbt` is `Some`.

- [ ] **Step 1: Write the failing tests**

  In ldk-node (unit, using fixture invoices built with lightning-invoice's
  builder and a fixed key):
  - `xbt_refuses_btc_invoice`
  - `btc_refuses_xbt_invoice`
  - `xbt_refuses_invoice_without_feature_field`: no field 9, amountless.
    (Review Focus 3.)
  - `xbt_refuses_btc_offer`

  Test against a function
  `pub(crate) fn check_chain_bolt11(inv: &Bolt11Invoice, xbt: bool) -> Result<(), Error>`
  and `check_chain_offer(offer: &Offer, xbt: bool)`, called first by every
  send.

  In lightning, `gossip.rs` tests:
  - `gossip_floor_ignores_old_channels`: with the floor at 961640, an
    announcement with an SCID at block 961639 is ignored, and one at 961640 is
    accepted.

  In `ln/xbt_tests.rs`:
  - `xbt_abandons_bolt12_invoice_without_blake2b`:
    - two XBT nodes; node 0 starts paying node 1's offer;
    - intercept node 1's `Bolt12Invoice` reply and replace it with one signed
      by node 1's offer key whose `invoice_features` lack bit 512;
    - deliver it to node 0;
    - the payment is abandoned (`Event::PaymentFailed`) and no HTLC is sent.

- [ ] **Step 2: Run and see them fail**

  Run: `cargo test -p ldk-node --lib check_chain`,
  `cargo test -p lightning --lib gossip_floor`, and
  `cargo test -p lightning --lib xbt_abandons`.

- [ ] **Step 3: Implement the Interfaces above**

- [ ] **Step 4: Run them all and see them pass**

  Then run `cargo test -p lightning --lib` and `cargo test -p ldk-node --lib`.
  Expected: no new failures.

- [ ] **Step 5: Commit**

  `XBT: refuse other-chain invoices and offers; gossip floor`

### Task 9: Regtest environment scripts

**Files:**
- Create: `tools/xbt-regtest/env.sh`
- Create: `tools/xbt-regtest/up.sh`
- Create: `tools/xbt-regtest/down.sh`
- Create: `tools/xbt-regtest/build-lnd.sh`
- Create: `tools/xbt-regtest/README.md`

**Interfaces (`env.sh`, sourced by the others and by Task 10's runner):**
- `XR` = `${XBT_REGTEST_DIR:-$HOME/.cache/fortis-xbt-regtest}`
- **Knots:** datadir `$XR/knots`, RPC `127.0.0.1:18543` (user `lab` / password
  `lab`), P2P `18544`, ZMQ `28541` (rawblock) and `28542` (rawtx).
  - `bitcoin.conf`:
    `regtest=1, server=1, txindex=1, blockfilterindex=1, fallbackfee=0.0002, testactivationheight=blake2b@20`
    and the `[regtest]` keys from spec §4 and the spike: rpcuser, rpcpassword,
    rpcport, port, bind/rpcbind/rpcallowip 127.0.0.1, the zmq ports,
    `listenonion=0`, `discover=0`.
- **fortis-index:** `--rpc-url http://127.0.0.1:18543 --rpc-auth lab:lab --network regtest --db $XR/index-db --bind 127.0.0.1:18094`
- **Fee file:** `python3 -m http.server 18095` in `$XR/fees`, serving
  `fees.json` = `{"fee_by_block_target":{"1":1000,"6":1000,"144":1000}}`.
- **LND fork:** `$XR/bin/lnd`, with:
  ```
  --lnddir=$XR/lnd-data --noseedbackup --bitcoin.regtest --bitcoin.node=bitcoind
  --bitcoin.blake2b-activation-height=20 --fee.url=http://127.0.0.1:18095/fees.json
  --bitcoind.rpchost=127.0.0.1:18543 --bitcoind.rpcuser=lab --bitcoind.rpcpass=lab
  --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28541 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28542
  --rpclisten=127.0.0.1:10109 --norest --listen=127.0.0.1:19735 --alias=lf-xbt
  --accept-keysend --protocol.wumbo-channels
  ```
- **`build-lnd.sh`:**
  - clones `https://github.com/paulscode/lightning-fork` at tag
    `v0.21.6-beta-blake2b.23` into `$XR/src/lightning-fork`;
  - builds with `mise exec go@1.27.3 -- go build -tags "signrpc walletrpc chainrpc invoicesrpc routerrpc peersrpc"`
    into `$XR/bin/{lnd,lncli}`;
  - skips both when the binaries exist.
- **`up.sh`:**
  - **Starts, in order:** Knots (`/opt/bitcoin-knots/current/bin/bitcoind`),
    then the index (`target/debug/fortis-index`), the fee server and LND.
  - **Waits for each:** RPC answering, `/blocks` returning, `lncli getinfo`
    succeeding.
  - **Mines to height 101.**
  - **Writes PIDs** to `$XR/pids`.
  - **Can run twice** without starting anything twice.
- **`down.sh`:** stops only the PIDs in `$XR/pids`.
  - **Never `pkill -f`** (that killed the session's own shell in the spike),
    and never touches the production `bitcoin-knots` or index units.
- **Port check:** `up.sh` refuses to start if any of 18543, 18544, 18094,
  18095, 10109 or 19735 is already in use by a process it didn't start.

- [ ] **Step 1: Write `env.sh`, `build-lnd.sh`, `up.sh`, `down.sh` and `README.md`**

  Write them to the Interfaces above.

- [ ] **Step 2: Verify**

  Run: `tools/xbt-regtest/build-lnd.sh && tools/xbt-regtest/up.sh && curl -s 127.0.0.1:18094/blocks | head -c 100 && tools/xbt-regtest/down.sh`
  Expected: JSON blocks at height 101 with a numeric `difficulty`. After
  `down.sh`, `pgrep -f "$XR"` is empty, and `systemctl is-active bitcoin-knots`
  is unchanged.

- [ ] **Step 3: Commit**

  `tools: XBT regtest environment (Knots BLAKE2b, index, LND fork)`

### Task 10: The XBT regtest suite (the release gate)

**Files:**
- Create: `crates/xbt-regtest/` (workspace member, `publish = false`), from the
  spike's `crates/xbt-spike`:
  - `src/main.rs`: CLI and scenario selection;
  - `src/env.rs`: the helpers `btc`, `ln`, `mine`, `wait_for`,
    `wait_lnd_active`, `send_retry` and `show_sighash_types`, reading ports
    from Task 9's values;
  - `src/check.rs`: on-chain assertions;
  - `src/scenarios.rs`.
- Create: `tools/xbt-regtest/run.sh`: runs `up.sh`, then
  `cargo run -p xbt-regtest -- all`, then `down.sh`. The exit code is the
  suite's.

**Interfaces:**
- **Nodes:**
  - **Node P** ("phone"): ldk-node with `xbt: Some({gossip_floor_height: 20, splice_peers: [S]})`,
    with an LSPS2 client pointed at node S.
  - **Node S** ("LSP"): ldk-node with LSPS2 service and
    `splice_peers: [P]`.
  - **LND:** the fork.
  - P and S use Esplora `http://127.0.0.1:18094`, each with its own storage
    under `$XR/ldk-{p,s}`.
- **`check::confirmed_with(txid, expected: &[&[u8]])`:**
  - fetches the transaction from Knots;
  - asserts `confirmations >= 1`;
  - asserts each input's DER signatures' last bytes equal
    `expected[input]`. Use `&[]` for "plain, not checked".
- **Pass condition:** every scenario's transactions are confirmed by Knots,
  with the expected bytes. At the end, P's and S's lightning balances are
  empty after closing, and their on-chain balances are within
  (fees + 1,000 sat) of the expected amounts.
  - Account for sweeps by confirmed txid, not by ldk-node's
    "awaiting confirmations" list (that list lagged in the spike).

- [ ] **Step 1: Write the scenarios**

  Each one is a function `fn <name>(env: &Env) -> Result<(), String>`, run in
  this order:
  1. `jit_open`: P creates an LSPS2 JIT invoice and LND pays it. Check: the
     funding transaction is confirmed and P received the amount minus the
     LSPS2 fee.
  2. `pay_both_ways`: P pays an LND invoice; LND pays a P invoice through S.
     This is a JIT-path route hint; it resolves the spike's alias-hint risk.
  3. `splice_in`: P splices in 50,000 sat. Check: the splice transaction's
     shared input is `[0x21, 0x21]` and P's wallet input is `[0x21]`.
  4. `splice_out`: P splices out 30,000 sat to its own wallet. Check: the
     shared input is `[0x21, 0x21]`.
  5. `bolt12_both_ways`: P pays an LND offer (`lncli` fork offer command), and
     LND pays S's offer.
     - On failure, record `BOLT12_INTEROP=fail` in the summary.
     - It doesn't fail the suite (spec risk "Payable from Fortis wallets").
     - It does fail if P↔S offers fail.
  6. `offer_while_stopped`: S pays P's async offer while P is stopped; P
     starts and claims.
     - Only if ldk-node 0.7.1's async-payments role can be set up between P
       and S as the BTC Plan 3 does.
     - Otherwise mark it `SKIPPED (Plan B covers with fortis-lsp)`.
  7. `coop_close`: P↔S cooperative close. Check: `[0x21, 0x21]`.
  8. `force_close_htlcs`:
     - P opens to LND (static_remote_key);
     - P pays a hold invoice and LND holds it;
     - P force-closes and the timeout path runs. Check: the commitment
       `[0x21, 0x21]` and the HTLC-timeout `[0x21, 0x21]`.
     - A second hold invoice is settled after force close, for the success
       path on LND's side. Its success transaction is checked with the LND
       signature bytes `[0x21, 0x21]`.
  9. `penalty`:
     - stop S and copy `$XR/ldk-s`;
     - restart S, have P pay S twice, then stop S;
     - restore the copy, start S and force-close (a revoked state);
     - P's justice transaction is confirmed (`&[]`, plain allowed), and P
       ends with the channel balance minus fees.
  10. `sweeps_settle`: mine up to 60 × 10 blocks until P and S have no
      lightning balances. Check the on-chain totals.

- [ ] **Step 2: Run the suite**

  Run: `tools/xbt-regtest/run.sh`
  Expected: the summary lists every scenario as `PASS`. Only
  `BOLT12_INTEROP` and `offer_while_stopped` may show `fail`/`SKIPPED` as
  described above. Exit code 0.

- [ ] **Step 3: Fix any failure at its root**

  Fix it in the vendored crate, with a unit test added to the matching task's
  test module. Re-run until it passes.

- [ ] **Step 4: Document the gate**

  Add to `vendor/XBT-PATCHES.md` a section "Release gate". It says that
  `tools/xbt-regtest/run.sh` must pass for any change under `vendor/`, and
  how to run it.

- [ ] **Step 5: Commit**

  `xbt-regtest: on-chain suite for the XBT LDK fork (release gate)`
