# wallet-core

The security-critical client wallet logic, shared by every platform. One
implementation, one audit.

```
              wallet-core (this crate, pure Rust, no I/O)
              keys · PSBT · HTLC scripts · SIGHASH_UNIFIED
              signing · swap state machine · watchtower packages
                 │                              │
        wasm-bindgen                          UniFFI
     crates/wallet-wasm                 crates/wallet-ffi
        │                                │            │
     web (React)                    iOS (Swift)   Android (Kotlin)
     IndexedDB + WebAuthn PRF       Keychain/SE   Keystore/StrongBox
```

## Design rules

- **No I/O.** The shell passes data in (UTXOs, confirmations, the counterparty's
  funding tx) and gets transactions back. Storage, networking, biometrics and RNG
  come through the traits in `storage.rs`.
- **State-machine shaped.** `swap::SwapMachine` is driven by `SwapEvent`s the shell
  feeds it; it emits the pre-signed `WatchtowerPackage` the platform needs to finish
  or refund a swap while the wallet is offline.
- **Per-swap keys.** Swap keypairs derive under a hardened branch
  (`m/84'/coin'/0'/2'/<swap>'`) so a leaked swap key can't reach wallet funds.
- `#![forbid(unsafe_code)]`.

## Module map

| Module | Role | State |
|--------|------|-------|
| `types` | `Preimage` alias; everything else comes from `bitcoin` | — |
| `chain` | `Chain` + `ChainParams` — the few values that differ between BTC and XBT | done |
| `keys` | BIP-32/39/84 HD keys, account xpubs, per-swap keys, ECDSA signing, P2WPKH funding-tx signing | done, BIP vector tests |
| `wallet` | Per-chain view: BIP-84 address derivation, largest-first coin selection (`plan_payment` / `plan_htlc_funding`) | done, tested |
| `htlc` | HTLC witness script (Decred/LN shape), funding output, redeem/refund txs, witness finalize | done, tested |
| `sighash` | BIP-143 segwit v0 + `SIGHASH_UNIFIED` (Knots PR #357, script types 0/1) | done — 142 reference vectors pass |
| `swap` | `SwapMachine` state machine, contract build + verify, unsigned redeem/refund + sighash | done, tested |
| `crypto` | Seal/unseal the seed — XChaCha20-Poly1305 + Argon2id password path | done, tested |
| `storage` | `SecureStorage` / `Entropy` traits implemented by each shell | done |

## Status

Everything except mobile bindings is implemented and tested. `cargo test -p
wallet-core` — 24 passing (BIP-32/39 vectors, the 142 non-taproot `SIGHASH_UNIFIED`
reference vectors, a XBT redeem round-trip, coin selection, seed sealing).

`cargo test -p wallet-core --features consensus-verify` adds a full swap round-trip
(`tests/swap_e2e.rs`) that drives both parties' `SwapMachine`s and checks the
**BTC-side** funding / redeem / refund spends against libbitcoinconsensus (real
script + CLTV + BIP-143). The BLAKE2b-side spends (`SIGHASH_UNIFIED`, which stock
libbitcoinconsensus can't verify) are checked for witness structure and signature
validity over the unified message.

Web bindings incl. `SwapSession` are in [`../wallet-wasm`](../wallet-wasm).
Remaining: `wallet-ffi` (UniFFI, mobile).

`SIGHASH_UNIFIED` (Knots PR #357) covers script types 0 (bare/P2SH) and 1 (segwit
v0) — all this wallet signs. Taproot is not implemented. Reference vectors are
vendored at `src/test_data/unified_sighash.json`.

## Dependencies still to add

`miniscript` 12 (descriptors), `argon2` 0.5 + `chacha20poly1305` 0.10 (seed sealing)
— versions pinned in the workspace `Cargo.toml`. Building the C parts of
`secp256k1-sys` needs MSVC Build Tools on Windows (found automatically by `cc` if
Visual Studio is installed); the wasm build additionally needs clang.

## Bindings

`crates/wallet-wasm` (wasm-bindgen) and `crates/wallet-ffi` (UniFFI) are added when
the core has real bodies to expose.
