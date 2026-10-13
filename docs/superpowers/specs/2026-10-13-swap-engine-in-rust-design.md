# The swap engine in the Rust core — design

**Date:** 2026-10-13 · **Status:** draft for the owner's approval

## Goal

Move the swap engine — today ~3,000 lines of Kotlin in the Android app
(`swap/SwapEngine.kt`, `swap/Recovery.kt`, `data/SwapStore.kt`, the swap parts
of `data/SwapApi.kt`) — into a shared Rust crate, and switch Android to it,
**with no change in behaviour**. One engine then serves Android (through
UniFFI) and, in a later project, the web wallet (through WASM): the
fund-handling logic is never written twice.

## Decisions (owner, 2026-10-13)

1. **Two projects.** This spec is project 1 only: engine → Rust, Android
   switched over. Web-wallet swaps (browser key storage, screens,
   notifications, watchtower cover for a closed tab) get their own design
   afterwards, on top of this engine.
2. **Release first.** The next release ships on the current Kotlin engine;
   the Rust engine follows in a later release, judged against an engine that
   has already been live.
3. **Approach A: an async Rust engine with platform ports.** The logic moves
   to Rust nearly line for line; everything it reaches outside itself goes
   through traits each platform implements. (Rejected: a sans-I/O state
   machine — a rewrite, hard to prove identical; Rust doing its own HTTP — a
   second network stack in the app.)

## Architecture

### The crate: `crates/swap-engine`

Depends on `wallet-core`. Holds:

- **`SwapRecord`** and `Step` — the record, exactly as stored today.
- **The engine** — `SwapEngine` with today's public surface: `step(id)`,
  `post_offer(client_id)`, `cancel_provisional(client_id)`, `note(id, msg)`,
  `due(now)` (active records plus those owing a release), and the trace /
  log-flush behaviour.
- **The store** — today's `SwapStore` logic (records, the per-seed swap-index
  counter, the edge token) over a raw blob port; read-modify-write sequences
  serialised inside the crate.
- **Recovery** — `scan_tags`, `recovered_record`, `recovery_wallets`, and the
  per-seed search itself.

Async without a runtime: no tokio, no threads, no `Send` bound on the ports
(so the same traits bind to JavaScript later). Per-swap and per-store locking
use a runtime-free async mutex. Time comes from the `Clock` port only.

### The ports

Each mirrors an interface the Kotlin engine already uses, method for method:

| Port | Methods (as today) | Android implements it with |
|---|---|---|
| `SwapApi` | quote, pricing_address, create_offer, offer, cancel_offer, take, take_offer, recover, book, match, post, messages, watch, log | the existing `EdgeSwapApi` (OkHttp) |
| `ChainWatch` | txout, spending_tx_hex, spend_confirmed, find_funding, feerates, broadcast | the existing `EdgeChainWatch` |
| `ChainCheck` | unspent, spending_tx_hex, spend_confirmed, broadcast, output | the existing `ExplorerChainCheck` |
| `Blob` | read() → string?, write(string) | the existing DataStore blob |
| `Funder` | fund(record, htlc) → txid? | the existing unlocked-wallet funder |
| `SwapKeys` | secrets(record) → (xbt, btc) | `SwapKeystore.unwrap` |
| `Clock` | now() | `System.currentTimeMillis()` |
| `Notifier` | event(record, event) | `SwapNotifications` |

Crypto is not a port: the engine uses the Rust swap session directly. It sits
behind an internal trait only so the engine tests keep their simple fakes.
Secrets fetched through `SwapKeys` are zeroised as soon as the session that
needed them is built, as today.

### `wallet-ffi`

Exposes one `SwapEngine` object with async methods and the ports as UniFFI
async foreign traits, plus the record, step and event types. The edge token
moves with the store; `EdgeSwapApi` reads it from the engine's store.

### Stays in Kotlin

Scheduling (`SwapRunner`, `SwapService`, `SwapWorker`, the boot receiver,
`StartGate`), notification text, the Keystore itself, the UI and its display
helpers (timeline, countdown, notes). They are platform or presentation code;
the browser gets its own in project 2, and any display logic the web needs
moves into Rust then.

### `wallet-wasm`

Unchanged in this project; the traits are written to bind to JavaScript
later.

## Compatibility

- **Stored swaps:** the Rust `SwapRecord` reads and writes today's JSON
  exactly — field names, marks (`accepted`, `release_signed`, …), defaults
  for absent fields, and the redacted `toString`. A phone upgraded mid-swap
  continues from the step the Kotlin engine saved. A test round-trips real
  records saved on both dev phones (secrets stripped) byte for byte.
- **Sessions and contracts:** `sessionJson` is already the Rust session's;
  v1 swaps stay v1.
- **Server:** unchanged — the same calls, order, client ids and 409 handling.
- **Keys:** still wrapped by the Android Keystore; the engine asks for a
  swap's two secrets only to sign.
- **Behaviour:** identical by design. A difference found while porting is
  either a bug in the old engine — fixed in both, and recorded — or kept as
  is. Nothing changes silently.

## Testing and switchover

1. **Proof first.** A minimal Rust async function calling an async Kotlin
   port, run on the Pixel: errors, cancellation, and calls from the
   background worker all behave. If UniFFI's async foreign traits fall short,
   stop and re-plan before porting.
2. **Tests before code.** The ~90 Kotlin engine tests (`SwapEngineTest`,
   `RecoveryTest`, the store and index tests) become Rust tests over the same
   fakes — server, chain, explorer, funder, crypto — with the same scenarios
   and assertions; each part of the engine is ported until its tests pass.
   Real crypto stays covered by the existing session, consensus and regtest
   tests.
3. **Both engines, for a while.** A debug-only switch picks the Kotlin or the
   Rust engine; the dev phones run Rust, release builds keep Kotlin.
4. **On the phones, on mainnet, with the Rust engine:** a full swap each way
   (each phone the initiator once); a cancelled match with the early release;
   wipe and "Find my swaps"; an upgrade from the Kotlin engine mid-swap; the
   app killed mid-step.
5. **Switch and delete.** Once those pass, the Rust engine is the only one;
   the Kotlin engine and its fakes are removed.
6. **One independent review** of the whole change.

## Out of scope

Web-wallet swaps; any change to swap behaviour, the server, or the contract;
moving display helpers into Rust.

## Risks

- **UniFFI async foreign traits** are fairly new — the proof (step 1) gates
  everything else.
- **Cancellation across the boundary** (the runner's step time limit, the
  service stopping): the proof covers it; the engine must leave a record
  consistent if a step is cancelled mid-way, as the Kotlin one does.
- **A silent behaviour change** — guarded by porting the tests first and by
  the mid-swap upgrade test.
