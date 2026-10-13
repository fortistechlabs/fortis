# Swap offer book — design

**Date:** 2026-10-12 · **Status:** design approved in chat, spec awaiting review

## Goal

On the Swaps page, a user sees the public offers waiting for a match, picks one,
and takes all or part of it — and before taking, the app tells them in plain
words whether they have the funds to.

What the owner asked for: a list of swaps waiting for a match; pick one; the app
says if the user has sufficient funds to match it. Decisions made in chat:
whole **or part** of an offer can be taken; the list shows **always**, below the
user's own swaps; taking uses a **new edge route** that takes exactly the offer
picked (not a crossing offer left to the matcher).

## Today

- The edge knows every open offer (`Store::open_offers`), but `GET /swap/quote`
  exposes only the best price and a count per direction.
- Public offers are matched automatically when a crossing offer is posted.
  Taking a *specific* offer exists only for private offers (`POST /swap/take/{code}`
  → `matcher::take_private` → `pair`), which already handles partial fills and
  cancels the taker's remainder.
- The app's New Swap screen has the amount slider (`maxSwapSat`, `swapFeeReserve`,
  `fractionOf`, `amountAtFraction`), wallet pickers, the send/commission/receive
  lines and the keep-phone-on note.

## Edge (fortis-edge, `crates/fortis-edge/src/swap`)

### `GET /swap/book`

Open public offers, newest state:

```json
{ "xbt_to_btc": [Entry…], "btc_to_xbt": [Entry…] }
Entry = { "id", "direction", "amount_sat", "price_sat_per_xbt",
          "btc_sat", "xbt_sat", "expires_at" }
```

- `amount_sat` is what is still open on the offer's own send chain (after partial
  fills); `btc_sat`/`xbt_sat` are that amount's two legs at the offer's price
  (`matcher` maths), so the app needn't recompute them.
- Excluded: private offers, expired offers (`expires_at <= now`), offers not
  `Open`, the **caller's own** offers (same install token), offers from tokens
  that are struck out (`struck_out`), and offers whose open amount is below the
  swap minimum.
- Sorted best price first (lowest ask for `xbt_to_btc`, highest bid for
  `btc_to_xbt`), then oldest first; at most **50 per direction**.
- Never contains tokens, secret hashes, share codes or client ids. An offer id
  grants nothing by itself: every owner action needs that offer's secret.

### `POST /swap/offers/{id}/take`

Body `{ "amount_sat", "price_sat_per_xbt", "client_id" }`:

- `amount_sat` — on the **taker's** send chain, from the minimum up to the
  offer's open amount (on that chain) and the cap.
- `price_sat_per_xbt` — the price the user saw; if the offer's differs, `409
  "price changed"` and nothing happens.
- `client_id` — idempotency, as for `POST /swap/offers`: a repeat with the same
  token and client id returns the same result (taken once).
- Builds the taker's side as an unlisted offer (opposite direction, the maker's
  price) and pairs it with the chosen offer at the maker's price, under the
  matching lock, reusing `pair` (partial fills: the maker's remainder stays
  listed; the taker's remainder is cancelled, as in `take_private`).
- Reply: `{ "match_id", "owner_secret", "offer_id" }` — the same shape as take by
  code; from here the swap is an ordinary match.
- Refusals (409 unless noted), each with a plain reason: no such / no longer open
  / expired offer; private offer (use its code); your own offer; amount below the
  minimum or above the open amount or the cap (400); price changed; taker struck
  out (403, the existing `STRUCK_OUT` text).
- Journal: `swap_event!` for every take and every refusal.

## App (`android/app/src/main/kotlin/com/fortis/wallet`)

### Swaps page

Own swaps first (Active, then History), then **"Offers waiting for a match"** in
two groups by what the user would do:

- **Get XBT for your BTC** — the `xbt_to_btc` book (people selling XBT);
- **Get BTC for your XBT** — the `btc_to_xbt` book.

Each row: the amount the user would receive (offer's open amount, on the user's
receive chain), the price, its difference from the market reference, and the
funds line (below). Loaded when the page opens and every 15 s while it is
visible; vanished offers drop off. Empty: "No offers waiting right now. Start your
own with New swap."

### Funds line — one pure function

`fundsFor(offer, wallets, balances, feeReserves): Funds`:

- The user's **send chain** is the offer's receive chain. Only wallets on it that
  can **sign** count (watch-only and hardware wallets are skipped — they can't
  fund a swap).
- `best` = the wallet with the largest `maxSwapSat(balance, feeReserve)` (balance
  minus commission minus the network-fee reserve, exactly as the slider); it is
  the Take screen's preselected wallet.
- `need` = the offer's open amount on the user's send chain.
- Result, in order:
  - no wallet on the **other** chain to receive into → **"Add an XBT/BTC wallet to
    receive into"** (grey);
  - `best ≥ need` → **"You can take all of it"** (green);
  - `best ≥` the swap minimum → **"You can take up to X"** (yellow);
  - else → **"Not enough BTC/XBT to take this"** (grey).

### Take offer screen

Reached by tapping a row. Shows the offer's price (fixed: theirs), From and To
wallet pickers, the amount field with the slider (from the minimum to
`min(offer's open amount, best max)`), You send / Commission / You receive (≈ —
the edge's rounding is final), the keep-phone-on note, and **Take**. Take calls
`POST /swap/offers/{id}/take` with a fresh `client_id` (reused on a retry), saves
the swap record as for take by code, starts the swap service, and opens the swap
detail screen.

### Errors, in plain words

- offer gone / filled / expired → "Someone else took this offer", back to the
  refreshed list;
- price changed → "This offer's price changed — check it again", screen reloads;
- amount outside limits → under the amount field;
- struck out → "You can't take offers for now. Try again later.";
- network failure → "Couldn't reach the server"; retry reuses the `client_id`.

## Testing

- **Edge `/swap/book`:** filters (private, expired, not open, own, struck, below
  minimum); sort and the 50 cap; the JSON never contains token, secret hash,
  share code or client id.
- **Edge `/take`:** every refusal; partial take leaves the maker's remainder listed
  with the right open amount; same `client_id` takes once; price changed refused;
  end-to-end (`tests/swap_e2e.rs`) a swap started by a take runs to closed.
- **App:** `fundsFor` in every state (incl. watch-only and hardware wallets
  skipped, no receive wallet); the Take screen's maximum; error mapping; a
  `SwapEngineTest` swap started from a take runs to DONE. Then on the phone with
  `.dev` and `.dev2`.

## Decision after the device test (2026-10-12)

The seller's app withdraws an offer's remainder once it is matched
(`cancelRemainder`: one swap record follows one match). So a partial take
**ends** the offer: the edge keeps the remainder open, but the app takes it
down. Kept deliberately for now (owner's choice); the Take screen says "Taking
part of it ends their offer: the rest is withdrawn." Several takers per offer
(the seller keeps the remainder listed, one record per match) is a later,
separate engine change.

## Out of scope

Editing or re-pricing someone else's offer; a chart or depth view; push alerts
for new offers; showing private offers.
