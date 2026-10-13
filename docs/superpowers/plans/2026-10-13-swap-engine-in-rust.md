# Swap engine in the Rust core — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Android swap engine (Kotlin) runs instead as one async Rust engine in `crates/swap-engine`, called through UniFFI, with identical behaviour; the Kotlin engine is then deleted.

**Architecture:** A runtime-free async engine owns the swap record, the store logic and recovery; everything it touches outside goes through ports (traits) the platform implements — on Android the existing OkHttp clients, DataStore blob, Keystore and wallet funder. The Kotlin tests become Rust tests over the same fakes first; a debug switch runs either engine until the on-phone tests pass.

**Tech Stack:** Rust (async traits via `async-trait`, `async-lock`, serde_json, `wallet-core`), UniFFI 0.29 async exports and foreign traits, Kotlin (adapters, Gobley build).

**Spec:** `docs/superpowers/specs/2026-10-13-swap-engine-in-rust-design.md`

## Rulings against the spec (owner told)

- **`Send` on native, not on wasm.** UniFFI's foreign traits and exported futures must be `Send + Sync`; the browser's must not. The ports use `#[cfg_attr(not(target_arch = "wasm32"), async_trait)]` / `#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]` — the spec's "no `Send` bound" holds for wasm only.
- **The funder port splits in two.** Today the Kotlin funder signs, saves the pending tx into the store, then broadcasts. The store moves to Rust, so the port is `sign_funding(record, htlc) -> Option<SignedTx>` and `broadcast_funding(record, hex)`, with the engine saving `pending_funding_txid`/`pending_funding_hex` in between — the same order as now.
- **Recovery's wallet work goes through a `RecoveryHost` port** (tags, wrapped keys, addresses, advancing a receive index): the search moves to Rust as the spec says, while seeds and addresses stay with the platform.

## Global Constraints

- Behaviour identical to the Kotlin engine: same server calls, order, client ids, 409/404/410 handling, marks, steps, events, and the same stored JSON byte for byte.
- No tokio, no threads, no blocking in `swap-engine`; per-swap and store locks are `async_lock::Mutex`.
- Stored record JSON keys (exact): `id from_wallet_id to_wallet_id send_chain amount_sat limit_price owner_secret swap_index account wrapped_xbt wrapped_btc relay_seq presigned step redeem_address refund_address marks match_id role session terms pending_funding_txid pending_funding_hex rejected_fundings funding_txid funding_vout deadline error their_xbt their_btc hashlock share_code created_at client_id private seed recovery_tag preimage_nonce release_txs release_sigs release_signed_txs their_txid their_vout match_status redeem_txid refund_txid`; absent ⇒ Kotlin's default; unknown `step` ⇒ `FAILED`; `toString`/`Debug` redacts `owner_secret wrapped_xbt wrapped_btc pending_funding_hex`.
- Mark strings (exact): `accepted remainder_cancelled counterparty funded_posted refund_watched redeem_watched release_asked released release_signed release_none` and the prefixes `close:` / `walk_away:`.
- Swap secrets fetched through `SwapKeys` are zeroised once the session is built.
- Only `.dev` / `.dev2` are installed on the Pixel, package id checked before each install; never commit (the owner commits).

## Review Focus

1. A record saved by the Kotlin engine mid-swap (every step, every mark) continues under the Rust engine from the same step — `kotlin_saved_records_continue_under_the_rust_engine` (Task 9).
2. A step cancelled mid-way (the runner's 5-minute limit, the service stopping) leaves the record consistent and the next step resumes — `a_cancelled_step_leaves_the_record_consistent` (Task 4).
3. A Kotlin port that throws (network, HTTP status) surfaces in Rust as the same error class the engine branches on (`SwapHttpException(code)` ⇒ `PortError::Http{code}`) — `http_errors_cross_the_boundary_with_their_code` (Task 9).
4. Two runner passes stepping the same swap at once never interleave (per-swap lock) — `concurrent_steps_of_one_swap_serialise` (Task 4).
5. The funder signing while the store is written by another swap's step loses neither write — `funding_save_and_another_swaps_save_both_survive` (Task 5).

---

### Task 1: Proof — async Rust calling an async Kotlin port, on the phone

**Files:** Create `crates/wallet-ffi/src/async_proof.rs` (debug feature `async-proof`); Kotlin `android/app/src/debug/kotlin/com/fortis/wallet/AsyncProofActivity.kt`.

**Interfaces — Produces:** a go/no-go recorded in the ledger. Nothing later depends on its code (deleted in Task 11).

- [ ] A UniFFI foreign trait `ProofPort { async fn fetch(&self, n: u32) -> Result<u32, ProofError> }` and an exported `async fn proof_sum(port: Arc<dyn ProofPort>, n: u32) -> Result<u32, ProofError>` that awaits `fetch` n times.
- [ ] The debug activity implements `fetch` with a real OkHttp call to `https://api.fortistechlabs.com/swap/quote` (a `delay` in between), and checks on the phone, logging PASS/FAIL to logcat tag `AsyncProof`: (a) 50 sequential calls return the right sum; (b) a Kotlin exception becomes `ProofError` with its message; (c) cancelling the Kotlin coroutine mid-call cancels the Rust future (no further `fetch`); (d) it runs from a `WorkManager` worker with the app in the background.
- [ ] Build, install on `.dev`, run; Expected: four PASS lines. If any FAIL: stop, ledger it, re-plan with the owner.

### Task 2: Crate, record types, exact JSON

**Files:** Create `crates/swap-engine/{Cargo.toml,src/lib.rs,src/record.rs}`; workspace `Cargo.toml` member; Test `crates/swap-engine/tests/record_json.rs` + fixtures `crates/swap-engine/tests/fixtures/records/*.json`.

**Interfaces — Produces:** `pub struct SwapRecord { … }` (fields = the Kotlin `SwapRecord`, snake_case, same types: `i64` sat/secs, `i32` index/vout, `Vec<String>`, `BTreeSet<String>` marks); `pub enum Step { Provisional, WaitingMatch, Matched, NeedFunding, Funded, TheyFunded, Redeemed, Done, Refunding, Refunded, Cancelled, Failed }` with `is_active()`; `pub enum Event { Matched, NeedFunding, TheyFunded, Done, Failed, Refunding }`; `SwapRecord::from_json(&str) -> Result<Self>` / `to_json(&self) -> String`; `recv_chain()`.

- [ ] Fixtures: pull the live swap stores from `.dev` and `.dev2` (`run-as … cat files/datastore/fortis_swaps.preferences_pb`), keep each record's JSON, replace `owner_secret`, `wrapped_xbt`, `wrapped_btc`, `pending_funding_hex` with fixed fakes; plus one record per `Step` and one with every optional field, made by the Kotlin `SwapRecord.toJson()` in a throwaway JVM test.
- [ ] Tests: `records_round_trip_byte_for_byte` (every fixture: `to_json(from_json(s)) == s` after Kotlin-identical key order), `absent_fields_take_kotlin_defaults`, `an_unknown_step_reads_as_failed`, `debug_redacts_secrets`. Run `cargo test -p swap-engine` → fail; implement with a hand-written serialiser matching Kotlin's key order; → pass.

### Task 3: Ports and test fakes

**Files:** Create `crates/swap-engine/src/ports.rs`, `src/errors.rs`, `crates/swap-engine/src/testkit/{mod.rs,api.rs,chain.rs,check.rs,funder.rs,crypto.rs}` (behind `cfg(any(test, feature = "testkit"))`).

**Interfaces — Produces:** traits `SwapApi`, `ChainWatch`, `ChainCheck`, `Blob { read() -> Result<Option<String>>, write(String) -> Result<()> }`, `Funder { sign_funding(&SwapRecord, &HtlcInfo) -> Result<Option<SignedTx>>, broadcast_funding(&SwapRecord, String) -> Result<()> }`, `SwapKeys { secrets(&SwapRecord) -> Result<Zeroizing<([u8;32],[u8;32])>> }`, `Clock { now() -> i64 }`, `Notifier { event(&SwapRecord, Event) }`, `RecoveryHost` (Task 8), each method as the Kotlin interface listed in the spec's port table; `PortError { Http { code: u16, message: String }, Io(String), Other(String) }`; internal `SwapCrypto` trait = the Kotlin `SwapCrypto` methods. The session logic now in `crates/wallet-ffi/src/swap.rs` (`Persisted`, every `SwapSession` method) moves to `crates/swap-engine/src/session.rs` as plain Rust (`swap-engine` cannot depend on the UniFFI crate); `wallet-ffi`'s `SwapSession` becomes a thin wrapper over it, and `crates/wallet-ffi/tests/swap_session.rs` passes unchanged. `RealCrypto` implements `SwapCrypto` over it. Testkit: `FakeApi`, `FakeChain`, `FakeChainCheck`, `FakeFunder`, `FakeCrypto` — line-for-line ports of the Kotlin fakes in `SwapEngineTest.kt` (lines 49–720), including their `fail*`/`refuse*`/`blind`/`down`/`unseen`/`lag` switches.
- [ ] Tests (ported from the Kotlin file): `edge_fake_refuses_a_watch_of_anything_but_the_exact_htlc`, `edge_fake_never_records_a_funding_from_a_redeem_watch`; Run → fail; implement; → pass.

### Task 4: Store and the engine skeleton (offers, matching, review, refollow)

**Files:** Create `src/store.rs`, `src/engine.rs` (public surface), `src/engine/run.rs`; Test `crates/swap-engine/tests/engine_offers.rs`, `tests/store.rs`.

**Interfaces — Produces:** `SwapStore::new(Arc<dyn Blob>)` with `all get find put update promote remove next_swap_index release_swap_index bump_swap_index token set_token due(now)` (Kotlin semantics; `update`/`promote` take `FnOnce(SwapRecord) -> SwapRecord`); `SwapEngine::new(api, store, chain, independent, crypto_factory, funder, keys, clock, notifier)` with `async fn step(&self, id) -> Result<SwapRecord>`, `post_offer(client_id)`, `cancel_provisional(client_id) -> bool`, `note(id, msg)`.
- [ ] Port these Kotlin tests (same names, snake_case), assertions unchanged: store — `store_round_trips_records_and_counts_swap_indices`, `provisional_fields_round_trip`, `record_to_string_redacts_secrets`, the four `SwapIndexTest` and four `WithFreshIndexTest` cases; offers and matching — `quick_offer_to_done_initiator`/`_participant` (kept failing until Task 6), `declines_initiator_terms_below_its_limit`, `accepts_terms_at_limit`, `declines_terms_worse_than_limit`, `wrong_fee_address_declines`, `keys_without_accept_do_not_fund`, `decline_ends_the_decliner_and_the_other_side_refollows`, `partial_fill_accept_cancels_the_remainder`, `pricing_endpoint_error_retries_without_declining`, `pricing_unreachable_retries_then_declines_near_accept_by`, every test under `// --- provisional records` (lines 1884–2018), `initiator_no_show_returns_the_participant_to_its_reopened_offer`, `participant_whose_offer_is_not_reopened_ends_cancelled`, `match_404_refollows_the_reopened_offer`, `match_and_offer_404_ends_cancelled`, `closed_match_with_our_offer_still_matched_ends_cancelled`, `counterparty_accept_timeout_refollows`, `user_cancel_of_a_refollowed_offer_ends_the_record`, `decline_refused_as_too_late_does_not_fund_and_stops_when_the_match_ends`, `the_record_carries_the_edges_match_status`, the three trace/log tests (lines 2279–2316); plus Review Focus `concurrent_steps_of_one_swap_serialise` (two `step` futures joined: the fake API sees one relay read at a time) and `a_cancelled_step_leaves_the_record_consistent` (drop a `step` future after its first port call; the next `step` resumes and ends where an uninterrupted run does).
- [ ] Run `cargo test -p swap-engine` → fail; port the Kotlin `step`/`drive`/`Run` (match, review, decline, refollow, cancel, trace) parts; → these pass.

### Task 5: Relay, funding and their funding

**Files:** Modify `src/engine/run.rs`; Test `tests/engine_funding.rs`.
- [ ] Port: `missed_deadline_cancels_unfunded`, `locked_wallet_notifies_need_funding_then_funds_on_unlock`, both `need_funding_alert_carries_the_computed_deadline_*`, `resume_after_funding_broadcast_does_not_fund_twice`, `resume_finds_btc_funding_by_address_without_a_pending_txid`, `a_deadline_relayed_by_the_counterparty_is_ignored`, `refund_watch_is_posted_even_when_funded_post_fails`, `pending_funding_not_visible_rebroadcasts_the_same_hex`, `unresolved_pending_funding_past_deadline_stays_active_and_is_refunded`, `match_cancelled_while_our_funding_is_in_flight_keeps_the_swap`, `definitively_rejected_funding_is_replanned`, `refund_watch_refused_by_the_edge_is_retried_until_accepted`, `mempool_conflict_keeps_the_pending_funding_and_resends_it`, `participant_does_not_fund_a_short_xbt_htlc`, the three `participant_*explorer*` tests and `initiator_does_not_reveal_against_a_funding_no_explorer_shows`, `participant_announces_its_funding_on_a_deadline_cancelled_match_and_the_swap_completes`, `a_refused_late_funded_is_kept_as_an_error_and_retried_and_the_refund_stays_due`, `a_funding_mined_after_the_deadline_is_announced_once_not_forever`; plus Review Focus `funding_save_and_another_swaps_save_both_survive`.
- [ ] Run → fail; port relay, `fund`, `resend_pending`, `after_funding`, `their_funding` with the split funder (save pending between `sign_funding` and `broadcast_funding`); → pass.

### Task 6: Redeem, refund, done

**Files:** Modify `src/engine/run.rs`; Test `tests/engine_settle.rs`.
- [ ] Port: `quick_offer_to_done_initiator`/`_participant` (now passing), `refund_after_locktime_when_counterparty_never_funds`, `a_refund_is_sent_and_settled_only_on_an_explorers_word`, `an_unconfirmed_refund_follows_the_live_feerate`, `done_waits_for_an_explorer_to_see_the_spend_confirmed`, `participant_redeems_from_initiator_btc_redeem`, `initiator_redeem_is_also_sent_through_an_explorer`, `initiator_redeem_goes_out_through_the_explorer_when_the_edge_refuses_it`, `participant_finds_the_preimage_through_an_explorer_when_the_edge_hides_it`, `participant_redeems_when_initiator_redeem_beats_its_refund`, `initiator_sees_participant_refund_and_refunds_its_own_xbt`, `initiator_does_not_start_a_btc_redeem_near_the_btc_locktime`, `initiator_redeem_is_recognised_after_fees_change`, `initiator_redeem_is_fee_bumped_while_unconfirmed`, `initiator_redeem_near_the_btc_locktime_uses_the_highest_variant`, `a_refund_sent_but_not_saved_is_still_followed_to_refunded`, `a_swap_started_by_taking_a_listed_offer_runs_to_done`.
- [ ] Run → fail; port `initiator_redeem`, `participant_redeem`, `done_check`, `bump_redeem`, `refund_if_due`, `refund_progress`, `broadcast_everywhere`, `broadcast_any`; → pass. Settled records blank `wrapped_*` exactly as Kotlin.

### Task 7: Restored sessions and the cooperative release

**Files:** Modify `src/engine/run.rs`; Test `tests/engine_release.rs`.
- [ ] Port: the six `a_recovered_*` tests (lines 1125–1262) and the eight release tests (lines 1263–1381, including `a_participant_owed_no_release_stops_being_stepped`).
- [ ] Run → fail; port `restore_session`, `release_back`, the participant `release`, `release_due`, `SwapStore::due`; → pass.

### Task 8: Recovery search

**Files:** Create `src/recovery.rs`; Test `tests/recovery.rs`.

**Interfaces — Produces:** `scan_tags`, `recovered_record`, `recovery_wallets` (as Kotlin); `RecoveryHost` port: `tag(seed, index) -> String`, `wrapped_secrets(seed, index) -> (String, String)`, `addresses(from_wallet, to_wallet) -> (redeem, refund)`, `advance_receive(wallet)`, `independent unspent` via `ChainCheck`; `async fn find_swaps(&self, seeds: Vec<SeedWallets>) -> RecoveryOutcome { found: u32, need_wallets: u32 }` on `SwapEngine` — the Kotlin `WalletViewModel.findSwaps` logic (look, then claim, skip local records, initiator-spent skip, cross-seed wallets).
- [ ] Port all six `RecoveryTest` cases and `recovery_pays_into_another_seeds_wallet_when_its_own_seed_has_none`; add `find_swaps_claims_only_what_this_phone_does_not_run` (a local record for one tag: no claim for it) and `find_swaps_bumps_each_seeds_counter_past_what_it_found`. Run → fail; implement; → pass.

### Task 9: FFI and the Android adapters, behind a debug switch

**Files:** Create `crates/wallet-ffi/src/engine.rs`; Kotlin `android/app/src/main/kotlin/com/fortis/wallet/swap/RustEngine.kt` (adapters + facade), modify `FortisApplication.kt`, `WalletViewModel.kt` (store/engine calls through a `SwapBackend` interface implemented by both engines), `SwapRunner`/`SwapService`/`SwapWorker` (use `SwapBackend.due`); debug setting `swap_engine = rust | kotlin` in Settings (debug builds only), default `kotlin`.

**Interfaces — Consumes:** Tasks 2–8. **Produces:** `uniffi` objects `SwapEngine`, `SwapStore` facade, records as `SwapRecordFfi` (JSON string across the boundary to keep one serialiser), foreign traits mirroring the ports (`Send + Sync`), `PortError` mapped from `SwapHttpException(code)`.
- [ ] Tests: Rust — `http_errors_cross_the_boundary_with_their_code`; Kotlin (instrumented on `.dev`, as Task 1 proved) — `kotlin_saved_records_continue_under_the_rust_engine` (load the Task 2 fixtures into the DataStore blob, step each with the Rust engine over fake ports, assert the step matches the Kotlin engine's result for the same input), and one smoke test per port adapter.
- [ ] Run → fail; implement; `gw.sh :app:testDebugUnitTest :app:assembleDebug` and the instrumented tests → pass. Release builds force `kotlin`.

### Task 10: On the phones, Rust engine selected

**Files:** none (ledger only).
- [ ] With `.dev` and `.dev2` on `rust`, on mainnet: (1) a full swap, `.dev2` initiator; (2) a full swap, `.dev` initiator; (3) a cancelled match with the early release; (4) wipe `.dev2` mid-swap, restore, Find my swaps, completes; (5) a swap started on `kotlin`, switched to `rust` mid-way (FUNDED), completes; (6) `am force-stop` during a step, completes. Expected: each ends DONE / REFUNDED as it would on Kotlin; logs show no step error that Kotlin wouldn't show.

### Task 11: Switch, delete, document

**Files:** Delete `swap/SwapEngine.kt`, `swap/Recovery.kt`, Kotlin store logic in `data/SwapStore.kt` (keep the blob), the Kotlin fakes and engine tests now in Rust, `async_proof.rs`/`AsyncProofActivity.kt`, the debug switch; Modify `android/README.md` (swap engine section), `CHANGELOG.md`, the spec's status line.
- [ ] `gw.sh :app:testDebugUnitTest :app:assembleDebug`, `cargo test -p swap-engine -p wallet-ffi -p wallet-core`, the regtest suites with nodes → pass; install both dev apps; one more full swap.
