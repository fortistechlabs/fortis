# Swap minimum commission Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Charge each swap side the larger of 0.5% and 400 sat as its commission, and lower the minimum swap from 80 000 to 20 000 sat of BTC.

**Architecture:** wallet-core owns the rule as one function, `swap_commission_sat`, plus two constants. The edge advertises the new minimum and refuses `--swap` when its send-fee floor is above the swap floor. The e2e harness and the phone fund with `ServiceFee { bps: 50, floor_sat: 400 }`. The phone mirrors the rule in a tested Kotlin helper for display.

**Tech Stack:** Rust (wallet-core, wallet-ffi, fortis-edge), Kotlin/Compose (Android).

**Spec:** `docs/superpowers/specs/2026-10-10-p2p-swap-design.md`, the section "Revision 3: minimum commission" (approved 2026-10-11).

## Global Constraints

- **No commits.** Never run `git commit`, `stash`, `reset` or `checkout`. The owner commits.
- **Never touch the store app.** Never install, clear, uninstall or launch `com.fortistechlabs.wallet`. Only `.dev` and `.dev2` may be touched.
- **Commission per side:** `max(send_sat * 50 / 10_000, 400)`, with the percentage part rounded down. It is paid on the sender's own chain to the fee address. The floor applies on both chains.
- **Constants:** `SWAP_FEE_BPS = 50` (unchanged), `SWAP_FEE_FLOOR_SAT = 400` (new), `MIN_BTC_LEG_SAT = 20_000` (was 80 000).
- **Edge startup:** with `--swap`, the edge refuses `--service-fee-floor-sat` above `SWAP_FEE_FLOOR_SAT`.
- **Screen copy:**
  - "Commission 0.5%" when the percentage applies;
  - "Commission (minimum)" when the 400 sat floor applies;
  - the note "The commission is paid on top of the amount, plus the network fee." stays.
- **Test commands:**
  - Rust: `export PATH=$HOME/.cargo/bin:$PATH; cargo test --workspace`. fortis-edge has no lib target, so use `cargo test -p fortis-edge --bins`.
  - e2e: `FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind cargo test -p fortis-edge --test swap_e2e -- --nocapture`.
  - Android: `cd android && JAVA_HOME=$HOME/.local/share/mise/installs/java/temurin-21 $JAVA_HOME/bin/java -cp gradle/wrapper/gradle-wrapper.jar org.gradle.wrapper.GradleWrapperMain testDebugUnitTest assembleDebug -q`.
  - Clippy: `cargo clippy -p wallet-core -p wallet-ffi -p fortis-edge --bins --tests -- -D warnings -A clippy::useless_conversion -A clippy::unnecessary_lazy_evaluations` (both allowed lints are pre-existing elsewhere).

## Review Focus

1. **The crossover at 80 000 sat:** 79 999 pays 400 (the floor), 80 000 pays 400, 80 200 pays 401. Test: `swap_commission_crossover` (Task 1) and `commissionCrossover` (Task 3).
2. **The display matches the payment:** the commission the screens show equals what funding pays, including the floor. Test: Kotlin `commission` equals the Rust rule on the crossover values (Task 3).
3. **A small swap really settles:** a 20 000 sat swap passes the edge's `/tx` check on both chains and both sides redeem. Test: e2e `small_swap_pays_the_minimum_commission` (Task 2).
4. **A remainder between 20 000 and 80 000:** a partly filled offer now keeps a remainder of 20 000 sat or more open, where it used to be cancelled. Test: the existing remainder unit test updated to the new minimum (Task 1).
5. **Operator misconfiguration:** `--swap` with `--service-fee-floor-sat 500` fails at startup with a clear message. Test: `build_swap_refuses_send_floor_above_swap_floor` (Task 1).

---

### Task 1: The rule in wallet-core, wallet-ffi and the edge

**Files:**
- Modify: `crates/wallet-core/src/swap_terms.rs` (constants, `swap_commission_sat`, tests), `crates/wallet-core/src/lib.rs` (re-export)
- Modify: `crates/wallet-ffi/src/swap.rs` (export `swap_commission_sat`), `crates/wallet-ffi/tests/swap_session.rs`, `crates/wallet-ffi/README.md`
- Modify: `crates/fortis-edge/src/main.rs` (`build_swap` check and its test), `crates/fortis-edge/src/swap/mod.rs` and `matcher.rs` tests that hard-code 80 000, `crates/fortis-edge/README.md` (minimum swap and commission text)

**Interfaces:**
- Produces:
  - `pub const SWAP_FEE_FLOOR_SAT: u64 = 400;`
  - `pub const MIN_BTC_LEG_SAT: u64 = 20_000;`
  - `pub fn swap_commission_sat(send_sat: u64) -> u64`, re-exported from `wallet_core`.
  - FFI `swap_commission_sat(send_sat: u64) -> u64` (Kotlin `swapCommissionSat(ULong): ULong`).

- [ ] **Step 1: Write the failing tests.**
  - In `swap_terms.rs`:

```rust
#[test]
fn swap_commission_crossover() {
    assert_eq!(swap_commission_sat(20_000), 400);
    assert_eq!(swap_commission_sat(79_999), 400);
    assert_eq!(swap_commission_sat(80_000), 400);
    assert_eq!(swap_commission_sat(80_200), 401);
    assert_eq!(swap_commission_sat(200_000), 1_000);
    assert_eq!(swap_commission_sat(u64::MAX), (u64::MAX as u128 * 50 / 10_000) as u64);
}

#[test]
fn minimum_leg_is_20_000() {
    // terms() helper with btc_sat = 20_000 (and xbt_sat consistent with its price) → Ok;
    // with btc_sat = 19_999-equivalent → Err containing "below minimum swap size".
}
```

The `u64::MAX` line asserts that there is no overflow, so the implementation computes in u128.

  - In `swap_session.rs`, `swap_commission_sat_matches_core`: 79 999 → 400 and 200 000 → 1 000.
  - In `main.rs` tests, `build_swap_refuses_send_floor_above_swap_floor`: args with `--swap --service-fee-address <regtest addr> --network regtest --service-fee-floor-sat 500` → the error message contains `"--service-fee-floor-sat"`. Floor 400 → passes this check. Build args the way the existing main.rs tests do; if `build_swap` opens a store, give it a test temp dir.
  - In the edge tests, update `quote_returns_price_and_limits` (`min_btc_sat == 20_000`) and every offer test that relies on 80 000 being the minimum. Add `offer_worth_20_000_is_accepted` (`btc_to_xbt`, 20 000 → 200; 19 999 → 400).

- [ ] **Step 2: Run to verify they fail.** Run `cargo test -p wallet-core swap_terms` and `cargo test -p fortis-edge --bins`. Expected: they fail to compile (no `swap_commission_sat`) or fail on the 20 000 minimum.

- [ ] **Step 3: Implement.**
  - Add the constants and `swap_commission_sat`: `max(((send as u128 * SWAP_FEE_BPS as u128) / 10_000) as u64, SWAP_FEE_FLOOR_SAT)`.
  - Re-export them. Add the FFI wrapper and its README line.
  - In `build_swap`: if `args.service_fee_floor_sat > SWAP_FEE_FLOOR_SAT`, return the error `"--service-fee-floor-sat must be at most {SWAP_FEE_FLOOR_SAT} with --swap: a swap funding pays the {SWAP_FEE_FLOOR_SAT} sat minimum commission and would be refused (402)"`.
  - Edge README: in the swap section, change the commission to "the larger of 0.5% and 400 sat per side", the minimum swap to 20 000 sat, and the `--swap` flag row to mention the floor rule.

- [ ] **Step 4: Run tests.** Run `cargo test --workspace` and the clippy command. Expected: pass. `swap_e2e` still compiles; Task 2 changes it.

- [ ] **Step 5: No commit.**

---

### Task 2: e2e harness and a small-swap scenario

**Files:**
- Modify: `crates/fortis-edge/tests/swap_e2e.rs`

**Interfaces:**
- Consumes: `wallet_core::swap_terms::{swap_commission_sat, SWAP_FEE_FLOOR_SAT, MIN_BTC_LEG_SAT}` (Task 1).

- [ ] **Step 1: Point the harness at the rule.**
  - The local `commission(sat)` becomes `swap_commission_sat(sat)`.
  - Funding uses `ServiceFee { bps: t.fee_bps, floor_sat: SWAP_FEE_FLOOR_SAT, cap_sat: 0, .. }`.
  - The edge the harness starts keeps the default `--service-fee-floor-sat` (400).

- [ ] **Step 2: Add `small_swap_pays_the_minimum_commission`.**
  - The BTC leg is `MIN_BTC_LEG_SAT` (20 000) at `PRICE`, so the XBT leg is `xbt_for_btc(20_000, PRICE)` (2 000 000).
  - Run it through `settle_both_ways` (the shared happy-path body), passing those legs.
  - Assert the BTC funding pays exactly 400 sat to the fee address, and the XBT funding pays `2_000_000 * 50 / 10_000` (10 000).
  - Both fundings pass the edge's `/tx` check (`settle_both_ways` already proves both redeem).
  - The existing happy-path legs (200 000 / 20 000 000) keep paying 1 000 / 100 000.

- [ ] **Step 3: Run it.** Run the e2e command.
  - Expected first, if you add the scenario before Step 1's harness change: the small swap's BTC funding is refused (402) or pays 100. Record this as RED.
  - Then all scenarios pass: 12 tests, which is 11 plus the new one.

- [ ] **Step 4: No commit.**

---

### Task 3: Android funding floor and commission display

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` (funding `ServiceFee`)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/SwapScreens.kt` (`commission`, the new `commissionIsMinimum`, three commission rows)
- Modify: `android/app/src/main/res/values/strings.xml` (`swap_commission_min`)
- Modify: `android/app/src/debug/kotlin/com/fortis/wallet/SwapPreviewActivity.kt` (the fake quote's minimum becomes 20 000, plus one small-swap screen named `new_small`)
- Modify: `CHANGELOG.md` (the swap entry)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/SwapMathTest.kt`

**Interfaces:**
- Consumes: the Task 1 constants. Mirror them as Kotlin constants `SWAP_FEE_FLOOR_SAT = 400L` next to `SWAP_FEE_BPS`.
- Produces:
  - `fun commission(sendSat: Long): Long`, now `max(pct, 400)`.
  - `fun commissionIsMinimum(sendSat: Long): Boolean`: `pct <= SWAP_FEE_FLOOR_SAT`, so it is true at exactly 80 000.

- [ ] **Step 1: Write the failing tests** in `SwapMathTest`. They replace the old `commission(199) == 0` and `commission(200) == 1` lines.
  - `commissionCrossover`: 20 000 → 400; 79 999 → 400; 80 000 → 400; 80 200 → 401; 1 000 000 → 5 000.
  - `commissionIsMinimumBelowCrossover`: 79 999 → true, 80 000 → true (both equal 400), 80 200 → false.
  - The `amountError` tests use a minimum of 20 000: 19 999 → error, 20 000 → null.

- [ ] **Step 2: Run to verify they fail.** Run the Android command. Expected: FAIL (`commission(20_000)` is 100, and `commissionIsMinimum` is unresolved).

- [ ] **Step 3: Implement.**
  - **Funding:** use `ServiceFee(pricing.address, 50u, SWAP_FEE_FLOOR_SAT.toULong(), 0uL)`.
  - **Display:** add `commission` and `commissionIsMinimum`. Each commission row (new swap, take preview, detail) uses the label `swap_commission_min` ("Commission (minimum)") when `commissionIsMinimum(sendSat)` is true, and the existing "Commission 0.5%" otherwise.
  - **Fakes:** the preview fakes use a minimum of 20 000, and `new_small` shows a 30 000 sat swap.
  - **CHANGELOG:** add under the swap entry: "Commission is 0.5% per side with a 400 sat minimum; swaps start at 20 000 sat."

- [ ] **Step 4: Run tests and build.** Run the Android command. Expected: PASS (145 before, plus the new tests), and `assembleDebug` succeeds.

- [ ] **Step 5: Device check (dev apps only), if the phone is connected.**
  - Run `assembleDebug -Pdev2` and then `assembleDebug`, and install both with `adb install -r`.
  - Run `adb shell am start -n com.fortistechlabs.wallet.dev/com.fortis.wallet.SwapPreviewActivity --es screen new_small` and take a screenshot.
  - Expected: "Commission (minimum)" with 0.00000400 BTC.
  - If the phone is absent or locked, say so in the report.

- [ ] **Step 6: No commit.**
