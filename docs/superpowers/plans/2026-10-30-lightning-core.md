# Lightning core (phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Android BTC wallets can send and receive Lightning payments through a fortis-run LSP, with encrypted VSS backup, under one BTC total.

**Architecture:** A new `fortis-lightning` crate wraps ldk-node (keys derived from the wallet seed by BIP-85) and is exposed to Kotlin through a feature-gated `wallet-ffi` module. A new `fortis-lsp` binary (ldk-node, LSPS2 service) and LDK's `vss-server` run on the home host; the edge gains `/ln/info` and a `/ln/vss/*` proxy. Android adds a `lightning` package and edits Settings, Home, Receive, Send and History.

**Tech Stack:** Rust (ldk-node 0.7 line / LDK v0.2, bitcoin 0.32), UniFFI via Gobley, Kotlin + Compose, vss-server + PostgreSQL, systemd.

**Spec:** `docs/superpowers/specs/2026-10-30-lightning-design.md`

## Global Constraints

- **The owner makes all git commits.** Never run `git commit`/`git add -A`/push. Where a task ends, list the changed files and stop; the owner commits.
- Phase 1 only: no splicing, no LSPS5/UnifiedPush, no web/wasm Lightning, no XBT/watch-only/BitBox Lightning, no fortis Lightning address, no Tor.
- Android only; `wallet-wasm` must still build with no Lightning code (`cargo build -p wallet-wasm --target wasm32-unknown-unknown`).
- Mainnet software BTC wallets only; Lightning is **off by default**, per wallet, *Settings → Lightning (beta)*.
- ldk-node 0.7 line (LDK v0.2). No ldk-node/LDK types cross the `wallet-ffi` boundary.
- Node seed = BIP-85 HEX application `128169`, 64 bytes, index 0 (path `m/83696968'/128169'/64'/0'`; used directly as ldk-node's 64-byte entropy seed, no expansion), from the BIP-39 seed; Kotlin never sees node keys.
- Lightning **refuses to pay** while VSS is unreachable; on-chain keeps working.
- LSP defaults: `open_fee_ppm` 10 000 (1%); `open_fee_min_sat` ≈ 2× next-block fee for an open; `min_payment_size` 10 000 sat; `channel_min_sat` 100 000; `channel_max_sat` 500 000; `channel_size_factor` 2×; starting liquidity 0.05 BTC. Zero-conf JIT channels ("client trusts LSP").
- Receive below the LSP minimum is on-chain only. One hardcoded LSP peer, found via `GET /ln/info`.
- LSP runs on the home host; port 9735/TCP must be reachable (cloudflared cannot carry it).
- Real-node tests need `FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind`. Gradle: run the wrapper jar with temurin-21 (`gradlew` is broken on this machine). Only install the `.dev`/`.dev2` debug apps on the Pixel, never the store app.

## Review Focus

1. **Relock or app backgrounding while a payment is in flight** — the payment must finish or show pending after restart; never silently lost or shown failed. (Task 6)
2. **Restart after a crash mid-write** — a VSS version conflict from our own previous run must not be reported as `ActiveOnAnotherDevice`. (Task 10)
3. **Paying your own invoice** — a clear error, not a hang. (Task 6)
4. **Starting with no network** — `Starting` must not block the UI or the on-chain wallet; it ends in `Error(LspUnreachable)` and retries on connectivity. (Tasks 4, 12)
5. **LSP closes the channel (or restarts under a new node id) while the app is closed** — on next start the balance moves to *transit/on-chain* ("Moving…") and is swept; the stale `/ln/info` is refetched. (Tasks 7, 12)

**Execution order:** 1, 2, 3, 4, **8**, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18 — Tasks 5–7 run their regtest suites against the `fortis-lsp` binary from Task 8, so build it before them. Task 1's findings can change Tasks 10 and 18; stop and tell the owner if either finding is negative.

## File Structure

| path | responsibility |
|---|---|
| `crates/wallet-core/src/keys.rs` (modify) | pure `bip85_hex` |
| `crates/fortis-lightning/` (new) | `parse.rs`, `lnurl.rs`, `error.rs`, `node.rs` (build/start/balances/events), `receive.rs`, `pay.rs`, `channel.rs` (open/close/transit), `lib.rs`; `tests/common/mod.rs` regtest harness |
| `crates/fortis-lsp/` (new) | `main.rs`, `config.rs`, `metrics.rs` |
| `crates/fortis-edge/src/ln.rs` (new), `main.rs` (modify) | `/ln/info`, `/ln/vss/*` |
| `crates/wallet-ffi/src/lightning.rs` (new), `Cargo.toml`, `lib.rs` | feature `lightning` bindings |
| `android/app/src/main/kotlin/com/fortis/wallet/lightning/` (new) | `LightningSession.kt`, `LnState.kt`, `LnInfoApi.kt` |
| `android/.../ui/screens/` (modify), `data/Store.kt`, `res/values/strings.xml`, `AndroidManifest.xml` | UI, prefs, intent filters |
| `deploy/lightning.md`, `deploy/linux-home/*.service`, `install.sh` (new/modify) | operations |

---

### Task 1: Feasibility spike and crate skeleton

**Files:**
- Create: `crates/fortis-lightning/{Cargo.toml,src/lib.rs,tests/common/mod.rs,tests/spike.rs}`
- Modify: `Cargo.toml` (workspace deps), the spec (record findings)

**Interfaces:**
- Produces: `tests/common/mod.rs` with `pub struct Regtest { pub rpc_url: String, pub esplora_url: String }`, `Regtest::start() -> Regtest`, `Regtest::mine(&self, n: u32)`, `Regtest::fund(&self, address: &str, sat: u64)`; an Esplora-compatible endpoint (use `electrs`/`esplora` if available, otherwise ldk-node's bitcoind-RPC chain source for tests only — decide here and document why). Copy helper style from `crates/fortis-edge/tests/common/mod.rs`.

- [ ] **Step 1:** Add `ldk-node` (0.7 line) and a pinned `vss-client` to `crates/fortis-lightning/Cargo.toml`; `cargo build -p fortis-lightning` succeeds on Linux.
- [ ] **Step 2:** `cargo check -p fortis-lightning --target aarch64-linux-android` using the NDK from `~/Android/Sdk`; Expected: succeeds. If a C dependency fails, record the fix (feature flag / env) in the crate README.
- [ ] **Step 3:** Write `tests/spike.rs::force_closed_channel_funds_recoverable_from_seed_only` — two nodes, open a channel, force-close from the peer, drop all local node state, rebuild from the same seed with an empty data dir, assert the output is swept to an on-chain address after the timelock (mine blocks).
- [ ] **Step 4:** Write `tests/spike.rs::vss_conflict_is_observable` — two nodes built from one seed against a local `vss-server`; assert what the second node's start/pay returns when the first has written. Record whether ldk-node surfaces a distinct error.
- [ ] **Step 5:** Run both; Expected: pass, or fail with the finding. Write results into the spec's *Lost VSS data* paragraph and a new *Findings (Task 1)* section: (a) sweep works or recovery is owner-assisted; (b) whether takeover can rely on ldk-node's VSS error or needs a separate `device-lock` key written with `vss-client` (Task 10 follows the finding).
- [ ] **Step 6:** Hand off for owner commit: files above.

### Task 2: Payment parser

**Files:**
- Create: `crates/fortis-lightning/src/{parse.rs,error.rs}`; Test: unit tests in `parse.rs`

**Interfaces:**
- Produces:
  - `pub enum PaymentTarget { Bolt11 { invoice: String, amount_msat: Option<u64>, description: String, expiry_secs: u64 }, Bolt12Offer { offer: String, amount_msat: Option<u64>, description: String }, LnurlPay { url: String }, LightningAddress { user: String, domain: String }, OnChain { address: String, amount_sat: Option<u64> }, Unified { address: String, amount_sat: Option<u64>, lightning: Box<PaymentTarget> } }`
  - `pub enum ParseError { Empty, Unrecognised, WrongNetwork, Expired, Invalid(String) }`
  - `pub fn parse(text: &str, network: bitcoin::Network) -> Result<PaymentTarget, ParseError>`
  - `pub enum LnError { BackupUnavailable, ActiveOnAnotherDevice, LspUnreachable, InsufficientLightning, NoRoute, InvoiceExpired, WrongNetwork, AmountOutOfRange { min_msat: u64, max_msat: u64 }, Other(String) }` (`thiserror`, `Display` = the English message key used by Kotlin).

- [ ] **Step 1: Tests** (names → assertions): `bolt11_plain` (a real mainnet BOLT11 test vector from BOLT 11 → `Bolt11` with its amount/description); `bolt11_uppercase_and_prefix` (`LIGHTNING:LNBC…` equals the lowercase parse); `bolt11_no_amount_is_none`; `wrong_network_rejected` (testnet invoice, `Network::Bitcoin` → `WrongNetwork`); `expired_rejected`; `bolt12_offer`; `lnurl_bech32_and_lnurlp_scheme` → `LnurlPay`; `lightning_address` (`name@example.com`; trailing newline/whitespace tolerated); `bip21_onchain_only` (amount in BTC → sat, exact for 8 decimals); `bip21_with_lightning_param` → `Unified` whose `lightning` is the parsed invoice; `bip21_uppercase_scheme`; `plain_address`; `garbage_and_empty` → `Unrecognised`/`Empty`; `subsat_amount_msat_kept` (amount `1000msat`-class invoice keeps exact `amount_msat`).
- [ ] **Step 2:** Run `cargo test -p fortis-lightning parse::` — Expected: FAIL (not defined).
- [ ] **Step 3:** Implement `parse` over `lightning-invoice`, `lightning::offers`, `bitcoin::Address`, and `bitcoin::bip21`-style manual query parsing; one normalisation pass first (trim, strip `lightning:`, lower-case for bech32 only).
- [ ] **Step 4:** Run the same command — Expected: PASS. Also `cargo clippy -p fortis-lightning -- -D warnings`.
- [ ] **Step 5:** Hand off for owner commit.

### Task 3: LNURL-pay resolution

**Files:** Create `crates/fortis-lightning/src/lnurl.rs`; Test: unit tests with a fake HTTP.

**Interfaces:**
- Consumes: `PaymentTarget` (Task 2).
- Produces: `pub trait HttpGet { fn get_json(&self, url: &str) -> Result<serde_json::Value, LnError>; }`; `pub struct LnurlPayParams { pub callback: String, pub min_msat: u64, pub max_msat: u64, pub description: String }`; `pub fn resolve(target: &PaymentTarget, http: &dyn HttpGet) -> Result<LnurlPayParams, LnError>`; `pub fn request_invoice(params: &LnurlPayParams, amount_msat: u64, http: &dyn HttpGet) -> Result<String /*bolt11*/, LnError>`.

- [ ] **Step 1: Tests:** `address_resolves_to_well_known_url` (asserts `https://example.com/.well-known/lnurlp/name` requested); `params_parsed` (min/max/`metadata` text description); `amount_below_min_errors_AmountOutOfRange`; `callback_invoice_amount_must_match` (returned invoice with a different amount → `Other`); `http_only_rejected` (non-https non-onion → error).
- [ ] **Step 2:** Run `cargo test -p fortis-lightning lnurl::` — Expected: FAIL.
- [ ] **Step 3:** Implement the two functions; the production `HttpGet` (blocking, rustls, 10 s timeout) lives in `lnurl.rs` behind the trait.
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 4: BIP-85 node seed and `Node` start/stop

**Files:**
- Modify: `crates/wallet-core/src/keys.rs` (+ tests)
- Create: `crates/fortis-lightning/src/node.rs`; Test: `crates/fortis-lightning/tests/node.rs`

**Interfaces:**
- Produces:
  - `wallet_core::keys::bip85_hex(bip39_seed: &[u8; 64], network: Network, num_bytes: u8, index: u32) -> Zeroizing<Vec<u8>>` (path `m/83696968'/128169'/{num_bytes}'/{index}'`, HMAC-SHA512 key `"bip-entropy-from-k"`, first `num_bytes`).
  - `pub struct LspInfo { pub node_id: String, pub host: String, pub port: u16, pub token: Option<String> }`; `pub struct VssConfig { pub url: String, pub auth_header: String }`; `pub struct NodeConfig { pub bip39_seed: Vec<u8> /*64*/, pub network: bitcoin::Network, pub data_dir: PathBuf, pub esplora_url: String, pub lsp: LspInfo, pub vss: VssConfig }`
  - `pub struct Balances { pub lightning_sat: u64, pub onchain_transit_sat: u64, pub inbound_sat: u64 }`
  - `pub enum LnEvent { PaymentReceived { id: String, amount_msat: u64 }, PaymentSent { id: String, fee_msat: u64 }, PaymentFailed { id: String, reason: String }, ChannelReady, ChannelClosed { reason: String } }`
  - `impl Node { pub fn new(cfg: NodeConfig) -> Result<Node, LnError>; pub fn start(&self) -> Result<(), LnError>; pub fn stop(&self); pub fn balances(&self) -> Balances; pub fn next_event(&self, timeout: Duration) -> Option<LnEvent>; pub fn node_id(&self) -> String }`

- [ ] **Step 1: Tests:** in `keys.rs`, `bip85_hex_matches_bip_vector` (take the BIP-85 *HEX* published test vector — master key, path and output — verbatim from the BIP text) and `bip85_hex_64_bytes_index_0_is_deterministic_and_distinct_from_index_1`. In `tests/node.rs`: `starts_connects_to_lsp_and_stops` (regtest + a stock ldk-node as stand-in LSP: assert `node_id()` stable across restarts from the same seed and different with another seed); `start_without_network_returns_lsp_unreachable_within_15s` (Review Focus 4); `vss_down_start_returns_backup_unavailable`.
- [ ] **Step 2:** Run `cargo test -p wallet-core bip85` and `cargo test -p fortis-lightning --test node` — Expected: FAIL.
- [ ] **Step 3:** Implement `bip85_hex` in `keys.rs` (reuse the existing BIP-32 helper used by `psbt.rs::derive_bip32`). Implement `Node` over `ldk_node::Builder` with `build_with_vss_store_and_fixed_headers` (store id = HMAC-SHA256 of the node seed with tag `fortis/ln/vss/v1`, hex); persist the node's data under `data_dir`; the LSP is added as a persisted peer; `start` maps every ldk-node failure to an `LnError` and never blocks longer than 15 s.
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 5: Receive (JIT quote and invoice)

**Files:** Create `crates/fortis-lightning/src/receive.rs`; Test: `crates/fortis-lightning/tests/receive.rs`. Depends on Task 8 (`fortis-lsp`) for the test LSP.

**Interfaces:**
- Consumes: `Node`, `LnError` (Task 2, 4).
- Produces: `pub struct ReceiveQuote { pub invoice: String, pub amount_msat: u64, pub fee_msat: u64, pub needs_channel: bool }`; `impl Node { pub fn receive(&self, amount_msat: u64, description: &str) -> Result<ReceiveQuote, LnError> }`. Amount below the LSP minimum or above its maximum → `AmountOutOfRange`.

- [ ] **Step 1: Tests:** `first_receive_quotes_fee_and_opens_jit_channel` (50 000 sat with 1% fee / 10-sat-floor config → `fee_msat == 500_000`, `needs_channel`; after a second client pays the invoice: `ChannelReady`, then `PaymentReceived`, `balances().lightning_sat == 49_500`); `second_receive_within_capacity_has_zero_fee`; `below_min_is_out_of_range`; `fee_floor_applies`.
- [ ] **Step 2:** Run `cargo test -p fortis-lightning --test receive` — Expected: FAIL.
- [ ] **Step 3:** Implement with ldk-node's liquidity (LSPS2 client) API: `receive_via_jit_channel` when inbound capacity is short, otherwise a normal invoice.
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 6: Pay and payment history

**Files:** Create `crates/fortis-lightning/src/pay.rs`; Test: `crates/fortis-lightning/tests/pay.rs`.

**Interfaces:**
- Consumes: `PaymentTarget`, `LnurlPayParams`, `Node`.
- Produces: `pub struct Payment { pub id: String, pub direction: Direction /*In|Out*/, pub amount_msat: u64, pub fee_msat: u64, pub status: Status /*Pending|Succeeded|Failed*/, pub description: String, pub timestamp: u64 }`; `impl Node { pub fn pay(&self, target: &PaymentTarget, amount_msat: Option<u64>) -> Result<String /*payment id*/, LnError>; pub fn payments(&self) -> Vec<Payment>; pub fn estimate_fee_msat(&self, target: &PaymentTarget, amount_msat: u64) -> u64 }`. `pay` returns `InsufficientLightning`, `InvoiceExpired`, `NoRoute`, or `BackupUnavailable` (checked before the HTLC is sent).

- [ ] **Step 1: Tests:** `pays_invoice_via_lsp_to_second_client` (both clients, amounts and fee asserted from `Payment`); `no_amount_invoice_requires_amount_param`; `insufficient_lightning_balance_error`; `expired_invoice_error`; `pay_with_vss_down_returns_backup_unavailable_and_sends_nothing` (assert peer received nothing); `paying_own_invoice_errors_immediately` (Review Focus 3); `payment_in_flight_survives_stop_and_restart` (hold the recipient's HTLC, `stop()` the payer, restart from the same data dir, assert the payment is still `Pending` then `Succeeded` once released; never `Failed`) (Review Focus 1).
- [ ] **Step 2:** Run `cargo test -p fortis-lightning --test pay` — Expected: FAIL.
- [ ] **Step 3:** Implement over ldk-node `bolt11_payment().send`/`send_using_amount` and `bolt12_payment()`; LNURL targets go through Task 3 first. `stop()` must wait up to 10 s for in-flight HTLC state to persist.
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 7: Move to Lightning (channel open/close) and transit sweep

**Files:** Create `crates/fortis-lightning/src/channel.rs`; Test: `crates/fortis-lightning/tests/channel.rs`.

**Interfaces:**
- Produces: `impl Node { pub fn transit_address(&self) -> String; pub fn open_channel(&self, amount_sat: u64) -> Result<(), LnError>; pub fn close_channel(&self) -> Result<(), LnError>; pub fn sweep_transit(&self, to_address: &str) -> Result<Option<String /*txid*/>, LnError>; pub fn channel_state(&self) -> ChannelState /*None|Opening|Ready|Closing*/ }`. `open_channel` requires `amount_sat >= channel_min` (value from `GET /ln/info`, passed in `LspInfo`-adjacent config) and enough transit balance.

- [ ] **Step 1: Tests:** `fund_transit_then_open_channel_becomes_ready_after_one_conf`; `open_below_channel_min_errors`; `cooperative_close_returns_funds_to_transit_then_sweeps_to_address`; `lsp_initiated_close_while_stopped_shows_transit_balance_on_next_start` (Review Focus 5); `sweep_with_empty_transit_returns_none`.
- [ ] **Step 2:** Run `cargo test -p fortis-lightning --test channel` — Expected: FAIL.
- [ ] **Step 3:** Implement over ldk-node `open_channel` / `close_channel` and `onchain_payment().send_all_to_address` (reserving anchor reserve only while a channel is open).
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 8: `fortis-lsp` service

**Files:** Create `crates/fortis-lsp/{Cargo.toml,src/main.rs,src/config.rs,src/metrics.rs}`; Test: unit tests in `config.rs`, plus use it as the harness LSP in Tasks 5–7 tests.

**Interfaces:**
- Produces: `pub struct LspConfig { listen: SocketAddr, bitcoind_rpc: String, rpc_user: String, rpc_password_file: PathBuf, data_dir: PathBuf, open_fee_ppm: u32, open_fee_min_sat: u64, min_payment_size_msat: u64, max_payment_size_msat: u64, channel_size_factor: f64, channel_min_sat: u64, channel_max_sat: u64, token: Option<String>, low_liquidity_sat: u64, metrics_listen: SocketAddr }` loaded from TOML (`--config /etc/fortis/lsp.toml`) with the Global Constraints defaults; `pub fn from_toml(&str) -> Result<LspConfig>`. Binary prints node id on start. Metrics at `GET /metrics` on localhost: `fortis_lsp_onchain_sat`, `fortis_lsp_channels`, `fortis_lsp_inbound_sat`, `fortis_lsp_outbound_sat`, `fortis_lsp_failed_forwards_total`, `fortis_lsp_pending_htlcs`.

- [ ] **Step 1: Tests:** `defaults_match_spec` (10 000 ppm, 100 000, 500 000, 2.0, 10 000 sat in msat); `rejects_min_greater_than_max`; `rejects_channel_min_greater_than_channel_max`.
- [ ] **Step 2:** Run `cargo test -p fortis-lsp` — Expected: FAIL.
- [ ] **Step 3:** Implement config + main (ldk-node builder with LSPS2 service config from the fields above, bitcoind RPC chain source, graceful shutdown on SIGTERM, a warning log when on-chain < `low_liquidity_sat`).
- [ ] **Step 4:** Rerun — Expected: PASS; `cargo build -p fortis-lsp`; then run Tasks 5–7's suites against it.
- [ ] **Step 5:** Hand off for owner commit.

### Task 9: Edge routes `/ln/info` and `/ln/vss/*`

**Files:** Create `crates/fortis-edge/src/ln.rs`; Modify `crates/fortis-edge/src/main.rs` (route dispatch near the `/swap` arm, new `--ln-info-file` and `--vss-upstream` flags); Test: `crates/fortis-edge/tests/ln_routes.rs`.

**Interfaces:**
- Produces: `GET /ln/info` → `{ "node_id", "host", "port", "lsps2_token", "fee": { "ppm", "min_sat" }, "min_payment_sat", "max_payment_sat", "channel_min_sat", "channel_max_sat" }` read from a JSON file given by `--ln-info-file` (404 when unset); `ANY /ln/vss/*` → reverse proxy to `--vss-upstream`, path prefix stripped. Both require a valid install token and use the same limiter as `swap_route`; bodies capped at 1 MiB.

- [ ] **Step 1: Tests:** `info_requires_token` (401 without), `info_404_when_unconfigured`, `info_returns_file_contents`, `vss_proxy_forwards_body_and_status`, `vss_proxy_rate_limited_like_swap`, `vss_proxy_rejects_oversize`.
- [ ] **Step 2:** Run `cargo test -p fortis-edge --test ln_routes` — Expected: FAIL.
- [ ] **Step 3:** Implement `ln::route(req, method, path, upstream)` and the `(_, p) if p == "/ln/info" || p.starts_with("/ln/vss/")` arm modelled on `swap_route`.
- [ ] **Step 4:** Rerun — Expected: PASS; `cargo test -p fortis-edge` still passes.
- [ ] **Step 5:** Hand off for owner commit.

### Task 10: Backup, restore and one-device guard (regtest integration)

**Files:** Create `crates/fortis-lightning/src/device.rs` (only if Task 1 found ldk-node does not expose a usable VSS conflict); Test: `crates/fortis-lightning/tests/restore.rs`. Uses `vss-server` + edge `/ln/vss` + `fortis-lsp`.

**Interfaces:**
- Produces: `impl Node { pub fn takeover(&self) -> Result<(), LnError> }`; `start`/`pay` return `LnError::ActiveOnAnotherDevice` when another instance of the same seed has written since this instance last did.

- [ ] **Step 1: Tests:** `restore_from_seed_via_vss_recovers_channel_and_balance` (fund + receive, wipe the data dir, rebuild from the seed, assert same `node_id`, `lightning_sat`, channel `Ready`); `second_device_start_is_rejected_then_takeover_works` (device B gets `ActiveOnAnotherDevice`, `takeover()` succeeds, device A's next `pay` returns `ActiveOnAnotherDevice`); `own_crash_restart_is_not_another_device` (kill the node process mid-payment, restart with the same data dir → starts normally, no `ActiveOnAnotherDevice`) (Review Focus 2); `vss_down_midway_stops_payments_not_receives_of_onchain`.
- [ ] **Step 2:** Run `cargo test -p fortis-lightning --test restore` — Expected: FAIL.
- [ ] **Step 3:** Implement `takeover` as determined by Task 1 finding (b): either surface ldk-node's VSS version error, or maintain a `device-lock` record `{ device_id }` through `vss-client` with a conditional version, checked on start and before every `pay`, rewritten by `takeover`. The device id is random per install and stored beside the node data dir.
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 11: `wallet-ffi` Lightning module

**Files:** Create `crates/wallet-ffi/src/lightning.rs`; Modify `crates/wallet-ffi/Cargo.toml` (feature `lightning`, optional dep on `fortis-lightning`), `src/lib.rs` (`#[cfg(feature = "lightning")] mod lightning;`), `android/app/build.gradle.kts` (enable the feature in the `cargo { }` block); Test: `crates/wallet-ffi/tests/lightning_ffi.rs`, `android/app/src/test/kotlin/com/fortis/wallet/lightning/LnBindingTest.kt`.

**Interfaces:**
- Produces (UniFFI): `LnNode` object with `new(cfg: LnNodeConfig) -> Result<Arc<LnNode>, LnFfiError>`, `start()`, `stop()`, `balances() -> LnBalances`, `receive(amount_msat: u64, description: String) -> LnReceiveQuote`, `pay(target: LnTarget, amount_msat: Option<u64>) -> String`, `payments() -> Vec<LnPayment>`, `next_event(timeout_ms: u64) -> Option<LnEventRecord>`, `transit_address() -> String`, `open_channel(amount_sat: u64)`, `close_channel()`, `sweep_transit(to_address: String) -> Option<String>`, `channel_state() -> LnChannelState`, `takeover()`; free functions `ln_parse(text: String, network: String) -> Result<LnTarget, LnFfiError>`, `ln_resolve_lnurl(target: LnTarget) -> Result<LnurlParams, LnFfiError>`. Records/enums mirror the Task 2–7 types one-to-one; `LnFfiError` mirrors `LnError` variant names.

- [ ] **Step 1: Tests:** Rust `parse_roundtrip_through_ffi_types` and `error_variants_map_one_to_one`; Kotlin `LnBindingTest.parseBolt11AndOnchain` (calls `lnParse` with a fixed invoice and a bech32 address, asserts the variants), `builds_without_lightning_feature_for_wasm` is the cargo command in Step 4.
- [ ] **Step 2:** Run `cargo test -p wallet-ffi --features lightning` — Expected: FAIL.
- [ ] **Step 3:** Implement the module as thin conversions; no logic.
- [ ] **Step 4:** Verify: `cargo test -p wallet-ffi --features lightning`; `cargo build -p wallet-wasm --target wasm32-unknown-unknown` (no Lightning code); Android: build the debug variant with temurin-21 and run `LnBindingTest`. Expected: all pass.
- [ ] **Step 5:** Hand off for owner commit.

### Task 12: `LightningSession` and Settings → Lightning (beta)

**Files:** Create `android/.../lightning/{LightningSession.kt,LnState.kt,LnInfoApi.kt}`; Modify `data/Store.kt`, `ui/screens/Screens.kt` (Settings tab), `res/values/strings.xml`; Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/LightningSessionTest.kt`.

**Interfaces:**
- Produces: `sealed interface LnState { Off; Starting; Ready(balances: LnBalances, channel: LnChannelState); Error(error: LnFfiError) }`; `class LightningSession(scope, store, nodeFactory: (LnNodeConfig) -> LnNodeApi, info: LnInfoApi)` with `val state: StateFlow<LnState>`, `val events: SharedFlow<LnEventRecord>`, `fun enable(walletId)`, `fun disable(walletId)`, `fun onUnlocked(walletId, bip39Seed: ByteArray)`, `fun onRelocked()`, `fun takeover()`; `Store.lightningEnabled(walletId): Boolean` / `setLightningEnabled(walletId, on)` (default false); `LnInfoApi.fetch(): LnInfo` (`GET /ln/info`, re-fetched on every start).

- [ ] **Step 1: Tests (fake `LnNodeApi`):** `off_by_default_and_never_starts`; `unlock_with_enabled_starts_and_reaches_ready`; `relock_stops_node_and_waits_for_inflight` (Review Focus 1); `start_failure_with_no_network_is_error_not_blocking` and `retries_on_connectivity` (Review Focus 4); `stale_ln_info_is_refetched_on_each_start` and `channel_closed_by_lsp_while_stopped_shows_transit_balance` (Review Focus 5); `only_enabled_for_software_mainnet_btc_wallet` (watch-only, BitBox, XBT, non-mainnet → `Off`, toggle hidden).
- [ ] **Step 2:** Run `LightningSessionTest` via Gradle (temurin-21 wrapper jar) — Expected: FAIL.
- [ ] **Step 3:** Implement. `onUnlocked` is called from `WalletSession` after biometric unlock with the seed the app already holds; the node's data dir is `filesDir/ln/<walletId>`; Esplora URL = `OwnServer` BTC URL if on, else the edge BTC URL. The Settings card shows the toggle, status line (LSP connected, backup OK / error text), and *Close channel and move to on-chain* (confirm dialog) using `close_channel` then `sweep_transit` to the next receive address.
- [ ] **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 13: Home total and balance split

**Files:** Modify `ui/screens/Screens.kt` (`WalletTab` hero), `WalletViewModel.kt`, `ui/Format.kt`; Test: extend `android/app/src/test/kotlin/com/fortis/wallet/HomeBalancesTest.kt`.

**Interfaces:**
- Consumes: `LightningSession.state`. Produces: `fun homeBalance(onchainSat: Long, ln: LnState): HomeBalance` with `HomeBalance(totalSat: Long, onchainSat: Long, lightningSat: Long, moving: Boolean)`; hero shows the total, with a line *On-chain · Lightning* (monospace, `textDim`, ≥ 13 sp per DESIGN.md) only when Lightning is enabled; `moving` shows *Moving…*.

- [ ] **Step 1: Tests:** `total_is_sum`; `lightning_off_equals_onchain_only`; `transit_counts_as_lightning_and_sets_moving`; `error_state_keeps_last_known_lightning_balance`.
- [ ] **Step 2:** Run `HomeBalancesTest` — Expected: FAIL. **Step 3:** Implement. **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 14: Receive screen

**Files:** Modify `ui/screens/Screens.kt` (`ReceiveTab`), `ui/Qr.kt`, `strings.xml`; Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/ReceiveQrTest.kt`.

**Interfaces:**
- Produces: `fun buildReceiveQr(address: String, amountSat: Long?, quote: LnReceiveQuote?, minLnSat: Long): ReceiveQr` where `ReceiveQr(uri: String, lightning: Boolean, feeSat: Long?, youGetSat: Long?, hint: ReceiveHint?)`; `uri` is `bitcoin:ADDR?amount=<BTC>&lightning=<invoice>` when Lightning applies, else the plain address URI.

- [ ] **Step 1: Tests:** `amount_with_ln_builds_bip21_with_lightning_param`; `amount_below_ln_min_is_onchain_only_with_hint`; `no_amount_is_onchain_with_add_amount_hint`; `first_receive_shows_fee_and_you_get` (50 000 sat, fee 500 → `youGetSat == 49_500`); `lightning_disabled_is_plain_address`; `sub_sat_msat_amount_rounds_up_for_display_only`.
- [ ] **Step 2:** Run `ReceiveQrTest` — Expected: FAIL. **Step 3:** Implement; the quote is requested off the main thread when the amount changes (debounced 500 ms). **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 15: Send screen (scan, parse, confirm)

**Files:** Modify `ui/screens/Screens.kt` (`SendTab`), `ui/Qr.kt` (`bareAddress` stays for on-chain; new `parseScan`), `MainActivity.kt`, `AndroidManifest.xml` (intent filters for `lightning:` and `bitcoin:`); Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/SendRoutingTest.kt`.

**Interfaces:**
- Produces: `sealed interface SendPlan { OnChain(address, amountSat?); Lightning(target: LnTarget, amountMsat?); NeedsAmount(target, minMsat, maxMsat); NeedsMove(shortfallSat); Invalid(reason) }`; `fun planSend(text: String, onchainSat: Long, ln: LnState, network: String): SendPlan`. Unified targets choose Lightning when `lightningSat` covers the amount, otherwise on-chain. Confirm screen shows amount, description, expiry, estimated fee (`estimate_fee_msat`), then the existing biometric prompt.

- [ ] **Step 1: Tests:** `bolt11_with_balance_plans_lightning`; `uppercase_scanned_qr_works`; `no_amount_invoice_needs_amount`; `lnurl_range_needs_amount_with_bounds`; `unified_prefers_lightning_when_covered_else_onchain`; `lightning_target_with_only_onchain_funds_needs_move`; `lightning_disabled_rejects_invoice_with_explanation`; `onchain_address_unchanged_behaviour`; `wrong_network_and_expired_are_invalid_with_distinct_reasons`; `view_intent_with_lightning_uri_prefills_send` (instrumented-free: test the intent-to-text function `textFromIntent(uri: Uri): String?`).
- [ ] **Step 2:** Run `SendRoutingTest` — Expected: FAIL. **Step 3:** Implement `planSend` over `lnParse`; wire the scanner result and paste box through it; sending uses `LightningSession` and shows *Sending…* until `PaymentSent`/`PaymentFailed`. **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 16: Move to Lightning flow, history and delete-wallet warning

**Files:** Create `ui/screens/MoveToLightning.kt`; Modify `ui/screens/Screens.kt` (history list, delete flow), `data/LocalSends` use, `strings.xml`; Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/MoveToLightningTest.kt`, `HistoryMergeTest.kt`.

**Interfaces:**
- Produces: `fun planMove(amountSat: Long, onchainSat: Long, channelMinSat: Long, feeSat: Long): MovePlan` (`Ok | BelowMin(min) | Insufficient`); flow = on-chain send to `transit_address()` through the existing send path (so `LocalSends` pending-spend protection applies), then `open_channel` once the transit balance is confirmed; `fun mergeHistory(onchain: List<TxRow>, ln: List<LnPayment>): List<HistoryRow>` sorted by time, Lightning rows flagged `lightning = true` with fee and description; delete-wallet dialog gains the Lightning-funds warning text when `lightningSat > 0`; failed sends are shown as failed.

- [ ] **Step 1: Tests:** `below_channel_min_rejected`; `insufficient_after_fee_rejected`; `second_move_opens_second_channel_in_phase1`; `merge_sorts_by_time_and_flags_lightning`; `failed_lightning_send_marked_failed`; `delete_warning_only_when_lightning_funds`.
- [ ] **Step 2:** Run both test classes — Expected: FAIL. **Step 3:** Implement. **Step 4:** Rerun — Expected: PASS. Then on the Pixel (`.dev`/`.dev2` only, against a regtest-or-canary LSP) walk the manual checklist in Task 18.
- [ ] **Step 5:** Hand off for owner commit.

### Task 17: Takeover UI and error copy

**Files:** Modify `ui/screens/Screens.kt` (Settings card + Home banner), `strings.xml`; Test: `android/app/src/test/kotlin/com/fortis/wallet/lightning/LnErrorCopyTest.kt`.

**Interfaces:**
- Produces: `@StringRes fun lnErrorMessage(e: LnFfiError): Int` covering every `LnError` variant; Home banner for `Error(ActiveOnAnotherDevice)` with button *Use Lightning here instead* calling `LightningSession.takeover()`.

- [ ] **Step 1: Tests:** `every_error_variant_has_a_message` (enumerate all variants; fail if one is unmapped); `backup_unavailable_text_says_try_again_shortly`; `active_elsewhere_shows_takeover_action`.
- [ ] **Step 2:** Run `LnErrorCopyTest` — Expected: FAIL. **Step 3:** Implement; new strings added to `values/strings.xml` and queued for the usual translation flow (the 16-untranslated-strings precedent applies; list the keys in the hand-off). **Step 4:** Rerun — Expected: PASS.
- [ ] **Step 5:** Hand off for owner commit.

### Task 18: Operations, runbook, site/privacy drafts, mainnet canary

**Files:** Create `deploy/lightning.md`, `deploy/linux-home/fortis-lsp.service`, `deploy/linux-home/fortis-vss.service`, `deploy/linux-home/lsp.toml.example`, `site/lightning.html` (draft); Modify `deploy/linux-home/install.sh`, `deploy/linux-home/edge.env` (new flags), `site/privacy.html` (draft Lightning section); Test: shell checks below.

**Interfaces:** consumes the edge flags from Task 9 and the config from Task 8.

- [ ] **Step 1:** Write `deploy/lightning.md`: reachability check (`ss -ltn | grep 9735`; test from outside with a Lightning peer connect; IPv4/IPv6 and router port-forward notes; if unreachable, the VPS trigger); funding the LSP with 0.05 BTC; opening 1–2 outbound channels to well-connected nodes; `lsp.toml` fields; PostgreSQL + `vss-server` setup on localhost; `/ln/info` JSON file; nightly off-host backup of the LSP data dir and VSS database; lost-VSS recovery steps (from the Task 1 finding); idle-channel closing; moving to a VPS (stop, copy data dir + config, start, update `/ln/info`).
- [ ] **Step 2:** Add the two systemd units (hardened like `fortis-edge.service`, `Restart=on-failure`) and extend `install.sh`; verify with `systemd-analyze verify deploy/linux-home/fortis-lsp.service deploy/linux-home/fortis-vss.service` — Expected: no errors.
- [ ] **Step 3:** Draft `site/lightning.html` and the privacy section exactly per the spec's *Rollout* wording (what the LSP sees: amounts and timing of payments routed through it and the node id; not who the user is; VSS stores encrypted blobs). Mark both DRAFT for the owner's review, as the swaps §8 draft was.
- [ ] **Step 4: Manual canary checklist** (record results in `deploy/lightning.md`): LSP on mainnet with `channel_max_sat` 500 000; on `.dev` — enable Lightning, receive 50 000 sat from Phoenix with the fee shown first, pay a real invoice by scan, pay `.dev2`, restore `.dev2` from words, two-device takeover, `lightning:` link from the browser opens Send, LSP restart under the same node id, Review Focus 5 (close the channel from the LSP with the app closed). Expected: all pass before the owner turns the beta on in a release.
- [ ] **Step 5:** Hand off for owner commit.

---

### Task 19: Security and robustness gate (added 2026-10-31, after Task 7)

Runs after Task 18 and gates turning the beta on for anyone but the owner. It is not a polish task: findings here block release.

**Files:** Create `docs/superpowers/specs/2026-10-31-lightning-threat-model.md`; fuzz/property tests under `crates/fortis-lightning/tests/` (and `fuzz/` if cargo-fuzz is used); `deploy/lightning.md` additions; fixes as needed.

- [ ] **Step 1: Threat model.** Write the document: assets (user funds in channels and transit wallet, the recovery phrase and derived node seed, channel state in VSS, LSP hot wallet, payment metadata) and adversaries (malicious or compromised LSP, malicious Esplora/edge/VSS operator, hostile LNURL server, hostile invoice/QR/URI/Lightning address, device thief, a second device on the same seed, network attacker, supply-chain compromise of a dependency, compromise of fortis infrastructure). For each: what they can and cannot do, the control in code (with the test that pins it), and the RESIDUAL risk stated plainly. List every accepted limitation from the ledger (e.g. LSP zero-conf trust, routing-fee income zero on JIT channels, local-only close/attempt records, Esplora dropped-funding behaviour, pending-HTLC funds not recovered by the seed-only sweeper).
- [ ] **Step 2: Adversarial input tests.** Property/fuzz tests (proptest or cargo-fuzz; state which) with a no-panic, no-hang, bounded-memory invariant for: `parse`, the LNURL JSON handling and `check_scheme`/`address_url`, the Esplora and bitcoind scan response handling in `recover.rs`, the close-record/attempt-record/notes file readers (arbitrary bytes), and the `/ln/info` JSON parsing in Kotlin/Rust. Seed the corpora with the vectors already in the tests.
- [ ] **Step 3: Fault injection on the real stack.** `kill -9` the client at: mid-payment, mid-channel-open, mid-close, mid-record-write; disk full; clock jumps forward/back; VSS flapping (pause/resume during every operation class); LSP restarted under a new node id; edge returning garbage/old `/ln/info`; assert after restart: no lost payment, no double payment, no state shown as `None` while a channel exists, no fund-moving action performed while VSS is unavailable, balances reconcile (the Task 7 reconciliation helper).
- [ ] **Step 4: Dependency and supply-chain hygiene.** `cargo audit` and `cargo deny` (install them in user space if absent; record versions and results), review of the new dependency tree for the Lightning crates (ldk-node 0.7.1, vss-client-ng, ureq, url, tiny_http…), exact pins in `Cargo.lock`, a note on the alpha `vss-server` tag and how it is built (`scripts/build-vss-server.sh` verifies the commit hash), reproducibility of the Android build with the new native library. Record every advisory and its disposition.
- [ ] **Step 5: Independent reviews.** (a) Run the `security-review` skill over the whole `lightning` branch; (b) a dedicated adversarial review (most capable model, separate subagent, "assume the author made a mistake in every money path") over: key derivation, VSS gate and device lock, pay/attempt-record, channel ops/sweep guards, recover.rs, the edge routes, the LSP config/bounds, the Android send/receive/move flows; (c) the final whole-branch review. All findings fixed or explicitly accepted by the owner in the threat model.
- [ ] **Step 6: Staged rollout and kill switch.** Document and test the order: regtest → signet/testnet LSP with small caps → mainnet canary with `channel_max_sat` 500 000 and the owner's own wallets → wider beta. Add a remote kill switch: `GET /ln/info` carries `"enabled": bool` (and `"message"`); when false the app stops creating new channels/JIT invoices and shows the message, but NEVER blocks force-close, sweep or paying out (an exit path must always work); test both states in Rust and Kotlin.
- [ ] **Step 7: External audit recommendation.** Write into the threat model and tell the owner plainly that an independent third-party review of the money paths is recommended before the beta is offered to anyone with more than trivial amounts, and that nothing in this process replaces one.
