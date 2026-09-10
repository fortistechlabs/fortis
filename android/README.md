# fortis — native Android wallet

Jetpack Compose. Keys and every signature come from `crates/wallet-ffi` (Rust,
via UniFFI); the encrypted seed lives in DataStore. Same design language as the
web wallet (`/DESIGN.md`), same chain backends (Esplora / a `fortisd` gateway).

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
| `ui/theme/Theme.kt` · `ui/Glass.kt` | the glassy design tokens + components |
| `ui/screens/Screens.kt` | all screens |

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
protection on BTC sends, "take the fee from the amount".

## Not done yet

The **atomic-swap flow** — `wallet-core`/`wallet-wasm` have the HTLC contract and
client state machine, but `wallet-ffi` bridges only `swap_pubkey` / `sign_swap`
to mobile, there is no `swap-orchestrator` service in this repo for offer
matching + the watchtower, and the web wallet has no swap UI to mirror. The
security-critical parts (derivation, coin selection, `SIGHASH_UNIFIED` signing)
are all in `wallet-ffi` and shared with the web wallet.
