# fortis web wallet

A static, offline-capable (PWA) browser wallet. Keys are generated and every
signature is produced **inside WebAssembly** (`wallet-wasm`); the encrypted seed
lives in this browser's IndexedDB and never leaves the device. Feature parity
with the Android app: up to 10 named wallets behind one app lock (password,
plus optional WebAuthn quick-unlock on supporting browsers), approximate USD
value, and the UI in ~75 languages (Settings → Language).

At onboarding you create or restore your first wallet, which connects to the
hosted `fortis` edge automatically (mainnet only — no picker shown). Each
wallet has its own backend, so a BTC wallet and an XBT wallet can use
different sources at once; switch a wallet's backend any time from Settings
→ that wallet's card → Reconnect, or create a regtest wallet, which always
asks since the hosted edge doesn't serve it:

```
                         ┌─ Public explorer ──────────────────────────────┐
browser (this app)       │  Esplora REST (mempool.guide / mempool.space).  │
 ├ wallet-wasm: sign     │  No node. The explorer sees which addresses you │
 ├ seed in IndexedDB     │  query; it can never move funds.                │
 └ fetch ────────────────┤                                                │
                         └─ Your own node ────────────────────────────────┘
                            fortisd gateway  →  Bitcoin Knots / BLAKE2b RPC
                            Private. /v1/utxos /v1/feerate /v1/broadcast …
```

## Build the wasm module

The wasm build compiles `secp256k1`'s C to wasm, so it needs clang/LLVM:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack        # or: npm i -g wasm-pack
# Windows: winget install LLVM.LLVM   (set CC=clang / AR=llvm-ar if cc can't find them)
# macOS:   xcode-select --install      Linux: apt install clang

wasm-pack build crates/wallet-wasm --target web --out-dir ../../web/pkg
```

## Serve it

A service worker + wasm need a real origin:

```sh
python -m http.server 5173 --directory web      # or: npx serve web
```

Open `http://localhost:5173`, create or restore a wallet, then choose a backend.

### Backend A — fortis (hosted)

The default. A [`fortis-edge`](../crates/fortis-edge) URL — the app does
`POST {url}/register` for a per-install token on first use, stores it, and sends
it as `Authorization: Bearer` on every request (re-registering once on a 401).
Requests go to `{url}/xbt/…` or `{url}/btc/…`. Runs a local edge by default
(`http://127.0.0.1:8098`); point it at the deployed service once there is one.

### Backend B — public explorer (no node)

Point it at an Esplora API. **Caveat:** browsers enforce CORS, and mempool.guide /
mempool.space do **not** send `Access-Control-Allow-Origin` on their address
endpoints, so a direct connection is usually blocked. Two ways around it:

- The explorer operator adds `Access-Control-Allow-Origin: *` (and an OPTIONS
  handler) to every `/api/` route — then the app connects directly, zero local
  processes.
- Run the tiny forwarder (no node, starts instantly):

  ```sh
  cargo run -p fortisd -- --esplora-proxy https://mempool.guide/api
  ```

  and use `http://127.0.0.1:8088/esplora` as the explorer URL.

### Backend C — your own node

```sh
cargo run -p fortisd -- --datadir "C:\Bitcoin\Knots"
# regtest:  cargo run -p fortisd -- --network regtest --datadir <regtest datadir>
```

It prints a URL + token; paste both, "Connect" imports your account xpub as a
watch-only descriptor on the node.

### Serving both chains from one `fortisd`

One process can serve XBT from a Knots node and BTC from a public explorer, and —
when you also run a local Bitcoin Core — route BTC **broadcast + fee estimation**
through your own node while address/history reads still come from the explorer:

```sh
cargo run -p fortisd -- \
  --datadir "C:\Bitcoin\Knots" --network mainnet \
  --esplora-proxy https://mempool.space/api \
  --btc-rpc-url http://127.0.0.1:8532 --btc-datadir "C:\Bitcoin\Core"
```

- XBT wallet → gateway URL `http://<host>:8088`
- BTC wallet → explorer URL `http://<host>:8088/esplora`

`--btc-*` needs no address index (a pruned node is fine); it only does
`sendrawtransaction` and `estimatesmartfee`.

### From a phone

`fortisd --bind 0.0.0.0:8088`, serve `web/` on the same machine, browse from the
phone on the same Wi-Fi. The token is the only guard — keep it on a trusted
network.

## Scope

Create / restore (12 or 24 words, optional passphrase), up to 10 named wallets
behind one app lock, balance with approximate USD value, receive addresses,
send + sweep with a fee-rate control, transaction history, on either backend,
in ~75 languages, a QR code on the receive address. Not yet: RBF fee-bump, coin control, swaps.

### Multi-wallet & app lock

One in-memory app secret unseals every wallet. It's wrapped under an app
password (mandatory — always the fallback) and, on browsers that support the
WebAuthn PRF extension, optionally also under this device's platform
authenticator ("quick unlock" — enable/disable it any time from Settings →
Security). PRF support is inconsistent across browsers today; the password
always works regardless.

Settings → a wallet's card → **Also add on BTC/XBT** clones a wallet onto the
other chain from the *same* recovery phrase (same derivation, just a different
chain code) — no new phrase to write down.

### Translations

UI strings live in `src/locales/*.json`, converted from the Android app's
`res/values*/strings.xml` by `tools/i18n_extract.py` (re-run it after Android's
strings change — it's a manual step, not part of any build). A locale not
fully translated falls back to English per missing key. Fiat is USD-only
regardless of language.

Verified in headless Chrome: send → broadcast against a regtest node with BLAKE2b
active, and the explorer path (scan / balance / fees / history) against live
mempool.guide data via `--esplora-proxy`; multi-wallet creation/switching,
the language picker, and wallet management actions against the hosted edge.

## Files

| | |
|---|---|
| `index.html`, `style.css` | shell |
| `src/app.js` | controller + screens (no framework) |
| `src/wallet.js` | `wallet-wasm` wrapper — the only file that touches keys |
| `src/webauthn.js` | WebAuthn PRF registration/assertion for quick unlock |
| `src/i18n.js`, `src/locales/*.json` | translation loader + per-locale strings |
| `src/gateway.js` | `fortisd` gateway client (your own node) |
| `src/esplora.js` | Esplora/edge client — address-gap-limit scan, USD price, same interface as the gateway |
| `src/store.js` | IndexedDB (wallet list + app-lock block; migrates a v1 single-wallet record) |
| `src/ui.js` | DOM helpers, formatting |
| `sw.js`, `manifest.webmanifest` | PWA / offline shell |
| `tools/i18n_extract.py` | Android strings.xml → `src/locales/*.json` (manual, one-off) |
| `vendor/qrcode.js` | [qrcode-generator](https://github.com/kazuhikoarase/qrcode-generator) (MIT) — receive-address QR |
| `pkg/` | `wasm-pack` output (git-ignored) |
