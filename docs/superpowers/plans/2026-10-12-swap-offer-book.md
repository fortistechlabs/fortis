# Swap Offer Book Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Users see the open public swap offers on the Swaps page, with a plain-words funds line each, and can take all or part of one.

**Architecture:** fortis-edge gains `GET /swap/book` (open public offers, sanitised) and `POST /swap/offers/{id}/take` (pair a fresh unlisted taker offer with the chosen one, via the existing `pair`). The app adds the two calls to `SwapApi`, a pure `fundsFor`, an offers section on the Swaps page and a Take offer screen that feeds the existing take flow (`TakePreview` → `acceptTaken`).

**Tech Stack:** Rust (tiny_http, RocksDB store) · Kotlin/Jetpack Compose · JUnit4

**Spec:** `docs/superpowers/specs/2026-10-12-swap-offer-book-design.md`

## Global Constraints

- No commits — the owner commits. No Claude attribution anywhere.
- Book entries never contain token, `secret_hash`, `share_code` or `client_id`.
- At most 50 book entries per direction; best price first, then oldest.
- Take amount: from `MIN_BTC_LEG_SAT` (2 000 sat BTC leg) to the offer's open amount and the cap.
- A taker swaps at the maker's price; a mismatched `price_sat_per_xbt` is refused (409 "price changed").
- Ruling: a struck-out taker gets the existing `429` + `STRUCK_OUT` text (as `take` by code), not the spec's 403 — cost if wrong: one status code.
- Book refreshed when the Swaps page shows and every 15 s while visible.
- Only wallets that can sign count for funds (`!watchOnly && !hardware`).
- Every take and refusal goes to the journal via `swap_event!`.

## Review Focus

1. An offer filled or cancelled between the list loading and Take → "Someone else took this offer" and a refreshed list, never a crash or a half-made swap (Task 2 test `taking_a_gone_offer_is_refused_and_creates_nothing`; Task 5 error mapping test).
2. A retry after a dropped response (same `client_id`) → the same match, never a second one (Task 2 test `a_repeat_take_with_the_same_client_id_takes_once`).
3. Partial take leaves exactly the right remainder listed, and a remainder below the minimum disappears from the book (Task 2 test `a_partial_take_leaves_the_rest_listed`).
4. Wallet with balance but no wallet on the other chain → "Add a … wallet to receive into", not "You can take all of it" (Task 4 test).
5. Watch-only / hardware wallet with a big balance → ignored for funds (Task 4 test).

---

### Task 1: Edge — `GET /swap/book`

**Files:**
- Modify: `crates/fortis-edge/src/swap/mod.rs` (route table near `(Method::Get, ["quote"])`; new `fn book(token: &str, sw: &Swap) -> Reply`; tests at the end of `mod tests`)

**Interfaces:**
- Consumes: `Store::open_offers(Direction)`, `struck_out(sw, token)`, `matcher::btc_leg_value`, `xbt_for_btc`, `btc_for_xbt`, `MIN_BTC_LEG_SAT`.
- Produces: JSON `{"xbt_to_btc": [Entry], "btc_to_xbt": [Entry]}`, `Entry = {id, direction, amount_sat, price_sat_per_xbt, btc_sat, xbt_sat, expires_at}`.

- [ ] **Step 1: Failing tests** (`mod.rs` tests): `the_book_lists_open_public_offers_best_price_first` (asks ascending, bids descending, ties oldest first; `btc_sat`/`xbt_sat` equal `btc_leg_value` maths for the open amount); `the_book_leaves_out_private_expired_own_struck_and_tiny_offers` (each excluded); `the_book_caps_each_side_at_50`; `book_entries_carry_no_secrets` (serialised text contains none of the offer's token, hex secret hash, share code, client id).
- [ ] **Step 2:** `cargo test -p fortis-edge --bin fortis-edge book` → FAIL (no route / no fn).
- [ ] **Step 3:** Implement `book`: route `(Method::Get, ["book"]) => book(token, sw)`.
- [ ] **Step 4:** Same command → PASS; then `cargo test -p fortis-edge` all green.

### Task 2: Edge — `POST /swap/offers/{id}/take`

**Files:**
- Modify: `crates/fortis-edge/src/swap/mod.rs` (route `(Method::Post, ["offers", id, "take"])`; `fn take_offer(req, id, token, sw) -> Reply`; tests)
- Modify: `crates/fortis-edge/src/swap/matcher.rs` (`pub fn take_public(store, offer_id, taker: &Offer, now, max_btc_sat, network, fee_address) -> anyhow::Result<Match>` beside `take_private`, same checks minus the code lookup, plus `!maker.private`)
- Modify: `crates/fortis-edge/README.md` (routes table: both new routes)

**Interfaces:**
- Consumes: `client_secret(sw, token, client_id)`, `pair`, `matchable`, `struck_out`, `STRUCK_OUT`, `new_id`, `swap_event!`.
- Produces: body `{amount_sat: u64, price_sat_per_xbt: u64, client_id: String}` → `200 {match_id, owner_secret, offer_id}`; refusals 404 no such offer · 409 "offer is no longer available" / "price changed" / "cannot take your own offer" / "use its share code" (private) · 400 amount (below minimum / above the open amount / above the cap) or bad client_id · 429 struck out.

- [ ] **Step 1: Failing tests:** `a_take_pairs_exactly_the_chosen_offer_at_its_price` (a better-priced offer exists and is NOT the one matched); `a_partial_take_leaves_the_rest_listed` (book shows the maker's remainder; a remainder below the minimum isn't listed); `a_repeat_take_with_the_same_client_id_takes_once` (same match id and secret, one match in the store); `a_take_with_a_stale_price_is_refused`; `taking_a_gone_offer_is_refused_and_creates_nothing` (filled, cancelled, expired; no match created); `own_private_tiny_and_oversized_takes_are_refused`; `a_struck_taker_cannot_take`; e2e in `tests/swap_e2e.rs`: `a_swap_started_by_taking_a_listed_offer_runs_to_closed` (copy the existing take-by-code e2e, swap the start).
- [ ] **Step 2:** `cargo test -p fortis-edge take` → FAIL.
- [ ] **Step 3:** Implement. The taker `Offer`: `private: true` (never listed), `direction = maker.direction.opposite()`, `price = maker.price`, `client_id` set; idempotency: if an offer with this token+client_id exists, return its match (as `create_offer` does). After pairing, cancel the taker's remainder (as `take`).
- [ ] **Step 4:** PASS; `cargo test -p fortis-edge` all green; `cargo clippy -p fortis-edge --all-targets` no new warnings.

### Task 3: App — `SwapApi.book()` and `SwapApi.takeOffer(…)`

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/SwapApi.kt`
- Modify: `android/app/src/test/kotlin/com/fortis/wallet/swap/SwapEngineTest.kt` (FakeApi implements both)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/data/SwapBookParseTest.kt` (new)

**Interfaces:**
- Produces: `data class BookEntry(val id: String, val direction: String, val amountSat: Long, val priceSatPerXbt: Long, val btcSat: Long, val xbtSat: Long, val expiresAt: Long)`; `data class Book(val xbtToBtc: List<BookEntry>, val btcToXbt: List<BookEntry>)`; `suspend fun book(): Book`; `suspend fun takeOffer(offerId: String, amountSat: Long, priceSatPerXbt: Long, clientId: String): Taken`; `fun parseBook(o: JSONObject): Book` (top-level, internal, for the test).

- [ ] **Step 1:** Failing test `parses_both_sides_of_the_book` (a fixture JSON with one entry per side → exact fields) and `a_missing_side_is_empty`.
- [ ] **Step 2:** Gradle `:app:testDebugUnitTest --tests '*SwapBookParseTest'` → FAIL.
- [ ] **Step 3:** Implement (`GET /swap/book`, `POST /swap/offers/$id/take`).
- [ ] **Step 4:** PASS; full `:app:testDebugUnitTest` green.

### Task 4: App — `fundsFor`

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/SwapScreens.kt` (pure functions near `maxSwapSat`)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/SwapFundsTest.kt` (new)

**Interfaces:**
- Consumes: `maxSwapSat(balanceSat, feeReserveSat)`, `WalletConfig` (`id`, `chain`, `watchOnly`, `hardware`), `BookEntry`.
- Produces: `sealed interface Funds { data class All(val walletId: String) ; data class UpTo(val walletId: String, val maxSat: Long) ; data object NotEnough ; data class NeedReceiveWallet(val chain: String) }`; `fun fundsFor(entry: BookEntry, wallets: List<WalletConfig>, balances: Map<String, Long>, feeReserves: Map<String, Long>): Funds`; `fun takerSendChain(entry: BookEntry): String` (`xbt_to_btc` → `"btc"`); `fun takerNeedSat(entry: BookEntry): Long` (the entry's leg on the taker's send chain).

- [ ] **Step 1: Failing tests:** `all_of_it_when_the_best_wallet_covers_it`; `up_to_x_when_only_part_fits` (X = best `maxSwapSat`, ≥ minimum); `not_enough_below_the_minimum`; `needs_a_receive_wallet_on_the_other_chain` (checked first); `watch_only_and_hardware_wallets_are_ignored`; `picks_the_wallet_that_can_swap_the_most`; `unknown_balance_or_fee_reserve_counts_as_nothing_yet`.
- [ ] **Step 2:** → FAIL. **Step 3:** implement. **Step 4:** PASS; suite green.

### Task 5: App — Swaps page offers section + Take offer screen

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/SwapScreens.kt` (offers section composable under the user's swaps; `TakeOfferScreen` composable; `fun takeError(e: Exception): Int` mapping)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` (`var book: Book?`; `fun refreshBook()`; `fun takeListed(entry: BookEntry, fromId: String, toId: String, amountSat: Long)` → `takeOffer` with a client id kept per (entry, amount) until it succeeds, then the same `match` + `validateSwapTerms` + record + `startSwap` path as `takeCode`/`acceptTaken`; a `Phase`/route for the Take screen as the take-by-code one)
- Modify: `android/app/src/main/res/values/strings.xml` (copy below)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/SwapTakeErrorTest.kt` (new)

**Copy (exact):** section "Offers waiting for a match"; groups "Get XBT for your BTC" / "Get BTC for your XBT"; funds "You can take all of it" · "You can take up to %1$s" · "Not enough %1$s to take this" · "Add an %1$s wallet to receive into"; empty "No offers waiting right now. Start your own with New swap."; errors "Someone else took this offer" · "This offer\'s price changed — check it again" · "You can\'t take offers for now. Try again later." · "Couldn\'t reach the server"; button "Take".

- [ ] **Step 1: Failing test** `take_errors_map_to_plain_words` (409 "offer is no longer available" → gone; 409 "price changed" → price; 429 → struck; IOException → network; 400 → its own message shown under the amount).
- [ ] **Step 2:** → FAIL. **Step 3:** implement the mapping, the screens and the ViewModel wiring (book polled with `LaunchedEffect` every 15 s while the Swaps page is composed). **Step 4:** PASS; `:app:assembleDebug :app:testDebugUnitTest` green.
- [ ] **Step 5: Engine test** in `SwapEngineTest`: `a_swap_started_by_taking_a_listed_offer_runs_to_done` (FakeApi `takeOffer` pairs like `take`), RED → GREEN.

### Task 6: Device check and docs

**Files:**
- Modify: `CHANGELOG.md` (Unreleased: one line), `android/README.md` (Swaps section)

- [ ] **Step 1:** Build and install `.dev` and `.dev2` (`-Pdev2`); deploy edge (owner runs `sudo install` + restart).
- [ ] **Step 2:** On the phone: `.dev2` posts a public offer; `.dev` sees it with the right funds line; takes part of it; the remainder stays listed; the swap starts. Screenshot each.
- [ ] **Step 3:** `cargo test --workspace` and `:app:testDebugUnitTest` green; summary to the owner. No commits.
