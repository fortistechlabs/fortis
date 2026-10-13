# Swap-key recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From the recovery phrase alone, a restored app finds its swaps that still have coins locked and refunds or redeems them itself.

**Architecture:** Swap indices become a per-seed counter (0, 1, 2, …). Every offer — posted or taken — carries a recovery tag `HMAC(k_rec, index)` the edge stores uniquely. After a restore the app derives tags in order, asks `POST /swap/recover`, gets back each funded match with a fresh owner secret, and hands a rebuilt record to the ordinary engine.

**Tech Stack:** Rust (wallet-core, wallet-ffi/UniFFI, fortis-edge on tiny_http + RocksDB), Kotlin (Android app).

**Spec:** `docs/superpowers/specs/2026-10-12-swap-key-recovery-design.md`

## Rulings against the spec (owner told)

- **Per seed, not per wallet:** two wallets can share a seed and swap keys don't depend on the chain (both coin type 0), so the counter is keyed by the seed's master fingerprint.
- **Takes carry tags too:** `POST /swap/offers/{id}/take` and `POST /swap/take/{code}` create the taker's swap.
- **Recover re-keys the owner secret:** the restored phone has none; `/swap/recover` issues a new one (only for matches where the tag owner's own leg is funded). The tag is the proof of ownership.
- **Spec §4 dropped** (tagging older random-index swaps): the scan only derives tags for 0, 1, 2, …, so it could never find them. Older swaps stay with the watchtower.

## Global Constraints

- `recovery_tag` = lowercase hex of `HMAC-SHA256(k_rec, index as 4 bytes big-endian)`, `k_rec` = the 32 private-key bytes at `m/84'/0'/<account>'/3'` (account 0).
- Edge: a tag is unique across all offers; reuse → `409 "this swap index was used before"`; an idempotent replay (same token + client_id) is not a reuse.
- `POST /swap/recover`: ≤ 100 tags per call (else 400); token-authenticated like every `/swap/*` route.
- Restore scan: batches of 100, stop after **20** consecutive unknown tags.
- A recovered swap never funds: only records whose own leg is funded (per the edge) and still unspent (per an independent explorer) are rebuilt.
- Never commit (the owner commits).

## Review Focus

1. A 409 "index used" on create/take must move to the next index and retry, never surface as a failure the user can't get past — `createOffer_moves_past_a_used_index`.
2. A restore where only one of the seed's two wallets exists — record skipped with a plain note, not a crash — `recovery_skips_a_swap_without_both_wallets`.
3. A recovered record must not fund again or re-accept destructively — engine test `a_recovered_initiator_refunds_and_never_funds_again`.
4. `/swap/recover` must never return another offer's data for a tag it doesn't know, nor a secret for an unfunded match — `recover_answers_only_known_tags_and_funded_secrets`.
5. Counter after restore must start past every found index (and past existing records) — `nextSwapIndex_never_reuses`.

---

### Task 1: Recovery tag in wallet-core and wallet-ffi

**Files:** Modify `crates/wallet-core/src/keys.rs`, `crates/wallet-ffi/src/lib.rs`; Test `crates/wallet-ffi/tests/swap_session.rs`

**Interfaces — Produces:** core `MasterKey::swap_recovery_tag(&self, account: u32, index: u32) -> Result<[u8; 32]>`; ffi `Wallet::swap_recovery_tag(&self, account: u32, swap_index: u32) -> Result<String>` (hex).

- [ ] Test `recovery_tag_is_deterministic_and_unlinkable`: same (seed, account, index) → same tag; index 0 vs 1 differ; account 0 vs 1 differ; tag ≠ hex of `swap_secret(…, 0)` for either chain; 64 hex chars.
- [ ] Run `cargo test -p wallet-ffi --test swap_session recovery_tag` → fails (no method).
- [ ] Implement: derive `m/84'/0'/<account>'/3'` (new `recovery_path`), HMAC-SHA256 over `index.to_be_bytes()`.
- [ ] Run → passes; `cargo test -p wallet-core -p wallet-ffi` all pass.

### Task 2: Edge stores tags uniquely

**Files:** Modify `crates/fortis-edge/src/swap/store.rs`

**Interfaces — Produces:** `Offer.recovery_tag: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`); CF `"tags"` tag → offer id written in `put_offer`; `Store::offer_by_tag(&self, tag: &str) -> Option<Offer>`; `Store::set_secret_hash(&self, offer_id: &str, hash: [u8;32]) -> anyhow::Result<()>`.

- [ ] Test `an_offer_is_found_by_its_tag` and `offers_without_tags_are_not_indexed` (store unit tests).
- [ ] Run → fail; implement (CF created on open; `put_offer` writes the index when a tag is set).
- [ ] Run → pass.

### Task 3: Edge routes — tags on create/take, `/swap/recover`

**Files:** Modify `crates/fortis-edge/src/swap/mod.rs`, `crates/fortis-edge/README.md`

**Interfaces — Consumes:** Task 2. **Produces:** request field `recovery_tag` (64 hex) on `NewOffer`, `TakeReq`, and an optional JSON body `{recovery_tag}` on `POST /swap/take/{code}`; route `POST /swap/recover {tags:[…]}` → `{ "found": [ { "tag", "offer_id", "offer_status", "direction", "match": null | { "id", "role", "status", "terms", "xbt_funding", "btc_funding" }, "owner_secret": null | "<hex>" } ] }`.

- [ ] Tests: `a_used_tag_is_refused_but_a_replay_is_not` (409 text "this swap index was used before"; same client_id replay → 200); `a_take_carries_its_tag`; `recover_answers_only_known_tags_and_funded_secrets` (unknown tag absent; matched-but-unfunded → `owner_secret: null`; own leg funded → a secret that then authenticates `GET /swap/matches/{id}` while the old one no longer does); `recover_takes_at_most_100_tags` (101 → 400); a malformed tag → 400.
- [ ] Run → fail. Implement: validate hex(64); check `offer_by_tag` before creating (after the replay check); recover = for each tag `offer_by_tag`, its match (`offer.match_id`), role from which side of the match the offer is; when the role's leg (`xbt_funding` for initiator, `btc_funding` for participant) is set: new secret via `new_secret()`, `set_secret_hash`, return the hex; `swap_event!` per call with the count only.
- [ ] Run `cargo test -p fortis-edge` (with nodes: `FORTIS_BITCOIND=… FORTIS_BTC_BITCOIND=…`) → all pass. README: document the field and route.

### Task 4: App API and the per-seed counter

**Files:** Modify `android/app/src/main/kotlin/com/fortis/wallet/data/SwapApi.kt`, `data/SwapStore.kt`; Tests `data/SwapRecoverParseTest.kt`, `data/SwapIndexTest.kt` (new, under `android/app/src/test/kotlin/com/fortis/wallet/`)

**Interfaces — Produces:** `SwapApi.createOffer(…, recoveryTag: String?)`, `takeOffer(…, recoveryTag: String?)`, `take(code, recoveryTag: String?)`; `SwapApi.recover(tags: List<String>): List<Recovered>`; `data class Recovered(val tag: String, val offerId: String, val direction: String, val match: RecoveredMatch?, val ownerSecret: String?)`; `data class RecoveredMatch(val id: String, val role: String, val status: String, val terms: JSONObject, val xbtFunding: Outpoint?, val btcFunding: Outpoint?)`; `internal fun parseRecovered(j: JSONObject): List<Recovered>`; `SwapStore.nextSwapIndex(seed: String): Int` (counter per seed fingerprint, never ≤ any index already in a record or issued for that seed); `SwapStore.bumpSwapIndex(seed: String, pastIndex: Int)`; `SwapRecord.seed: String?` (the fingerprint; serde key `seed`).

- [ ] Tests: `parseRecovered` on a full and a minimal reply; `nextSwapIndex_never_reuses` (0, 1, 2 in order; separate seeds independent; after `bumpSwapIndex(seed, 7)` → 8; an existing record at index 4 for that seed → never 4).
- [ ] Run `gw.sh :app:testDebugUnitTest --tests '*SwapRecoverParseTest' --tests '*SwapIndexTest'` → fail; implement; → pass. `FakeApi` in `SwapEngineTest` gains the new parameters (ignored).

### Task 5: Tags on every new swap, moving past used indices

**Files:** Modify `WalletViewModel.kt` (`swapKeys`, `createOffer`, the two take paths), `swap/SwapEngine.kt` (`postOffer` sends `r.recoveryTag`); `SwapRecord.recoveryTag: String?`

**Interfaces — Consumes:** Tasks 1, 4.

- [ ] Test (VM logic extracted to `internal suspend fun <T> withFreshIndex(seed: String, tries: Int = 5, attempt: suspend (Int) -> T): T` in `swap/SwapIndex.kt`): `createOffer_moves_past_a_used_index` — attempt throws `SwapHttpException(409, "this swap index was used before")` for index 0 and 1, succeeds at 2; other 409s are rethrown at once.
- [ ] Run → fail; implement; wire `createOffer`/takes through it (provisional record removed before the retry); `swapKeys` takes the index and seed and derives `recoveryTag` via `wallet.swapRecoveryTag(0u, index)`.
- [ ] Run all app tests → pass.

### Task 6: Recovery scan and rebuilt records

**Files:** Create `swap/Recovery.kt`; Modify `WalletViewModel.kt` (restore hook + Settings action), `ui/screens/Screens.kt` (Settings row), `res/values/strings.xml` (+ the seven translated locales); Test `swap/RecoveryTest.kt`, additions to `SwapEngineTest.kt`

**Interfaces — Consumes:** Tasks 1–5. **Produces:** `suspend fun scanTags(tagAt: (Int) -> String, recover: suspend (List<String>) -> List<Recovered>, gap: Int = 20, batch: Int = 100): Pair<List<Pair<Int, Recovered>>, Int>` (found entries with their index, highest index seen or -1); `fun recoveredRecord(index: Int, r: Recovered, seed: String, from: WalletConfig, to: WalletConfig, keys: SwapKeys): SwapRecord?` (null unless the role's own leg is funded and `ownerSecret` is set; fills `matchId, role, termsJson, fundingTxid/Vout, limitPrice = terms price, amountSat = own leg, marks = {accepted, remainder_cancelled}, step = FUNDED (initiator) or THEY_FUNDED (participant)`).

- [ ] Tests (`RecoveryTest`): `scan_stops_after_20_unknown_in_a_row` (found at 0 and 25 → both found when gap 20 starts after 25; none after 45 → stops; batches of 100); `recovered_record_only_for_a_funded_own_leg`; `recovery_skips_a_swap_without_both_wallets` (VM-level helper returns a note, no record).
- [ ] Engine tests: `a_recovered_initiator_refunds_and_never_funds_again` (record from `recoveredRecord`, counterparty never funds, clock past XBT locktime → REFUNDED, `funder.calls == 0`); `a_recovered_participant_redeems_from_the_btc_redeem`.
- [ ] Run → fail; implement. VM `recoverSwaps()`: per seed group (seed wallets by fingerprint), scan, `bumpSwapIndex(seed, highest)`, for each found entry with a record built, check `independent.unspent(ownChain, funding) != null`, save, start the service; result note "Found N swaps" / "No swaps to recover". Called after `restoreWallet` and from Settings ("Find my swaps").
- [ ] Run all app tests + `assembleDebug` → pass.

### Task 7: Docs and end-to-end

**Files:** `android/README.md`, `CHANGELOG.md`, spec status line.

- [ ] Document the tag, counter and recovery in the README's swap section; changelog line.
- [ ] On the phones: create a swap on `.dev2`, fund, clear `.dev2`'s data, restore the seed, "Find my swaps" → the swap reappears and settles.
