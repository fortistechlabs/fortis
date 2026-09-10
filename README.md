# fortis

A non-custodial wallet for **Bitcoin XBT** and **Bitcoin BTC**.

The security-critical logic is one audited Rust crate. It compiles to WebAssembly
for the browser and to a native library (UniFFI) for iOS / Android — no
reimplementation of the crypto per platform.

## Layout

| Crate | |
|---|---|
| [`crates/wallet-core`](crates/wallet-core) | keys (BIP-32/39/84), addresses, coin selection, HTLC atomic-swap contracts, `SIGHASH_UNIFIED` (Knots PR #357), the client swap state machine, seed sealing. Pure — no I/O. |
| [`crates/wallet-wasm`](crates/wallet-wasm) | `wasm-bindgen` bindings for the web |
| [`crates/wallet-ffi`](crates/wallet-ffi) | UniFFI bindings for mobile — full parity with `wallet-wasm` (keys, `WalletView`, coin selection, `SIGHASH_UNIFIED` signing, seed sealing) |
| [`crates/fortis-node`](crates/fortis-node) | watch-only chain-gateway logic: a Bitcoin Core / Knots JSON-RPC client plus wallet ops (descriptors, UTXOs, fees, broadcast). No keys. |
| [`crates/wallet-cli`](crates/wallet-cli) | `fortis` — desktop shell: watch-only reporting plus send / sweep (coin selection, `SIGHASH_UNIFIED` signing, fee estimation, broadcast). |
| [`crates/fortisd`](crates/fortisd) | token-guarded local HTTP gateway over the node for the web wallet, or an Esplora CORS proxy (`--esplora-proxy`, no node). No keys. |
| [`crates/fortis-index`](crates/fortis-index) | address index over a Knots / BLAKE2b node, served as the Esplora REST subset the wallet already speaks — stateless, so one instance serves many wallets. No keys. |
| [`crates/fortis-edge`](crates/fortis-edge) | public front for the backends: per-install tokens, rate limiting, response caching, CORS, `/metrics` — in front of `fortis-index` (XBT) and an Esplora upstream (BTC). No keys. |
| [`web/`](web) | the browser wallet — a static PWA; keys stay in wasm, encrypted seed in IndexedDB. Reads the chain via a public Esplora explorer or your own node. |
| [`android/`](android) | native Android wallet — Jetpack Compose; keys via `wallet-ffi`, encrypted seed in DataStore. Same design language and backends as `web/`. |
| [`deploy/`](deploy) | running the hosted backend (`fortis-index` + `fortis-edge` + Caddy) — systemd units, a Docker Compose stack, and the Tailscale / public-DNS paths. |

## Tests

```
cargo test                                  # unit + BIP + SIGHASH_UNIFIED vectors
cargo test --features consensus-verify      # + a full swap round-trip, BTC legs
                                            #   validated by libbitcoinconsensus
```

Building the C parts of `secp256k1-sys` needs a C toolchain (MSVC Build Tools on
Windows); the wasm build additionally needs clang + `rustup target add
wasm32-unknown-unknown`.

## Running against a node

Two shells sit on the same [`fortis-node`](crates/fortis-node) gateway logic:

- **CLI** — [`crates/wallet-cli`](crates/wallet-cli) (`fortis`): `init` a wallet,
  `connect` to import watch-only descriptors, then `address` / `balance` / `utxos`,
  and `import-seed` + `send` to spend.
- **Web** — the [`web/`](web) PWA does keys + signing in wasm and reads the chain
  either from a public Esplora explorer (no node — `fortisd --esplora-proxy` works
  around the explorer's missing CORS) or from your own node via a [`fortisd`](crates/fortisd)
  gateway. See `web/README.md` for the build (needs `wasm-pack` + clang).

The full send path — coin selection, `SIGHASH_UNIFIED` signing, fee estimation,
broadcast — is exercised against a real regtest node with BLAKE2b active in
`crates/wallet-cli/tests/regtest_e2e.rs` (the CLI) and
`crates/fortis-edge/tests/regtest_e2e.rs` (the hosted stack:
`fortis-edge` → `fortis-index` → node, including token auth and the mempool
overlay). Both are opt-in via `FORTIS_BITCOIND`.

## What it is *not*

fortis is the wallet. The exchange that coordinates swaps between fortis users —
offer matching, the watchtower, the order book — is a separate service. fortis
holds keys and signs; it never depends on that service to move funds (the swap
escrow is an on-chain HTLC with a unilateral refund path).

"# fortis" 
