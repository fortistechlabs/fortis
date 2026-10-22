# Android own Esplora server — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Android users point each chain (BTC, XBT) at their own HTTPS Esplora server, switchable with fortis servers, with no fallback while it's on.

**Architecture:** A new `data/OwnServer.kt` holds URL validation (HTTPS, Esplora tip, fork-block hash) and a pure `chooseBackend` that decides edge-plus-fallback vs own-server-only. `Store` persists per-chain URL + on/off. `WalletViewModel` builds its existing `EsploraBackend` from that plan; a Settings card drives it.

**Tech Stack:** Kotlin, Jetpack Compose, OkHttp, DataStore Preferences, JUnit 4.

**Spec:** `docs/superpowers/specs/2026-10-22-android-own-server-design.md`

## Global Constraints

- Android app only (`android/app/src/main/kotlin/com/fortis/wallet/…`). No changes to web, `fortisd`, `fortis-edge`.
- HTTPS only: `res/xml/network_security_config.xml` is not touched.
- Own server applies to `network == "mainnet"` wallets only.
- `FORK_CHECK_HEIGHT = 961_640L`; BTC hash `00000000000000000001d82da6ecccf08e07afa383f9212b0e1b95cc72430c00`; XBT hash `0000000000000050c1e5f69672f459293be14f46e5a494e7a8c8541396f18eeb`.
- Own-server backend: no token, no pricing URL, no `bulkPrewarm`, no `batchScan`, **no fallback**.
- Swaps keep using `HOSTED_EDGE`; `swapFunder` reads the commission address from `$HOSTED_EDGE/pricing`.
- **No commits.** The user makes all git commits; each task ends by leaving its changes in the working tree and reporting the file list.
- Unit tests: `cd android && ./gradlew :app:testDebugUnitTest`. Build check: `./gradlew :app:assembleDebug`.
- New strings go in `res/values/strings.xml` only; other locales fall back to English.

## Review Focus

- One chain on own server, the other on fortis (e.g. XBT own, BTC fortis) → each wallet uses its own chain's setting; changing one chain never rebuilds or redirects the other's backends (Task 3 keys are per chain; Task 4 `onOwnServerChanged(chain)` touches only that chain; Task 6 "Mixed" step).

- A pasted URL with surrounding whitespace or a trailing `/` → normalized and accepted (Task 1 test `trims_whitespace_and_trailing_slash`).
- A server whose tip answer has a trailing newline (`"972599\n"`) → still a valid tip (Task 1 test `tip_with_trailing_newline_is_accepted`).
- A block-hash answer with surrounding whitespace or uppercase hex → compared normalized, not rejected as wrong chain (Task 1 test `hash_compare_ignores_case_and_whitespace`).
- Switching a chain's server while a refresh for that chain is in flight → the stale result is discarded, not shown under the new server (Task 4: invalidation sets `backend = null` before `refresh()`; `load()`'s existing `backend !== b` guard drops the old answer — checked in Task 6 manual step).
- A test request that hangs → the dialog doesn't spin forever (Task 1: validation uses a client with 10 s call timeout; test `timeout_is_unreachable`).

---

### Task 1: Own-server validation

**Files:**
- Create: `android/app/src/main/kotlin/com/fortis/wallet/data/OwnServer.kt`
- Test: `android/app/src/test/kotlin/com/fortis/wallet/data/OwnServerTest.kt`

**Interfaces:**
- Produces:
  ```kotlin
  const val FORK_CHECK_HEIGHT: Long            // 961_640L
  val FORK_CHECK_HASH: Map<String, String>     // "btc", "xbt" → hash (lowercase)
  sealed interface OwnServerCheck {
      data class Ok(val url: String) : OwnServerCheck
      data object NotHttps : OwnServerCheck
      data object Unreachable : OwnServerCheck
      data object NotEsplora : OwnServerCheck
      data object WrongChain : OwnServerCheck
      data class NotSynced(val tip: Long) : OwnServerCheck
  }
  suspend fun validateOwnServer(http: OkHttpClient, chain: String, url: String): OwnServerCheck
  ```

- [ ] **Step 1: Write the failing tests** using the fake-interceptor pattern from `ExplorerChainCheckTest` (routes map `"GET url"` → `(code, body)`, code 0 throws `IOException`, unlisted → 404, record `asked`). Helper `serve(base, tip = 972_599, hash = FORK_CHECK_HASH["btc"]!!)` sets `GET $base/blocks/tip/height` and `GET $base/block-height/961640`.

  ```kotlin
  @Test fun a_valid_btc_server_is_ok()        // serve("https://n/api") → assertEquals(Ok("https://n/api"), validateOwnServer(http, "btc", "https://n/api"))
  @Test fun a_valid_xbt_server_is_ok()        // serve with xbt hash → Ok for "xbt"
  @Test fun http_is_rejected_without_a_request()   // "http://n/api" → NotHttps; assertTrue(asked.isEmpty())
  @Test fun the_other_chains_hash_is_wrong_chain() // xbt hash, validate "btc" → WrongChain
  @Test fun a_tip_below_the_fork_is_not_synced()   // tip = 961_000 → NotSynced(961_000)
  @Test fun an_html_tip_is_not_esplora()           // tip body "<html>" on both candidates → NotEsplora
  @Test fun a_connection_failure_is_unreachable()  // both candidates code 0 → Unreachable
  @Test fun a_bare_host_gets_api_appended()        // serve("https://n/api") only; input "https://n" → Ok("https://n/api"); asked contains "GET https://n/blocks/tip/height" first
  @Test fun trims_whitespace_and_trailing_slash()  // "  https://n/api/ " → Ok("https://n/api")
  @Test fun tip_with_trailing_newline_is_accepted()// tip body "972599\n" → Ok
  @Test fun hash_compare_ignores_case_and_whitespace() // hash body = btc hash uppercased + "\n" → Ok
  @Test fun timeout_is_unreachable()               // interceptor throws java.net.SocketTimeoutException → Unreachable
  @Test fun an_unknown_chain_is_wrong_chain()      // chain "ltc" with any hash → WrongChain
  ```

- [ ] **Step 2: Run to verify they fail**

  Run: `cd android && ./gradlew :app:testDebugUnitTest --tests 'com.fortis.wallet.data.OwnServerTest'`
  Expected: compilation failure — `validateOwnServer` unresolved.

- [ ] **Step 3: Implement `OwnServer.kt`**

  Order of checks exactly as the spec's Validation section. Run on `Dispatchers.IO`. Build the request client as `http.newBuilder().callTimeout(10, TimeUnit.SECONDS).build()`. Candidate list: trimmed URL without trailing `/`, then `+ "/api"` unless it already ends in `/api`. A candidate "answers" if any HTTP response came back; it is the base if the trimmed body parses with `toLongOrNull()`. Hash compare: `body.trim().lowercase() == FORK_CHECK_HASH[chain]`; missing chain key → `WrongChain`. Any `IOException` on the hash request → `Unreachable`; non-2xx → `NotEsplora`. KDoc on the file in the style of `ChainCheck.kt` (one paragraph: why the fork block).

- [ ] **Step 4: Run to verify they pass**

  Run: same command. Expected: `BUILD SUCCESSFUL`, 13 tests passed.

- [ ] **Step 5: Hand off** — report `OwnServer.kt`, `OwnServerTest.kt` as changed; do not commit.

---

### Task 2: Backend choice

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/OwnServer.kt`
- Test: `android/app/src/test/kotlin/com/fortis/wallet/data/OwnServerTest.kt` (or a new `ChooseBackendTest.kt` alongside)

**Interfaces:**
- Consumes: `PUBLIC_BTC_ESPLORA`, `PUBLIC_XBT_ESPLORA` from `WalletViewModel.kt:69-70` (top-level `const val`s in package `com.fortis.wallet`).
- Produces:
  ```kotlin
  sealed interface BackendTarget {
      data object Edge : BackendTarget
      data class Esplora(val url: String) : BackendTarget
  }
  data class BackendPlan(val primary: BackendTarget, val fallback: BackendTarget?)
  fun chooseBackend(chain: String, network: String, ownServer: String?): BackendPlan
  ```
  (Named `BackendTarget`, not the spec's `Target`, to avoid clashing with Compose/other `Target` names.)

- [ ] **Step 1: Write the failing tests**

  ```kotlin
  @Test fun own_server_on_mainnet_has_no_fallback() =
      assertEquals(BackendPlan(BackendTarget.Esplora("https://n/api"), null), chooseBackend("btc", "mainnet", "https://n/api"))
  @Test fun own_server_off_mainnet_is_ignored() =
      assertEquals(BackendPlan(BackendTarget.Edge, BackendTarget.Esplora(PUBLIC_BTC_ESPLORA)), chooseBackend("btc", "regtest", "https://n/api"))
  @Test fun no_own_server_is_edge_then_public_btc() =
      assertEquals(BackendPlan(BackendTarget.Edge, BackendTarget.Esplora(PUBLIC_BTC_ESPLORA)), chooseBackend("btc", "mainnet", null))
  @Test fun no_own_server_is_edge_then_public_xbt() =
      assertEquals(BackendPlan(BackendTarget.Edge, BackendTarget.Esplora(PUBLIC_XBT_ESPLORA)), chooseBackend("xbt", "mainnet", null))
  ```

- [ ] **Step 2: Run to verify they fail** — same Gradle command with the test class; expected: `chooseBackend` unresolved.
- [ ] **Step 3: Implement `chooseBackend`** in `OwnServer.kt`.
- [ ] **Step 4: Run to verify they pass** — expected all 4 pass.
- [ ] **Step 5: Hand off** — report changed files; do not commit.

---

### Task 3: Store preferences

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/data/Store.kt` (`object K` at ~107-122; new methods after `setSelected`)

**Interfaces:**
- Produces (on `class Store`):
  ```kotlin
  data class OwnServerPrefs(val url: String?, val on: Boolean)          // top-level in Store.kt
  suspend fun ownServerPrefs(chain: String): OwnServerPrefs              // raw saved state, for Settings
  suspend fun ownServer(chain: String): String?                         // url only when saved AND on
  suspend fun setOwnServer(chain: String, url: String?)                 // null = Remove: clears url AND on
  suspend fun setUseOwnServer(chain: String, on: Boolean)
  ```
  Keys: `stringPreferencesKey("own_server_$chain")`, `booleanPreferencesKey("use_own_server_$chain")` (import `booleanPreferencesKey`).

`Store` wraps DataStore and has no unit tests today (spec: checked on device), so this task has no test step.

- [ ] **Step 1: Add the keys and methods** — `setOwnServer(chain, url)` with non-null `url` stores it and leaves `on` untouched; the ViewModel decides when to switch on.
- [ ] **Step 2: Build** — `cd android && ./gradlew :app:compileDebugKotlin`; expected `BUILD SUCCESSFUL`.
- [ ] **Step 3: Hand off** — report `Store.kt`; do not commit.

---

### Task 4: ViewModel wiring

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/WalletViewModel.kt` — `init` (~304), `overviewBackend` (~411), `swapFunder` (~442), `reconnect` (~1400), `edgeBackend` (~1413), `ensureBackend` (~1440), refresh error (~1579).
- Modify: `android/app/src/main/res/values/strings.xml`

**Interfaces:**
- Consumes: Task 1 `validateOwnServer`, `OwnServerCheck`; Task 2 `chooseBackend`, `BackendPlan`, `BackendTarget`; Task 3 `Store.ownServerPrefs/ownServer/setOwnServer/setUseOwnServer`, `OwnServerPrefs`.
- Produces (public on `WalletViewModel`, for Task 5):
  ```kotlin
  var ownServers: Map<String, OwnServerPrefs>   // "btc"/"xbt"; mutableStateOf, private set; loaded in init
  var ownServerError: Map<String, OwnServerCheck>  // last failed check per chain, cleared on success; private set
  var ownServerBusy: String?                       // chain being tested, or null; private set
  fun saveOwnServer(chain: String, url: String)    // validate → on Ok: setOwnServer(chain, ok.url) + setUseOwnServer(chain, true) + switch
  fun useOwnServer(chain: String, on: Boolean)     // on=true re-validates the saved URL first; on failure stays off
  fun removeOwnServer(chain: String)               // setOwnServer(chain, null) + switch
  val usingOwnServer: Boolean                      // the selected wallet's chain/network currently resolve to an own server
  ```

- [ ] **Step 1: Load prefs in `init`** — after `store.load()`, fill `ownServers` for `"btc"` and `"xbt"`. Add a private `ownServerFor(chain, network): String?` returning `ownServers[chain]?.takeIf { it.on }?.url` (mainnet filtering stays in `chooseBackend`).

- [ ] **Step 2: Build backends from the plan.** Add `private fun esploraFor(target: BackendTarget, w: WalletConfig, view, counters): EsploraBackend?` style helper or inline:
  - `ensureBackend()`: `val plan = chooseBackend(c.chain, c.network, ownServerFor(c.chain, c.network))`. `Edge` → existing `edgeBackend(token)`; `Esplora(url)` → `EsploraBackend(http, url, view, counters, chain = c.chain, txCache = txCache)`. `fallback` is built only when `plan.fallback != null`. Guard so `fallback == null` with an own server doesn't re-run the build every call (track with the plan, e.g. compare `backend.label`).
  - `overviewBackend()`: same plan; own-server case builds the plain `EsploraBackend` (no token/pricing/prewarm/batch).
  - `reconnect()`: if `usingOwnServer`, skip `edgeRegister`; set `backend = null` and call `ensureBackend()`.

- [ ] **Step 3: Invalidate on change** — private `onOwnServerChanged(chain)`: reload `ownServers[chain]` from `Store`, remove every `overviewBackends` entry whose wallet is on `chain`, and if `config?.chain == chain` do `backend = null; fallback = null; usingFallback = false; status = null; lastRefreshError = null`, then `ensureBackend(); refresh()`. Called by `saveOwnServer`, `useOwnServer`, `removeOwnServer`. These three run in `viewModelScope.launch`, set `ownServerBusy = chain` around validation, and record failures in `ownServerError`.

- [ ] **Step 4: Swap funding pricing** — in `swapFunder`, replace `b.status().pricing` with a fetch of `"$HOSTED_EDGE/pricing"` parsed into `ServicePricing`. Reuse `EsploraBackend`'s existing parser if it is reachable (it reads `pricingUrl` at ~572); otherwise extract that parse into a top-level `internal fun parsePricing(json: String): ServicePricing?` in `EsploraBackend.kt` and call it from both. Error text stays `"the edge advertises no commission address"` when it is absent.

- [ ] **Step 5: Own-server error copy** — add to `strings.xml`:
  ```xml
  <string name="wallet_own_server_unreachable">can\'t reach your server (%1$s) · will retry</string>
  ```
  The hint in `Screens.kt:900` uses it when `vm.usingOwnServer` (host from `vm.status?.via` or the saved URL's host), else the existing `wallet_refresh_failed`.

- [ ] **Step 6: Build and run the full unit suite**

  Run: `cd android && ./gradlew :app:testDebugUnitTest :app:assembleDebug`
  Expected: `BUILD SUCCESSFUL`, no test regressions.

- [ ] **Step 7: Hand off** — report changed files; do not commit.

---

### Task 5: Settings "Your server" card

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/Screens.kt` — new `GlassCard` in `SettingsTab` (649) after the wallets card; new private `OwnServerDialog` composable next to the existing rename `AlertDialog` (~766).
- Modify: `android/app/src/main/res/values/strings.xml`

**Interfaces:**
- Consumes: Task 4's `ownServers`, `ownServerError`, `ownServerBusy`, `saveOwnServer`, `useOwnServer`, `removeOwnServer`; `OwnServerCheck` from Task 1.

- [ ] **Step 1: Strings** — add to `values/strings.xml`, copy fixed by the spec:
  ```xml
  <string name="settings_own_server">Your server</string>
  <string name="own_server_fortis">Fortis servers</string>
  <string name="own_server_yours">Your server (%1$s)</string>
  <string name="own_server_yours_unset">Your server</string>
  <string name="own_server_dialog_title">%1$s server</string>
  <string name="own_server_hint">Esplora / mempool URL, e.g. https://node.example/api</string>
  <string name="own_server_test_save">Test &amp; save</string>
  <string name="action_change">Change</string>
  <string name="action_remove">Remove</string>
  <string name="own_server_err_https">This server must use HTTPS.</string>
  <string name="own_server_err_unreachable">Can\'t reach this server.</string>
  <string name="own_server_err_not_esplora">Not an Esplora server.</string>
  <string name="own_server_err_wrong_chain">This server is on the other chain.</string>
  <string name="own_server_err_not_synced">This server isn\'t synced yet (block %1$s).</string>
  ```
  Reuse `action_change` / `action_remove` if `strings.xml` already has equivalents (`grep -n 'name="action_' res/values/strings.xml`).

- [ ] **Step 2: The card** — for `chain in listOf("btc", "xbt")`: label `BTC`/`XBT`; two selectable rows (match the existing settings radio/segmented style in this file) for `own_server_fortis` and `own_server_yours` (host) / `own_server_yours_unset`. Selecting *Your server*: saved URL → `vm.useOwnServer(chain, true)`; none → open dialog. Selecting *Fortis* → `vm.useOwnServer(chain, false)`. With a saved URL, `GhostButton`s *Change* (dialog prefilled) and *Remove*. Show `ownServerError[chain]` as text in `Fx.danger` (or the file's existing error colour) under the row. Rows disabled while `ownServerBusy == chain`.

- [ ] **Step 3: The dialog** — `AlertDialog` with an `OutlinedTextField` (single line, `KeyboardType.Uri`), confirm `own_server_test_save` → `vm.saveOwnServer(chain, text)`; shows a progress indicator while `ownServerBusy == chain`, the mapped error string on failure, and dismisses itself when the chain's prefs turn `on` with that URL.

- [ ] **Step 4: Build** — `cd android && ./gradlew :app:assembleDebug`; expected `BUILD SUCCESSFUL`.
- [ ] **Step 5: Hand off** — report changed files; do not commit.

---

### Task 6: Device check

No code. Install with `cd android && ./gradlew installDebug` and walk the spec's manual list:

- [ ] BTC → *Your server* → `https://mempool.emzy.de/api` → saves; balance and history load; status shows `mempool.emzy.de`.
- [ ] Switch BTC to *Fortis servers* → status back to `fortis`; URL still listed. Switch back → re-checked, loads.
- [ ] Enter `https://mempool.guide/api` (XBT) in the BTC row → "This server is on the other chain."
- [ ] Enter `http://mempool.emzy.de/api` → "This server must use HTTPS."
- [ ] While on own server, set an unreachable URL by editing then breaking DNS (airplane mode) → "can't reach your server (host)"; `curl -s http://127.0.0.1:8098/metrics | grep requests_total` on the edge host does not climb from that phone's refreshes.
- [ ] Switch server while a refresh is running → no flash of the previous server's balance.
- [ ] Mixed: XBT on *Your server*, BTC on *Fortis servers* → an XBT wallet's status shows the own host, a BTC wallet's shows `fortis`; the Home overview loads both; switching XBT back and forth never changes the BTC wallet's backend.
- [ ] *Remove* → row back to *Fortis servers*, URL gone; restart the app → still gone.
- [ ] Start a swap from a BTC wallet on own server → funding succeeds and pays commission to the `/pricing` address.
