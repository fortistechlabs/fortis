# Lightning wallet: a separate wallet type

Date: 2026-11-03 · Status: design approved in conversation, awaiting spec review

## Goal

Replace today's per-BTC-wallet "Lightning (beta)" toggle with **one separate
Lightning wallet per app**, created by the user, with its own recovery phrase
and its own functionality: send, receive by invoice, a **reusable BOLT12
offer that is paid even when the app is not running**, and transfers of any
amount to and from on-chain wallets.

Success: a user sees at a glance what is on Lightning and what is on-chain;
moves money between them with one screen; receives to a permanent offer with
the app closed; Lightning features grow without touching the on-chain screens.

## Decisions

| Topic | Decision |
|---|---|
| Wallet model | New wallet type `chain = "ln"` (mainnet) in the existing wallet list (approach 1 of 3) |
| How many | At most **one** per app; "Add wallet" offers Lightning only while none exists |
| Keys | Its **own** BIP-39 phrase (create or restore), sealed like other wallets. Node seed derived from it exactly as today (BIP-85 child), so `fortis-lightning`, VSS and seed-only recovery are unchanged |
| Background keys | The node's working seed is also kept in the **Android Keystore without user authentication** (usable after first unlock since boot) so the app can claim payments in the background. The phrase stays PIN/biometric-sealed. Lightning is presented as a spending wallet |
| Funding fee (on-chain → Lightning) | `max(1% of amount, 1,000 sat, network fee)`; the network fee is included, never charged twice |
| Withdrawal fee (Lightning → on-chain) | Same rule |
| JIT receive fee | Unchanged LSPS2 fee `max(1%, 1,000 sat)`; the operator absorbs the network fee |
| Forwarding | `fortis-lsp` forwards for free (done 2026-11-03); Fortis-to-Fortis payments cost 0 |
| v1 scope | Create/restore, transfers both ways (any amount, slider), send, invoice receive, **reusable offer**, **async payments with push wake**, history |
| Later | Human-readable name (BIP353 / Lightning Address on fortistechlabs.com), ldk-node 0.8 upgrade, push without Google Play |
| Library | Build on **ldk-node 0.7.1** (latest release). Do not use unreleased `main` (0.8.0+git) or LDK 0.3 RCs with funds. Wrap splice/transfer calls so the 0.8 upgrade (`splice_out_max`, fixed funding fee rates) is contained |

Splicing (`splice_in`/`splice_out`) and async payments (`receive_async`,
`AsyncPaymentsRole`) are **experimental** in ldk-node 0.7.1. Lightning ships as
beta with caps (below) and canary coverage of both.

## 1. The wallet

- **Store:** a `WalletConfig` with `chain = "ln"`, `network = "bitcoin"`, the
  sealed phrase and salt like BTC wallets; no addresses/UTXO fields used.
- **Create:** generate a 12/24-word phrase, show, confirm (the BTC flow).
  **Restore:** enter a phrase; the node restores channels/balance from VSS,
  with the existing "active on another device" / takeover handling.
- **Unlock** (foreground): starts the node, connects to the LSP, checks the
  backup — today's `LightningSession`, keyed by the Lightning wallet's id.
- **Delete:** warning when it holds funds (recoverable with the phrase); stops
  the node and wipes local state and the Keystore copy; the VSS backup stays.
- **Removed:** the per-BTC-wallet toggle (`ln_enabled_<walletId>`), the
  combined on-chain + Lightning total, "Move to Lightning" on BTC wallets, and
  paying Lightning targets from on-chain wallets.
- **No migration:** Lightning never shipped; only the `.dev` canary wallet is
  reset.
- **Display:** the wallet list shows the Lightning wallet with its balance and
  a "pending" line while a transfer or close settles. No cross-chain total.

## 2. Transfers

One **Transfer** screen, opened from either wallet: from-wallet, to-wallet,
then the shared amount control extracted from `SwapScreens.kt`'s
`SwapAmount` (field, two-way slider, 25/50/75 % marks, Min, Max) into a common
component used by swaps and transfers. A summary line: "You move X · fee F ·
arrives ≈ Y · ready after 1 confirmation". PIN/biometric to confirm.

**On-chain → Lightning**
1. The app gets a **transfer quote** from the edge (section 5) for the amount.
2. The chosen on-chain wallet sends the deposit to the Lightning wallet's
   on-chain (transit) address; the operator's share rides as an output to the
   fee address (none when the share is 0 or below dust).
3. After 1 confirmation the Lightning wallet **opens** a channel (no channel
   yet) or **splices in** (channel open; it stays usable throughout).
4. Minimum: the LSP's `channel_min_sat` for a first open; no upper bound
   beyond the balance and the beta cap.

**Lightning → on-chain**
- **Some:** `splice_out` straight to an address of the chosen on-chain wallet.
- **All:** cooperative close and sweep to that wallet on 0.7.1;
  `splice_out_max` once on ldk-node 0.8.
- Then the operator's share is paid as a Lightning payment to an invoice from
  the LSP (none when the share is 0).

**Fee:** `max(1% of amount, 1,000 sat, network fee)`. "Network fee" is every
on-chain transaction the transfer needs (in: deposit + open/splice), estimated
up front and shown before confirming. Operator share = fee − network fee (≥ 0).

**Rules:** one transfer at a time; both wallets show it as pending. A failed or
stalled splice leaves the channel at its old size and the deposit on the
transit side, retried, and sendable back. Every on-chain spend syncs the
wallet first (`sync_before_spend`, 2026-11-03).

## 3. Receiving

Receive screen, two tabs.

**Invoice:** amount → BOLT11 (Lightning only, no on-chain address in the QR).
JIT quote with the LSPS2 fee when a channel must open; this is how a new
wallet can get its first channel without a transfer.

**Offer:** one permanent amountless BOLT12 offer (`lno1…`) per wallet; QR,
copy, share. Needs a channel (offers cannot open one); before that the tab
says "Fund your Lightning wallet or receive one invoice payment first".
Shows "Can receive up to N sat with this offer" (inbound capacity).

**Async setup** (background, once a channel exists): the phone calls
`GET /ln/async-paths` (edge → LSP admin) for paths keyed by its node id,
passes them to `set_paths_to_static_invoice_server`, and gets the offer from
`receive_async`. It re-registers when paths change; the stored static invoice
is refreshed whenever the node is online.

**Wake on push (app not running):**
1. On create/restore the phone registers its Firebase token (`POST /ln/push`).
2. A payer's invoice request for the phone's static invoice raises
   `StaticInvoiceRequested` on the LSP, which calls the edge's loopback
   `POST /internal/ln/wake {node_id}`; the edge sends a high-priority,
   data-only push ("wake", no amount or identity).
3. The app starts in the background from the Keystore seed, starts the node,
   connects; the LSP releases the held HTLC; the node claims it, persists to
   VSS, stops; a notification says "Received ⚡ N sat".
4. Phone off / offline: the LSP keeps holding; the claim happens on return.
   Without Google Play Services: an hourly WorkManager check instead, and the
   Offer tab says payments may take up to an hour while the app is closed.

## 4. Sending and history

- **Send** (Lightning wallet): BOLT11, BOLT12 offers (incl. `lno=` in BIP21),
  Lightning Addresses, LNURL-pay, `lightning:` deep links — as today. Confirm
  sheet with amount, description, expiry and the routing fee cap
  (1 % + 50 sat); through the LSP alone the fee is 0. PIN/biometric.
- An **on-chain address** scanned in the Lightning wallet offers a transfer
  (splice-out to it, transfer fee rule) or "pay from an on-chain wallet".
- **Not enough on Lightning:** prompt with a pre-filled transfer.
- A **Lightning target** scanned in an on-chain wallet offers "Pay with your
  Lightning wallet".
- **History** (Lightning wallet): sends, receives (invoice/offer, incl.
  background claims), transfers with state (waiting for confirmation, moving
  into channel, done), each with amount, fee, description, time. On-chain
  wallets label transfers "Transfer to/from Lightning".

## 5. Server side

**fortis-lsp**
- `Builder::set_async_payments_role(Server)`; stores static invoices (ldk-node).
- Loopback admin API on `metrics_listen` (127.0.0.1:9736, never public):
  `POST /async-paths {node_id}` → `blinded_paths_for_async_recipient` bytes,
  only for a node id with a channel to the LSP; `POST /fee-invoice {amount}` →
  BOLT11 from the LSP for an operator share.
- On `StaticInvoiceRequested`: `POST` the edge's `/internal/ln/wake`.

**fortis-edge** (install-token routes unless noted)
- `GET /ln/async-paths` → LSP admin, for the caller's node id.
- `POST /ln/push {fcm_token}` → stored against the node id in the edge DB,
  bound to the registering install token.
- `POST /internal/ln/wake {node_id}` (loopback only) → FCM HTTP v1 push using
  a service-account key in `/etc/fortis/`.
- `POST /ln/transfer-quote {amount, direction, destination}` → fee, network
  estimate, operator share, quote id; stored 30 min, bound to destination and
  amount.
- Broadcast fee check: a deposit matching a live quote must pay that quote's
  share (possibly 0) instead of the usual service-fee rule. Lightning
  transactions stay exempt (`ln_fee.rs`).

**Already done** (2026-11-03, uncommitted at time of writing): Esplora
endpoints in `fortis-index`, `sync_before_spend`, free forwarding, outbound
channels, edge Lightning fee exemption.

**Enforcement:** as with the on-chain percentage today, fees are enforced for
the official app; a modified client could skip the Lightning fee payment.

**Security:** admin API and wake endpoint loopback-only; public routes need an
install token; pushes carry no amounts or identities; paths only for nodes
with a channel; Keystore seed is per-device, never exported, wiped on delete.

## 6. Errors, testing, rollout

**Errors:** LSP unreachable → clear message; withdrawals offer force close
(existing dialog); deposits wait on the transit side as "pending, retrying".
VSS down → every state change refuses with "backup unavailable" (background
claims wait for the next wake). Expired quote → re-quote and show the new
fee. No Play Services → hourly check, message on the Offer tab.

**Tests**
- Unit: fee rule boundaries (1 %, 1,000 sat floor, network fee); quote match
  in the broadcast check; push/wake routes; transfer screen amounts; the
  one-Lightning-wallet rule; Keystore seed lifecycle.
- Real-stack regtest **through the edge and index** (not bitcoind directly):
  fund (open), fund again (splice in), part out (splice out), all out;
  invoice receive (JIT); offer receive with the client node stopped →
  simulated wake → claim; fee outputs/invoices correct in every case.
- Pixel canary (deploy/lightning.md §10, extended): create/restore; transfers
  both ways; offer paid with the app closed (push wake); same in airplane mode
  (claimed on return).

**Rollout:** "beta" label; per-channel cap as configured on the LSP plus a
beta cap per transfer, published in `/ln/info` as `max_transfer_sat`
(default 500,000 sat; the app's slider Max is the smaller of the balance and
this cap); ldk-node 0.8 upgrade as a separate step.

## Out of scope (v1)

BIP353 / Lightning Address names; push without Google Play; multiple Lightning
wallets; custodial receiving; LDK Server in place of `fortis-lsp` (revisit at
its v0.1).
