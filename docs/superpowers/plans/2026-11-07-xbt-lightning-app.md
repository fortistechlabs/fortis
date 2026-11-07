# XBT Lightning, Plan C: the app — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Lightning (XBT) wallet in the Fortis Android app, alongside the BTC
Lightning wallet. It has the same features (receive, send, move funds, offer,
history), refuses invoices for the other chain, and routes pushes by chain.

**Architecture:**

- **fortis-lightning:** gains `LnChain` (`Btc` or `Xbt`) in `NodeConfig`. XBT
  turns on the LDK fork's XBT mode, peer-to-peer gossip and the LSP-only splice
  list.
- **wallet-ffi:** exposes the chain setting, a `WrongChain` error and
  `ln_target_chain` (reads bit 512/513).
- **The app:** a second wallet type `ln-xbt`, and a second `LightningSession`
  set up for XBT. It uses `/xbt` Esplora on the edge, the `xbt` object of
  `/ln/info`, and `/ln/account?chain=xbt`. The screens are reused, with chain
  labels.
- **The gate:** a regtest step proves the app's node layer (fortis-lightning
  plus VSS) against the real XBT LSP.

**Tech Stack:** Rust (fortis-lightning, wallet-ffi via UniFFI and Gobley),
Kotlin and Jetpack Compose, the regtest harness from Plans A and B.

**Spec:** `docs/superpowers/specs/2026-11-06-xbt-lightning-design.md` §3 and §4
(app and canary).

**Depends on:** Plan B, branch `feat/xbt-lightning-server`. Plan C branches
from it.

## Global Constraints

- **Branch:** work in a new worktree `~/Work/fortis-xbt-app`, branch
  `feat/xbt-lightning-app`, made from `feat/xbt-lightning-server`. Never push.
- **Wallet type:** `chain = "ln-xbt"` (constant `LN_XBT_CHAIN`), at most one
  per app, next to the BTC Lightning wallet (`"ln"`).
  - Same create and restore flow as BTC Lightning, with its own phrase and
    node seed derived the same way.
  - Amounts in sat, with the XBT label and colours, as XBT on-chain wallets
    show them.
- **Edge:**
  - chain data `"$edge/xbt"` (Esplora);
  - LSP and limits from `/ln/info`'s `xbt` object;
  - Fortis balance and transfers through `POST /ln/account?chain=xbt`;
  - VSS at `/ln/vss` (shared; store ids are per node).
- **Node settings for XBT:**
  - `XbtConfig { gossip_floor_height: 961640, splice_peers: [lsp node id], splice_all_peers: false }`;
  - peer-to-peer gossip, never RGS;
  - Esplora only;
  - no anchor reserve (XBT has no anchors).
- **When "Add wallet" offers Lightning (XBT):** only while `/ln/info`'s `xbt`
  object has `enabled: true`, or in debug builds (`.dev` / `.dev2`).
  - Otherwise the option is hidden.
  - An existing `ln-xbt` wallet shows `xbt.message` when it's disabled.
- **Offer tab** on the XBT wallet: shown only while the XBT LSP holds custody
  (`/ln/info` `xbt` reports `custody_max_sat > 0`, or the account API reports
  room). Otherwise it's hidden.
- **Copy, exact:**
  - `"This is a BTC Lightning invoice"`, and `"This is a BTC Lightning offer"`
    for offers;
  - `"This is an XBT Lightning invoice"`, and `"This is an XBT Lightning offer"`
    for offers;
  - `"Pay from your Lightning (XBT) wallet"` / `"Pay from your Lightning wallet"`;
  - QR caption `"XBT Lightning"`.
- **Push:** the data `chain` (`"btc"` or `"xbt"`) picks the wallet a
  notification opens. A missing `chain` means `"btc"` (older LSP builds).
- **BTC must stay unchanged:** the `ln` wallet's behaviour and stored config.
  No migration.
- **Commit messages** end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_019QuZCvQdMzZS7AMNP6Hiqf
  ```
- **This host runs production services:** never use `pkill`, `killall` or
  `systemctl`. Regtest goes only through `tools/xbt-regtest`.

## Review Focus

1. **Scanning an XBT invoice in the BTC Lightning wallet** (and the reverse):
   it's refused before any payment, and the other wallet is offered when it
   exists. Tested in Tasks 2 and 6.
2. **A Lightning address or LNURL whose fetched invoice is for the other
   chain:** refused after the fetch, with the same message. Tested in Task 6.
3. **Both Lightning wallets open at once:** both nodes run in one process,
   each with its own data dir, VSS store and LSP. The BTC node must not pick
   up XBT settings. Tested in Tasks 1 and 5.
4. **A push with no `chain`** (from an old BTC LSP) opens the BTC Lightning
   wallet. Tested in Task 6.
5. **`/ln/info` with no `xbt` object** (old edge): the XBT option is hidden in
   release builds, and the BTC wallet still loads. Tested in Task 4.

---

### Task 1: fortis-lightning — the chain setting

**Files:**
- Modify: `crates/fortis-lightning/src/node.rs`
  - `NodeConfig`, about line 189;
  - the builder wiring, about lines 1310–1360: anchors, chain source, RGS;
  - error mapping.
- Modify: `crates/fortis-lightning/src/error.rs`
- Test: in `node.rs` tests, plus `crates/fortis-lightning/tests/node.rs` if a
  build test fits there.

**Interfaces:**
- Produces:
  - `pub enum LnChain { Btc, Xbt }` (`Copy`, `PartialEq`, `Debug`);
  - `NodeConfig.chain: LnChain`;
  - `LnError::WrongChain`, mapped from `ldk_node::NodeError::WrongChain`;
  - `LnError::Config(String)` (or the crate's existing config-error variant)
    for an XBT node with a `BitcoindRpc` chain source.
- A pure function
  `pub(crate) fn ldk_settings(chain: LnChain, network: Network, lsp_id: PublicKey) -> LdkSettings`.
  The `LdkSettings` fields are `xbt: Option<XbtConfig>`, `rgs_url: Option<String>`
  and `anchors_trust_lsp: bool`. The builder applies them.
  - **Xbt:** `xbt = Some(XbtConfig { gossip_floor_height: 961640, splice_peers: vec![lsp_id], splice_all_peers: false })`,
    `rgs_url = None`, `anchors_trust_lsp = false`.
  - **Btc:** exactly today's settings (`xbt = None`, RGS on mainnet, LSP
    trusted for anchors).

- [ ] **Step 1: Write the failing tests**
  - `btc_settings_unchanged`
  - `xbt_settings`: the exact values above.
  - `xbt_with_bitcoind_rpc_is_a_config_error`
  - `wrong_chain_maps_to_ln_error`
- [ ] **Step 2: Run them and see them fail**

  Run: `$HOME/.cargo/bin/cargo test -p fortis-lightning --lib`
- [ ] **Step 3: Implement the Interfaces above.** Update every place that
  builds `NodeConfig` (tests and wallet-ffi) with `chain: LnChain::Btc` for
  now. Task 2 exposes the setting.
- [ ] **Step 4: Run the tests.** Expected: the lib tests pass, and
  `cargo check --workspace --all-targets` is clean.
- [ ] **Step 5: Commit**

  `fortis-lightning: LnChain (XBT mode, P2P gossip, LSP splice list)`

### Task 2: wallet-ffi — the chain setting, `WrongChain`, `ln_target_chain`

**Files:**
- Modify: `crates/wallet-ffi/src/lightning.rs`
  - `LnNodeConfig`, about line 188;
  - `LnFfiError`, about line 24;
  - the new exported function.
- Test: `crates/wallet-ffi` tests, following the existing `ln_parse` tests.

**Interfaces:**
- Consumes: Task 1's `LnChain` and `LnError::WrongChain`.
- Produces:
  - `#[derive(uniffi::Enum)] pub enum LnChainFfi { Btc, Xbt }`. Use the name
    `LnChain` in Kotlin if UniFFI allows it.
  - `LnNodeConfig.chain`;
  - `LnFfiError::WrongChain`;
  - `#[uniffi::export] pub fn ln_target_chain(target: LnTarget) -> LnTargetChain`,
    where `LnTargetChain` is `Btc`, `Xbt` or `Unknown`:
    - BOLT11 and BOLT12 offers: `Xbt` when the features have bit 512 or 513,
      otherwise `Btc`;
    - `bitcoin:` links with a Lightning part: the same rule, applied to that
      part;
    - Lightning address and LNURL: `Unknown`, because the chain is only known
      once the invoice is fetched (Task 6 checks the fetched invoice).

- [ ] **Step 1: Write the failing tests**
  - `target_chain_btc_invoice`: a fixture without bit 512.
  - `target_chain_xbt_invoice`: a fixture with bit 512, built with the
    vendored lightning-invoice builder and a fixed key.
  - `target_chain_xbt_offer`
  - `target_chain_lnurl_unknown`
  - `node_config_chain_roundtrip`: the chain reaches `NodeConfig.chain`.
- [ ] **Step 2: Run them and see them fail**

  Run: `cargo test -p wallet-ffi --features lightning`
- [ ] **Step 3: Implement the Interfaces above.**
- [ ] **Step 4: Run the tests,** then build the Android bindings once
  (`./gradlew :app:assembleDebug` from `android/`; Gobley regenerates
  `uniffi.wallet_ffi`). Expected: it compiles, with `LnNodeConfig(chain=…)`
  and `lnTargetChain` in the Kotlin bindings. Fix every Kotlin
  `LnNodeConfig(...)` call to pass `LnChain.BTC`, so behaviour is unchanged.
- [ ] **Step 5: Commit**

  `wallet-ffi: LnNodeConfig.chain, WrongChain, ln_target_chain`

### Task 3: Regtest — the app's node layer on XBT

**Files:**
- Modify:
  - `crates/xbt-regtest/src/{env.rs,scenarios.rs,main.rs}`
  - `crates/xbt-regtest/Cargo.toml` (it depends on `fortis-lightning`)
  - `tools/xbt-regtest/{env.sh,run.sh}`

**Interfaces:**
- Consumes:
  - Task 1's `LnChain::Xbt`;
  - the Plan B scenario `lsp_xbt_jit`'s running `fortis-lsp` (chain xbt);
  - a VSS server, started the way `crates/fortis-lightning/tests/common`
    starts one. Reuse or lift that launcher; don't invent a new VSS.
- Produces: the scenario `app_node_xbt`. It runs inside or right after
  `lsp_xbt_jit`, while the LSP is up.
  - A `fortis_lightning::Node` with `chain: Xbt`, regtest Esplora and the
    LSP's info, with the VSS store on its own regtest port (added to
    `XR_PORTS`).
  - **The steps:**
    1. start;
    2. a JIT receive paid by LND (funding `[0x21]` on our inputs);
    3. pay an LND invoice;
    4. `pay` an LND BTC-style invoice without bit 512, which must fail with
       `LnError::WrongChain` before any HTLC is sent;
    5. stop and restart from VSS: the balance is the same;
    6. cooperative close `[0x21, 0x21]`.

- [ ] **Step 1: Add the scenario.**
- [ ] **Step 2: Run `tools/xbt-regtest/run.sh`.** Expected:
  `RESULT: PASS (14 PASS of 14 steps)`, and the environment stopped.
- [ ] **Step 3: Commit**

  `xbt-regtest: fortis-lightning (chain Xbt) against the XBT LSP`

### Task 4: fortis-lsp push text names XBT

**Files:**
- Modify: `crates/fortis-lsp/src/push.rs` (the `wake` and `received`
  title/body).

**Interfaces:**
- The title and body for `Chain::Xbt`:
  - `"Incoming XBT Lightning payment"`
  - `"XBT Lightning payment received"`
- BTC text is unchanged.

- [ ] **Step 1: Write the failing test.** `xbt_push_text`, and
  `btc_push_text_unchanged`.
- [ ] **Step 2: Run them and see them fail**

  Run: `cargo test -p fortis-lsp push::`
- [ ] **Step 3: Implement it.**
- [ ] **Step 4: Run `cargo test -p fortis-lsp`.**
- [ ] **Step 5: Commit**

  `fortis-lsp: XBT push titles`

### Task 5: App data layer — the `ln-xbt` type, `/ln/info` `xbt`, per-chain session wiring

**Files (Kotlin, under `android/app/src/main/kotlin/com/fortis/wallet/`):**
- Modify:
  - `data/Store.kt` (`LN_XBT_CHAIN`)
  - `ui/Format.kt` (`coinOf`: `"ln-xbt"` is XBT)
  - `lightning/LnInfoApi.kt` (parse the `xbt` object)
  - `lightning/FortisAccount.kt` (`EdgeAccountTransport` gains a chain,
    giving the URL `/ln/account?chain=xbt`)
  - `lightning/LightningSession.kt` (the session takes an `LnChain`: Esplora
    path `/btc` or `/xbt`, LSP info source, account transport,
    `LnNodeConfig.chain`)
- Tests, under `android/app/src/test/kotlin/com/fortis/wallet/`: `LnInfoApiTest`,
  `FortisAccountTest` and `LightningSessionTest` (new or extended, following
  the existing tests).

**Interfaces:**
- Produces:
  - `const val LN_XBT_CHAIN = "ln-xbt"`
  - `fun isLightningChain(chain: String): Boolean` (true for `"ln"` and
    `"ln-xbt"`)
  - `fun lnChainOf(chain: String): LnChain?`
  - `data class LnInfoDoc(val btc: LnInfo, val xbt: LnInfo?)` and
    `fun parseLnInfoDoc(json: String): LnInfoDoc`. `xbt` is null when it's
    missing or invalid. The existing `parseLnInfo` stays for the BTC part.
  - `EdgeAccountTransport(base, chain: LnChain)`
  - `LightningSession(..., chain: LnChain)`. The BTC session is built with
    `LnChain.BTC` and doesn't change.
  - `LightningEnv.esploraUrl(walletId, chain)` returns `"$edge/xbt"` for XBT.
    Own-server users keep the hosted edge for XBT Lightning, the same rule BTC
    Lightning follows today.

- [ ] **Step 1: Write the failing tests**
  - `ln_info_doc_with_xbt`
  - `ln_info_doc_without_xbt` (Review Focus 5)
  - `ln_info_doc_bad_xbt_ignored`
  - `account_url_has_chain_xbt`
  - `account_url_btc_unchanged`
  - `session_xbt_uses_xbt_esplora_and_config`: a fake node factory records the
    `LnNodeConfig`.
  - `coin_of_ln_xbt_is_xbt`
- [ ] **Step 2: Run them and see them fail.** From `android/`:
  `./gradlew :app:testDebugUnitTest --tests '*LnInfo*' --tests '*Account*' --tests '*LightningSession*' --tests '*Format*'`
- [ ] **Step 3: Implement the Interfaces above.**
- [ ] **Step 4: Run `./gradlew :app:testDebugUnitTest`.** Expected: all pass.
- [ ] **Step 5: Commit**

  `android: ln-xbt type, /ln/info xbt, per-chain Lightning session`

### Task 6: App — wallet, screens, wrong-chain, transfers, push routing

**Files:**
- Modify:
  - `WalletViewModel.kt`: a second session for `ln-xbt`; the add, delete and
    restore paths; `canAddLightningWallet` per chain.
  - `ui/screens/Screens.kt` (`ChainRow`: `"Lightning (XBT)"`)
  - `ui/screens/LightningWallet.kt` (chain-aware labels and colours, QR
    caption, Offer tab rule)
  - the send path: parse, then `lnTargetChain`; after an LNURL or address
    fetch, check the invoice's chain; refuse or offer the other wallet with
    the exact copy.
  - `lightning/Transfer.kt` and the move screens: XBT on-chain wallets ↔
    `ln-xbt`.
  - `lightning/LightningPush.kt` and `MainActivity.kt`: open the wallet named
    by data `chain` (default `"btc"`).
  - `res/values/strings.xml`
- Tests: the existing view-model and screen tests, extended.

**Interfaces:**
- Consumes: Task 5's types, and Task 2's `lnTargetChain` and
  `LnFfiError.WrongChain`.
- Produces:
  - `fun canAddLightningWallet(wallets: List<WalletConfig>, chain: String, xbtEnabled: Boolean, debug: Boolean): Boolean`
  - `fun wrongChainMessage(walletChain: String, target: LnTargetChain, isOffer: Boolean): String?`,
    which returns the exact copy, or null when the target matches.
  - `fun pushTargetChain(data: Map<String, String>): String`, which returns
    `"ln"` or `"ln-xbt"`.

- [ ] **Step 1: Write the failing tests**
  - `can_add_xbt_only_when_enabled_or_debug`
  - `one_xbt_lightning_wallet_per_app`
  - `wrong_chain_messages_exact_copy`: all four strings.
  - `lnurl_invoice_for_other_chain_refused` (Review Focus 2): the fetched
    invoice has bit 512 and the wallet is `ln`. Expect the refusal, and that
    `pay` is never called.
  - `xbt_onchain_scan_offers_ln_xbt` (Review Focus 1)
  - `push_without_chain_opens_btc` (Review Focus 4)
  - `push_xbt_opens_ln_xbt`
  - `both_lightning_wallets_run_side_by_side` (Review Focus 3): the view model
    has two sessions, with distinct data dirs and node configs.
  - `transfer_xbt_onchain_to_ln_xbt_uses_xbt_transit`
- [ ] **Step 2: Run them and see them fail**

  Run: `./gradlew :app:testDebugUnitTest`
- [ ] **Step 3: Implement the Interfaces above.**
- [ ] **Step 4: Run all unit tests,** then `./gradlew :app:assembleDebug`.
  Expected: everything passes and the debug APK builds.
- [ ] **Step 5: Commit**

  `android: Lightning (XBT) wallet — screens, wrong-chain refusals, transfers, push routing`

### Task 7: Canary runbook

**Files:**
- Modify: `deploy/lightning.md`, a new section 12, "XBT Lightning canary
  (real phone)".

**Content:** these are the operator's steps, in order:

1. **Prerequisites:** Plan B's §11 is done, with `enabled: false`, and the XBT
   LSP is funded with outbound channels.
2. **Build and install the `.dev` debug APK.** Debug builds show "Lightning
   (XBT)" even while it's disabled.
3. **Create the wallet,** write down the phrase, and restore it on a second
   install, so the takeover is checked.
4. **JIT receive:**
   - with the app open, for an amount above `channel_min_sat`, paid from an
     outside XBT Lightning node (e.g. the LND fork);
   - then with the app closed, checking the push wakes it (the title is
     "Incoming XBT Lightning payment").
5. **Move funds:**
   - in, from an XBT on-chain wallet;
   - out, part of it;
   - **Move all.**
6. **Pay an outside XBT invoice and an outside XBT offer.**
7. **Wrong chain:** pasting a BTC invoice into the XBT wallet shows "This is a
   BTC Lightning invoice", and the reverse.
8. **Offer receive with the app closed,** only once custody is on.
9. **Enable:** set `"enabled": true` in `/etc/fortis/ln-info-xbt.json` once
   steps 3–7 pass. The edge reloads the file without a restart.

- [ ] **Step 1: Write the section.** Verify every command and file name
  against the repo.
- [ ] **Step 2: Commit**

  `deploy: XBT Lightning real-phone canary`

---

## Self-review notes

- **Spec §3 coverage:**
  - wallet type and keys (Tasks 5–6);
  - running both nodes (Tasks 1 and 6);
  - fortis-lightning chain setting (Task 1);
  - receive, JIT and the Fortis balance (unchanged session code with the
    chain wiring, Tasks 5–6);
  - offer gating (Task 6);
  - send and wrong-chain (Tasks 2 and 6);
  - transfers (Task 6);
  - history (unchanged ledger, per session);
  - push routing (Tasks 4 and 6).
- **Spec §4:** the regtest part is Task 3, and the real-phone canary is
  Task 7.
- **Deliberately small:** no new screens. The existing Lightning screens take
  the chain. Own-server users keep the hosted edge for XBT Lightning, the same
  rule as BTC Lightning.
