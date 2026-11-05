# Lightning Wallet — Plan 2a: the Fortis account

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the Lightning wallet receive and send any amount, however small, by holding what its own channel cannot take in a capped custodial account on `fortis-lsp`, converted into the user's own channel once it passes 50,000 sat.

**Architecture:** `fortis-lsp` gains an SQLite ledger (`account.rs`), signed-request auth (`account_auth.rs`), a loopback admin API on its existing metrics listener, event wiring (credits on `PaymentReceived`, settlement on send results) and a conversion loop. `fortis-edge` passes `/ln/account/*` through with the install token. The phone signs account requests with its node key (new FFI), and the Kotlin app routes receives and sends between its channel, the LSP's JIT channel and the account.

**Tech Stack:** Rust (ldk-node 0.7.1, `lightning::util::message_signing`, rusqlite 0.31 bundled, tiny_http, ureq), UniFFI, Kotlin/Compose.

**Spec:** `docs/superpowers/specs/2026-11-03-lightning-wallet-design.md` — §7 (and the Decisions rows "Small receives", "Account → channel", "Account after a channel", "Total custody"). Transfers (§2) are Plan 2b; offers/async/push (§3) are Plan 3.

## Global Constraints

- Per-account cap: **50,000 sat** (`account_max_sat`, default 50000). Over it the account converts; it never refuses a credit for the per-account cap.
- Total custody: `custody_max_sat` in `lsp.toml`, **default 0 (account disabled)** until the operator confirms the legal position; and never more than the LSP's backing (`spendable_onchain_balance_sats` + total outbound capacity). At either limit new account invoices are refused with: "Fortis can't hold more small balances right now — receive at least N sat to open your own channel" (N = the LSP's effective `min_payment_size_msat` / 1000).
- Conversion fee: `max(1% of the amount moved, 1,000 sat)`, kept by the LSP; the account goes to 0.
- Fortis-to-Fortis (account invoice paid from another account): settled in the ledger, fee 0.
- Account sends: actual routing fee, debited after the payment resolves; held up front at the existing cap `amount/100 + 50 sat`.
- Identity: account key = the Lightning wallet's node id (33-byte compressed pubkey, hex). Requests are signed by the node key over `"fortis-account-v1:" + <exact body bytes>`; body JSON `{node_id, op, params, nonce, expires_at}`; expiry ≤ 300 s ahead; each nonce usable once.
- Every ledger change is committed (SQLite `synchronous=FULL`) before the event it records is acknowledged (`event_handled`) or the HTTP reply is sent; credits are idempotent per payment hash.
- Metric: `fortis_lsp_custody_sat` (sum of balances + holds).
- App copy: balance lines "In your channel: X" and "Held by Fortis: Y" (EN + de, es, fr, pt-rBR, pt-rPT, zh-rCN, zh-rTW, zh-rHK).
- Receive routing (plan decision consistent with §7's intent of minimal custody): own invoice when the channel's inbound room covers the amount; else the LSP's JIT channel when the amount ≥ the LSP's JIT minimum; else an account invoice.
- Conversion opens a **new** channel with the balance pushed (`push_to_counterparty_msat`); splicing into an existing channel is Plan 2b. When the user already has a channel with inbound room for the amount, conversion pays the balance over it instead.

## Review Focus

- A **payment arriving for an account while the LSP restarts** (event replayed after a crash) must credit exactly once — Task 3 test `replayed_payment_received_credits_once`.
- An **account send that fails** must return the held amount; one that succeeds must debit only amount + actual fee — Task 3 test `failed_send_releases_the_hold`.
- A **replayed or expired signed request** (same nonce twice, or `expires_at` past) must be refused without touching the ledger — Task 2 test `replayed_nonce_is_refused`.
- **Conversion whose channel never becomes ready** (funding dropped): the account must still hold the balance — Task 4 test `balance_is_debited_only_on_channel_ready`.
- **Custody at its limit**: an account invoice request is refused before an invoice is issued, so a payer is never able to pay into a full account — Task 3 test `account_invoice_refused_at_total_custody`.

---

### Task 1: The ledger

**Files:**
- Create: `crates/fortis-lsp/src/account.rs`
- Modify: `crates/fortis-lsp/Cargo.toml` (add `rusqlite = { version = "0.31", features = ["bundled"] }`), `crates/fortis-lsp/src/config.rs` (keys `account_max_sat` default 50000, `custody_max_sat` default 0), `crates/fortis-lsp/src/main.rs` (`mod account;`)
- Test: unit tests in `account.rs`

**Interfaces:**
- Produces: `pub struct Ledger` with `Ledger::open(path: &Path) -> Result<Ledger>` (file `<data_dir>/accounts.sqlite`, created mode 0600); `balance(&self, node: &PublicKey) -> Result<Account>` where `pub struct Account { pub balance_msat: u64, pub held_msat: u64 }`; `credit(&self, node: &PublicKey, payment_hash: [u8; 32], amount_msat: u64) -> Result<bool>` (false = this hash was already credited); `hold(&self, node: &PublicKey, id: &str, amount_msat: u64) -> Result<()>` (errors `InsufficientBalance` when balance − held < amount); `settle_hold(&self, id: &str, spent_msat: u64) -> Result<()>` (debits `spent_msat`, releases the rest; `spent_msat = 0` = release); `transfer(&self, from: &PublicKey, to: &PublicKey, payment_hash: [u8; 32], amount_msat: u64) -> Result<()>` (atomic, idempotent per hash); `history(&self, node: &PublicKey, limit: usize) -> Result<Vec<Entry>>` with `pub struct Entry { pub at_secs: u64, pub kind: EntryKind, pub amount_msat: i64, pub fee_msat: u64, pub payment_hash: Option<[u8;32]> }`, `pub enum EntryKind { Received, Sent, Internal, Converted }`; `total_custody_msat(&self) -> Result<u64>` (balances + holds); `use_nonce(&self, node: &PublicKey, nonce: &str, expires_at: u64) -> Result<bool>` (false = seen; rows past expiry are pruned); `pending_invoice(&self, payment_hash: [u8;32], node: &PublicKey, amount_msat: u64, expires_at: u64) -> Result<()>` and `invoice_owner(&self, payment_hash: [u8;32]) -> Result<Option<PublicKey>>`.

- [ ] **Step 1: Write the failing tests** (temp dir per test)
  - `credit_then_balance`: credit 100_000 msat → `balance == Account { balance_msat: 100_000, held_msat: 0 }`.
  - `credit_is_idempotent_per_payment_hash`: same hash twice → second returns false, balance credited once.
  - `hold_settle_and_release`: balance 50_000; `hold("a", 30_000)` → `held 30_000`; `hold("b", 30_000)` → `InsufficientBalance`; `settle_hold("a", 21_000)` → `balance 29_000, held 0`; `hold("c", 10_000)` + `settle_hold("c", 0)` → balance unchanged.
  - `transfer_moves_atomically_and_once`: A 10_000 → transfer A→B 4_000 (hash h) twice → A 6_000, B 4_000.
  - `custody_counts_balances_and_holds`, `history_is_newest_first`, `nonce_is_single_use_and_pruned_after_expiry`, `ledger_file_is_owner_only` (mode & 0o077 == 0).
  - Config: `account_limits_default_to_disabled` — `with("")` gives `account_max_sat == 50_000`, `custody_max_sat == 0`.
- [ ] **Step 2: Run `cargo test -p fortis-lsp account`; expect FAIL (module missing)**
- [ ] **Step 3: Implement** — one SQLite file, `PRAGMA journal_mode=WAL; synchronous=FULL`; each mutating call is one transaction; amounts as INTEGER msat; `CHECK (balance_msat >= 0)`.
- [ ] **Step 4: Run; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "fortis-lsp: account ledger"`

---

### Task 2: Signed account requests

**Files:**
- Create: `crates/fortis-lsp/src/account_auth.rs`
- Test: unit tests in `account_auth.rs`

**Interfaces:**
- Consumes: Task 1 `Ledger::use_nonce`.
- Produces: `pub const SIG_PREFIX: &str = "fortis-account-v1:"`; `pub struct AccountRequest { pub node_id: PublicKey, pub op: String, pub params: serde_json::Value, pub nonce: String, pub expires_at: u64 }`; `pub fn verify(body: &[u8], sig_zbase32: &str, now_secs: u64, ledger: &Ledger) -> Result<AccountRequest, AuthError>` with `pub enum AuthError { Malformed, BadSignature, Expired, TooFarAhead, Replayed }`. Uses `ldk_node::lightning::util::message_signing::verify(msg, sig, pk)` with `msg = SIG_PREFIX + body`. `expires_at < now` → `Expired`; `expires_at > now + 300` → `TooFarAhead`; nonce 16–64 chars `[A-Za-z0-9_-]`.

- [ ] **Step 1: Write the failing tests** — sign with `message_signing::sign(msg, &secret_key)` in tests: `valid_request_is_accepted`; `signature_by_another_key_is_refused` (BadSignature); `body_changed_after_signing_is_refused`; `expired_request_is_refused`; `request_too_far_ahead_is_refused`; `replayed_nonce_is_refused` (second verify of the same body → `Replayed`; ledger balance untouched); `malformed_json_is_refused`.
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement `verify`**
- [ ] **Step 4: Run; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "fortis-lsp: signed account requests"`

---

### Task 3: Admin API and event wiring

**Files:**
- Create: `crates/fortis-lsp/src/account_api.rs`
- Modify: `crates/fortis-lsp/src/metrics.rs` (route `POST /account` on the loopback listener to `account_api::handle`; metric `fortis_lsp_custody_sat`), `crates/fortis-lsp/src/main.rs` (open the `Ledger` in `run`; in `handle_event`: `PaymentReceived` for a hash with an `invoice_owner` → `credit` before `event_handled`; `PaymentSuccessful`/`PaymentFailed` for an account send → `settle_hold`)
- Test: unit tests in `account_api.rs` (handler over a fake node trait), `crates/fortis-lsp/tests/` not needed (Task 7 is the real-stack test)

**Interfaces:**
- Consumes: Tasks 1-2.
- Produces: one endpoint `POST /account` (body = signed request, header `X-Fortis-Sig`), ops:
  - `balance` → `{balance_msat, held_msat, max_msat}`
  - `invoice {amount_msat, description}` → `{invoice}`; refused (HTTP 409, body `{"error":"custody_full","min_channel_sat":N}`) when `custody_max_sat == 0`, or total custody + amount > `custody_max_sat`, or > backing. Records `pending_invoice` (expiry 3600 s).
  - `pay {invoice, amount_msat?}` → `{payment_id}`; holds `amount + amount/100 + 50_000 msat`; if the invoice's payment hash belongs to another account (`invoice_owner`) → `transfer` (fee 0) and reply `{internal: true}`; else `bolt11_payment().send` and settle on the event.
  - `history {limit}` → `[Entry…]`
  - Errors: 401 `{"error":"auth","reason":<AuthError>}`, 402 `{"error":"insufficient"}`, 400 malformed.
  - A trait `AccountNode` (`receive_invoice`, `send_bolt11`, `backing_sat`) implemented for `ldk_node::Node`, so handlers are unit-testable.

- [ ] **Step 1: Write the failing tests** (fake `AccountNode`): `balance_of_a_new_account_is_zero`; `account_invoice_refused_when_disabled` (custody_max 0 → 409); `account_invoice_refused_at_total_custody`; `account_invoice_refused_above_backing`; `pay_holds_amount_plus_fee_cap`; `failed_send_releases_the_hold`; `successful_send_debits_amount_plus_actual_fee`; `paying_another_accounts_invoice_is_internal_and_free`; `replayed_payment_received_credits_once` (call the credit hook twice with one hash); `bad_signature_is_401_and_changes_nothing`.
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement**; `cargo clippy -p fortis-lsp --all-targets` clean.
- [ ] **Step 4: Run `cargo test -p fortis-lsp`; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "fortis-lsp: account admin API and settlement"`

---

### Task 4: Conversion into the user's channel

**Files:**
- Create: `crates/fortis-lsp/src/conversion.rs`
- Modify: `crates/fortis-lsp/src/main.rs` (check every 60 s; on `ChannelReady` for a conversion channel → `settle` the conversion)
- Test: unit tests in `conversion.rs`

**Interfaces:**
- Consumes: Task 1 ledger (add `start_conversion(node, id, amount_msat)` = hold the whole balance, and `finish_conversion(id, fee_msat)` = debit balance and record `Converted` with the fee; `abandon_conversion(id)` = release).
- Produces: `pub fn conversion_fee_msat(moved_msat: u64) -> u64` = `max(moved/100, 1_000_000)`; `pub enum Plan { None, PayOverChannel { amount_msat }, OpenChannel { channel_sat, push_msat } }`; `pub fn plan(balance_msat: u64, cap_msat: u64, peer_connected: bool, inbound_room_msat: u64, in_progress: bool, channel_min_sat: u64) -> Plan` — `None` unless `balance > cap && peer_connected && !in_progress`; `PayOverChannel { amount: balance − fee }` when `inbound_room_msat ≥ balance − fee`; else `OpenChannel { push = balance − fee, channel_sat = max(channel_min_sat, 2 × push/1000) }`. Pay-over-channel uses `spontaneous_payment().send(amount, node_id)`; debit on `PaymentSuccessful`. Open uses `open_channel(node_id, <any addr: the peer is connected>, channel_sat, Some(push_msat), Some(fee config))`; debit on that channel's `ChannelReady`; `ChannelClosed` before ready → `abandon_conversion`.

- [ ] **Step 1: Write the failing tests** — `fee_is_one_percent_with_a_1000_sat_floor` (`conversion_fee_msat(55_000_000) == 1_000_000`, `(150_000_000) == 1_500_000`); `nothing_under_the_cap`; `nothing_while_offline_or_in_progress`; `pays_over_a_channel_with_room`; `opens_a_channel_otherwise` (`plan(55_000_000, 50_000_000, true, 0, false, 20_000) == OpenChannel { channel_sat: 108_000, push_msat: 54_000_000 }`); `balance_is_debited_only_on_channel_ready` (ledger: start → abandon leaves the balance; start → finish debits it with the fee).
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "fortis-lsp: account conversion into the user's channel"`

---

### Task 5: Edge passthrough

**Files:**
- Modify: `crates/fortis-edge/src/main.rs` (flag `--lsp-admin-url <URL>`, e.g. `http://127.0.0.1:9736`; route `POST /ln/account` → `<lsp-admin-url>/account`, forwarding the body and `X-Fortis-Sig`; install token required; body ≤ 16 KiB; the existing per-token rate limit; 404 unless the flag is set)
- Modify: `crates/fortis-edge/README.md`, `deploy/lightning.md` §5 (add the flag to `EXTRA_ARGS`)
- Test: unit tests in `main.rs` tests module (fake upstream like the `ln_fee_exempt` test)

**Interfaces:**
- Produces: `POST https://api.fortistechlabs.com/ln/account` (Bearer install token, header `X-Fortis-Sig`).

- [ ] **Step 1: Write the failing tests** — `account_route_404s_without_the_flag`; `account_route_needs_a_token`; `account_route_forwards_body_and_signature_header` (fake upstream echoes them); `account_route_refuses_bodies_over_16_kib` (413); upstream status codes (401/402/409) passed through unchanged.
- [ ] **Step 2: Run `cargo test --bin fortis-edge account_route`; expect FAIL**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run the edge suite; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "fortis-edge: /ln/account passthrough"`

---

### Task 6: Signing on the phone; the Kotlin account client

**Files:**
- Modify: `crates/fortis-lightning/src/node.rs` (`pub fn sign_account_request(&self, body: &str) -> Result<String, LnError>` = `node.sign_message((SIG_PREFIX + body).as_bytes())`; `pub const ACCOUNT_SIG_PREFIX: &str = "fortis-account-v1:"`), `crates/wallet-ffi/src/lightning.rs` (`LnNode::sign_account_request`), `android/app/src/main/kotlin/com/fortis/wallet/lightning/LightningSession.kt` (`LnNodeApi.signAccountRequest(body: String): String`, `nodeId()` already exists)
- Create: `android/app/src/main/kotlin/com/fortis/wallet/lightning/FortisAccount.kt`
- Test: `crates/fortis-lightning` unit test; `android/app/src/test/kotlin/com/fortis/wallet/lightning/FortisAccountTest.kt`

**Interfaces:**
- Produces: Kotlin `class FortisAccount(private val http: HttpPost, private val edge: String = HOSTED_EDGE, private val token: suspend () -> String)` with `suspend fun balance(node: LnNodeApi): AccountBalance`, `suspend fun invoice(node: LnNodeApi, amountMsat: ULong, description: String): String`, `suspend fun pay(node: LnNodeApi, invoice: String, amountMsat: ULong?): AccountPayResult`, `suspend fun history(node: LnNodeApi, limit: Int = 50): List<AccountEntry>`; `data class AccountBalance(val balanceMsat: ULong, val heldMsat: ULong, val maxMsat: ULong)`; `sealed interface AccountError { CustodyFull(val minChannelSat: Long); Insufficient; Auth; Network }`; `fun buildAccountRequest(nodeId: String, op: String, params: JSONObject, nonce: String, expiresAt: Long): String` (stable key order: node_id, op, params, nonce, expires_at; expires_at = now + 120 s; nonce 32 random url-safe chars).

- [ ] **Step 1: Write the failing tests**
  - Rust `account_request_signature_verifies_against_the_node_id`: a started test node's `sign_account_request(body)` verifies with `message_signing::verify(PREFIX+body, sig, node_id)`.
  - Kotlin `requestBodyIsStableAndSigned` (fake node returns a fixed signature; the HTTP fake receives the exact body and header `X-Fortis-Sig`); `custodyFullMapsToTheMinimumChannel` (409 body → `AccountError.CustodyFull(11_000)`); `paymentRequiredMapsToInsufficient` (402).
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run `cargo test -p fortis-lightning sign_account` and the Kotlin test; expect PASS**
- [ ] **Step 5: Commit** — `git commit -m "lightning wallet: sign account requests; account client"`

---

### Task 7: Routing and display in the app

**Files:**
- Create: `android/app/src/main/kotlin/com/fortis/wallet/lightning/AccountRouting.kt` (pure functions)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/lightning/LightningSession.kt` (hold a `FortisAccount`, expose `accountBalance: StateFlow<AccountBalance?>`, refreshed with balances), `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/LightningWallet.kt` (two balance lines; receive and send through the routes; history merges account entries), `android/app/src/main/res/values*/strings.xml` (9 files)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/AccountRoutingTest.kt`

**Interfaces:**
- Consumes: Task 6.
- Produces: `enum class ReceiveRoute { OwnInvoice, JitChannel, Account }`; `fun receiveRoute(amountMsat: ULong, inboundRoomMsat: ULong, jitMinMsat: ULong): ReceiveRoute` (`OwnInvoice` if inbound ≥ amount; else `JitChannel` if amount ≥ jitMin; else `Account`); `enum class PayRoute { Channel, Account, Insufficient }`; `fun payRoute(amountMsat: ULong, feeCapMsat: ULong, sendableMsat: ULong, accountFreeMsat: ULong): PayRoute` (channel when sendable ≥ amount + feeCap; else account when free ≥ amount + feeCap; else insufficient).
  - Strings (EN; translate for the 8 locales): `ln_balance_in_channel` "In your channel: %1$s"; `ln_balance_held_by_fortis` "Held by Fortis: %1$s"; `ln_account_explainer` "Small amounts are held by Fortis until they're worth a channel of your own (over 50,000 sat)."; `ln_account_custody_full` "Fortis can't hold more small balances right now — receive at least %1$s sat to open your own channel."; `ln_history_converted` "Moved into your channel".

- [ ] **Step 1: Write the failing tests** — `receiveRoute`: `(100_000 msat, 0, 51_000_000) == Account`; `(60_000_000, 0, 51_000_000) == JitChannel`; `(100_000, 500_000, 51_000_000) == OwnInvoice`. `payRoute`: `(1_000, 50_000, 0, 200_000) == Account`; `(1_000, 50_000, 100_000, 0) == Channel`; `(1_000, 50_000, 0, 10_000) == Insufficient`.
- [ ] **Step 2: Run; expect FAIL**
- [ ] **Step 3: Implement the routing and the UI**; the Receive tab uses `receiveRoute` (Account → `FortisAccount.invoice`; `CustodyFull` → `ln_account_custody_full`); the Send confirm uses `payRoute` (Account → `FortisAccount.pay`, still behind the same PIN/biometric gate).
- [ ] **Step 4: Run the full unit suite; expect PASS; `:app:assembleDebug` builds**
- [ ] **Step 5: Commit** — `git commit -m "lightning wallet: Fortis account routing and display"`

---

### Task 8: Real-stack test and docs

**Files:**
- Create: `crates/fortis-lightning/tests/account.rs`
- Modify: `crates/fortis-lightning/tests/common/lsp.rs` (expose the LSP admin URL), `deploy/lightning.md` (§3 config keys, §6 back up `accounts.sqlite`, §10 canary items), `deploy/linux-home/lsp.toml.example` (`account_max_sat = 50000`, `custody_max_sat = 0` with the legal note), `crates/fortis-lsp/README.md`

**Interfaces:**
- Consumes: everything above (the test calls the LSP admin API directly with requests signed by the client node).

- [ ] **Step 1: Write the test** `small_amounts_live_in_the_account_and_convert_over_the_cap` (LSP with `custody_max_sat = 1_000_000`, `account_max_sat = 50_000`; client node without a channel; a `Payer`):
  1. account invoice for 100 sat → payer pays → balance 100_000 msat, `fortis_lsp_custody_sat == 100`.
  2. account pays a 30 sat payer invoice → balance ≈ 70 sat minus actual fee; payer received exactly 30 sat.
  3. a second client's account invoice paid from the first account → internal, both balances right, fee 0.
  4. credits up to 55,000 sat → within 2 conversion checks a channel opens to the client with push ≈ 54,000 sat; after mining it is ready; account 0; client `lightning_sat ≈ 54_000`; history has `Converted` with fee 1,000 sat.
  5. `custody_max_sat` reached → account invoice refused with 409.
- [ ] **Step 2: Run** `FORTIS_BTC_BITCOIND=… FORTIS_VSS_SERVER=… cargo test -p fortis-lightning --test account -- --ignored --test-threads=1`; expect PASS
- [ ] **Step 3: Update the docs listed above**
- [ ] **Step 4: Commit** — `git commit -m "fortis account: real-stack test and docs"`
