# fortis — native Android wallet

Jetpack Compose. Keys and every signature come from `crates/wallet-ffi` (Rust,
via UniFFI); the encrypted seed lives in DataStore. Design language in `/DESIGN.md`; same chain backends as the web wallet (Esplora / a `fortisd` gateway).

```
Compose UI  ─►  WalletViewModel  ─►  wallet-ffi (Rust .so)   keys, coin-select, sign
                                 └►  Backend (OkHttp)         Esplora | fortisd
```

## Toolchain

`gradle/libs.versions.toml`, `gradle/wrapper/gradle-wrapper.properties`:

| | |
|---|---|
| Gradle | 9.7.1 |
| AGP | 9.2.0 (compileSdk / targetSdk 37 — matches your installed platform) |
| Kotlin | 2.2.20 |
| Gradle JDK | 21 recommended; 25 works on Gradle 9.7 if you'd rather keep the bundled JBR |
| Rust ↔ Gradle | [Gobley](https://gobley.dev) `dev.gobley.cargo` + `dev.gobley.uniffi` 0.3.7 |
| `wallet-ffi` uniffi | 0.29 (workspace `Cargo.toml`) |

AGP 9 is a major release — `android.builtInKotlin=false` in `gradle.properties`
keeps the explicit Kotlin plugin; if a plugin trips on the new DSL, add
`android.newDsl=false` there too (temporary, gone in AGP 10).

## First-time setup

1. **Gradle JDK** — Settings → Build, Execution, Deployment → Build Tools →
   Gradle → **Gradle JDK: 21** (Download JDK → 21 if not listed). AGP wants
   JDK 17+; the bundled JBR 25 is too new for some plugins.
2. **Install the NDK** — Settings → Languages & Frameworks → Android SDK →
   **SDK Tools** → check **NDK (Side by side)** + **CMake** → Apply.
3. **`cargo` on PATH** — Gobley calls `cargo` directly; make sure `~/.cargo/bin`
   is on the PATH the Gradle daemon sees (a normal `rustup` install does this;
   otherwise set `CARGO_HOME`).
4. `rustup target add aarch64-linux-android x86_64-linux-android` (done if you
   built from this repo).

**Open the `android/` folder** → Sync → Run on an emulator (`x86_64`) or an
`arm64-v8a` device.

## How the Rust wiring works

Gobley's **cargo** plugin cross-compiles `crates/wallet-ffi` (pointed at by
`cargo { packageDirectory = ... }`) to the app's `jniLibs` for each
`ndk.abiFilters` ABI. Its **uniffi** plugin then runs library-mode bindgen and
drops the generated Kotlin (`package uniffi.wallet_ffi`) into the build. JNA is
the JVM runtime it calls through — Gobley wires it; if a sync error says JNA or
`kotlinx-atomicfu` is unresolved, add `implementation("net.java.dev.jna:jna:5.17.0@aar")`.

### Manual fallback (if Gobley fights the bleeding-edge AGP)

```sh
cargo install cargo-ndk
cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --release -p wallet-ffi
cargo run -p wallet-ffi --bin uniffi-bindgen -- generate \
  --library android/app/src/main/jniLibs/arm64-v8a/libwallet_ffi.so \
  --language kotlin --out-dir android/app/src/main/kotlin
```
then drop the `dev.gobley.*` plugins + `cargo {}` / `uniffi {}` blocks and add
`implementation("net.java.dev.jna:jna:5.17.0@aar")`.

## Layout

| | |
|---|---|
| `MainActivity.kt` / `App.kt` | entry + phase switch (Onboard / Gen / Create / Restore → AppLock → Shell) |
| `WalletViewModel.kt` | orchestration — mirror of `web/src/app.js` |
| `wallet/WalletSession.kt` | wraps the `wallet-ffi` `Wallet` + `WalletView`; seal/unseal |
| `data/Store.kt` | DataStore — the encrypted seed + config |
| `data/EsploraBackend.kt` · `GatewayBackend.kt` | chain backends (OkHttp) |
| `ui/theme/Theme.kt` · `ui/Glass.kt` | the Graphite signal tokens (`Fx`), type scale (`FxType`) + components — see `/DESIGN.md` |
| `ui/screens/Screens.kt` | all screens, except `HwConnectScreen.kt` (connect a hardware wallet) and `SwapScreens.kt` (swaps) |

## Done since first cut

Biometric / device-credential app lock (`ui/Biometric.kt` + `data/SeedKeystore.kt`
— one unlock opens every wallet; the app secret is wrapped by a Keystore key
gated on `BIOMETRIC_STRONG | DEVICE_CREDENTIAL`, generated in the **StrongBox**
secure element where present, TEE otherwise; Settings warns if it fell back to
software). QR on receive (`ui/Qr.kt`, long-press to copy as an image). Balance
count-up animation on the Wallet hero (`countUpSat`). Gap-limit auto-advance —
`Backend.firstUnusedReceive` walks the scan past receive addresses already seen
on-chain so "Receive" shows a fresh one. Multi-wallet (up to 10, per-chain),
in-app locale picker (75 languages), approximate USD value, opt-in XBT replay
protection on BTC sends, "take the fee from the amount". Connect a hardware
wallet (`ui/screens/HwConnectScreen.kt`): its BIP-84 account key arrives as text
or a BC-UR QR code — animated ones via `ui/MultiScan.kt`'s continuous scanner —
and is parsed and checked in Rust (`wallet-core/src/account_import.rs`, bound as
`importAccount` / `UrAccountScanner`); the BTC watch-only wallet it creates
is offered as an XBT twin too.

## Swaps

Peer-to-peer BTC ⇄ XBT atomic swaps through a `fortis-edge --swap` coordinator
(API, price rules and the watchtower: `crates/fortis-edge/README.md`). The Swap
button on Home appears only while the edge answers `GET /swap/quote` (or a swap is
still running), so nothing shows until the server enables it. Screens are in
`ui/screens/SwapScreens.kt` (list, new offer, take a private code, detail).

```
SwapScreens ─► WalletViewModel ─► SwapEngine ─► SwapApi (edge /swap/*)
                                      │       └► ChainWatch (edge /xbt, /btc)
SwapService (foreground, 20 s) ─┐     └► SwapCrypto ─► wallet-ffi SwapSession
SwapWorker (15 min, boot)      ─┼─► SwapRunner ─► SwapEngine.step(id)
unlock hook                    ─┘
```

| | |
|---|---|
| `swap/SwapEngine.kt` | the per-swap state machine, one `step` at a time: post the offer, validate the matched terms against our own NeoxEX price (`validateSwapTerms`), exchange keys / hashlock over the edge relay, fund our HTLC (through a `Funder`), verify theirs, redeem or refund. Hands the refund (and the participant's presigned redeem) to the edge watchtower, and refunds by itself once the timelock passes. |
| `swap/SwapRunner.kt` | steps every active swap, each in its own coroutine; a swap still inside its step is skipped, not queued. One instance, shared by everything below. |
| `swap/SwapService.kt` + `StartGate.kt` | `dataSync` foreground service ("Swap in progress"), ticking every 20 s while any swap is active and stopping itself when none is. `StartGate` makes the start / stop decisions atomic so the service is never left alive with no loop. |
| `swap/SwapWorker.kt` | WorkManager fallback: one pass every 15 min (unique work `swap-sync`) and whenever Android won't run the service (started from the background, or the daily `dataSync` budget spent). |
| `swap/BootReceiver.kt` | after a reboot, one `SwapWorker` pass (Android 15 forbids starting a `dataSync` service from boot). |
| `swap/SwapCrypto.kt` | the seam over wallet-ffi's `SwapSession`, so the engine's JVM tests run against a fake. |
| `swap/SwapNotifications.kt` | the in-progress line and one alert each for matched, needs funding, they funded, done, failed and refunding. Tapping opens the app, which steps the swaps at once. |
| `data/SwapApi.kt` | the edge client. Offers are posted with a `client_id` so a retry returns the same offer. |
| `data/SwapStore.kt` | all swap records in one DataStore blob (`fortis_swaps`), plus the edge token. |
| `data/SwapKeystore.kt` | wraps each swap's two signing secrets with an AES-GCM Keystore key (`fortis_swap_v1`) that does **not** need biometrics, so a swap can refund or redeem in the background while the app is locked. Each secret controls one swap's HTLCs only and is blanked when the swap settles. |

How a swap survives the process dying:
- **Swap index.** Each swap's key branch index is drawn at random from `[0, 2^31)`
  and redrawn on any collision, so two swaps never share keys, even across a
  restore.
- **Provisional record.** A swap is saved (step `PROVISIONAL`, keyed by its
  `client_id`) before the offer is posted. If the app dies mid-POST, the retry
  finds the same offer on the edge and the record is re-keyed to the real offer id.
- **Funding.** The signed funding transaction and its txid are saved before
  broadcast, so a funding the chain hasn't shown yet is re-sent as it was, never
  re-planned.
- **Watchtower.** Once funded, the phone gives the edge its refund, and the
  participant also gives a presigned redeem of the initiator's XBT HTLC. If the
  phone is gone when the preimage appears on BTC, the edge finishes the redeem.
- **Locked wallet.** If the from-wallet is locked when funding is due, the
  `Funder` returns nothing and the swap waits (with a "needs funding"
  notification) until a later pass finds the wallet unlocked.

## Manual test

What you can and can't point the app at:
- The app has **one fixed backend**: `HOSTED_EDGE` in `WalletViewModel.kt` (also
  used by `SwapApi` / `ChainWatch`). There is no custom-edge setting, and
  NeoxEX (`data/NeoxEx.kt`) is fixed too.
- The wallets are mainnet only (no regtest network in the app). So **the app
  cannot be tested against a regtest edge.** The regtest end-to-end test is the
  Rust one below; this section is the on-device check, which uses real coins
  and a staging edge.

Setup:
1. Run a staging edge on a mainnet XBT node + `fortis-index` and a BTC upstream,
   with the swap flags (see `crates/fortis-edge/README.md`), a small cap, and a
   fee address you control:
   ```sh
   cargo run --release -p fortis-edge -- --bind 0.0.0.0:8098 \
     --xbt-upstream http://127.0.0.1:8094 --btc-upstream https://mempool.space/api \
     --service-fee-address <bc1…> --swap --swap-max-sat 100000
   ```
   The minimum BTC leg is 80 000 sat, so the cap leaves little room; each side
   also pays 0.5% plus network fees.
2. Build two debug installs that point at it: change `HOSTED_EDGE` to the staging
   URL (`http://10.0.2.2:8098` from an emulator; for a device on the LAN use the
   host's address, which must be in `src/debug/res/xml/network_security_config.xml`
   because debug builds allow cleartext only to `10.0.2.2`, `127.0.0.1`,
   `localhost` and that LAN entry), then
   `cd android && ./gradlew installDebug` to each device or emulator. Debug
   builds are `com.fortistechlabs.wallet.dev`. **Never** install over, clear or
   uninstall the store app `com.fortistechlabs.wallet`. Use two emulators
   (`adb -s <serial> …`), or one device and one emulator; revert `HOSTED_EDGE`
   afterwards.
3. In each app create a wallet. For XBT → BTC, A holds XBT and B holds BTC.

Run:
1. **XBT → BTC.** On A: Home → Swap → New, XBT to BTC, an amount above the
   minimum, then post. On B: New, BTC to XBT, the same amount. They match at once.
   Both apps raise "matched". The XBT sender (the initiator) funds first; the BTC
   sender (the participant) funds after the XBT HTLC has 6 confirmations. Unlock
   if asked. Watch it reach done on
   both and the redeemed coins arrive.
2. **BTC → XBT.** Repeat the other way round: A posts BTC to XBT, B posts XBT to BTC.
   (A private offer works too: the poster shares the code, the other side uses Take.)
3. **Kill one app mid-swap, then confirm resume:** start a swap and, once both
   sides have funded, kill the *participant's* (BTC sender's) app (`adb -s <serial> shell am
   force-stop com.fortistechlabs.wallet.dev`) and put its device offline.
   - The initiator's app redeems the BTC, which reveals the preimage.
   - Confirm the **watchtower**: the edge log shows the redeem job, and the
     participant's XBT redeem appears on the XBT chain (check the HTLC output is
     spent to the participant's address) without that app ever running.
   - Bring the device back and open the app: the swap shows done (it picks up the
     watchtower's redeem). Force-stop also cancels pending WorkManager jobs until the
     app next opens; to see the service / worker resume instead, swipe the app
     away from recents, or reboot the device, then watch the "Swap in progress"
     notification and the swap move on its own.
   - Also kill the app between posting an offer and the match: on relaunch the
     swap must appear once, not twice.
4. To see a refund: post an offer, let it match, and do not fund on one side;
   the waiting side's HTLC refunds after the timelock (the BTC leg is 24 h on mainnet, the XBT leg 144 h).

## End-to-end test (Rust)

`crates/fortis-edge/tests/swap_e2e.rs` covers the protocol on regtest, with two
`wallet-core` users in place of phones. It needs two nodes: a Knots BLAKE2b
`bitcoind` for XBT and a stock Bitcoin Core `bitcoind` for BTC.

```sh
cargo build --workspace
FORTIS_BITCOIND=…/knots/bitcoind FORTIS_BTC_BITCOIND=…/core/bitcoind \
  cargo test -p fortis-edge --test swap_e2e -- --nocapture
```

Without both variables the scenarios skip and only the `harness_*` checks run.
It starts the edge with the hidden test-only `--swap-tick-secs` and
`--swap-clock-offset-secs`; never set those in production.

## Tests

`gradlew :app:testDebugUnitTest` — JVM unit tests under `app/src/test` (no
device needed), including the swap engine, runner, start gate, store and notifications. The parsing behind them is tested in Rust: `cargo test -p
wallet-core account_import`, `cargo test -p wallet-ffi --test account_import`.
