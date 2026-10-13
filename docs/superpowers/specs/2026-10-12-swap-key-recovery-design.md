# Swap-key recovery after a reinstall — design

**Date:** 2026-10-12 · **Status:** approved; implemented 2026-10-13 (plan `docs/superpowers/plans/2026-10-13-swap-key-recovery.md`; §4 dropped, see the plan's rulings)

## Problem

A swap's keys live at `m/84'/<coin>'/<account>'/2'/<swap_index>'`, and the
initiator's preimage is derived from its swap key. So the seed can rebuild
everything — *if* the swap index is known. Today it isn't: `SwapStore.nextSwapIndex`
draws it at random in `[0, 2^31)` and keeps it only in app data. After a
reinstall, a cleared app or a new phone mid-swap, the user restores the seed
but cannot find the swap: their refund depends entirely on the edge's
watchtower, and a participant who hadn't handed over its presigned redeem
loses the XBT side.

## Goal

From the recovery phrase alone, a restored app finds every swap it was in
that still has coins locked, and refunds or redeems them itself, as if it
never lost its data.

## Design

### 1. Swap indices in order, never reused

Each wallet's swaps take indices 0, 1, 2, … in order (per wallet = per chain
and account). The app keeps the next index, as now in `SwapStore`, but it is
now a counter, not a random draw.

Reusing an index would reuse its keys *and its preimage* — never allowed:

- The edge refuses an offer whose recovery tag (below) it has already seen
  (`409 "this swap index was used before"`), so even an app that lost its
  counter can't reuse one; it moves on to the next index and retries.
- A restored app first runs recovery (step 3) and sets its counter past the
  highest index found.

### 2. A recovery tag on every offer

With each new offer the app sends `recovery_tag = HMAC-SHA256(k_rec, index)`,
where `k_rec` is a key derived from the wallet's seed for this purpose only
(`m/84'/<coin>'/<account>'/3'` → private key bytes). The edge stores the tag
with the offer (unique, indexed) and with any match made from it.

- A tag reveals nothing: unlinkable to the wallet, to other tags, to keys.
- A tag is a capability: whoever knows it can read that swap's recovery view
  (terms, role, outpoints) — exactly what the owner may see anyway. 256 bits,
  not guessable.

### 3. Recovery on restore

After a seed restore (and from Settings → "Find my swaps"):

1. Derive tags for indices `0, 1, 2, …` and ask `POST /swap/recover
   {tags: [...≤100]}`. The edge answers, per known tag: offer id and status,
   and per match its terms, role, status, `xbt_funding`, `btc_funding`.
2. Stop after 20 consecutive unknown tags (a gap limit, like addresses).
3. Set the wallet's next swap index past the highest index found.
4. For each match with coins still at stake (one of ours funded, not yet
   spent), rebuild a swap record: keys and preimage from the seed and index,
   contract from the terms, funding outpoints from the edge — **checked on
   chain** (independent explorer, exact script and amount) before anything
   is signed, like every other chain fact.
5. The ordinary engine takes it from there: redeem if the counterparty's
   HTLC is still there and we can (initiator: our preimage; participant: the
   preimage from the BTC redeem), else refund at our locktime.

The owner secret is not needed: moving coins needs only keys, terms and the
chain. Relay messages are not replayed; a recovered swap never funds — it only
redeems or refunds (an unfunded match is left to lapse).

### 4. Existing swaps (random indices)

Swaps made before this change have random indices and no tag: they can't be
found by the scan. On first launch of the new version the app sends the edge
the tags for its *existing* records too (`POST /swap/offers/{id}/tag`), so a
later restore finds them; nothing else changes for them.

## Edge changes

- `Offer.recovery_tag: Option<[u8;32]>` (serde default), unique index
  `CF_TAGS: tag → offer id`; `POST /swap/offers` accepts `recovery_tag` (hex),
  `409` if already used.
- `POST /swap/recover {tags}` (token-authenticated, rate limited, ≤100 tags):
  the recovery view per known tag. Journal line per call (count only).
- `POST /swap/offers/{id}/tag` (owner secret): attach a tag to an older offer.

## App changes

- `SwapStore.nextSwapIndex`: a counter per wallet; `recovery_tag` sent with
  each offer.
- wallet-ffi: `swap_recovery_tag(account, index) -> hex` on the wallet.
- A recovery step after restore (and in Settings), building records for found
  swaps with coins at stake, then the normal engine.

## Testing

- Edge: tag stored, duplicate refused, recover returns only known tags and
  never another offer's data, ≤100 tags, rate limit.
- Core/ffi: tag deterministic per (seed, account, index), different per
  index and per account, unrelated to swap keys.
- App: counter never reuses (incl. after a 409); recovery scan with gap limit
  sets the counter; a recovered initiator refunds at its locktime and a
  recovered participant redeems from the BTC redeem — both in `SwapEngineTest`
  with a "lost the store" phone; on-chain check refuses an edge-faked funding.
- End to end on regtest: fund, wipe one phone's store, restore, recover, settle.

## Out of scope

Recovering swaps from before this change that were never tagged (step 4
covers any still on the phone when it updates); recovering an unfunded match.
