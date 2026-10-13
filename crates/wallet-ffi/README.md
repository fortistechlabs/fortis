# wallet-ffi

[UniFFI](https://mozilla.github.io/uniffi-rs/) bindings over `wallet-core` for the
iOS and Android shells. Same core the web app uses via `wallet-wasm`.

## Exposed so far

`generateMnemonic(entropy)`, and a `Wallet` object: `fromMnemonic`, `accountXpub`,
`swapPubkey`, `swapSecret`, `signSwap`, plus `WalletView` and the P2P swap object
`SwapSession` (one swap from one phone: both HTLCs, funding checks, signed refunds
and redeems, presigned redeems; its secrets are never in its JSON), with the
helpers `validateSwapTerms` (checks matched terms against the user's limit price: the lowest price for the XBT sender, the highest for the BTC sender), `participantFundLatest`, `swapCommissionSat` (the commission one side pays: 0.5%, at least 400 sat) and `txidOf`.

Swaps: this `SwapSession` (P2P, one phone's half of a swap through `fortis-edge --swap`) exists only
here; `wallet-wasm` has a different, older `SwapSession` and the web wallet has no P2P swap UI.

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

Both talk to the swap coordinator (`fortis-edge --swap`) over HTTP; the Android
client is in [`android/`](../../android).
