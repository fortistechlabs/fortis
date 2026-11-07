# Lightning on XBT

Date: 2026-11-06 · Status: draft for review

## Goal

A Lightning wallet for XBT (the Bitcoin BLAKE2b chain) in Fortis, with the
same features as the BTC Lightning wallet
(`2026-11-03-lightning-wallet-design.md`). That means:

- Keys on the phone.
- Receiving through JIT channels from a Fortis-run XBT LSP.
- A capped Fortis balance for small amounts.
- Sending to BOLT11 invoices, Lightning addresses, LNURL-pay and BOLT12 offers.
- A reusable BOLT12 offer that can be paid while the app is closed.
- Moving funds between on-chain and Lightning in either direction, by
  splicing.
- The ledger-style history.

Success means:

- An XBT user can do on Lightning everything a BTC user can, with the same
  screens.
- A payment can never reach the wrong chain.
- Every transaction Fortis signs on XBT is accepted by Knots.

## Background

The XBT Lightning spec (lightning-blake2b/bolts, read at 1f3d88f) changes
Lightning in four ways:

- **Chain membership: `option_blake2b`, bit 512/513.**
  - Set in `init`, `node_announcement`, BOLT11 (field 9) and the three BOLT12
    feature fields.
  - A reader must refuse an invoice or offer without it, because XBT keeps
    BTC's `chain_hash` and the `lnbc` prefix. The bit is the only thing that
    tells the chains apart.
- **Unified signatures: `option_unified_sigs`, bit 514/515.**
  - Carried in `channel_type`.
  - Every signature the other node checks (commitment, closing, HTLC) uses
    `SIGHASH_UNIFIED` (0x20): 0x21 for ALL, and 0xa3 for the anchor HTLC
    signature.
  - Spends that only one node signs may stay plain.
- **Gossip floor:** gossip about channels confirmed before height 961,640 is
  ignored.
- **Block headers:** from activation onward they are 164 bytes, and the block
  id is not sha256d of the first 80 bytes.

The LDK spike (2026-11-06, worktree `~/Work/fortis-spike-xbt`, branch
`spike/ldk-xbt`) tested this on regtest:

- **Software:** LDK 0.2.7 / ldk-node 0.7.1 with about 400 lines of patches,
  against paulscode's LND fork v0.21.6-beta-blake2b.23, on Knots 29.4.3 with
  BLAKE2b active at height 20.
- **Steps that worked:** opening channels in both directions, payments in both
  directions, a cooperative close, and a force close with an HTLC timeout.
- **Result:** every transaction confirmed with 0x21 signatures and all funds
  were recovered.
- **Bug it caught:** one unpatched signing site (`sign_holder_htlc_transaction`)
  produced an HTLC timeout that Knots rejected. Only an on-chain test catches
  this class of bug.

## Decisions

| Topic | Decision |
|---|---|
| Implementation | **LDK on the phone and the LSP, patched for XBT.** We do not embed the LND fork: that would mean a 39 MB APK, plus about 110 MB of neutrino headers, and a new LSP |
| Fork form | A **patch set** on vendored crates, each change marked `FORTIS XBT`, replayed on each LDK upgrade |
| Chain switch | **Per node, not per process.** A BTC node and an XBT node can run in one process |
| Counterparty | Fortis runs its own XBT LSP: a second `fortis-lsp` instance |
| Receiving | JIT channels (LSPS2), as on BTC |
| Small amounts | Fortis balance (custodial, capped), as on BTC; ships with custody 0 |
| Transfers | Splicing in and out, as on BTC. Splicing only between our own nodes |
| BOLT12 | Yes: a reusable offer paid into the Fortis balance, and paying offers |
| Anchors | **Off** on XBT in the first release: `static_remote_key` channels only |
| Routing data | Peer-to-peer gossip, phone from the LSP; no RGS for XBT |
| Not in the first release | BTC↔XBT bridge, moving the node between devices, anchors, our own XBT RGS server |

## 1. LDK for XBT

**Vendored crates.** Patched copies sit in `vendor/` alongside `vendor/ldk-node`,
wired in through `[patch.crates-io]`:

- `lightning`
- `lightning-types`
- `lightning-invoice`
- `lightning-transaction-sync`
- `esplora-client`
- the on-chain wallet's signing path (BDK, or ldk-node's wallet code, whichever
  owns PSBT signing in 0.7.1)

Each change carries a `FORTIS XBT` comment. `vendor/XBT-PATCHES.md` lists every
patched site and why it was patched, so that an LDK upgrade is a checklist.

**Where the XBT rules come from:**

1. **Signatures follow the channel.**
   - A signature for a channel whose `channel_type` has
     `option_unified_sigs` uses the unified digest and hash type 0x21. A
     channel without it uses BIP143 and 0x01.
   - This applies to the commitment, closing, HTLC-success and HTLC-timeout
     signatures, and to the splice shared input, whether we create them or
     check them.
   - It is decided by the channel, not by a global setting, so BTC channels
     cannot be affected.
2. **The node's chain setting** (an `xbt: bool` in LDK's `UserConfig`,
   surfaced in the ldk-node `Config`) controls:
   - Bit 512 set (required) in `init`, `node_announcement`, BOLT11 and the
     BOLT12 offer, invoice request and invoice features.
   - Bit 514 required in every channel type we propose; incoming opens
     without it are refused.
   - Paying a BOLT11 invoice, or a BOLT12 offer or invoice, without bit 512
     is refused (`PaymentError::WrongChain`). A BTC node refuses those *with*
     bit 512.
   - Gossip for channels with a short channel id below 961,640 is dropped.
   - Funding and splice inputs from the on-chain wallet are signed with 0x21.
     This prevents replay on BTC for coins that exist on both chains.
   - Anchor channel types are never proposed, and are refused when offered.
   - Splicing is offered only to peers on the allow-list in the node's
     configuration (the phone lists the XBT LSP, and the XBT LSP lists no one
     outside Fortis). The LND fork cannot splice.
3. **Block ids come from the index.**
   - The Esplora client reads 164-byte headers by their length.
   - The chain-sync code uses the block hash the index returns, never
     `Header::block_hash()`.
   - This replaces the spike's prefix-to-hash map in the `bitcoin` crate,
     which is not carried forward.

**The unified digest** (`xbt.rs`, from the spike):

- It implements Knots PR #357 `SignatureHashUnified` for script types 0/1 and
  is checked against Knots' 142 non-taproot test vectors.
- Without ANYONECANPAY it covers every prevout. For a splice, LDK has every
  input's previous transaction from `tx_add_input`, and the digest is built
  from those.
- A single-input shortcut is used only when the transaction has one input.

**Unchanged:** spends only we sign (penalty claims, sweeps, closing outputs to
our wallet) keep plain hash types, as the spec allows.

## 2. Server side

**A second `fortis-lsp` instance**, built from the same binary:

| | BTC (today) | XBT |
|---|---|---|
| Config | `/etc/fortis/lsp.toml` | `/etc/fortis/lsp-xbt.toml` with `chain = "xbt"` |
| Unit | `fortis-lsp.service` | `fortis-lsp-xbt.service` |
| Seed / data / DB | `/var/lib/fortis/lsp` | `/var/lib/fortis/lsp-xbt` (its own `keys_seed`, backed up off the host like the BTC one) |
| Chain data | fortis-index BTC (Esplora) | fortis-index XBT (Esplora) |
| Admin + metrics (loopback) | 127.0.0.1:9736 | 127.0.0.1:9746 |
| Peer port | 9735 via the GCP relay | 9835 via the GCP relay (new DNAT rule; the wg0 MSS clamp already covers it) |
| Gossip | RGS (`rgs_url`) | peer-to-peer from its channel peers |

**What the XBT LSP does, all as on BTC:**

- LSPS2 JIT channels.
- The Fortis balance (§7 of the BTC spec).
- Holding offer payments and asking for wake pushes.
- Free forwarding.
- The revenue ledger.

**Limits:** every limit is its own setting in `lsp-xbt.toml`:

- `channel_min_sat`
- `custody_max_sat`, which starts at 0
- the 50,000 sat move threshold
- the beta caps

**Getting connected:** the operator opens outbound channels from the XBT LSP
to well-connected XBT nodes (LND or CLN fork) and funds them. LSP invoices
carry route hints through those peers until the LSP is well known (the same
mechanism as on BTC).

**The edge:**

- `/ln/info` gains an `xbt` object with the same fields as the BTC document:
  `enabled`, `message`, LSP node id and address, and limits.
  - Old app versions ignore it.
  - It starts with `enabled: false`.
- The Fortis balance, transfer quote, fee invoice, push registration and
  async-path routes take `chain` (`"btc"` default, or `"xbt"`) and forward to
  the matching LSP admin port.
- `/internal/ln/wake` carries `chain`. The push data says which chain to wake.
- VSS is shared; each wallet's store is keyed by its own node id.

**fortis-index:** the `/blocks` summary's `difficulty` falls back to
`difficulty_blake2b` (Knots' name for that field on BLAKE2b blocks). Today it
returns `null` there, which breaks Esplora clients. This fix ships first, on
its own.

## 3. The app

**The wallet:**

- A new wallet type, `chain = "ln-xbt"`. "Add wallet" offers
  **Lightning (XBT)** while none exists; there is at most one per app,
  alongside the BTC Lightning wallet.
- It has its own phrase, sealed like the others. The node seed is derived from
  that phrase in the same way as for BTC Lightning, in its own domain: BIP-85
  HEX index 1 (BTC: index 0) and VSS store id
  `HMAC-SHA256(node seed, "fortis/ln/vss/xbt/v1")` (BTC: `"fortis/ln/vss/v1"`),
  so one phrase never gives both wallets the same node or VSS store. The app
  also refuses a Lightning wallet whose phrase (and passphrase) is the other
  Lightning wallet's.
- It has its own VSS store, its own "one device at a time" takeover, and its
  own copy of the seed in the Keystore for background claims.
- Amounts are in sat, with the XBT label and colours, as XBT on-chain wallets
  show them today.
- `fortis-lightning` takes a chain setting. That setting picks:
  - the LSP, from `/ln/info`'s `xbt` object;
  - the index URL;
  - gossip (RGS for BTC, peer-to-peer from the LSP for XBT);
  - XBT mode in LDK (§1).
- **Running:** each node starts when its wallet is opened. A wake push starts
  only the chain it names. Both nodes may run in the app process at once.

**Receiving:**

- **Invoice:** a JIT invoice when no channel can take the amount. When even
  that is too small, the payment goes to the Fortis balance. The QR is
  captioned "XBT Lightning".
- **Offer:** one permanent BOLT12 offer with bit 512, paid while the app is
  closed (async payments, push wake). It shows only when the XBT LSP's
  `custody_max_sat` is above 0.

**Sending:** BOLT11, Lightning addresses, LNURL-pay and BOLT12 offers.

- **Wrong chain:**
  - The XBT wallet refuses anything without bit 512 with "This is a BTC
    Lightning invoice/offer".
  - The BTC Lightning wallet refuses bit-512 artifacts the same way, and
    offers to pay from the XBT Lightning wallet if one exists.
  - Scanning a Lightning invoice in an XBT on-chain wallet offers the XBT
    Lightning wallet.
- Lightning addresses and LNURL return a BOLT11 invoice, and the bit decides
  which wallet can pay it.

**Moving funds:** as on BTC.

- **Move in:** opens your channel the first time and splices in after that.
- **Move out:** splices out any amount; Move all closes the channel
  cooperatively.
- The transfer quote and fee rule are the same: `max(1%, 1,000 sat, network
  fee)`, with the XBT LSP's settings.
- Deposits from XBT on-chain wallets are signed by wallet-core, which already
  uses 0x21.

**History:** the same ledger view and itemised fees.

## 4. Errors, testing, rollout

**Errors:**

- **Wrong chain:** a named refusal before anything is paid.
- **XBT LSP or VSS unreachable:** the same behaviour as BTC, with a clear
  message and a force close offered for moving out.
- **XBT index behind:** if the index's tip block is more than 2 hours old
  (its header time), opens and splices wait with "Waiting for the XBT index".
  Payments over existing channels continue.
- **Offer unpayable from outside wallets** (see risks): the Offer tab says
  "Payable from Fortis wallets".

**Tests:**

1. **Unit:**
   - the Knots vectors;
   - the bit-512 and bit-514 setting and checks in every context (init, node
     announcement, BOLT11, the three BOLT12 fields, channel type);
   - the wrong-chain refusals in both directions;
   - the gossip floor;
   - BTC and XBT nodes in one process, each signing its own way;
   - the edge's routing by chain;
   - the index difficulty fallback.
2. **Regtest suite**, the spike harness grown up and run on this machine.
   - **Setup:** Knots (BLAKE2b at 20), the XBT index, the XBT LSP, a
     phone-side ldk-node, and paulscode's LND fork as an outside node.
   - **Cases:**
     - a JIT open;
     - payments both ways;
     - LND paying a JIT invoice through the LSP;
     - splice in, splice out, and a force close after a splice;
     - a cooperative close;
     - a force close with HTLCs in flight, settled by timeout and by
       success;
     - a penalty claim against a revoked state;
     - paying an offer from LND and to an LND offer;
     - an offer paid while the phone node is stopped, then claimed.
   - **Pass condition:** every transaction is confirmed by Knots and all
     funds end where expected.
   - **Gate:** it must pass for every release that touches `vendor/`.
3. **Real phone:** the `.dev` canary on XBT mainnet with small amounts:
   - create and restore;
   - JIT receive, with the app open and through a push wake;
   - move in and move out;
   - pay from an outside XBT node;
   - pay an outside XBT invoice;
   - offer receive with the app closed, once custody is enabled.

**Rollout:**

1. Ship the fortis-index difficulty fix.
2. Deploy `fortis-lsp-xbt`, fund it, and open its outbound channels.
3. Ship the app with `xbt.enabled = false`.
4. Pass the real-phone canary.
5. Enable it, labelled beta, with low caps and custody 0. The operator turns
   custody on separately, after the legal check in BTC spec §7.

## Risks

- **The fork tracks a moving spec.** The bits were renumbered on 2026-09-28.
  We follow lightning-blake2b/bolts. A spec change means a patch and a rerun
  of the regtest suite.
- **A signing site we haven't patched** loses money in a force close. The
  regtest suite's on-chain cases are the only guard, so it gates releases.
- **BOLT12 interop with the LND fork is unproven.** The LND fork has its own
  BOLT12 code, and it may not relay onion messages to LDK or pay LDK offers.
  If the regtest case fails, offers ship as "Payable from Fortis wallets"
  instead of being dropped.
- **Private-channel route hints:** in the spike, LND could not pay an LDK
  invoice whose hint used the channel's alias SCID when LND was the channel
  peer itself. JIT invoices through the LSP are a different path; the regtest
  case "LND paying a JIT invoice through the LSP" checks it.
- **A small network:** few XBT routes exist, so payments to outside nodes
  depend on the LSP's outbound channels.

## Out of scope (first release)

- BTC↔XBT bridge swaps.
- Moving a node between devices (beyond the existing takeover).
- Anchor channels.
- Our own XBT RGS server.
- Splicing with nodes outside Fortis.
- Upstreaming generic hooks to LDK (possible later, to shrink the patch set).
