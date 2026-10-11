# Peer-to-peer BTC ⇄ XBT swaps — design

**Status:** approved · 2026-10-10
**Scope:** phase 1 — Android app, `wallet-core`, `wallet-ffi`, `fortis-edge`.
Hardware-wallet participation is phase 2 (see the end).

## Why

Users holding both coins want to move between them without an exchange
account. fortis already has the on-chain half — an HTLC atomic-swap state
machine in `wallet-core/src/swap.rs` — but nothing drives it: no matching, no
price, no app UI.

## Decisions

| question | decision |
|---|---|
| Counterparty | **Other fortis users only.** No operator liquidity, no custody. Every swap is an HTLC atomic swap between two users. |
| Modes | **Quick swap** (matched with any open offer) and **swap with a person** (private offer + share code). Both pay out into the user's own wallet on the other chain — the user picks *from* and *to* wallets, never pastes an address. |
| Price | **NeoxEX `BTCB2_BTC` at match time.** Offers carry an amount and a max slippage, not a price. Each phone re-checks NeoxEX itself before accepting. |
| Commission | **0.5% per side**, paid on the chain each user sends, to the service-fee address the edge already advertises at `GET /pricing` (one `bc1…` address — valid on both chains, since XBT shares Bitcoin's HRP and BIP-84 layout). |
| Liveness | **Android foreground service** drives the swap while active; **a watchtower** on the edge broadcasts pre-signed refunds/redeems if a phone is offline. |
| Where the service lives | New `swap/` modules in **`fortis-edge`**. |
| Hardware wallets | Phase 2. |
| Commits | None made by the implementer — the maintainer commits. |

## Non-goals

Operator-provided liquidity or market making. Executing trades on NeoxEX.
User-set prices / a real order book. Swaps in the web wallet (`wallet-wasm`) and
in self-hosted `fortisd`. Watch-only and hardware wallets as swap participants.
Multi-instance edge deployments for swap state.

## Gaps in the existing code this closes

1. **Locktime disagreement.** `swap::timelocks(role, send, recv)` takes both legs
   from the *sender's own* `ChainParams`, so the XBT holder expects a 72 h BTC
   leg while the BTC holder builds a 24 h one — the contracts would never match.
   Replaced by `swap_timelocks(btc, xbt)` (below), used by both the edge and the
   phones.
2. **No mobile bindings.** `wallet-ffi` exposes only `swap_pubkey` / `sign_swap`.
3. **The edge's fee check would block refunds.** `POST /<chain>/tx` rejects any
   transaction that doesn't pay `floor_sat` (400) to the fee address
   (`fortis-edge/src/pricing.rs`); an HTLC redeem or refund can't.

## The swap, end to end

Roles are as `swap.rs` already fixes them: **the XBT sender is the initiator**
(knows the preimage, funds first, redeems on Bitcoin); the BTC sender is the
participant.

1. **Offer.** Alice: *from XBT · Savings, 1.0 XBT → to BTC · Main*, slippage 1%
   (choices 0.5 / 1 / 2%), expiry 24 h. App shows the NeoxEX rate and "you
   receive ≈ … after 0.5%", then `POST /swap/offers`. A private offer also returns
   a share code.
2. **Match.** Bob posts BTC → XBT. The edge pairs opposite offers FIFO, partially
   filling the larger one (remainder stays open if ≥ the minimum), takes one
   NeoxEX price and fixes the **terms**: amounts, price, fee bps (50), locktimes
   (from `swap_timelocks`, measured from match time), required confirmations
   (XBT 6, BTC 2), deadlines.
3. **Accept.** Each phone fetches NeoxEX itself and runs `validate_terms`. Both
   must accept within **2 minutes** or the match dissolves and both offers
   reopen.
4. **Keys.** Over the relay: each side's swap pubkeys for both chains
   (`swap_pubkey(chain, account, swap_index)`, seed-derived), and Alice's
   hashlock = SHA-256 of `swap_preimage(account, swap_index)`.
5. **Alice funds** the XBT HTLC within **30 minutes** of acceptance: HTLC output
   + 0.5% to the fee address + change. Posts the txid; hands the watchtower her
   pre-signed XBT refund.
6. **Bob funds** the BTC HTLC after Alice's funding has 6 confirmations and
   passes `verify_their_contract`, within **2 hours** of that — and never with
   less than **12 hours** left on the BTC leg. Hands the watchtower his
   pre-signed BTC refund and his pre-signed (preimage-less) XBT redeem.
7. **Alice redeems** the BTC after 2 confirmations, into her BTC wallet —
   revealing the preimage. Once Bob's funding is confirmed she also hands the
   watchtower that redeem, fully signed.
8. **Bob redeems** the XBT — his phone, or the watchtower — using the preimage
   read from Alice's redeem witness (`preimage_from_witness`).

**Failure paths.** Nobody funds before a deadline → match cancelled, nothing
moved. Alice funds, Bob doesn't → Alice refunds after the XBT leg (144 h). Bob
funds, Alice doesn't redeem → Bob refunds after the BTC leg (24 h); Alice after
144 h. Refunds are broadcast by the phone or the watchtower, whichever is first.

**Locktimes.** XBT leg = `xbt.htlc_initiator_locktime_secs` (144 h), BTC leg =
`btc.htlc_participant_locktime_secs` (24 h), both from match time.

**Minimum swap.** The 0.5% on each leg must clear the edge's 400 sat fee floor,
so each leg ≥ 80 000 sat. At ~0.0112 BTC/XBT the BTC leg binds: **BTC leg ≥
0.0008 BTC (≈ 0.071 XBT)**. The edge computes and advertises the minimum per
quote.

**Anti-griefing.** An install token that misses a funding deadline 3 times in
24 h can't post offers for 24 h.

## `fortis-edge`

The edge is otherwise stateless (HMAC install tokens, no shared store). Swap
state lives in **one RocksDB on one instance**; running several edges would need
`/swap/*` pinned to one (out of scope).

### Modules — `crates/fortis-edge/src/swap/`

| file | job |
|---|---|
| `neoxex.rs` | Fetch `GET https://neoxa.exchange/api/exchange/orderbook/BTCB2_BTC` (10 s cache) and `/ticker/BTCB2_BTC`. Price = **depth-weighted mid** over the top ~0.05 BTC of each side. Refuse to quote if spread > 2%, data > 60 s old, the pair is paused, or the weighted mid differs from last trade by > 2%. |
| `store.rs` | RocksDB: offers, matches, relay messages, watch jobs. Purge settled/expired after 7 days. |
| `matcher.rs` | FIFO matching, partial fills, terms via `wallet_core::swap_timelocks`. |
| `relay.rs` | Per-match mailbox. |
| `watchtower.rs` | Background thread, every 60 s. |
| `mod.rs` | Routes, auth, config. |

`main.rs` gains route wiring only. `wallet-core` moves from a dev-dependency to a
normal dependency (it is pure — no I/O).

### API

All routes need the existing install token. `POST /swap/offers` and
`/swap/take` return a random **owner secret**; every later call about that offer
or match must carry it (`X-Swap-Secret`).

| route | |
|---|---|
| `GET /swap/quote` | `{price, fee_bps, min_btc_sat, max_btc_sat, open: {xbt_to_btc, btc_to_xbt}}`; `503` if NeoxEX can't be quoted |
| `POST /swap/offers` | `{direction, amount_sat, max_slippage_bps, expires_in, private}` → `{offer_id, owner_secret, share_code?}` |
| `GET /swap/offers/{id}` | status, and `match_id` once matched |
| `DELETE /swap/offers/{id}` | cancel — only while unmatched |
| `POST /swap/take/{share_code}` | take a private offer → `{match_id, owner_secret}` |
| `GET /swap/matches/{id}` | terms + deadlines + status |
| `POST /swap/matches/{id}/messages` | `accept` / `decline` / `keys` / `hashlock` / `funded` |
| `GET /swap/matches/{id}/messages?after=<seq>` | long-poll (≤ 25 s) |
| `POST /swap/watch` | `{match_id, chain, kind: refund\|redeem, htlc_outpoint, not_before, variants: [tx_hex…]}`; for a preimage-less redeem, `{sigs: [sig_hex…], pubkey, witness_script}` instead of finished txs, `sigs[i]` signing `variants[i]` (400 if the lengths differ) |

### Watchtower

For each job, query the edge's existing upstream for the chain (`fortis-index`
for XBT, Esplora + fallbacks for BTC): `GET /tx/{txid}/outspend/{vout}`.

- **Refund:** past `not_before` and the HTLC still unspent → broadcast.
- **Waiting redeem:** when the counterparty's HTLC on the other chain is spent
  via the hashlock path, extract the preimage, finish the witness with
  `HtlcContract::finalize_redeem`, broadcast.
- **Fee choice:** pick the lowest variant whose feerate meets the upstream's
  current estimate; else the highest.

Its broadcasts go straight to the upstream and skip the fee check.

### Fee-check exemption

`POST /<chain>/tx` additionally accepts a transaction **every input of which
spends a swap HTLC outpoint the store knows**. Anything else is unchanged.

### Flags

`--swap` (off by default; off → `/swap/*` 404s, as `/pricing` does without a
fee), `--swap-db <dir>`, `--swap-fee-bps 50`, `--swap-max-sat` (BTC-leg cap; launch
≈ 1 000 000), `--neoxex-url` (default `https://neoxa.exchange`).

## `wallet-core`

1. `swap_timelocks(btc: &ChainParams, xbt: &ChainParams) -> SwapTimelocks
   { xbt_leg_secs, btc_leg_secs }` replaces `timelocks()`.
2. `SwapTerms` (serde): amounts, price, fee bps, locktimes, confirmations,
   deadlines, both sides' fee address.
   `validate_terms(&terms, neoxex_price, max_slippage_bps, role, now)` checks:
   amounts within slippage of our own price, fee = 50 bps, locktimes equal
   `swap_timelocks` ± 10 min, confirmations as expected, amount ≥ minimum.
3. `swap_preimage(account, swap_index)` — HMAC-SHA256 keyed by that swap's
   private key, so a seed restore can still redeem.
4. `presign_redeem(…) -> PresignedRedeem { tx, sig, pubkey, witness_script }` —
   signed without the preimage (the sighash commits to the transaction, not the
   witness). Uses the existing `build_redeem` + `sign_swap`.
5. Refunds and redeems are produced at **two feerates** (normal, high).
6. `SwapMachine` gains serde so a phone can persist and resume it.
7. **Funding reuses `plan_send`:** `PayTo(htlc_p2wsh_address, amount)` with
   `ServiceFee { bps: 50, … }` — coin selection, fee output, replay protection
   and `SIGHASH_UNIFIED` unchanged.

## `wallet-ffi`

New `SwapSession` object:

```
new(terms_json, role, account, swap_index) / from_json(json) / to_json()
our_htlc() -> {address, amount_sat}
hashlock(), our_pubkeys()
verify_their_funding(tx_hex) -> outpoint
signed_refund(outpoint, feerates) -> [tx_hex]
signed_redeem(outpoint, preimage, feerates) -> [tx_hex]
presigned_redeem(outpoint, feerates) -> PresignedRedeem
preimage_from_tx(tx_hex) -> preimage?
on_event(event) -> state
```

`wallet-wasm` does not get these in phase 1; `crates/wallet-ffi` / `wallet-wasm`
READMEs note the parity gap.

## Android

### Keys while the app is locked

The seed is sealed behind password/biometrics, so a background service can't
sign with it. When the user confirms a swap (app unlocked), the phone derives
**that swap's key** and stores it wrapped by an Android Keystore key that needs
no user authentication; it is deleted when the swap ends. That key can spend
only that swap's two HTLCs, and every transaction it signs pays the user's own
wallets. Everything signable early (refunds; the participant's redeem) is signed
at funding time and stored locally as well as with the watchtower.

### Files

| file | |
|---|---|
| `data/SwapApi.kt` | OkHttp client for `/swap/*` |
| `data/NeoxEx.kt` | the phone's own NeoxEX price, same depth-weighted mid rule as the edge |
| `data/SwapStore.kt` | one record per swap: `SwapSession` JSON, owner secret, wrapped key, pre-signed txs |
| `swap/SwapEngine.kt` | one idempotent step: load → poll relay + chain → act → save |
| `swap/SwapService.kt` | foreground service, `dataSync` type; runs the engine every 20 s while any swap is active |
| `swap/SwapWorker.kt` | WorkManager, every 15 min, and on boot — fallback when the service isn't running |
| `ui/screens/SwapScreens.kt` | the UI (`Screens.kt` is already ~1 200 lines) |

### UI

- **Swap** button on Home beside Send / Receive — shown only when the edge
  answers `GET /swap/quote`.
- **New swap:** from-wallet + amount, to-wallet (other chain, seed wallets
  only), live rate, 0.5% commission, you receive, slippage selector. **Find a
  match** or **Swap with a person** (shows share code + QR).
- **Take a code:** type or scan; terms shown before accepting.
- **Swap detail:** timeline (waiting for match → matched → you funded → they
  funded n/6 → redeemed → done), deadline countdown, Cancel while unmatched,
  amounts, txids with explorer links.
- **Swap list:** active first, then history.

### Service and notifications

Persistent notification "Swap in progress · step n of 5" while active.
Notifications for matched, counterparty funded, complete, failed/refunding.
Android 15 caps `dataSync` at ~6 h/day — a normal swap is ~1–2 h; beyond that
the worker and watchtower carry it. New permissions: `FOREGROUND_SERVICE`,
`FOREGROUND_SERVICE_DATA_SYNC`, `POST_NOTIFICATIONS` (asked on first swap). New
dependency: WorkManager.

### Strings

English only; other locales fall back until translated.

## Testing

- **wallet-core unit:** `validate_terms` rejects tampered amounts, fee,
  locktimes, confirmations, and out-of-slippage prices; both roles derive equal
  locktimes; `presign_redeem` + late `finalize_redeem` yields a valid spend;
  `swap_preimage` is deterministic; `SwapMachine` serde round-trip.
- **wallet-core `consensus-verify`:** extend the swap round-trip so funding
  carries the 0.5% output and the participant's redeem is finished after the
  fact.
- **fortis-edge unit:** matcher (FIFO, partial fill, remainder below minimum,
  expiry, 2-minute accept timeout, private offers), NeoxEX price rules against
  fixture JSON, owner-secret auth, fee-check exemption, anti-griefing.
- **`crates/fortis-edge/tests/swap_e2e.rs`** (opt-in via `FORTIS_BITCOIND`):
  BTC regtest + BLAKE2b regtest + edge `--swap` + a stub NeoxEX + two
  `wallet-core` clients over HTTP:
  1. happy path; 0.5% outputs on both chains; amounts match terms
  2. participant offline after funding → watchtower redeems
  3. participant never funds → initiator refunded by the watchtower (`setmocktime`)
  4. skewed stub price → both clients decline
  5. redeem/refund pass `POST /<chain>/tx`; an unrelated fee-less tx still fails
- **Android unit:** `SwapEngine` against fake API/chain — each step, missed
  deadlines, resume after process death, slippage rejection.
- **Manual:** two devices/emulators on a regtest edge, one swap each way; kill
  one app mid-swap and confirm resume + watchtower.

## Rollout

Ship the app first; enable on the server with `--swap` — no app release needed.
Start with `--swap-max-sat` ≈ 0.01 BTC and raise it after real swaps settle
cleanly. Docs: README crate table, `fortis-edge/README.md` (API + flags),
`android/README.md`, CHANGELOG.

**Non-technical:** charging a commission for facilitating swaps may be a
regulated activity in some jurisdictions even when non-custodial — check
before launch.

## Revisions during planning (2026-10-10, approved by the owner)

1. **XBT chain reads.** `fortis-index` indexes only P2WPKH and has no per-transaction
   routes, so it can't answer `/tx/{txid}/outspend/{vout}`. It gains one route:
   `GET /txout/{txid}/{vout}` → the node's `gettxout` (mempool included):
   `{value_sat, scriptpubkey_hex, confirmations}`, or `404` when spent or unknown.
   On XBT, phones and the watchtower use only this: to verify funding (script +
   value + confirmations), and to decide whether a refund or redeem is still needed.
   The preimage is always revealed on **BTC** (Alice redeems there), where Esplora's
   `/tx/{txid}/outspend/{vout}` and `/tx/{txid}` give the spending witness.
2. **Funding while the phone is locked.** Funding spends wallet coins and needs the
   sealed seed. When it is a user's turn to fund and the app is locked, the phone
   posts a high-priority notification ("Your turn: open to fund your swap ·
   deadline 1h 52m"); the app funds automatically as soon as it is unlocked. If
   the deadline passes, the match is cancelled (the no-show takes a strike; a
   funded initiator refunds after the XBT leg).

## Phase 2 (not in this spec)

1. Hardware-wallet BTC funding: PSBT export/import by file or animated QR
   (BC-UR), with per-swap ephemeral swap keys and the refund pre-signed to the
   device's address before funding. No protocol change — the relay only carries
   pubkeys.
2. Guided "sell XBT from a hardware wallet": move BTC to a new seed first, then
   restore the old seed as an XBT-only fortis wallet; refuse to continue if the
   old seed still holds BTC. (Hardware wallets can't produce `SIGHASH_UNIFIED`.)
3. BitBox02 over USB on Android.
4. Swaps in the web wallet (wasm bindings).
5. Shared swap state for multiple edge instances.

Separately, ahead of swaps: **Connect hardware wallet** onboarding (scan the
device's account QR, then one-tap open the matching XBT watch-only wallet) — its
own short design.
