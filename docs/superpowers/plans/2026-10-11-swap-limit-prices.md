# Swap limit prices Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the NeoxEX-priced swap with user-set limit prices: offers carry a price, crossing offers match at the resting offer's price, and NeoxEX becomes a reference only.

**Architecture:** `wallet-core::validate_terms` checks the matched price against the user's own limit instead of a NeoxEX price and slippage. The edge's `Offer` gains `price_sat_per_xbt`. The matcher pairs only crossing offers, best price first and then the oldest, and trades at the resting offer's price. `GET /swap/quote` reports a NeoxEX reference and the best open prices, and never fails because of NeoxEX. On Android, the slippage picker becomes a price field with a reference and a warning.

**Tech Stack:** Rust (wallet-core, wallet-ffi/UniFFI, fortis-edge on tiny_http + RocksDB), Kotlin/Compose (Android).

**Spec:** `docs/superpowers/specs/2026-10-10-p2p-swap-design.md`, the section "Revision 2: user-set prices" (approved 2026-10-11). The rest of the spec stands.

## Global Constraints

- **No commits.** Never run `git commit`, `stash`, `reset` or `checkout`. The owner commits.
- **Never touch the store app.** Never install, clear, uninstall or launch `com.fortistechlabs.wallet`. Only `.dev` and `.dev2` may be installed.
- **Price unit:** `price_sat_per_xbt` is a whole number of BTC sats per 1 XBT and must be > 0.
- **Amounts:** `btc_for_xbt` and `xbt_for_btc` (rounded down, u128 maths) stay the only conversion.
- **Limits:**
  - XBT → BTC (initiator): the price is the *lowest* the user accepts.
  - BTC → XBT (participant): the price is the *highest* the user accepts.
- **Crossing:** two offers cross when `seller.price <= buyer.price`.
- **Trade price:** the trade is done at the resting (already open) offer's price. A private take trades at the maker's price.
- **Matching order:** best price first (the lowest ask for a buyer, the highest bid for a seller), then the oldest `created_at`.
- **Leg limits:** the minimum and maximum BTC legs (`MIN_BTC_LEG_SAT` = 80,000 and `--swap-max-sat`) are checked at the offer's own price. A partly filled remainder keeps its price.
- **NeoxEX reference:**
  - The same depth-weighted mid is used (`DEPTH_BTC` 0.05).
  - `reference_ok` = the old rules pass: spread ≤ 2%, data ≤ 60 s old, not paused, and ≤ 2% from the last trade.
  - Nothing refuses on NeoxEX, and the quote is never `503` because of NeoxEX.
- **Quote JSON:** `{reference_price: u64|null, reference_ok: bool, best: {xbt_to_btc: u64|null, btc_to_xbt: u64|null}, fee_bps, min_btc_sat, max_btc_sat, open: {xbt_to_btc, btc_to_xbt}}`.
  - `best.xbt_to_btc` is the lowest open public XBT → BTC price.
  - `best.btc_to_xbt` is the highest open public BTC → XBT price.
- **Phone warning:** posting asks for one confirmation when the price is **more than 5%** worse for the user than the reference, or when there is no reference.
- **Price field:** BTC per XBT with up to 8 decimals. 1 BTC/XBT = 100,000,000 sat/XBT.
- **No migration:** no swap has run in production. Stored offer and record shapes may change without an upgrade path.
- **Test commands:**
  - Rust: `cargo test --workspace` (with `PATH=$HOME/.cargo/bin:$PATH`).
  - Android: `cd android && JAVA_HOME=$HOME/.local/share/mise/installs/java/temurin-21 $JAVA_HOME/bin/java -cp gradle/wrapper/gradle-wrapper.jar org.gradle.wrapper.GradleWrapperMain testDebugUnitTest assembleDebug`.
  - Swap e2e: `FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind cargo test -p fortis-edge --test swap_e2e -- --nocapture`.

## Review Focus

1. **Declined pair rematching:** a declined match reopens both offers. The new rematch pass must never pair those two offers again, or they would loop through match → decline forever. Test: `rematch_skips_pairs_that_already_cancelled_together` (Task 2).
2. **Equal prices:** an offer priced exactly at a resting offer's price crosses, and the trade runs at that price. Test: `equal_prices_cross` (Task 2).
3. **Rounding at the limit:** after `btc_for_xbt` rounds down, the terms must still pass `validate_terms` for both sides at the resting price. Test: `terms_from_a_cross_validate_for_both_limits` (Task 2).
4. **Typed prices on the phone:**
   - `0`, empty, more than 8 decimals, a comma decimal separator or a huge number must not post or crash.
   - `parsePrice` returns null for anything not in (0, 21e6] BTC/XBT with ≤ 8 decimals.
   - Test: `parsePriceRejectsJunk` (Task 4).
5. **No reference:** with NeoxEX down, the screen still prefills from the best open price or leaves the field empty, shows "No reference price", and asks for confirmation. Test: `needsConfirmWithoutReference` (Task 4).

---

### Task 1: Terms checked against the user's limit (wallet-core + wallet-ffi)

**Files:**
- Modify: `crates/wallet-core/src/swap_terms.rs` (`validate_terms` and its tests)
- Modify: `crates/wallet-ffi/src/swap.rs` (`validate_swap_terms`), `crates/wallet-ffi/tests/swap_session.rs`, `crates/wallet-ffi/README.md`
- Modify: callers that only need the new signature to compile: `crates/fortis-edge/src/swap/matcher.rs` (test at the `validate_terms(t, PRICE, 50, …)` call) and `crates/fortis-edge/tests/swap_e2e.rs` (`u.own_price, u.slippage` → `u.limit`, adding `limit: u64` to `User`). Task 3 reworks the e2e scenarios.

**Interfaces:**
- Produces:
  - `pub fn validate_terms(t: &SwapTerms, limit_price_sat_per_xbt: u64, role: SwapRole, now: u32) -> Result<()>`.
  - FFI `validate_swap_terms(terms_json: String, limit_price_sat_per_xbt: u64, role: String, now: u32) -> Result<()>` (Kotlin: `validateSwapTerms(String, ULong, String, UInt)`).

- [ ] **Step 1: Write the failing tests** in `swap_terms.rs` tests, replacing `rejects_out_of_slippage_price`:

```rust
#[test]
fn initiator_rejects_price_below_its_limit() {
    let t = terms(); // price PRICE
    assert!(validate_terms(&t, PRICE, SwapRole::Initiator, MATCHED_AT + 10).is_ok());
    assert!(validate_terms(&t, PRICE - 1, SwapRole::Initiator, MATCHED_AT + 10).is_ok());
    let e = validate_terms(&t, PRICE + 1, SwapRole::Initiator, MATCHED_AT + 10).unwrap_err();
    assert!(e.to_string().contains("worse than your limit"), "{e}");
}

#[test]
fn participant_rejects_price_above_its_limit() {
    let t = terms();
    assert!(validate_terms(&t, PRICE, SwapRole::Participant, MATCHED_AT + 10).is_ok());
    assert!(validate_terms(&t, PRICE + 1, SwapRole::Participant, MATCHED_AT + 10).is_ok());
    assert!(validate_terms(&t, PRICE - 1, SwapRole::Participant, MATCHED_AT + 10).is_err());
}

#[test]
fn zero_limit_is_rejected() {
    assert!(validate_terms(&terms(), 0, SwapRole::Participant, MATCHED_AT + 10).is_err());
}
```

Update the other existing calls to the 4-argument form. In `swap_session.rs`, rename `validate_swap_terms_checks_price_and_role` to `validate_swap_terms_checks_limit_and_role` with these assertions:
- `(json, price, "participant")` → ok;
- `(json, price - 1, "participant")` → err;
- `(json, price + 1, "initiator")` → err;
- `"taker"` → err;
- `now = accept_by + 1` → err.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p wallet-core swap_terms` and `cargo test -p wallet-ffi --test swap_session`
Expected: compile errors (the arity is wrong). This is the RED for a signature change.

- [ ] **Step 3: Implement.**
  - Replace the slippage block with a role check. The initiator rejects `t.price_sat_per_xbt < limit`; the participant rejects `t.price_sat_per_xbt > limit`. Both reject with `reject("price is worse than your limit")`. A `limit == 0` is rejected with `reject("zero limit price")`.
  - The doc comment says that `role` decides the direction.
  - Mirror the change in the FFI wrapper and its doc comment.
  - Update the wallet-ffi README's `validate_swap_terms` line.

- [ ] **Step 4: Run tests**

Run: `cargo test -p wallet-core && cargo test -p wallet-ffi && cargo test -p fortis-edge --lib`
Expected: PASS. `cargo build -p fortis-edge --tests` compiles.

- [ ] **Step 5: No commit.** The owner commits.

---

### Task 2: Edge offers carry a price; crossing, price-priority matching; reference quote

**Files:**
- Modify: `crates/fortis-edge/src/swap/store.rs` (`Offer`)
- Modify: `crates/fortis-edge/src/swap/matcher.rs`
- Modify: `crates/fortis-edge/src/swap/neoxex.rs`
- Modify: `crates/fortis-edge/src/swap/mod.rs` (`quote`, `create_offer`, `take`, `offer_json`, drop `price()` and `SLIPPAGE_CHOICES`, plus tests)
- Modify: `crates/fortis-edge/src/swap/watchtower.rs` (call the rematch pass)
- Modify: `crates/fortis-edge/README.md` (the swap API rows)

**Interfaces:**
- Consumes: `validate_terms(&t, limit, role, now)` (Task 1).
- Produces:
  - `Offer.price_sat_per_xbt: u64`, which replaces `max_slippage_bps`.
  - `matcher::crosses(seller_price: u64, buyer_price: u64) -> bool`.
  - `matcher::match_offer(store, incoming, now, max_btc_sat, network, fee_address)` (no `price`).
  - `matcher::take_private(store, code, taker, now, max_btc_sat, network, fee_address)` (no `price`).
  - `matcher::rematch(store: &Store, now: u64, max_btc_sat: u64, network: &str, fee_address: &str)`.
  - `neoxex::Reference { price_sat_per_xbt: u64, ok: bool }`, `neoxex::reference_from(&Book, &Ticker, now) -> Option<Reference>`, and `NeoxEx::reference(&self) -> Option<Reference>`.
  - HTTP:
    - `POST /swap/offers` takes `price_sat_per_xbt` (required, > 0, else 400 `"price_sat_per_xbt must be > 0"`) instead of `max_slippage_bps`.
    - `POST /swap/take/{code}` takes an empty body (any body is ignored).
    - Offer JSON shows `price_sat_per_xbt`.
    - The quote JSON is as in Global Constraints.

- [ ] **Step 1: Write the failing matcher tests** (`matcher.rs` tests; the helpers build offers with a price argument):

```rust
#[test]
fn offers_that_do_not_cross_never_match() {
    // seller wants 1_010_000, buyer pays at most 1_000_000
    let s = store_with(&[sell("a", 1_000_000_00, 1_010_000, 1)]);
    assert!(match_offer(&s, &buy("b", 900_000, 1_000_000, 2), NOW, MAX, "bitcoin", "bc1qfee").unwrap().is_none());
}

#[test]
fn equal_prices_cross() { /* sell at P, buy at P → Some(m), m.terms.price_sat_per_xbt == P */ }

#[test]
fn trade_runs_at_the_resting_price() {
    // resting sell at 990_000; incoming buy willing to pay 1_000_000 → price 990_000
    // and the reverse: resting buy at 1_000_000, incoming sell at 990_000 → 1_000_000
}

#[test]
fn best_price_beats_older_offer() {
    // resting sells: old at 1_000_000 (t=1), newer at 995_000 (t=2); incoming buy at 1_000_000
    // → matched with the newer, cheaper one at 995_000
}

#[test]
fn oldest_wins_among_equal_prices() { /* two sells at P, t=1 and t=2 → t=1 */ }

#[test]
fn remainder_keeps_its_price() { /* partial fill: remaining offer's price_sat_per_xbt unchanged, status Open */ }

#[test]
fn private_take_trades_at_the_makers_price() { /* maker sell at 1_020_000 → terms price 1_020_000 */ }

#[test]
fn terms_from_a_cross_validate_for_both_limits() {
    // sell limit 990_001, buy limit 1_000_003, odd amounts; resting = sell
    // validate_terms(&m.terms, 990_001, Initiator, ..) and (&m.terms, 1_000_003, Participant, ..) both Ok
}

#[test]
fn rematch_pairs_crossing_open_offers() {
    // two crossing public offers left open together (e.g. both reopened by a deadline
    // cancel of separate matches) → rematch() makes one match at the older offer's price
}

#[test]
fn rematch_skips_pairs_that_already_cancelled_together() {
    // match a+b, on_decline → both reopen; rematch() → no new match
}
```

Fill in each body using the existing helpers in this module (`store_with`, `NOW`, `MAX`). The comments give the exact assertions. Delete any test that only asserted NeoxEX-priced matching, and rewrite the others to give offers prices.

- [ ] **Step 2: Write the failing reference and quote tests**
  - In `neoxex.rs`:
    - `reference_of_wide_book_is_not_ok`: `BOOK_WIDE` → `Some(Reference { ok: false, .. })`, with the price equal to the weighted mid.
    - `reference_of_fixture_is_ok`: `BOOK_OK`/`TICKER_OK` at `COMPUTED + 10` → `Some(Reference { price_sat_per_xbt: 1_008_980, ok: true })`.
    - `reference_of_empty_book_is_none`.
    - Also cover stale, paused and far-from-last → `ok: false`.
  - In `mod.rs`:
    - Rename `quote_503_when_neoxex_refuses` to `quote_works_when_neoxex_refuses`: status 200, `reference_ok == false`, and posting an offer still works.
    - `quote_reports_best_open_prices`: post a sell at 1_000_000 and at 990_000 (crossing nothing) and a buy at 900_000. Then `best.xbt_to_btc == 990_000` and `best.btc_to_xbt == 900_000`.
    - `quote_reference_null_when_neoxex_down`.
    - `offer_requires_positive_price`: a missing price → 400, and 0 → 400.
    - Update the existing offer and take tests to send `price_sat_per_xbt`.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p fortis-edge --lib swap::`
Expected: compile errors (no `price_sat_per_xbt`, no `reference_from`, no `rematch`).

- [ ] **Step 4: Implement.**
  - **`store.rs`:** in `Offer`, `max_slippage_bps` → `price_sat_per_xbt: u64`.
  - **`matcher.rs`:**
    - `pair(.., price, ..)` keeps its `price` argument. The callers pass the resting or maker offer's price.
    - `settle` checks the remainder's minimum at `o.price_sat_per_xbt`.
    - `match_offer` filters the candidates (public, matchable, other token, `crosses`). It sorts them:
      - for a buyer, by `(price asc, created_at asc)`;
      - for a seller, by `(price desc, created_at asc)`.
    - It pairs at `cand.price_sat_per_xbt`.
    - `take_private` pairs at `maker.price_sat_per_xbt`.
    - `rematch` takes the matching lock. It walks open public offers oldest first. For each one, it runs the same candidate search, excluding any candidate that shares a `Cancelled` match with it (by `initiator_offer`/`participant_offer`). The trade is at the older offer's price, since that offer was resting first. Before rematch exists, `match_offer`'s body moves into a helper that runs with the lock already held, so `rematch` can reuse it without locking twice.
  - **`neoxex.rs`:** `reference_from` computes the mid as `price_from` does, but returns it whatever the checks say, with `ok = price_from(..).is_ok()`. It is `None` when either side is empty. `NeoxEx::reference` is like `price` but maps any fetch error to `None`. `price()` is removed if nothing uses it; `price_from` stays.
  - **`mod.rs`:**
    - `quote` uses `sw.neoxex.reference()` and computes `best` from `open_offers` (public, unexpired).
    - `create_offer` validates the price and checks the BTC leg with `btc_leg_value(n.direction, n.amount_sat, n.price_sat_per_xbt)`. It calls the new `match_offer` signature.
    - `take` derives the taker amount at `maker.price_sat_per_xbt`, gives the taker offer that price, and drops the slippage input.
    - `offer_json` emits `price_sat_per_xbt`.
  - **`watchtower.rs`:** after `matcher::tick(store, now)`, call `matcher::rematch(store, now, max_btc_sat, network, fee_address)`, threading those values from the `Swap` config the way the watchtower already gets its store.
  - **README:** update the `/swap/quote`, `POST /swap/offers` and `POST /swap/take` rows, and say that matching is by limit price.

- [ ] **Step 5: Run tests**

Run: `cargo test -p fortis-edge && cargo clippy -p fortis-edge --all-targets -- -D warnings`
Expected: PASS and no warnings. `swap_e2e` may fail to compile here; Task 3 owns it. If it does, run `cargo test -p fortis-edge --lib --bins` and note this in the report.

- [ ] **Step 6: No commit.**

---

### Task 3: Swap e2e scenarios for limit prices

**Files:**
- Modify: `crates/fortis-edge/tests/swap_e2e.rs`

**Interfaces:**
- Consumes: the Task 2 HTTP shapes and the Task 1 `validate_terms`.

- [ ] **Step 1: Update the harness.**
  - A `User` posts `price_sat_per_xbt: u.limit`. Its terms check is `validate_terms(&t, u.limit, u.role(), now)`.
  - `world(price)` still stubs NeoxEX, which is now only a reference.
  - The existing scenarios post both sides at the stub price, so they keep their meaning.

- [ ] **Step 2: Replace `skewed_price_both_decline` with `offers_whose_prices_do_not_cross_never_match`.**
  - Alice sells at P + 1,000 and Bob buys at P.
  - Both offers stay `open` after one watchtower tick.
  - `GET /swap/quote` reports `best.xbt_to_btc == P + 1000` and `best.btc_to_xbt == P`.

- [ ] **Step 3: Add `crossing_offers_settle_at_the_resting_price`.**
  - Alice posts XBT → BTC at P − 5,000 first (resting). Bob posts BTC → XBT at P.
  - The terms price is P − 5,000, and both sides' `validate_terms` pass.
  - The swap completes as in the happy path: the BTC paid to Alice equals `btc_for_xbt(xbt_sat, P − 5000)`, and the commission is paid on both chains.

- [ ] **Step 4: Run.**

Run: the swap e2e command in Global Constraints.
Expected: all scenarios pass (11 tests: 10 today, minus 1 replaced, plus 2 new).

- [ ] **Step 5: No commit.**

---

### Task 4: Android data, engine and price maths

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/SwapApi.kt` (`Quote`, `createOffer`, `take`)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/SwapStore.kt` (`SwapRecord.slippageBps` → `limitPrice: Long`, JSON key `limit_price`)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/NeoxEx.kt` (`reference`)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/swap/SwapCrypto.kt`, `swap/SwapEngine.kt` (`termsProblem`, `postOffer`), `FortisApplication.kt` (drop `ownPrice` wiring)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/SwapScreens.kt` (pure helpers only: remove `SLIPPAGE_BPS`, and add `parsePrice`, `fmtPrice`, `pctVsReference`, `worseByPct`, `needsConfirm`, `defaultPrice`)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/SwapMathTest.kt`, `swap/SwapEngineTest.kt`, `data/NeoxExTest.kt`, and the fixtures in `SwapNotificationsTest` and `SwapTimelineTest`

**Interfaces:**
- Consumes: the Task 2 quote and offer JSON, and the FFI `validateSwapTerms(String, ULong, String, UInt)`.
- Produces:
  - `data class Quote(val referencePrice: Long?, val referenceOk: Boolean, val bestXbtToBtc: Long?, val bestBtcToXbt: Long?, val feeBps: Int, val minBtcSat: Long, val maxBtcSat: Long)`.
  - `SwapApi.createOffer(direction: String, amountSat: Long, priceSatPerXbt: Long, private: Boolean, clientId: String?)`.
  - `SwapApi.take(code: String): Taken`.
  - `SwapRecord.limitPrice: Long`.
  - `NeoxEx.reference(http): Reference?`, with `data class Reference(val priceSatPerXbt: Long, val ok: Boolean)`.
  - `SwapCrypto.validateTerms(termsJson: String, limitPrice: Long, role: String, now: Long)`.
  - Pure helpers in `SwapScreens.kt`:
    - `parsePrice(text: String): Long?`: BTC/XBT text → sat/XBT. It accepts `.` or `,` as the separator and at most 8 decimals, and requires a result in 1..2_100_000_000_000_000. Otherwise it returns null.
    - `fmtPrice(sat: Long): String`: 8 decimals, trailing zeros trimmed to at least 5 decimals, e.g. `412000` → `"0.00412"`.
    - `pctVsReference(price: Long, ref: Long): Double`: `(price - ref) * 100.0 / ref`.
    - `worseByPct(price: Long, ref: Long, sendChain: String): Double`: positive when worse for the user. For `"xbt"` (selling) it is `-pctVsReference`; for `"btc"` it is `+pctVsReference`.
    - `needsConfirm(price: Long, ref: Long?, sendChain: String): Boolean`: `ref == null || worseByPct(..) > 5.0`.
    - `defaultPrice(q: Quote?, ownRef: Reference?, sendChain: String): Long?`. Selling XBT prefills `q.bestBtcToXbt`; selling BTC prefills `q.bestXbtToBtc`. Otherwise it uses `ownRef?.priceSatPerXbt`, then `q.referencePrice`.

- [ ] **Step 1: Write the failing tests.**
  - In `SwapMathTest`:
    - `parsePriceReadsDecimals`: `"0.00412"` → 412000, `"0,00412"` → 412000, `"1"` → 100000000.
    - `parsePriceRejectsJunk`: `""`, `"0"`, `"0.000000001"`, `"abc"`, `"1.2.3"`, `"-1"` and `"99999999"` → null.
    - `fmtPriceTrims`: 412000 → `"0.00412"` and 100000000 → `"1.00000"`.
    - `worseByPctIsSidedForSeller`: ref 1000, price 928, `"xbt"` → 7.2 (±0.001).
    - `worseByPctIsSidedForBuyer`: ref 1000, price 1017, `"btc"` → 1.7.
    - `needsConfirmAboveFivePercent`: worse by 5.0 → false, and worse by 5.01 → true.
    - `needsConfirmWithoutReference`: `ref = null` → true.
    - `defaultPricePrefersBestOpposite`, which also covers the fallbacks to the own reference and then the quote reference.
  - In `SwapEngineTest`:
    - `declinesTermsWorseThanLimit`: a record with `limitPrice` above the terms price on the initiator side → the engine declines with "price is worse than your limit". Use the existing fake crypto, delegating to the real rule if the fake already does; otherwise add the rule to the fake.
    - `acceptsTermsAtLimit`.
  - In `NeoxExTest`, the wide-book fixture gives `reference(..)` with `ok == false` and a non-null price.

- [ ] **Step 2: Run to verify they fail**

Run: the Android test command (Global Constraints).
Expected: compile errors / FAIL on the new names.

- [ ] **Step 3: Implement.**
  - **`SwapApi`:** `quote()` parses the new JSON, reading null-able fields with `longOrNull`. `createOffer` sends `price_sat_per_xbt`. `take` POSTs `{}`.
  - **`SwapStore`:** use `limit_price` (`optLong`, default 0, which `validate_terms` then rejects, so a stray old record declines rather than crashes).
  - **`NeoxEx.reference`:** same maths as `price`. `ok` is whether the old checks pass; the result is null only when there is no data.
  - **`SwapEngine.termsProblem`:** drop the `ownPrice` lookup and call `c.validateTerms(json, r.limitPrice, role, now)`. `postOffer` sends `r.limitPrice`.
  - **`FortisApplication`:** `ownSwapPrice()` becomes `swapReference(): NeoxEx.Reference?`. The engine no longer takes `ownPrice`.
  - **`WalletViewModel`:** keep it compiling with the minimum change:
    - `createOffer(..., priceSatPerXbt: Long, ...)` checks `amountError` at that price and stores `limitPrice`.
    - `takeCode(code)` validates with the terms' own price as the limit, so it checks everything but price. `TakePreview` carries `reference: NeoxEx.Reference?` instead of `slippageBps`/`ownPrice`.
    - `acceptTaken` stores `limitPrice = terms price`.

- [ ] **Step 4: Run tests**

Run: the Android test command.
Expected: all pass (130 today, plus the new ones). The UI still compiles, because Task 5 changes the screens and this task touches only what compilation needs. The temporary UI keeps the slippage picker deleted and passes `defaultPrice(..) ?: 0` until Task 5.

- [ ] **Step 5: No commit.**

---

### Task 5: Android new-swap price field, take preview and detail

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/SwapScreens.kt` (`NewSwapContent`, `TakeCodeContent`, take preview, `SwapDetailContent`; delete `SlippagePicker`)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` (exposes `swapReference` for the screen)
- Modify: `android/app/src/main/res/values/strings.xml`
- Modify: `android/app/src/debug/kotlin/com/fortis/wallet/SwapPreviewActivity.kt` (fake data)
- Modify: `android/README.md` (swap test steps: the price field), `CHANGELOG.md`
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/SwapTimelineTest.kt` or a new `NewSwapPriceTest.kt` if state logic is extracted

**Interfaces:**
- Consumes: the Task 4 helpers and `Quote`/`Reference`.

- [ ] **Step 1: Write the failing test** for the screen's posting decision, extracted as a pure function in `SwapScreens.kt`:
  `fun postGate(priceText: String, ref: Long?, sendChain: String): PostGate`, where `sealed interface PostGate { object Invalid; data class Ready(val price: Long); data class Confirm(val price: Long, val worsePct: Double?) }`.
  The `postGateCases` test covers:
  - `""` → `Invalid`;
  - `"0.00412"` with ref 412000 → `Ready(412000)`;
  - `"0.0038"` with ref 412000, selling XBT → `Confirm(380000, ~7.77)`;
  - any valid price with ref null → `Confirm(price, null)`.

- [ ] **Step 2: Run to verify it fails.** Expected: `postGate` is unresolved.

- [ ] **Step 3: Implement.**
  - **New swap:**
    - Add a price `TextField` (decimal keyboard), prefilled once from `defaultPrice`. The label is `swap_price` ("Price, BTC per XBT").
    - Below it is one meta line: `swap_price_vs_ref` ("NeoxEX mid %1$s · you %2$s"), with the reference shown via `fmtPrice` and the difference signed to one decimal (`+1.7%`). If `!reference.ok`, append `swap_ref_thin` (" · thin market"). With no reference, show `swap_no_ref` ("No reference price").
    - The receive and fee lines use the typed price.
    - Post and Share-code call `postGate`. `Confirm` opens a dialog: `swap_confirm_price` ("Your price is %1$s worse than the market reference. Post anyway?"), or `swap_confirm_no_ref` ("There is no market reference price right now. Post at %1$s BTC per XBT?").
  - **Take preview:** the price row, the reference row (or "No reference price"), and the signed difference. The slippage picker on the code screen is removed.
  - **Detail:** the slippage row becomes `swap_your_limit` ("Your limit") → `fmtPrice(r.limitPrice)`.
  - **Strings:** delete `swap_slippage`, `swap_check_ok`, `swap_check_unreachable` and `swap_check_refused` if unused.
  - **SwapPreviewActivity:** update the fakes.
  - **Docs:** in the README swap test, Alice and Bob each type a price, and crossing prices match at the resting one. Add a CHANGELOG line under the swap entry: "Offers carry your own limit price; NeoxEX is shown as a reference."

- [ ] **Step 4: Run tests and build**

Run: the Android test command.
Expected: PASS, and `assembleDebug` succeeds.

- [ ] **Step 5: Device check (dev apps only).**
  - Build `.dev` and `.dev2` (`assembleDebug`, then `assembleDebug -Pdev2`) and install both with `adb install -r`.
  - Open the debug preview with `adb shell am start -n com.fortistechlabs.wallet.dev/com.fortis.wallet.SwapPreviewActivity --es screen new` and take a screenshot.
  - Expected: the price field, the reference line and no slippage picker.

- [ ] **Step 6: No commit.**
