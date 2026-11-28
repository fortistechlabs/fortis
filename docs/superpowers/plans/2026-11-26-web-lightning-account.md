# Web wallet: account-mode Lightning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The web wallet gets Fortis-account Lightning (balance, history, receive, send, Lightning address, withdraw) for BTC and XBT software wallets, with the same node identity and account as the phone.

**Architecture:** `wallet-core`/`wallet-wasm` derive the node key (BIP-85 then LDK's `m/0'`) and sign account requests inside wasm, and decode BOLT11/BOLT12. JS (`web/src/ln*.js`) builds the request, posts it to the edge `/ln/account[?chain=xbt]`, and renders a new Lightning tab. Fee rule, Send all and name rules are ported from Android.

**Tech Stack:** Rust (wallet-core, wallet-wasm, vendored `lightning`/`lightning-invoice` for decoding only), wasm-pack, plain ES modules + `node --test`.

**Spec:** `docs/superpowers/specs/2026-11-26-web-lightning-account-design.md`

## Global Constraints

- The node key never leaves wasm; JS gets the node id and signatures only.
- Signature: `message_signing::sign(SIG_PREFIX + body)` with `SIG_PREFIX = "fortis-account-v1:"` (LN message signing, zbase32); body JSON has `node_id`, `op`, `params`, `nonce` (16..64 chars of `A-Za-z0-9-_`; use 32), `expires_at` (now+120 s; server accepts at most 300 s ahead). The server verifies the exact bytes received, so the JS serialisation only needs to be the string it signs and sends.
- Node seed: `bip85_hex(bip39_seed, 64, index)`, index 0 = BTC, 1 = XBT (`BTC_NODE_SEED_INDEX`, `XBT_NODE_SEED_INDEX` in `fortis-lightning/src/node.rs`). KeysManager seed = BIP-32 master private key (32 bytes) of those 64 bytes; node secret = that seed's `Xpriv::new_master` derived at `m/0'`.
- Edge: `POST {edge}/ln/account` (BTC) or `/ln/account?chain=xbt`; headers `Authorization: Bearer <token>`, `X-Fortis-Sig`. Status map as `FortisAccount.kt`: 401 auth, 402 insufficient, 409 `custody_full` (+`min_channel_sat`) / `name_taken` / `has_name`, 400 `name_invalid` (+`reason`).
- Fee cap `max(amount_msat/100, 1000)`; `sendAllSat(B)`: `B/1010 >= 100` -> `B/1010`; else `B >= 2000` -> `(B-1000)/1000`; else 0 (B msat, result sat).
- Names: `normalizeName` = trim + lowercase; rules and reserved list exactly as `LightningAddressName.kt`; domain `fortis.cash`; chain sent as `btc`/`xbt`.
- Software wallets only (not watch-only, not BitBox), mainnet only. Paying, withdrawing, claim/release re-ask identity via the existing `confirmIdentity`.
- English strings in `web/src/locales/en.json` only; other locales fall back. Every user-visible string goes through `t()`.
- Owner commits, releases, deploys; never commit, push or deploy. Never run cargo on several crates at once: use `nice cargo ... -j 4`.
- `wasm-pack` is not installed on this machine: Task 2 installs it (`cargo install wasm-pack` or `npm i -g wasm-pack`) and needs clang.

## Review Focus

1. A double click on Pay or Withdraw must not send twice: one in-flight lock per wallet; the button stays disabled until a result.
2. Device clock wrong by minutes: server answers `expired`/`too_far_ahead`; the UI says "check your device's clock".
3. Invoice already expired, for the other network, or amountless with an amount above the free balance: refused before any signed call, with a clear message.
4. Lightning address / LNURL server without CORS or returning a bad description hash: a plain message, never a payment.
5. `custody_full` on a receive: show the cap and that the app can open a channel; the account never shows a balance that does not match `balance_msat - held_msat`.

---

### Task 1: Node identity and account signing in wallet-core (golden test)

**Files:**
- Create: `crates/wallet-core/src/ln_account.rs`; modify `crates/wallet-core/src/lib.rs` (export), `crates/wallet-core/src/keys.rs` (make `bip85_hex_from_master` `pub(crate)`), `crates/wallet-core/Cargo.toml` (secp256k1 `recovery` feature via bitcoin).
- Test: `crates/fortis-lightning/tests/web_account_identity.rs` (has both libraries).

**Interfaces:**
- Produces: `pub enum LnChainId { Btc, Xbt }`; `impl MasterKey { pub fn ln_node_id(&self, chain: LnChainId) -> String /* 33-byte compressed hex */; pub fn ln_sign_account(&self, chain: LnChainId, body: &str) -> String /* zbase32 */ }`. Signing input is `"fortis-account-v1:" + body`.

- [ ] **Step 1: Write failing golden test** `web_account_identity.rs`: for a fixed 12-word phrase and for both chains, `MasterKey::from_phrase(..).ln_node_id(c)` equals the node id of `ldk_node::Node` seed from `fortis_lightning::derive_node_seed(&to_seed, Network::Bitcoin, c)` via `KeysManager::new(&Xpriv::new_master(..).private_key.secret_bytes(), 0, 0, true).get_node_id(Recipient::Node)` (the `ldk_node_id` recipe in `node.rs:1126`); and `ln_sign_account(c, body)` satisfies `lightning::util::message_signing::verify(("fortis-account-v1:"+body).as_bytes(), &sig, &node_pk)` and also equals `message_signing::sign` of the same bytes with the node secret. Also assert BTC and XBT ids differ.
- [ ] **Step 2: Run** `nice cargo test -p fortis-lightning --test web_account_identity -j 4`; expected FAIL (items undefined).
- [ ] **Step 3: Implement** `ln_account.rs` with the signatures above. Seed: `bip85_hex_from_master(master, 64, index)`; km seed = `Xpriv::new_master(Network::Testnet, node_seed).private_key.secret_bytes()`; node secret = `Xpriv::new_master(Network::Testnet, km_seed).derive_priv(m/0')`. Sign: sha256d(`"Lightning Signed Message:"` + msg), recoverable ECDSA, bytes `[31+recid] ++ compact64`, zbase32-encode (alphabet `ybndrfg8ejkmcpqxot1uwisza345h769`; mirror `vendor/lightning-0.2.7/src/util/message_signing.rs`). Zeroize secrets.
- [ ] **Step 4: Run the test**; expected PASS. Also run `nice cargo test -p wallet-core -j 4`.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 2: wasm bindings for identity and signing

**Files:** Modify `crates/wallet-wasm/src/lib.rs`; rebuild `web/pkg/*` (committed artefacts, see `web/README.md`).

**Interfaces:**
- Consumes: Task 1.
- Produces (JS on `Wallet`): `lnNodeId(chain: "btc"|"xbt"): string`, `lnSignAccount(chain, body: string): string`; JS errors for any other chain value.

- [ ] **Step 1: Failing test** in `wallet-wasm` (`#[cfg(test)]`): `Wallet::new(phrase,"")` `.ln_node_id("btc")` equals the Task 1 golden node id (copy the hex from the passing Task 1 run into the test) and `ln_sign_account` verifies; `"foo"` errors.
- [ ] **Step 2: Run** `nice cargo test -p wallet-wasm -j 4`; expected FAIL.
- [ ] **Step 3: Implement** the two `#[wasm_bindgen(js_name = ...)]` methods.
- [ ] **Step 4: Run test**; PASS. Install wasm-pack if missing, then `wasm-pack build crates/wallet-wasm --target web --out-dir ../../web/pkg` (README command); confirm `web/pkg/wallet_wasm.d.ts` lists both methods.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 3: Decode invoices and offers in wasm

**Files:** Modify `crates/wallet-wasm/Cargo.toml` (deps `lightning-invoice`, `lightning` from the workspace vendor paths, `default-features = false`, std off if it builds), `crates/wallet-wasm/src/lib.rs`, `crates/wallet-wasm/src/dto.rs`.

**Interfaces:**
- Produces: `decodeInvoice(bolt11: string): { amountMsat: number|null, description: string|null, descriptionHash: string|null, paymentHash: string, createdSecs: number, expirySecs: number, payee: string, network: string }` and `decodeOffer(offer: string): { amountMsat: number|null, description: string|null, expired: boolean }`; both throw a JS error on junk. Amounts as JS numbers (< 2^53 msat is checked).

- [ ] **Step 1: Failing tests**: a real mainnet BOLT11 fixture from `lightning-invoice` tests (amount, description), an amountless invoice (`amountMsat` null), a testnet invoice (`network` not `bitcoin`), junk and an empty string (error, no panic), an offer fixture from `lightning` offer tests.
- [ ] **Step 2: Run** `nice cargo test -p wallet-wasm -j 4`; FAIL.
- [ ] **Step 3: Implement** with `lightning_invoice::Bolt11Invoice::from_str` and `lightning::offers::offer::Offer::from_str`. If the offer decoder will not compile for `wasm32-unknown-unknown`, ship `decodeInvoice` only, make `decodeOffer` return `{amountMsat:null,...}` from a minimal bech32 check, and require the user to type the amount for offers (note it in the UI task).
- [ ] **Step 4: Run tests**; PASS; `nice cargo build -p wallet-wasm --target wasm32-unknown-unknown --release -j 4` succeeds; rebuild `web/pkg`; note the wasm size change in the task report.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 4: Account client in JS (`web/src/lnaccount.js`)

**Files:** Create `web/src/lnaccount.js`, test `web/src/lnaccount.test.js`.

**Interfaces:**
- Consumes: `Wallet.lnNodeId/lnSignAccount` (injected as `signer = { nodeId(): string, sign(body): string }`).
- Produces:
  - `newNonce(): string` (32 chars of `A-Za-z0-9-_`), `buildAccountRequest(nodeId, op, params, nonce, expiresAt): string` (key order `node_id, op, params, nonce, expires_at`).
  - `class AccountError extends Error { kind: 'custody_full'|'insufficient'|'auth'|'rejected'|'network'|'name_invalid'|'name_taken'|'has_name'; minChannelSat?; reason? }`.
  - `class FortisAccount { constructor(transport, nowSecs?) ; balance(s); invoice(s, amountMsat, description); pay(s, invoice, amountMsat?); payOffer(s, offer, amountMsat?); payStatus(s, id); offer(s); myName(s, chain); claimName(s, chain, name); releaseName(s, chain); withdraw(s, amountMsat, address, feerateSatVb); history(s, params?); feeInvoice(...) }` mirroring `FortisAccount.kt` op names and params exactly.
  - `edgeTransport({ base, chain, getToken, refreshToken, fetchImpl? }): (body, sig) => Promise<{status, text}>` posting with `Authorization: Bearer`, `X-Fortis-Sig`, a 15 s timeout, one token refresh retry on a 401 with no JSON reason.

- [ ] **Step 1: Write failing tests** (`node --test web/src/lnaccount.test.js`): balance request body parses with `op:"balance"`, `expires_at == now+120`, nonce length 32, sig passed through unchanged; error mapping for 409 `custody_full` (carries `minChannelSat`), 402, 401 with reason `replayed`, 502 -> `network`; BTC URL has no query, XBT URL ends `?chain=xbt`; a fetch that throws -> `network`; `history` op name matches `FortisAccount.kt` (read the file for its params).
- [ ] **Step 2: Run** `node --test web/src/lnaccount.test.js`; FAIL.
- [ ] **Step 3: Implement** the module. Read the remaining ops in `FortisAccount.kt` (below `feeInvoice`) so every op name and param matches.
- [ ] **Step 4: Run test**; PASS.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 5: Fee, Send all and name rules (`web/src/lnmath.js`, `web/src/lnname.js`)

**Files:** Create both plus `lnmath.test.js`, `lnname.test.js`.

**Interfaces:**
- Produces: `routingFeeCapMsat(amountMsat: bigint|number): number`, `offerReserveMsat(amountMsat)`, `sendAllSat(balanceMsat): number`, `normalizeName(s)`, `checkName(name): null|'characters'|'length'|'edges'|'double_dot'|'reserved'`, `LN_ADDRESS_DOMAIN = 'fortis.cash'`. Use JS numbers (balances are far below 2^53 msat).

- [ ] **Step 1: Write failing tests** porting `AccountRoutingTest.kt` `theFeeCapIsOnePercentAndAtLeastOneSat` and `sendAllIsTheLargestAmountThatFitsWithItsOwnFeeCap` (same brute-force loop and fixed cases: 1_999 -> 0, 2_000 -> 1, 3_000 -> 2, 101_000 -> 100) and the name rules (check order characters, length 3..32, edges, `..`, reserved list from `LightningAddressName.kt`; uppercase input normalises).
- [ ] **Step 2: Run** `node --test web/src/lnmath.test.js web/src/lnname.test.js`; FAIL.
- [ ] **Step 3: Implement** to match `AccountRouting.kt` and `LightningAddressName.kt` line by line.
- [ ] **Step 4: Run**; PASS.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 6: LNURL-pay and Lightning address resolution (`web/src/lnurl.js`)

**Files:** Create `web/src/lnurl.js`, `web/src/lnurl.test.js`.

**Interfaces:**
- Consumes: wasm `decodeInvoice`, injected `sha256Hex(string)` (use `crypto.subtle`), `fetchImpl`.
- Produces: `parseTarget(input): { kind: 'bolt11'|'offer'|'lnurl'|'address'|'unknown', value }` (strips `lightning:` prefix, case-insensitive, `name@domain`, `lnurl1...` bech32 decode via a small local function); `resolvePay(target, fetchImpl): { callback, minMsat, maxMsat, metadata, description }`; `fetchInvoice(pay, amountMsat, fetchImpl, sha256Hex, decodeInvoice): bolt11` which refuses an invoice whose amount differs from the request or whose description hash is not `sha256(metadata)`.
- Errors: `LnurlError` with `kind` in `cors|http|bad_response|out_of_range|hash_mismatch|amount_mismatch`.

- [ ] **Step 1: Write failing tests** with a fake `fetch`: address -> `https://domain/.well-known/lnurlp/name`; amount outside min/max -> `out_of_range`; tampered description hash -> `hash_mismatch`; callback invoice with another amount -> `amount_mismatch`; fetch rejecting with `TypeError` -> `cors`; `lightning:LNBC...` upper-case parsed as bolt11.
- [ ] **Step 2: Run**; FAIL.
- [ ] **Step 3: Implement.** Only `https` URLs (except `localhost` for tests via an option); callback URL query appended with `amount=<msat>`.
- [ ] **Step 4: Run**; PASS.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 7: Lightning tab shell, balance and history

**Files:** Create `web/src/lightning-ui.js` (all Lightning panels; exports `renderLightningTab(ctx)`); modify `web/src/app.js` (add the tab and the context: `ensureSession`, `confirmIdentity`, `copy`, `qrCode`, `el`, `t`, `state`, `render`, backend token + `edgeRegister` refresh), `web/src/style.css`, `web/src/locales/en.json`, `web/sw.js` (add new files to `ASSETS`).

**Interfaces:**
- Produces: `lightningEligible(w): boolean` (software wallet, not watch-only/hw, `network === 'mainnet'`, edge backend); `makeAccountFor(w, ctx): { account: FortisAccount, signer }` building `edgeTransport` from the wallet's edge backend (`w.backend.url`, token, refresh) and `chain`.
- Consumes: Tasks 2, 4, 5.

- [ ] **Step 1: Failing test** for `lightningEligible` and the history reconciliation helper `reconcile(entries, balanceMsat)` in a new `web/src/lightning-ui.test.js` (pure functions only): watch-only/hw/regtest wallets are ineligible; `reconcile` itemises received/sent/fees/withdrawn and reports any non-zero difference to the balance (shown as a visible "unexplained" row, never hidden).
- [ ] **Step 2: Run** `node --test web/src/lightning-ui.test.js`; FAIL.
- [ ] **Step 3: Implement** pure helpers, then the tab: balance card (free, held, cap, "Lightning on this device: account only" note), history list (kinds `received|sent|internal|converted|withdrawn`, fiat via existing `fiatText`), refresh on tab show and on the existing poll. Eligible wallets get a Lightning tab beside Receive/Send/History; ineligible wallets do not.
- [ ] **Step 4: Run tests**; PASS. Serve `web/` (`python -m http.server 5173 --directory web`) and check the tab against a dev wallet on the hosted edge; report what was seen.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 8: Receive (invoice, offer, Lightning address card)

**Files:** Modify `web/src/lightning-ui.js`, `en.json`, `style.css`.

**Interfaces:** Consumes Task 7's `makeAccountFor`, Task 5 name rules. Produces `receivePanel(ctx, w)`.

- [ ] **Step 1: Failing test** for `receiveError(e)` mapping `AccountError` to message keys (custody_full -> `ln_custody_full` with the cap, network, auth, other), added to `lightning-ui.test.js`.
- [ ] **Step 2: Run**; FAIL.
- [ ] **Step 3: Implement**: address card first (heading `"{CHAIN} Lightning address"`, `name@fortis.cash`, QR/copy/share via `navigator.share` when present, claim form with live `checkName` errors, release behind `confirmIdentity` and a confirmation dialog), then invoice form (amount sat, optional description, QR + copy), then the BOLT12 offer (`offer` op, QR + copy). Claim and release call `confirmIdentity` first. `custody_full` shows the cap and the minimum channel size.
- [ ] **Step 4: Run tests**; PASS. Manual: claim a throwaway name with a dev wallet, see the address on the phone and web (shared account), release it; report the evidence.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 9: Send (invoice, offer, LNURL/address, Send all)

**Files:** Modify `web/src/lightning-ui.js`, `en.json`, `style.css`.

**Interfaces:** Consumes Tasks 3, 4, 5, 6. Produces `sendPanel(ctx, w)` and pure `planSend({ target, balanceMsat, amountMsat? }): { ok, amountMsat, feeCapMsat, error? }`.

- [ ] **Step 1: Failing tests** for `planSend`: amount + cap over the free balance -> error `insufficient`; amountless invoice with a typed amount; the 2-sat case from the user (balance 2_000 msat can pay 1 sat, cannot pay 2 sat); an expired invoice (`createdSecs + expirySecs < now`) -> `expired`; non-mainnet invoice -> `wrong_network`; Send all fills `sendAllSat`.
- [ ] **Step 2: Run**; FAIL.
- [ ] **Step 3: Implement** the panel: paste/scan input (reuse the existing paste field; no camera scanning in this task), review step showing amount, fee cap ("up to"), description/payee, then `confirmIdentity`, then one call (`pay` or `payOffer` + poll `payStatus` every 2 s up to 2 min, show `pending|succeeded|failed|unknown` honestly). One in-flight lock per wallet id (Review Focus 1); on `network` error after sending, say the payment may have gone through and point to History, never auto-retry. If `decodeOffer` was reduced in Task 3, require a typed amount for offers.
- [ ] **Step 4: Run tests**; PASS. Manual with dev wallets: pay a BOLT11, pay a `name@fortis.cash`, Send all on a small balance; report each result.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 10: Withdraw to on-chain

**Files:** Modify `web/src/lightning-ui.js`, `en.json`.

**Interfaces:** Produces `withdrawPanel(ctx, w)`; pure `planWithdraw({ balanceMsat, amountSat, feerateSatVb })` using the LSP `withdrawal(amount_sat, feerate)` rules in `crates/fortis-lsp/src/account_api.rs:53` (read it; copy its fee and minimum so the UI prediction equals the server's).

- [ ] **Step 1: Failing tests** for `planWithdraw` with the server's own fixtures (read its tests): below the minimum, exact balance, fee over balance.
- [ ] **Step 2: Run**; FAIL.
- [ ] **Step 3: Implement** panel (address validated with the wallet's `checkAddress`, fee rate from the existing backend `feerate`, `confirmIdentity`, in-flight lock, show returned txid with explorer link).
- [ ] **Step 4: Run tests**; PASS. Manual: withdraw a small amount on a dev wallet; report the txid.
- [ ] **Step 5: Checkpoint** (owner commits).

### Task 11: Docs, strings, release bookkeeping and adversarial pass

**Files:** Modify `web/README.md` (Scope: Lightning account mode, custody note, CORS note, shared account with the phone), `CHANGELOG.md`, `web/VERSION`, `web/sw.js` cache name if the asset list changed, `site/lightning.html` (one line: the web wallet supports it, only after the owner has deployed).

- [ ] **Step 1:** Run all JS tests: `node --test web/src/*.test.js`; all Rust: `nice cargo test -p wallet-core -j 4`, then `-p wallet-wasm`, then `-p fortis-lightning` one after another, never together.
- [ ] **Step 2: Adversarial pass** against each Review Focus line using a dev wallet: double-click Pay, change the device clock by 10 minutes, paste an expired invoice, a testnet invoice, an address on a non-CORS domain, receive above the custody cap; record result for each, and anything not checked, in the task report.
- [ ] **Step 3:** Update docs and CHANGELOG (note: balance shared with the phone; account only, not channels).
- [ ] **Step 4:** Report what was verified and what was not (nothing is deployed; owner commits, builds `web/pkg`, runs `deploy/publish-wallet.md`).
