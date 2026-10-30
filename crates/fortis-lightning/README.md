# fortis-lightning

The only fortis code that touches LDK: a wrapper around
[ldk-node](https://github.com/lightningdevkit/ldk-node) `=0.7.1` (LDK v0.2) with
a VSS backend (`vss-client-ng =0.4.1`, the release ldk-node itself uses).
Design: `docs/superpowers/specs/2026-10-30-lightning-design.md`.

Not a dependency of `wallet-core` or `wallet-wasm`; the web build never
compiles LDK.

## Android cross-build

ldk-node's C dependencies (`secp256k1-sys`, `ring`, bundled `libsqlite3-sys`)
need the NDK's clang; without it the build scripts fail with
`failed to find tool "aarch64-linux-android-clang"`. With NDK 30 (API 26 =
the app's `minSdk`):

```sh
NDK=$HOME/Android/Sdk/ndk/30.0.16248370/toolchains/llvm/prebuilt/linux-x86_64/bin
export CC_aarch64_linux_android=$NDK/aarch64-linux-android26-clang
export AR_aarch64_linux_android=$NDK/llvm-ar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK/aarch64-linux-android26-clang
cargo check -p fortis-lightning --target aarch64-linux-android
```

No feature flags needed.

## Regtest tests

`tests/common` starts Bitcoin Core regtest, a throwaway PostgreSQL cluster
and LDK's `vss-server`. Tests needing them are `#[ignore]`d:

```sh
FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
  cargo test -p fortis-lightning -- --ignored --nocapture --test-threads=1
```

- `build-vss-server.sh` clones `vss-server` `v0.1.0-alpha.0` into
  `target/tools/` and builds it with `--cfg noop_authorizer` (tests only).
- PostgreSQL: `initdb` / `pg_ctl` from `PATH`, or set `FORTIS_PG_BIN`.
- Chain source is bitcoind RPC (no Esplora on the dev host); a node built
  that way does not rescan history — see `tests/common/mod.rs`.
