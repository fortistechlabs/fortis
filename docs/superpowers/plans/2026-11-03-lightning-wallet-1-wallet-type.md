# Lightning Wallet — Plan 1: the wallet type

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Lightning a separate wallet (`chain = "ln"`, one per app, its own recovery phrase) that holds send, invoice-receive and history, and remove Lightning from BTC wallets.

**Architecture:** `WalletConfig` gains the `"ln"` chain; a small `LightningWalletKeys` holds the node's BIP-39 seed derived from the Lightning wallet's own phrase (same `lnBip39Seed` derivation as today), so `fortis-lightning`, VSS and recovery are unchanged. `LightningSession` loses the per-wallet toggle and runs for the one Lightning wallet whenever the app is unlocked. The Lightning send/receive/history UI moves out of the BTC wallet tabs into `ui/screens/LightningWallet.kt`.

**Tech Stack:** Kotlin, Jetpack Compose, UniFFI `wallet_ffi` (Rust `fortis-lightning` / ldk-node 0.7.1), DataStore, JUnit.

**Spec:** `docs/superpowers/specs/2026-11-03-lightning-wallet-design.md` (sections 1 and 4; transfers are Plan 2, offers/async/push are Plan 3).

## Global Constraints

- Wallet type: `chain = "ln"`, `network = "bitcoin"`. At most one per app.
- Keys: its own BIP-39 phrase, sealed like BTC wallets (`sealSeed`/`unsealSeed`); node seed = `uniffi.wallet_ffi.lnBip39Seed(mnemonic, passphrase)` — unchanged derivation.
- No migration: the per-BTC-wallet `ln_enabled_<walletId>` flag and "Move to Lightning" are removed outright (Lightning never shipped).
- On-chain wallets no longer show Lightning balance, Lightning receive QR, Lightning history, or pay Lightning targets; scanning one offers "Pay with your Lightning wallet".
- Closing the channel sends funds to the first software mainnet BTC wallet (`lightningSweepWallet`) until Plan 2's transfer screen exists.
- Deferred to Plan 2 (transfers): funding from on-chain, splice in/out, the transfer fee, "not enough on Lightning → transfer" and "on-chain address in the Lightning wallet → transfer". Until then a Lightning wallet is funded by its first invoice receive (JIT channel), and a scanned on-chain address in it shows "Pay from an on-chain wallet" only.
- New user-facing strings ship in English and the 8 locales that already have Lightning strings: de, es, fr, pt-rBR, pt-rPT, zh-rCN, zh-rTW, zh-rHK.
- Unit tests: `cd android && JAVA_HOME=$HOME/.local/share/mise/installs/java/temurin-21 $JAVA_HOME/bin/java -cp gradle/wrapper/gradle-wrapper.jar org.gradle.wrapper.GradleWrapperMain --offline :app:testDebugUnitTest` (add `--tests <Class>` to narrow).

## Review Focus

- The app's **only** wallet is the Lightning wallet: app unlock (`appUnlockWithPassword`) and lock must work without any BTC wallet — Task 3 test `appUnlockUnsealsALightningOnlyApp`.
- A **Lightning deep link with no Lightning wallet**: it must not try to pay from a BTC wallet; it should prompt to create one — Task 5 test `lightningLinkWithoutLightningWalletAsksToCreateOne`.
- **Restoring a second Lightning wallet** (phrase entered while one exists) must be refused before anything is saved — Task 3 test `secondLightningWalletIsRefused`.
- **Deleting the Lightning wallet while the node is running with a channel**: warn with the balance, stop the node, wipe local state — Task 4 test `forgetStopsTheNodeAndWipesLocalState`.
- **Closing the channel with no BTC wallet** to receive the funds: clear error, nothing broadcast — Task 4 test `noSweepWalletMeansNoSweepAddress`.

---

### Task 1: `"ln"` wallet type in the model

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/Store.kt` (`WalletConfig`, lines 30-75; Lightning keys at 219-240; `remove` at ~287)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/lightning/LnState.kt:41-48`
- Test: `android/app/src/test/kotlin/com/fortis/wallet/data/WalletConfigTest.kt`
- Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/LightningWalletRulesTest.kt` (new)

**Interfaces:**
- Produces: `const val LN_CHAIN = "ln"` (in `Store.kt`); `WalletConfig.isLightning: Boolean`; `WalletConfig.display` returns `"Lightning · <name>"` for `"ln"`; in `LnState.kt`: `fun isLightningWallet(w: WalletConfig?): Boolean`, `fun canAddLightningWallet(wallets: List<WalletConfig>): Boolean`, `fun lightningWalletOf(wallets: List<WalletConfig>): WalletConfig?`, `fun lightningSweepWallet(wallets: List<WalletConfig>): WalletConfig?` (first wallet with `chain == "btc"`, network `mainnet`/`bitcoin`, not watch-only, not hardware, `sealed != null`).
- Removes: `WalletConfig.supportsLightning`, `showLightningToggle`, the `ln_enabled_` keys and `Store.lightningEnabled/setLightningEnabled`.

- [ ] **Step 1: Write the failing tests**

`WalletConfigTest`: `lightningWalletRoundTripsThroughJson` — a `WalletConfig(id, "Spend", "ln", "bitcoin", sealed, salt)` survives `toJson()`/parse with `chain == "ln"`, `isLightning == true`, `display == "Lightning · Spend"`; a BTC config has `isLightning == false`.

`LightningWalletRulesTest`:
- `onlyOneLightningWalletPerApp`: `canAddLightningWallet(listOf(btc))` is true, `canAddLightningWallet(listOf(btc, ln))` is false; `lightningWalletOf(listOf(btc, ln)) == ln`.
- `sweepWalletIsTheFirstSoftwareMainnetBtcWallet`: from `[xbt, btcWatchOnly, btcHardware, btcRegtest, btcA, btcB]` returns `btcA`; from `[xbt, ln]` returns `null`.

- [ ] **Step 2: Run them; expect compile failures (unresolved `isLightning`, `canAddLightningWallet`, …)**

- [ ] **Step 3: Implement the interfaces above; delete `supportsLightning`, `showLightningToggle`, the `ln_enabled_` key functions and their `remove` cleanup**

Fix every compile error the removals cause by replacing `supportsLightning`/`showLightningToggle(w)` with `isLightningWallet(w)` where the code is about the Lightning wallet, or deleting the branch where it gated Lightning UI on a BTC wallet (those branches are rewritten in Tasks 4-5; leave a `// Task 5` marker only if the file would not compile otherwise).

- [ ] **Step 4: Run the two test classes; expect PASS**

- [ ] **Step 5: Commit** — `git commit -m "lightning wallet: ln chain in the wallet model"`

---

### Task 2: `LightningWalletKeys`

**Files:**
- Create: `android/app/src/main/kotlin/com/fortis/wallet/wallet/LightningWalletKeys.kt`
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/wallet/WalletSession.kt:63-80` (drop `bip39Seed`, `lnSeed`, `copyBip39Seed`)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/LightningWalletKeysTest.kt` (new)

**Interfaces:**
- Produces: `class LightningWalletKeys private constructor(seed: ByteArray) : AutoCloseable` with `companion fun fromMnemonic(mnemonic: String, passphrase: String): LightningWalletKeys` (throws on an invalid phrase), `fun copySeed(): ByteArray`, `override fun close()` (zeroes the seed; `copySeed()` after close throws `IllegalStateException`).
- Removes: `WalletSession.copyBip39Seed()`.

- [ ] **Step 1: Write the failing tests** — `seedIsTheLnBip39SeedOfThePhrase`: for the BIP-39 test vector "abandon … about" with passphrase "", `copySeed()` equals `uniffi.wallet_ffi.lnBip39Seed(phrase, "")` and has 64 bytes; `invalidPhraseIsRefused`: `fromMnemonic("abandon abandon", "")` throws; `closeZeroesTheSeed`: after `close()`, `copySeed()` throws.
- [ ] **Step 2: Run; expect FAIL (class missing)**
- [ ] **Step 3: Implement; remove the seed from `WalletSession`** (BTC sessions no longer carry a Lightning seed).
- [ ] **Step 4: Run; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "lightning wallet: its own keys"`

---

### Task 3: Create, restore and unlock the Lightning wallet

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` (`ensureSession` ~495, `appUnlockWithPassword` ~515, `createWallet`/`restoreWallet`/`finishOnboard` 1342-1385, `syncLightningForSelected` 427-442, init 446-455, `lock` ~1321)
- Create: `android/app/src/main/kotlin/com/fortis/wallet/wallet/LightningOnboard.kt` (pure helpers)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/LightningOnboardTest.kt` (new)

**Interfaces:**
- Consumes: Task 1 helpers; Task 2 `LightningWalletKeys`.
- Produces: in `LightningOnboard.kt`: `fun checkCanAddLightning(wallets: List<WalletConfig>)` (throws `LightningWalletExists`), `class LightningWalletExists : Exception()`, `fun newLightningConfig(id: String, name: String, sealed: SealedSeed): WalletConfig` (`chain = "ln"`, `network = "bitcoin"`). In the view model: `ensureSession(id)` returns `null` for `"ln"` wallets; `private fun ensureLightningKeys(id: String): LightningWalletKeys?`; `createWallet`/`restoreWallet` accept `chain = "ln"` and route to Lightning onboarding; `syncLightning()` (renamed from `syncLightningForSelected`) starts the node for `lightningWalletOf(wallets)` whenever `appSecret != null`, independent of the selected wallet; `appUnlockWithPassword` unseals a Lightning wallet through `LightningWalletKeys` when it is the wallet it checks the password with; `lock()` closes all `LightningWalletKeys`.

- [ ] **Step 1: Write the failing tests**
  - `secondLightningWalletIsRefused`: `checkCanAddLightning(listOf(ln))` throws `LightningWalletExists`; `checkCanAddLightning(listOf(btc))` does not.
  - `newLightningConfigIsLnOnMainnet`: `newLightningConfig("id", "Spend", sealed)` has `chain == "ln"`, `network == "bitcoin"`, `sealed`/`salt` from `sealed.blobHex`/`sealed.saltHex`, `watchOnly == false`.
  - `appUnlockUnsealsALightningOnlyApp`: seal a phrase with `sealSeed(phrase, "", "pw")`, then `val (m, p) = unsealSeed(s.blobHex, s.saltHex, "pw"); LightningWalletKeys.fromMnemonic(m, p)` gives the same seed as Task 2's vector (the path `appUnlockWithPassword` takes for an `ln` wallet).
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement the helpers and the view-model changes listed above.** `finishOnboard` for `"ln"`: `checkCanAddLightning(wallets)` before sealing (error string `R.string.error_lightning_wallet_exists`), validate with `LightningWalletKeys.fromMnemonic`, seal, save `newLightningConfig`, keep the keys in the view model's Lightning keys map, `syncLightning()`. `recoverSwaps` is skipped for `"ln"`.
- [ ] **Step 4: Run the new tests and `LightningSessionTest`; expect PASS** (fix `LightningSessionTest` cases that used `enable`/`disable` in Task 4)
- [ ] **Step 5: Commit** — `git commit -m "lightning wallet: create, restore, unlock"`

---

### Task 4: `LightningSession` without the toggle; delete and close

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/lightning/LightningSession.kt` (`LightningStore` 36-48, `InMemoryLightningStore` 56-95, `enable`/`disable` 225-250, move-intent code 319-345)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/Store.kt` (`pendingMove` keys `ln_move_*`)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` (`setLightningEnabled` 368-391, `closeLightningAndMoveToOnchain` 399-425, `removeWallet` 1535, move intent 1897-1935 and in `signAndSend` 2041-2046, the `LightningSession(...)` construction with `sweepAddressProvider`)
- Delete: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/MoveToLightning.kt` (keep `deleteWalletLightningWarningSat`, moved into `LightningWallet.kt` in Task 5), `android/app/src/test/kotlin/com/fortis/wallet/lightning/MoveToLightningTest.kt`
- Test: `LightningSessionTest.kt`, `LightningReviewFixesTest.kt` (drop the move-intent and enable/disable cases), new cases below

**Interfaces:**
- Consumes: Task 1 `lightningSweepWallet`, Task 3 `syncLightning()`.
- Produces: `LightningStore` without `lightningEnabled`, `setLightningEnabled`, `pendingMove`, `setPendingMove`; `LightningSession.forget(walletId: String): Job` (stops the node if it is `walletId`'s, deletes `store.dataDir(walletId)`, sets `LnState.Off`); `LightningSession.requestMoveToChannel`/`commitMove`/`activeMove` removed (Plan 2 adds transfers); `WalletViewModel.closeLightningAndMoveToOnchain(force)` sweeps to `lightningSweepWallet(wallets)`'s next receive address and fails with `R.string.error_no_onchain_wallet_for_lightning` when there is none; `removeWallet(id)` of the Lightning wallet calls `lightning.forget(id)` and closes its keys.

- [ ] **Step 1: Write the failing tests** (in `LightningSessionTest`)
  - `forgetStopsTheNodeAndWipesLocalState`: after `onUnlocked(id, seed)` with a fake node holding a channel, `forget(id).join()` calls the fake's `stop()`, removes the data dir, and leaves `state.value == LnState.Off`.
  - `noSweepWalletMeansNoSweepAddress`: a session whose `sweepAddressProvider` returns `null` (no BTC wallet) does not call `sweepTransit` after a close event, and `closeAndSweep` reports `LnFfiError.Other("no_sweep_wallet")` without calling `closeChannel`.
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement the interfaces; delete the toggle and move-intent code paths and their tests**
- [ ] **Step 4: Run the whole `com.fortis.wallet.lightning` test package; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "lightning wallet: session without the per-wallet toggle"`

---

### Task 5: The Lightning wallet screens; Lightning out of BTC wallets

**Files:**
- Create: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/LightningWallet.kt`
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/Screens.kt` — `WalletTab` 843-1005, `ReceiveTab` 1040-1183, `SendTab` 1184-1463, `HistoryTab` 1500-1599, `SettingsTab` 664-842 (Lightning card at 754 and delete dialog 815-830), `HomeTab` 598-642, `ChainRow` 388-392 and its callers in `CreateScreen`/`RestoreScreen`
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/Qr.kt` (lightning-only receive QR; Lightning-target check)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` (`openSendWithUri` 1936; `payWithLightningWallet`)
- Modify: `android/app/src/main/res/values*/strings.xml` (9 files)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/ReceiveQrTest.kt`, `SendRoutingTest.kt`, `HomeBalancesTest.kt`

**Interfaces:**
- Consumes: Tasks 1-4.
- Produces:
  - `Qr.kt`: `fun buildLightningReceiveQr(amountSat: Long?, quote: LnReceiveQuote?, minLnSat: Long): ReceiveQr` (uri `lightning:<invoice>` when a quote exists, `null`-invoice hint `AddAmountForLightning`/`BelowLightningMin` otherwise, fee/you-get as today's Lightning branch of `buildReceiveQr`); `fun isLightningTarget(text: String, network: String, nowSecs: ULong? = null): Boolean` (true for every `parseScan` result except a plain on-chain address/BIP21 without `lightning=`/`lno=`). `buildReceiveQr` loses its `lightningEnabled` parameter and always returns an on-chain QR.
  - `LightningWallet.kt`: `@Composable fun LightningWalletTab(vm: WalletViewModel)` (header: `homeBalance(0L, lnState)` total with the pending line; sub-tabs Receive / Send / History), `LightningReceiveTab`, `LightningSendTab`, `LightningHistory`, `LightningWalletSettingsCard` (status, takeover, "Close channel and move to on-chain" with the force-close dialog), and the moved `deleteWalletLightningWarningSat`.
  - View model: `fun payWithLightningWallet(text: String)` (selects the Lightning wallet and hands it `text` through `pendingSendText`); `openSendWithUri(text)` selects the Lightning wallet for Lightning targets, or sets `lightningWalletPrompt = true` when there is none.
  - Strings (EN copy fixed here; translate for the 8 locales): `wallet_chain_lightning` "Lightning"; `error_lightning_wallet_exists` "This app already has a Lightning wallet."; `error_no_onchain_wallet_for_lightning` "Add a BTC wallet to move Lightning funds on-chain."; `send_pay_with_lightning_wallet` "Pay with your Lightning wallet"; `send_create_lightning_wallet` "Create a Lightning wallet to pay this"; `ln_wallet_first_receive_hint` "Receive a payment to open your first channel."

- [ ] **Step 1: Write the failing tests**
  - `ReceiveQrTest.lightningWalletQrIsTheInvoiceOnly`: `buildLightningReceiveQr(50_000, quote(invoice = "lnbc1…", fee = 1_000_000 msat), 10_000).uri == "lightning:lnbc1…"`, `feeSat == 1_000`, `youGetSat == 49_000`; with `amountSat = null` the hint is `ReceiveHint.AddAmountForLightning` and the uri is empty.
  - `ReceiveQrTest.btcWalletQrIsOnChainOnly`: `buildReceiveQr(addr, 50_000, quote, 10_000).uri == "bitcoin:$addr?amount=0.0005"` (no `lightning=`).
  - `SendRoutingTest.lightningTargetsAreRecognised`: `isLightningTarget` is true for a BOLT11, an offer, `alice@example.com`, `bitcoin:addr?lightning=lnbc…`, and false for a bare address and `bitcoin:addr?amount=0.1`.
  - `SendRoutingTest.lightningLinkWithoutLightningWalletAsksToCreateOne`: for wallets `[btc]` and a BOLT11, the routing decision (`lightningLinkRoute(wallets, text, network)`, a pure helper next to `isLightningTarget` returning `LightningLinkRoute.OpenLightningWallet(id)` / `CreateLightningWallet` / `OnChain`) is `CreateLightningWallet`; for `[btc, ln]` it is `OpenLightningWallet(ln.id)`; for a bare address it is `OnChain`.
  - `HomeBalancesTest.lightningWalletBalanceIsLightningPlusMoving`: `homeBalance(0L, Ready(lightning = 80_000, transit = 20_000)).totalSat == 100_000`, `moving == true`.
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement**
  - Move the Lightning branches of `ReceiveTab`, `SendTab` (the `isLightningFlow`/`SendPlan.Lightning`/LNURL code and `LnConfirmSheet` use) and `HistoryTab` (payments list) into `LightningWallet.kt`; the BTC tabs keep only on-chain behaviour and call `planSend(..., ln = LnState.Off)`. In the BTC `SendTab`, a `isLightningTarget` text shows `send_pay_with_lightning_wallet` (→ `vm.payWithLightningWallet(text)`) or `send_create_lightning_wallet` (→ add-wallet flow with `chain = "ln"` preselected).
  - `WalletTab`: `if (c.isLightning) { LightningWalletTab(vm); return }`; BTC header shows on-chain only.
  - `SettingsTab`: remove `LightningSettingsCard`; the Lightning wallet's settings show `LightningWalletSettingsCard`; the delete dialog uses `deleteWalletLightningWarningSat` only for the Lightning wallet.
  - `HomeTab`: the Lightning wallet's row shows `homeBalance(0L, lnState).totalSat` and the pending marker; BTC rows no longer add Lightning.
  - `ChainRow`: options `xbt`, `btc`, and `ln` ("Lightning") when `canAddLightningWallet(vm.wallets)`; for `ln` the create/restore screens call `vm.createWallet(name, "ln", "bitcoin", passphrase, lock)` / `vm.restoreWallet(...)`.
  - Strings: add the six keys to `values/strings.xml` and the 8 locales; remove strings only used by the deleted toggle and Move to Lightning.
- [ ] **Step 4: Run the full unit test suite; expect PASS** (`:app:testDebugUnitTest`)
- [ ] **Step 5: Build and install the debug APK on the Pixel; check by hand:** create a Lightning wallet beside the existing BTC wallet; the BTC wallet shows no Lightning anywhere; the Lightning wallet reaches "LSP connected · backup OK"; receive 51,000+ sat by invoice from an outside wallet opens a channel; send 1 sat to `donal@bitbox.cash` with fee 0; a `lightning:` link from the browser opens the Lightning wallet.
- [ ] **Step 6: Commit** — `git commit -m "lightning wallet: its own screens; Lightning out of BTC wallets"`

---

### Task 6: Docs

**Files:**
- Modify: `deploy/lightning.md` §10 (canary: replace "Enable Lightning on `.dev`" with "Create a Lightning wallet on `.dev`"; restore by its own phrase)
- Modify: `CHANGELOG.md` (0.6.0 entry: Lightning is a separate wallet)

- [ ] **Step 1: Update both files to the behaviour above**
- [ ] **Step 2: Commit** — `git commit -m "docs: lightning wallet"`
