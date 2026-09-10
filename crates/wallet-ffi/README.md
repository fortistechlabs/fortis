# wallet-ffi

[UniFFI](https://mozilla.github.io/uniffi-rs/) bindings over `wallet-core` for the
iOS and Android shells. Same core the web app uses via `wallet-wasm`.

## Exposed so far

`generateMnemonic(entropy)`, and a `Wallet` object: `fromMnemonic`, `accountXpub`,
`swapPubkey`, `signSwap`. The full swap surface (`WalletView`, HTLC, `SwapSession`)
mirrors `wallet-wasm` and is added as the mobile apps need it.

## Generating the bindings

```
# build the native lib
cargo build --release -p wallet-ffi

# Swift
cargo run -p wallet-ffi --bin uniffi-bindgen -- generate \
  --library target/release/wallet_ffi.dll --language swift --out-dir out/swift

# Kotlin
cargo run -p wallet-ffi --bin uniffi-bindgen -- generate \
  --library target/release/wallet_ffi.dll --language kotlin --out-dir out/kotlin
```

(`wallet_ffi.dll` on Windows, `.so` on Linux, `.dylib` on macOS. Cross-compile to
`aarch64-apple-ios` / `aarch64-linux-android` etc. for device builds.)

## Native shells

- **iOS** — SwiftUI app; store the sealed seed in Keychain / Secure Enclave, unlock
  with Face ID, then construct `Wallet`.
- **Android** — Jetpack Compose; Keystore / StrongBox + BiometricPrompt.

Both talk to the swap orchestrator over HTTP exactly as `apps/trade` does.
