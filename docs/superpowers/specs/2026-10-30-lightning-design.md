# Lightning (BTC) — design

**Status:** awaiting review · 2026-10-30
**Scope of this spec:** phase 1 (Lightning core), Android only. Phases 2 and 3
are described in the overview so phase 1 doesn't paint them into a corner;
each gets its own spec and plan later.

## Why

Users should be able to send and receive Lightning payments as easily as in
the popular Lightning wallets — scan a QR in a shop, pay an invoice, show a
QR and get paid — without giving up what fortis is: non-custodial, no
account, no Google Play Services, keys from the one recovery phrase.

## Overview (all phases)

The end state is **one BTC balance** (Phoenix-style): the BTC wallet holds
on-chain coins and one Lightning channel to the fortis LSP, and the app
picks on-chain or Lightning per payment.

| phase | what ships |
|---|---|
| **1 — Lightning core** (this spec) | On-phone LDK node, `fortis-lsp` (LSPS2 JIT channels), encrypted VSS backup, send/receive over Lightning, scanning/parsing, one total with an on-chain / Lightning split, history, restore, two-device guard. Moving funds on-chain ⇄ Lightning by channel open / cooperative close. Behind *Settings → Lightning (beta)*. |
| 2 — one balance via splicing | Splice-in / splice-out; a "Move to Lightning?" card (always asks, never automatic); paying an on-chain address from Lightning by splice-out; paying an invoice from on-chain by splice-in (asks first). Falls back to close-and-reopen when splicing fails. |
| 3 — background receive | LSPS5 webhooks pointed at a UnifiedPush endpoint (e.g. ntfy); a foreground job claims held payments with the app closed; a separate receive-only Android Keystore key for the node seed (sends still need biometrics). Without a distributor the app receives only while open. |

Splicing is phase 2 because ldk-node 0.7 marks it experimental (LDK v0.2,
Dec 2025; BOLT splicing spec merged Mar 2026). Phase 1 works without it.

## Decisions

| question | decision |
|---|---|
| Lightning implementation | **ldk-node** (LDK v0.2 line), wrapped in a new Rust crate `fortis-lightning`, exposed through `wallet-ffi` (UniFFI). Not ldk-node's own Android AAR; not raw rust-lightning. |
| Liquidity | **fortis runs its own LSP** (`fortis-lsp`, ldk-node in LSPS2-service mode). One hardcoded peer; discovered via the edge. |
| Where the LSP runs | **Home host for now**, next to the edge, chain from Bitcoin Core 31.2 over RPC. Data dir + config kept portable for a later move to a VPS. |
| Starting liquidity | **0.05 BTC** in the LSP's on-chain wallet. |
| LSP fee | **Fee on channel open**: `open_fee_ppm` (start 10 000 = 1%) with a floor `open_fee_min_sat` (start ≈ 2× next-block fee for an open). Shown to the user before they accept. Normal routing fees otherwise. |
| Balance display | **One total** in the BTC hero; a line under it splits it: *On-chain · Lightning*. |
| Platforms | **Android only.** Web wallet unchanged (`wallet-wasm` does not build the Lightning feature). |
| Wallet kinds | **Software BTC wallets on mainnet only.** No Lightning for watch-only, BitBox, or XBT wallets. |
| Node keys | **Derived from the BTC wallet's BIP-39 seed by BIP-85** (HEX application 128169, 64 bytes, index 0 — path `m/83696968'/128169'/64'/0'`; the 64 bytes are ldk-node's entropy seed directly, with no further expansion (decided 2026-10-30)). Nothing new to write down; a separate key tree so ldk-node's own on-chain wallet never collides with the BIP-84 addresses the wallet already uses. |
| Backup | **VSS** (LDK Versioned Storage Service) hosted by fortis behind the edge; client-side encrypted with seed-derived keys. Lightning **refuses to pay** while VSS is unreachable. |
| Multiple devices | **One active Lightning device per seed**, enforced by VSS versioning; explicit "Use Lightning here instead" takeover. |
| Scanning | Send accepts BOLT11, BOLT12 offers, LNURL-pay, Lightning addresses, BIP21 with `lightning=`, and on-chain addresses; `lightning:` / `bitcoin:` links open Send. |
| Receiving own Lightning address (`name@fortis…`) | **Not in phase 1.** |
| Commits | The owner commits. |

## Components

### `crates/fortis-lightning` (new)

The only code that touches LDK. Not pure (it does I/O), so it lives beside
`wallet-core`, not in it. Uses `wallet-core` for seed handling.

- **`Node`** — wraps `ldk_node::Node`. Built from: BIP-39 seed bytes (from
  which it derives the BIP-85 node seed itself — Kotlin never sees node
  keys), data directory, Esplora URL (edge BTC, or the user's own server
  from `OwnServer`), LSP info (node id, host:port, LSPS2 token), VSS URL +
  auth header. Methods: `start`, `stop`, `balances() -> Balances
  { lightning_sat, onchain_transit_sat, inbound_sat }`,
  `receive(amount_msat: Option, description) -> ReceiveQuote`,
  `pay(PaymentTarget, amount_msat: Option) -> PaymentId`,
  `open_channel(amount_sat)` (funded from the node's transit wallet),
  `close_channel()`, `transit_address()`, `sweep_transit(to_address)`,
  `payments() -> Vec<Payment>`, `events()` (polled stream of received /
  sent / failed / channel-ready / channel-closed).
- **`ReceiveQuote`** — `{ invoice, amount_msat, fee_msat, needs_channel }`.
  `needs_channel` + `fee_msat` come from LSPS2 `get_info` / `buy` when
  inbound capacity is short; otherwise fee is 0 and the invoice is a plain
  one. A no-amount receive returns a BOLT12 offer (no JIT channel possible;
  the UI says an amount is needed for a first receive).
- **`parse(text) -> PaymentTarget`** — pure, unit-tested:
  `Bolt11 { invoice, amount_msat?, description, expiry, network }`,
  `Bolt12Offer`, `LnurlPay { callback, min, max, description }` (resolved
  by a separate `resolve_lnurl` that does the HTTP fetch),
  `LightningAddress` (→ LNURL-pay via `/.well-known/lnurlp/`),
  `OnChain { address, amount? }`, `Unified { onchain, lightning }` (BIP21
  with `lightning=`). Tolerates `lightning:` prefix, upper case, whitespace.
  Rejects wrong-network invoices and expired invoices with distinct errors.
- **Errors** — one `LnError` enum mapped to user-facing messages in Kotlin:
  `BackupUnavailable`, `ActiveOnAnotherDevice`, `LspUnreachable`,
  `InsufficientLightning`, `NoRoute`, `InvoiceExpired`, `WrongNetwork`,
  `AmountOutOfRange { min, max }`, `Other(String)`.

ldk-node's built-in on-chain wallet is used **only as transit**: funds for a
channel open pass through it, and funds from a close land in it and are swept
to the BTC wallet's next receive address. Its balance is shown as part of
*Lightning* while in transit ("Moving…").

### `crates/wallet-ffi`

A `lightning` module behind a cargo feature `lightning` (on for the Android
build) that re-exports the `fortis-lightning` API as UniFFI objects/records.
No LDK types cross the boundary. The wasm build and `wallet-wasm` are
untouched.

### `crates/fortis-lsp` (new binary)

ldk-node in LSPS2-service mode. Config file (`/etc/fortis/lsp.toml`):
listen `0.0.0.0:9735`, bitcoind RPC, data dir, `open_fee_ppm`,
`open_fee_min_sat`, `min_payment_size_msat` (start 10 000 sat),
`max_payment_size_msat`, `channel_size_factor` (2×), `channel_min_sat`
(100 000), `channel_max_sat` (start 500 000 — canary cap), LSPS2 token.
Exposes Prometheus metrics on localhost (on-chain balance, channel count,
total inbound/outbound, failed forwards, pending HTLCs) and logs a warning
when on-chain liquidity drops below `low_liquidity_sat`.

Zero-conf JIT channels: the client trusts the LSP not to double-spend the
funding transaction until it confirms (LSPS2 "client trusts LSP").

### Edge (`fortis-edge`)

Two new route groups, both behind the existing per-install token and rate
limiting:

- `GET /ln/info` → `{ node_id, host, port, lsps2_token, fee: { ppm, min_sat }, min_payment_sat, max_payment_sat }`, from edge config. Lets the LSP's address or key change without an app release.
- `/ln/vss/*` → reverse proxy to the local `vss-server`. The edge adds no
  state; VSS's own versioning does the conflict detection.

### `vss-server` (new service on the home host)

LDK's `vss-server` with PostgreSQL, listening on localhost only. Storage
keys are a seed-derived store id; values are encrypted on the phone. Backed
up nightly off-host with the LSP's data dir.

### Android

New package `com.fortis.wallet.lightning`:

- **`LightningSession`** — starts the node when a software BTC wallet with
  Lightning enabled is unlocked, stops it on relock / app background (phase
  3 adds the background path). Exposes `StateFlow<LnState>`
  (`Off`, `Starting`, `Ready(balances)`, `Error(LnError)`) and an events
  flow consumed by the UI for "Received ⚡ 12,000 sat" toasts.
- Node data dir under app-private storage (`filesDir/ln/<wallet-id>`), not
  backed up by Android backup (already disabled app-wide).

Changes to existing screens:

- **Settings → Lightning (beta)** — on/off per BTC wallet (default off),
  status line (LSP connected, backup OK), "Close channel and move to
  on-chain".
- **BTC hero** — total = on-chain + Lightning; the split line under it.
- **Receive** — amount field. With an amount: one BIP21 QR
  (`bitcoin:ADDR?amount=…&lightning=lnbc…`); if a channel is needed, the fee
  is shown above the QR ("Receiving 50,000 sat · fee 2,100 · you get
  47,900"). Below the LSP's `min_payment_size` the QR is on-chain only, with
  a note. Without an amount: the on-chain address QR plus a "Lightning needs
  an amount" hint (until a channel exists — then a BOLT12 offer joins the
  BIP21).
- **Send** — the scanner and paste box go through `parse`. Confirm screen
  shows amount, description, expiry, estimated fee; LNURL ranges and
  no-amount invoices ask for an amount first; biometric prompt as for
  on-chain sends. Unified QRs pay over Lightning when the Lightning balance
  covers it, otherwise on-chain. A Lightning target with insufficient
  Lightning balance shows "Not enough on Lightning — Move to Lightning
  first" linking to the move flow.
- **Move to Lightning** (phase 1 form) — pick an amount ≥ `channel_min`;
  the BTC wallet sends it to the node's transit address (existing send path,
  local pending send applies), the node opens a channel to the LSP once
  it has 1 confirmation. While a channel exists, adding more means a second
  channel in phase 1 (splice-in replaces this in phase 2).
- **History** — Lightning payments merge into the BTC list with a ⚡ marker,
  fee and description; failed sends shown as failed.
- **Links** — intent filters for `lightning:` and `bitcoin:` open Send
  prefilled.
- **Delete wallet** — if Lightning holds funds, warn first ("You have
  85,000 sat on Lightning; move them out or make sure you have your
  phrase"). VSS data is left for a restore.
- **Takeover** — `ActiveOnAnotherDevice` shows "Lightning is active on
  another device" with "Use Lightning here instead", which claims the VSS
  store (the other device's next write fails and it stops its node).

## Data flows

**First receive (no channel):** app → `receive(50 000 sat)` → node asks LSP
(LSPS2 `get_info` → shows fee → `buy`) → JIT invoice with an LSP route hint
→ payer pays → LSP opens a zero-conf channel sized `max(2×amount,
channel_min)` capped at `channel_max`, forwards amount − fee → node claims →
event → toast, history, balance.

**Pay an invoice:** scan → `parse` → confirm → biometrics → `pay` → LDK
routes via the LSP → `PaymentSuccessful` / `PaymentFailed` event → history.

**Restore:** new phone, enter words → enabling Lightning derives the node
seed and VSS keys → ldk-node loads state from VSS → channel reconnects with
the LSP → balance appears.

**VSS down:** node fails its write → `BackupUnavailable` → "Lightning backup
unavailable, try again shortly"; on-chain unaffected.

**Lost VSS data:** the node has no channel state, so it can't resume.
Recovery needs the LSP to force-close (the restored node reconnecting is
not enough on its own; the owner closes channels of inactive node ids on
request, runbook). LDK 0.2 peer storage does **not** help here: on
retrieval it only checks for stale monitors of channels the node still
knows. When the LSP force-closes, the user's balance lands on one of 2000
scripts derived from the seed alone (LDK 0.2 `v2_remote_key_derivation`,
on in ldk-node 0.7.1), spendable after 1 block. **ldk-node 0.7.1 does not
scan for or sweep that output by itself** (Task 1, finding a), so
`fortis-lightning` carries its own seed-only sweeper: derive the static
payment keys, find outputs paying them, sign a spend to the node's
on-chain wallet. Funds in HTLCs pending at the time of loss are not
recovered this way (only the settled balance is).

## Findings (Task 1)

Spike tests: `crates/fortis-lightning/tests/spike.rs` (regtest, Bitcoin Core
31.2, ldk-node 0.7.1, vss-server v0.1.0-alpha.0 on PostgreSQL 18).

**(a) Seed-only recovery of a force-closed channel — works, but not by
ldk-node.** A opens a 200 000 sat channel to B, loses all local state, is
rebuilt from the same seed with an empty data dir, and B force-closes.
The restored ldk-node never shows the funds (on-chain 0, no pending
closure balance after 30 blocks). The commitment's to_remote output
(199 056 sat, anchor P2WSH with 1-block CSV) *does* pay one of
`KeysManager::possible_v2_counterparty_closed_balance_spks` derived from
the seed alone; a ~50-line spend built from the seed-derived key (master
`/8'/idx'`, idx < 1000) confirms and lands in the restored node's on-chain
wallet. So: recovery is **not owner-assisted for the funds**, but it is
for the trigger (the LSP must force-close), and it needs a fortis sweeper —
the plan's restore work must add "scan the 2000 static scripts and sweep"
(an Esplora script lookup per script, or a batched scan) rather than rely
on ldk-node. Caveat: the test chain source is bitcoind RPC, which does not
rescan history; the production Esplora source does a full scan of the
BDK wallet, but the static scripts are outside that wallet either way.

**(b) VSS conflict — ldk-node surfaces nothing; takeover needs its own
`device-lock` key.** Two nodes from one seed on one VSS store: the second
node builds and starts `Ok`, loads the first node's state (sees its
balance), and both keep writing (`new_address`, on-chain send, sync) with
`Ok` results — they even hand out the **same** new address. vss-server
reports zero conflicts: ldk-node 0.7.1's `VssStore` writes every key
unconditionally (`version = -1`, no `global_version`), so VSS versioning is
never engaged and two devices silently overwrite each other's channel
state — a fund-loss risk, not just an error to show. A `device-lock` key
written with `vss-client` using conditional versions behaves as needed:
insert-if-absent (`version 0`) by a second device → `ConflictError`;
takeover at the version read → `Ok`; the old device's refresh at its stale
version → `ConflictError`. **Task 10 follows this:** the node must hold
the lock before `start` and re-check it (conditional write) before every
payment/channel operation and periodically, stopping its node on
`ConflictError`; `ActiveOnAnotherDevice` comes from the lock, never from
ldk-node. The window between the other device's takeover and this
device's next check is not closed by VSS — keep the check before every
state-changing call.

**Other:** `cargo check --target aarch64-linux-android` passes with the
NDK clang env (crate README); no feature flags. vss-server (v0.1.0-alpha.0)
refuses to start without an authorizer unless built with
`--cfg noop_authorizer` (which commingles every user under one token, so
isolation rests on the store id alone). Production needs either the
signature authorizer (whose header ldk-node 0.7.1's
`build_with_vss_store_and_fixed_headers` does not produce) or JWT —
Task 9 must pick one (e.g. the edge minting a per-install JWT and passing
it as a fixed header). Tests use a `noop_authorizer` build.

## Operations (runbook in `deploy/lightning.md`)

- **Reachability prerequisite:** port 9735/TCP must be reachable from the
  internet on the home host (public IPv4 + port forward, or public IPv6).
  cloudflared does not carry raw Lightning TCP. If neither works, this is the
  trigger to move `fortis-lsp` to a VPS early.
- Fund the LSP with 0.05 BTC; open 1–2 outbound channels to well-connected
  nodes (e.g. ACINQ) so users can pay the wider network.
- systemd units `fortis-lsp.service` and `fortis-vss.service`;
  `install.sh` extended.
- Nightly off-host backup of the LSP data dir and VSS database.
- Close idle low-balance channels after 6–12 months (manual in phase 1).
- Moving to a VPS: stop, copy data dir + config, start; update `/ln/info`.

## Testing

- **Unit (`fortis-lightning`):** `parse` (BOLT11/12, LNURL, Lightning
  address, BIP21 `lightning=`, upper case, prefixes, garbage, wrong network,
  expired); BIP-85 node-seed derivation against fixed vectors; fee-quote
  maths; error mapping.
- **Regtest integration** (same real-node harness as the swap tests,
  `FORTIS_BTC_BITCOIND`): bitcoind + `fortis-lsp` + `vss-server` + two
  client nodes in-process. Cases: first receive opens a JIT channel and
  charges the configured fee; pay out; pay client→client via the LSP;
  move-to-Lightning channel open and cooperative close + transit sweep;
  restore from seed via VSS; second device's write rejected and takeover;
  VSS down → `BackupUnavailable`; amounts outside LSP min/max rejected.
- **Android:** a Kotlin test exercising the UniFFI binding (parse + node
  start/stop against regtest); manual device checklist on the Pixel with
  the `.dev` / `.dev2` apps — scan-and-pay a real invoice, receive with a
  JIT channel, restore on `.dev2`, two-device takeover, link intents.
- **Mainnet canary:** LSP on mainnet with `channel_max_sat` 500 000; test
  between `.dev`, `.dev2` and an outside wallet (Phoenix) before release.

## Rollout

- Ships off by default behind *Settings → Lightning (beta)*.
- Site: a Lightning page; privacy policy gains a Lightning section — what
  the LSP sees (amounts and timing of payments routed through it, the
  node id; not who the user is) and what VSS stores (encrypted blobs). The
  owner reviews both drafts.
- New strings go through the usual translation flow.

## Out of scope (phase 1)

Splicing and the "Move to Lightning?" prompt (phase 2); background receive,
UnifiedPush and LSPS5 (phase 3); web wallet Lightning; a fortis Lightning
address for receiving; Lightning for watch-only/hardware/XBT wallets;
multiple LSPs or user-chosen peers; Tor.
