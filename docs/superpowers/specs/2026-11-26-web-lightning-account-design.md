# Web wallet: account-mode Lightning (design)

Date: 2026-11-26. Status: draft for owner approval.

## Intent and why

The Android app has Lightning; the web wallet (`web/` + `crates/wallet-wasm`) has none
(the 2026-10-30 design said "Android only"). The owner wants the web wallet to get the
Lightning features, "especially all the lightning stuff". A browser cannot run
`ldk-node` (no raw TCP, no background process), so the web wallet uses the **Fortis
account**: the LSP holds a balance keyed by the wallet's Lightning node id, and every
request is a signed HTTPS call to the edge `POST /ln/account[?chain=xbt]` that the app
already uses (`FortisAccount.kt`). Decision by the owner: account-mode only.

## Goals

For a BTC or XBT software wallet on the web, with the same node identity as the phone
(same seed => same node id => the **same account balance on phone and web**):

1. Show the account balance (free / held / cap) and the account's history.
2. Receive: BOLT11 invoice for an amount, QR + copy + share; the reusable BOLT12 offer.
3. Send: pay a BOLT11 invoice, a BOLT12 offer (`pay_offer` + `pay_status` polling), an LNURL-pay
   or a Lightning address (`name@domain`), with amountless support, the fee shown before
   paying, and **Send all** (same rule as the app, `sendAllSat`).
4. Lightning address: show, claim, release (`my_name`, `claim_name`, `release_name`), with the
   chain in every heading ("XBT Lightning address"). Address card with QR/copy/share on Receive.
5. Withdraw from the account to an on-chain address (`withdraw`).
6. Fee rule identical to the phone and LSP: cap `max(amount/100, 1000 msat)`;
   `offerReserveMsat`, `sendAllSat` ported with the same tests.
7. Translations: new strings go through the existing locale flow (English first, others fall back).

## Non-goals

- Self-custody channels, JIT channels, VSS, push wake-ups, "Use Lightning here instead":
  none of it exists in the browser. If the app's own channel holds funds, the web sees only
  the account part (state this in the UI: "Lightning on this device").
- Watch-only and BitBox wallets (no seed in the browser, cannot sign account requests).
- Receiving while the tab is closed beyond what the account already does (the LSP holds the
  balance; the tab just shows it next time).

## Anti-goals

- No secret leaves wasm: the node key is derived and used for signing inside `wallet-wasm`;
  JS gets only the node id and signatures.
- A balance the user cannot reconcile (project rule: every money screen adds up). Account
  history rows must itemise received / sent / fees / withdrawn and sum to the balance.
- Never present account funds as self-custody. The custody cap is shown.

## Approaches

1. **Crypto and parsing in wasm, transport and UI in JS (chosen).** `wallet-wasm` gains
   node-key derivation (BIP-85 index per chain, then the LDK key path), request signing
   (message signature in the exact format `account_auth::verify` accepts), and BOLT11
   decoding (amount, description hash, expiry, payee) via `lightning-invoice`. JS builds the
   request body (byte-identical to the Android builder), posts it, and renders. Matches the
   existing "every signature inside WebAssembly" rule and gives golden tests against Rust.
2. All in JS with noble-secp256k1 libraries: smaller wasm, but a second secp256k1 and key
   handling in JS, which the project avoids. Rejected.
3. A server-side signing proxy: breaks self-custody of the key. Rejected.

Spike risk: wasm must reproduce the node id and the signature that `fortis-lightning` /
`fortis-lsp` use. Task 1 of the plan is a golden-vector test (fixed seed => node id and
signature that the LSP's `account_auth::verify` accepts), run before any UI.

## Key handling (my call, owner to confirm)

Balance, invoices and history need a signature, so the node key is held in wasm memory while
the app is unlocked (same lifetime as the app secret) and zeroized on relock. **Paying and
withdrawing re-ask for the password/passkey**, as on-chain sends do today. Claiming or
releasing a name also re-asks (it is hard to undo).

## Constraints

- Static PWA, no build step for JS; wasm built with `wasm-pack` (README of `web/`).
- CORS: the edge already serves the web app; LNURL/Lightning-address lookups to other domains
  depend on the remote server sending CORS headers. Where blocked, say so in the UI
  ("this address does not allow browsers; paste an invoice") instead of failing silently.
- Mainnet and the hosted edge only for now; regtest wallets get no Lightning card.
- The owner commits, releases and deploys the site; I do not.

## Left to the builder

Screen layout (follow the existing web wallet panels), error strings, polling interval for
`pay_status`, how the history merges account rows with on-chain rows.

## Open calls for the owner

- Same account on phone and web (shared balance): intended, since the node id is the same.
- Web Lightning behind a Settings toggle, or on by default for eligible wallets? I propose on by
  default, shown as a "Lightning" tab in the wallet.
