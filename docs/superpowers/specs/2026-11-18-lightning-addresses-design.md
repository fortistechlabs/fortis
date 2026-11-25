# Lightning addresses (`name@fortis.cash`) — design

Status: draft for the owner's approval, 2026-11-18. Nothing is built.

## Intent and why
Let a Fortis user claim a human name and receive Lightning payments at `name@fortis.cash` from any wallet that
supports Lightning addresses (LUD-16). Owner's reason: a short address people can pay without a BOLT12 offer
(which needs the receiving wallet to be reachable) and without pasting invoices. Payments land in the user's
Fortis balance on the LSP and are delivered to the channel as today, so the phone need not be online.

## What the owner decided
- One namespace on `fortis.cash`. A name is claimed from a Lightning wallet's settings and is linked to
  that wallet.
- The chain is fixed by the wallet that claims it: claimed from the BTC Lightning wallet = a BTC address;
  from the XBT Lightning wallet = an XBT address. Never both. The same person wanting both claims two names.
- Names are unique across both chains. Releasing a name frees it at once for anyone.
- The list of names is kept in SQL, in the edge (approach 1 below).
- The chain cannot be detected from a payer's request (an LNURL lookup carries no wallet or chain), so it is
  part of the claim and is stated in the payment page ("Pay donal on Fortis (XBT Lightning)").

## Goals / non-goals / anti-goals
Goals: claim, show, release a name in settings; payers get a valid LUD-06/16 invoice; credit lands in the
owner's account; no change to how payments are delivered.
Non-goals: a name for both chains at once; names changing hands by transfer; payer comments; zaps (LUD-18/NIP-57);
on-chain fallback; a web page to browse names.
Anti-goals: a name ever paying a wallet other than its owner's; a claim taken over by anyone but the key that
made it; a payer on the wrong chain ending up with lost money (their payment must simply fail to route).

## Approaches considered
1. Names table in the edge, new SQLite file; edge asks the right LSP for an invoice. **Chosen.** One list for
   both chains, uniqueness trivial; edge is already the public face with a rate limiter and the Cloudflare route.
   Cost: a new data file to back up on the edge.
2. A table in each LSP's ledger with a cross-check between the two. Rejected: two lists, a race when two people
   claim one name at once.
3. All names in one LSP's ledger. Rejected: XBT would depend on the BTC LSP being up; blurs what each server is for.

## Design
**Storage (edge).** `names.sqlite`, owner-only, WAL, `synchronous=FULL`. Table `names(name TEXT PRIMARY KEY,
chain TEXT CHECK(chain IN ('btc','xbt')), node_id TEXT NOT NULL, created_at INTEGER)`, plus a unique index on
`(chain, node_id)` so a wallet holds at most one name. `INSERT` of a taken name fails on the primary key: the
database, not application code, decides who got it first.

**Name rules.** Lowercase `a-z 0-9 . _ -`, 3 to 32 chars, must start and end with a letter or digit, no `..`;
a reserved list (`admin`, `info`, `support`, `fortis`, `root`, `postmaster`, `abuse`, `security`, `www`, `api`,
`lnurlp` and the like) refused. The app lowercases input; the server refuses anything not already lowercase.

**Claim and release (signed by the wallet).** Reuse the existing account signature scheme (`fortis-account-v1:`
+ body, signed with the Lightning node key; body has node_id, op, params, nonce, expiry). Two new ops,
`claim_name {name}` and `release_name`, sent through the existing `/ln/account?chain=` route. The edge keeps no
crypto: it forwards the signed body to the matching LSP, which checks signature, expiry and nonce with its
existing `verify` and answers with the verified node id and params (new LSP op that verifies and does nothing
else). The edge then writes or deletes the row for exactly that `node_id` and chain. Release deletes only the row
whose `node_id` and chain match the signer. A wallet with a name that claims again gets an error (release first).

**Public endpoints (edge, host `fortis.cash`).**
- `GET /.well-known/lnurlp/<name>` → LUD-06 JSON: `tag: payRequest`, `callback`, `minSendable`/`maxSendable`
  (msat, within the account's per-invoice and balance caps), `metadata` =
  `[["text/identifier","name@fortis.cash"],["text/plain","Pay <name> on Fortis (BTC|XBT Lightning)"]]`,
  `commentAllowed: 0`. Unknown name → LUD-06 error JSON (`{"status":"ERROR","reason":"…"}`, HTTP 200 as
  the spec expects) so wallets show it.
- `GET /lnurlp/callback/<name>?amount=<msat>` → asks the LSP of the name's chain for an invoice and returns
  `{"pr": "<bolt11>", "routes": []}`. The invoice's description hash is sha256 of the exact metadata string
  returned by the first call (LUD-06 requirement; Fortis wallets already refuse invoices that miss it).
- CORS `Access-Control-Allow-Origin: *` on both; no auth; per-IP rate limit and a global cap on callbacks.

**New LSP op `invoice_for {node_id, amount_msat, description_hash}`** (admin interface, called by the edge
only; not reachable by phones). Creates a BOLT11 with that description hash, expiry 10 minutes, records
it with `pending_invoice` for the node so the payment credits that account exactly as a normal account
invoice does (existing code path: credit, push notice, delivery into the channel). Refuses when custody is full
or the node has no account/never connected (`accounts` row or push token absent is not an error — a first
payment may be the first credit — but the node id must belong to a claimed name; the edge checks that).
Needs a node call that builds the invoice with a hash description (ldk-node supports it).

**App (Android).** Settings, per Lightning wallet: a "Lightning address" row. Not claimed: a field, a live
"available / taken / not allowed" check, a Claim button, a note that the chain is fixed. Claimed: shows
`name@fortis.cash` with Copy and Share, a Release button with a confirmation that says anyone can take the name
afterwards. Errors shown in words (taken, not allowed, already has a name, server unreachable).

**Operations (owner).** Point `fortis.cash` at the edge (add the domain to Cloudflare if it is not there yet, then a tunnel hostname + DNS record; it
does not resolve today), add the host to the edge's accepted hosts, add `names.sqlite` to the backup routine. TLS is
Cloudflare's. LSP binaries (both chains) and the edge are rebuilt and installed; app released as 0.7.14.

## Risks and how they are handled
- Squatting: one name per wallet, reserved list, rate limits on claims per IP.
- A name pointing at a wallet that has since gone: payments credit its balance, which waits on the LSP until
  the wallet is restored (as seen on 2026-11-18). Documented to users in the claim screen.
- Wrong-chain payer: invoice of the other chain fails to route; nothing is lost. The payment page says which.
- Enumeration of names via the lookup: accepted; names are public by design.
- Edge restarts or loss of `names.sqlite`: names vanish; backups cover it. A lost name can be reclaimed by
  its wallet if still free.
- No time lock after release (owner decision).

## Left to the builder
Exact JSON field names for the two ops, rate-limit numbers, the choice of edge HTTP handler layout, the LSP's
node call for hash-description invoices, test layout. Tests required before release: claim uniqueness under a
concurrent double claim, release only by the owner, replayed and expired claim bodies, name-rule table,
LUD-06 response shape against the published spec, description-hash match, caps and custody-full behaviour.
