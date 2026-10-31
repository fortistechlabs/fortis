# fortis Lightning — threat model and security gate (Task 19)

**Status: DRAFT for the owner's review. Written 2026-10-31 against branch `lightning` at `f2be6e7` plus the Task 19 changes on branch `security-perf-fixes` (uncommitted in that worktree).**

This is the document Task 19 step 1 asks for, with the results of steps 2, 4, 6 and 7 and an honest statement of what steps 3 and 5 still need. Every claim names the code that implements the control and the test that pins it; where there is no such test, it says so. "Residual" is what is still true after the control.

> **Release gate.** Section 6 lists the findings that block turning the beta on for anyone but the owner. Section 8 repeats the recommendation that matters most: nothing in this process replaces an independent third-party review of the money paths.

## 1. What is being protected

| Asset | Where it lives | Why it matters |
|---|---|---|
| Recovery phrase / BIP-39 seed | Android, sealed at rest; in memory while unlocked | Everything derives from it |
| Node seed (BIP-85 HEX, 64 bytes, index 0) | derived on unlock, handed to ldk-node | Controls every channel and the transit wallet |
| VSS store id | `HMAC-SHA256(node seed, "fortis/ln/vss/v1")` | The only namespace key to the encrypted channel backup (the VSS server has no per-user auth) |
| Channel state (VSS blobs) | the fortis VSS server | Losing or rolling it back risks the channel balance |
| User funds in channels | ldk-node / LSP peer | The main asset |
| Transit wallet | ldk-node on-chain wallet (same seed) | Holds a "Move to Lightning" deposit until the channel opens |
| Install token | Android, per wallet | Rate-limit identity and access to `/ln/*`, swaps, chain proxy |
| LSP hot wallet and channels | fortis home host (`fortis-lsp`, `keys_seed` mode 0400, SQLite) | The operator's own money and every client's counterparty |
| Payment metadata | LSP (amounts, timing, node id), recipients' servers (LNURL), local files | Privacy |
| Local record files | `fortis-device-id`, close records, attempt records, payment notes | Safety state that is *not* in VSS |

## 2. Adversaries: what they can and cannot do

For each: **can / cannot**, the **control** (code, and the test that pins it), and the **residual** risk.

### 2.1 Malicious or compromised LSP

- **Cannot** take channel funds unilaterally on a normal channel: commitment transactions are signed by both sides; the client can force-close at any time (`StartMode::Exit`, `channel.rs::force_close_channel`, `tests/exit.rs::exit_mode_force_close_and_sweep_recovers_funds`). Its fee on a JIT receive is now capped: see below.
- **Can** (accepted, `client_trusts_lsp: true`, `fortis-lsp/src/config.rs`) double-spend the zero-conf funding transaction of a JIT channel after the client has accepted the payment: the client's received amount is then lost. Bounded by `channel_max_sat` (500 000 sat during the canary) and by the payment bounds.
- **Can** see amounts and timing of payments through it and the client's node id; not who the user is.
- **Can** go offline: the user keeps the exit path.
- **Control (new, this task):** the client refuses an LSP quote above `amount × 10 % + 5 000 sat` (`receive.rs::jit_fee_ceiling_msat`; before this change `receive_via_jit_channel` was called with `None`, which in ldk-node 0.7.1 takes the cheapest offer at any price). Pinned by `receive::tests::jit_fee_ceiling_is_ten_percent_plus_five_thousand_sat` and `fee_too_high_maps_to_its_own_error`. The real quote's ceiling interaction with a live LSP is **not** covered by an integration test.
- **Residual:** zero-conf trust. The cap is a fixed policy, not derived from the fee the LSP advertises: at the smallest first receive (51 000 sat) it still allows about 20 % (10 100 sat) and at 250 000 sat about 12 %. A cap derived from `/ln/info` (`fee.ppm`, `fee.min_sat`, plus a small tolerance) would be much tighter but needs `LspLimits` and the FFI record to carry the fee (about a dozen struct literals in the tests). When the cap refuses a quote the Receive screen silently falls back to an on-chain QR without saying why (`ReceiveTab` swallows the error). The UI does show the fee and the net amount.

### 2.2 Malicious or compromised edge / `/ln/info` source (the LSP's identity is whatever the edge says)

- **Can** point wallets at an attacker's LSP: `node_id`, host, port and token come from `GET /ln/info` and are not pinned in the app, and there is no certificate pinning (`LnInfoApi.kt`, `network_security_config.xml` is HTTPS-only). **Can** switch Lightning off for everyone with `enabled:false` (the kill switch is deliberate; it never blocks exits, see 2.12).
- **Cannot** touch the seed or sign anything.
- **Control:** the document is validated before use — in the edge (`ln.rs::parse_info`) and in the app (`LnInfoApi.kt::parseLnInfo`): key, host charset, port, fee, bounds ordered and now **held to 21 million BTC** so `sat × 1000` cannot wrap (new: `LightningHardeningTest.info_amounts_are_held_to_the_total_supply`, `LnInfoFuzzTest`, and the Rust `ln::fuzz` module, which also pin that the edge refuses anything the wallet would refuse so an operator typo cannot disable every client).
- **Residual (OPEN — decision needed, see 6.2):** no LSP pinning. A compromised edge, tunnel (Cloudflare) or DNS record can redirect new users to an attacker LSP.

### 2.3 Malicious or compromised VSS operator (the same operator runs the edge, VSS and the LSP)

- **Cannot** read channel state (ldk-node encrypts values and obfuscates keys with a seed-derived key) or the seed.
- **Can** delete it (denial) or **roll it back**: serve an older state at restore time. The device-lock record is plain JSON authenticated only by VSS (`device.rs`); `global_version` is not used. A client that restores a stale state and broadcasts it loses the channel balance to a punishing peer, and the punishing peer is operated by the same party.
- **Control:** none in code can detect a rollback; this is the architecture.
- **Residual (OPEN, accepted by design — owner must accept explicitly):** one operator holds backup *and* counterparty. Mitigations that need design work: independent VSS operator, signed/monotonic state counters, or an external watchtower.

### 2.4 Malicious Esplora (the edge's, or the user's "own server")

- **Can** lie about blocks and fee rates to ldk-node (no header/PoW validation in the Esplora client): delay or hide a counterparty's close or a breach; make a funding look confirmed or unconfirmed. **Can** return "nothing" to the seed-only recovery scan (below).
- **Cannot** redirect funds: sweep transactions pay the user's own address and segwit-v0 signatures commit to the amount, so wrong values only produce invalid transactions (`recover.rs::sweep_static_outputs`; property `a_sweep_never_burns_more_than_half_and_never_pays_elsewhere`).
- **Control (new):** the sweep refuses a feerate above 1 000 sat/vB (`invalid_feerate`; it was 10 000) and a fee above half of the value rescued, reported as `sweep: fee_too_high` and not, as before, as "nothing to sweep" when the fee would eat the whole value (`recover.rs`; pinned by `sweep_refuses_a_fee_above_half_the_value`, `sweep_returns_none_when_dust_or_fee_exceeds_value` and the property `a_sweep_decides_the_fee_cap_correctly_on_small_utxos`, mutation-checked: loosening the cap makes the property fail). The adversarial reviewer showed that a ratio alone does not bound the fee on large balances: at 1 000 sat/vB a 111 vB sweep costs 111 000 sat, which is under half only for balances over 222 000 sat, so a hostile fee source can still take up to half. That is the residual.
- **Residual:** the chain source is trusted for liveness and truth; the user's own server only helps if it is honest. The scan treats HTTP 404 on a script as "no UTXOs" — a wrong base URL looks like "nothing to recover" (OPEN, 6.3). The scan also reveals the 2 000 script hashes of the node seed to the Esplora operator.

### 2.5 Hostile LNURL / Lightning-address server

- **Cannot** make the app pay a different amount or hide the description: the invoice must be for exactly the requested amount and commit to `sha256(metadata)` (`lnurl.rs::request_invoice`, `pay.rs::tests::lnurl_target_goes_through_resolve_and_hash_check`).
- **Can** learn the user's IP and that they scanned the address; can serve a hostile `callback`.
- **Control:** https only (http for `.onion`); responses capped at 64 KiB; no redirects; 10 s timeout; **hosts must be public** — loopback, private, link-local, CGNAT, documentation, multicast, IPv6 unique-local/link-local/mapped (and 6to4/NAT64 embedding) and single-label / `.local` / `.internal` names are refused, for the initial URL and the callback, and a name that *resolves* to a private address is refused by `PublicResolver` (this part was added by the earlier review; it also covers the DNS64 case). Pinned by `lnurl::tests::private_and_local_hosts_are_refused` and, for arbitrary inputs, the properties `a_lightning_address_never_makes_the_phone_contact_a_private_host` and `an_lnurl_and_its_callback_never_reach_a_private_host` (mutation-checked: removing the check makes both fail).
- **Residual:** lookups happen as the user types (after a 1 s pause; was 300 ms) and for a `lightning:` link without a tap, so a half-typed address can be sent to an unrelated domain. Mitigated, not eliminated; an explicit "look up" step would remove it.

### 2.6 Hostile invoice / QR / URI / deep link

- **Cannot** crash or hang the parser: input over 16 KiB is refused and a `bitcoin:` URI inside `lightning=` is refused instead of followed (`parse.rs`, pinned by `oversized_input_is_refused_without_parsing` and `a_bitcoin_uri_inside_lightning_param_is_refused_not_followed`; properties `parse_survives_*`, which run arbitrary text, bytes, mutated real vectors and hostile BIP-21 shapes with a per-case time budget).
- **Can** prefill the Send screen via a `bitcoin:`/`lightning:` link from any app or page; the app also switches the selected wallet to the first signing BTC wallet (`WalletViewModel.openSendWithUri`). Sending still needs the confirm sheet and re-authentication.
- **Control:** network and expiry are re-checked at pay time against the node's own network and the clock (`pay.rs::prepare`); `fixed_or_given` refuses an amount that disagrees with the invoice; "pay yourself" is refused.
- **Residual:** the unified-QR on-chain amount and the invoice amount are not compared (a hostile QR can disagree with itself; the confirm screen shows what will be paid).

### 2.7 Device thief / local attacker

- **Control:** seed sealed at rest, auto-lock after 60 s in the background stops the node and wipes the session seed (`WalletViewModel.lock`, `Relock.kt`); `allowBackup="false"` and extraction rules exclude all domains, so the device id cannot be cloned to another phone; every Lightning payment and every on-chain send (including the "Move to Lightning" deposit) needs fresh re-authentication (`reauth.consume()`). Closing a channel asks for a confirmation but **not** re-authentication (`closeLightningAndMoveToOnchain`); the funds go to the wallet's own next receive address, so it cannot steal, but it can be done on an unlocked phone without the second factor (low; listed in 6.3).
- **New, small:** the short-lived seed copy handed to the node factory is wiped as soon as the node exists and on every exit path (`LightningSession.startLocked`, pinned by `LightningHardeningTest.the_seed_copy_*`). This shortens the life of ONE of several copies and should not be read as keeping the seed out of memory: the session keeps its own copy for the whole unlocked period (`unlockedSeed`), the uniffi lowering/lifting buffers are not zeroed, and the JVM heap may hold more.
- **Residual:** a rooted attacker with the phone unlocked can read process memory; payment notes and close records are plaintext in the app's private storage.

### 2.8 A second device on the same seed

- **Cannot** run a second node unnoticed: the device lock in VSS is a conditional write; a node re-checks it before every payment, channel and transit action and every 60 s, and stops itself when it loses it (`device.rs`, tests `acquire_refuses_another_device_and_writes_nothing`, `concurrent_takeover_or_refresh_makes_the_takeover_lose`, `refresh_after_a_takeover_or_release_is_lost`, and `tests/restore.rs`).
- **Can**, after a takeover, still write over the new device's channel state for up to ~60 s because VSS writes are unconditional (documented in `Node::takeover`). The app now asks for confirmation and says the other device must be closed first.
- **Residual:** that window; a lost phone must be dealt with by taking over.

### 2.9 Network attacker

- **Control:** HTTPS only (no cleartext in release); VSS and `/ln/*` over TLS to the edge.
- **Residual:** no certificate pinning (see 2.2).

### 2.10 Supply-chain compromise of a dependency

- **Control (step 4, this task):** `cargo audit` (0.22.2) and `cargo deny` (0.20.2, policy in `deny.toml`: advisories, licences, bans, sources = crates.io only, no git) run clean on the new lockfile; `Cargo.lock` pins exact versions; `vss-server` is pinned by tag *and* commit hash (`scripts/build-vss-server.sh` refuses any other commit and the no-op authorizer build is documented tests-only); the one dependency that was patched, `tiny_http`, is vendored with the change in a marked block (`vendor/tiny_http`, `[patch.crates-io]`).
- **Results:** two vulnerabilities fixed by lockfile bumps — `libz-sys` 1.1.29→1.1.31 (RUSTSEC-2026-0418; build-script environment leak, reached only through RocksDB in the host servers), `zstd-sys` 2.1.0→2.1.2 (RUSTSEC-2026-0410; WebAssembly allocator, not a target we build) — and `tokio` 1.53.2→1.53.3 (RUSTSEC-2026-0386, `AsyncFd` I/O safety; I found no caller of `AsyncFd::new` in ldk-node, the lightning crates, vss-client or reqwest). One accepted warning: `basic-toml` unmaintained (RUSTSEC-2026-0374), build-time only through `uniffi_bindgen → askama_derive`, not in any shipped binary.
- **Residual:** ldk-node 0.7.1, `vss-client-ng` 0.4.1 and the alpha `vss-server` are young and not independently audited; the Android build with the new native library has not been checked for reproducibility.

### 2.11 Compromise of fortis infrastructure

- **Edge VSS proxy:** token required, exact path allowlist, body capped (4 MiB; Caddy also caps at 5 MB), per-token write quota (256 MiB/h), an optional global write quota that is **off by default** (`--vss-global-quota-bytes-per-hour`: with a free token each, a handful of tokens can spend a shared budget and then every real install's writes — the lock refresh, every payment's state — are refused with 429, which is worse than a full disk the operator can watch; use `fortis_edge_vss_put_bytes_total`), and a **new** cap on concurrent VSS requests so slow uploads cannot occupy every worker (`ln.rs`, tests `global_quota_bounds_all_installs_together_and_refunds_the_loser`, `in_flight_requests_are_capped_and_slots_are_freed`). Tokens are free (`POST /register` is open, rate-limited per IP), so per-token limits are a speed bump, not a bound; the table that holds them (10 000 keys) refuses new tokens while full, which a token farm could use to keep new installs out of VSS for an hour (pre-existing design, open).
- **`tiny_http` allocation bug and desync:** an unread request body made the server allocate the whole declared `Content-Length` and abort. The vendored copy no longer allocates it. **It also found a worse problem in the first versions of that patch:** skipping the drain left the unread body on the connection, where the server parsed it as the next request — request smuggling and response mix-up between users behind a proxy that reuses upstream connections (Caddy and cloudflared do). The patch now drains small remainders and, when it cannot, flags the connection and the server closes it after the response (`vendor/tiny_http`, `util/equal_reader.rs`, `request.rs`, `client.rs`; chunked bodies too, which upstream never drained). Pinned by `crates/fortis-edge/tests/body_desync.rs` over real sockets (mutation-checked: without the close the three smuggling tests fail; a normal keep-alive test still passes). The Caddy `request_body` cap stays as defence in depth.
- **LSP host:** `keys_seed` (0400) and SQLite are the hot wallet; a host compromise is a loss of the LSP's own funds and channel state. Nightly off-host backups are documented in `deploy/lightning.md`, with the warning that restoring an older `ldk_node_data.sqlite` invites a justice transaction.
- **Residual:** a single home host, no HA; the LSP's liquidity can be locked by many small JIT channels (each costs the requester the fee).

### 2.12 The kill switch and the exit path

- `enabled:false` in `/ln/info` starts the node in `StartMode::Exit`: no new channels, no JIT invoices (`lightning_disabled`), the Move dialog is blocked, but force-close, sweep and paying out keep working. Rust: `tests/exit.rs` (`exit_mode_refuses_lightning_actions_promptly`, `exit_mode_force_close_and_sweep_recovers_funds`, …); Kotlin: `LightningSessionTest.remote_kill_switch_starts_in_exit_mode_blocks_move_and_allows_force_close_and_sweep`.

## 3. Accepted limitations (from the task ledger and this review)

1. **LSP zero-conf trust** (`client_trusts_lsp`): see 2.1.
2. **Routing-fee income is zero on JIT channels** (economics, not safety).
3. **Local-only records** — close records, payment-attempt records, payment notes, device id are in the app's data directory, not VSS. After a wipe or on a new device: descriptions are lost; the double-pay guard only covers the window in which ldk-node failed to persist; the device id is new (the lock then asks for a takeover).
4. **Esplora dropped-funding behaviour** is not verified on mainnet; the "dropped funding" case is on the canary checklist.
5. **An Unconfirmed funding after a sync past the close is treated as void on Esplora** even if the funding is still in the mempool and may confirm later; a sweep in that window can take the anchor reserve.
6. **Pending-HTLC funds are not recovered by the seed-only sweeper** (it scans the 2 000 static scripts only).
7. **The node runs only while the wallet is unlocked** (60 s auto-lock stops it): payments to the user fail while it is stopped, and nothing watches for a revoked-state broadcast. A LSP-initiated close while the phone is locked leaves the user's output to be swept on the next unlock.
8. **A restart with the LSP down needs the Exit mode** (Task 7c).
9. **`ln_recover_*` and `ln_vss_state_exists` have no Android caller yet** (see 6.1).

## 4. Fixed in this task (branch `security-perf-fixes`)

Security: JIT fee ceiling (2.1); sweep feerate ceiling and fee share cap with a correct error (2.4); `/ln/info` amounts bounded in the app and the edge (2.2); parser input length and nesting (2.6); LNURL lookups delayed to 1 s and receive quotes to 1.5 s and no longer repeated on balance updates (2.5); LNURL / Lightning-address / BOLT12 payments wait while any outbound payment is still pending, because only BOLT11 is guarded against a repeat by its payment hash (`LightningSession.pay`, `LightningHardeningTest.a_fresh_invoice_target_*`); IPv6 site-local addresses blocked; transit sweeps advance the receive index so the next address is not reused; concurrency cap on VSS requests (2.11); the connection desync in the vendored `tiny_http` (2.11); lockfile bumps (2.10); `deploy/lightning.md` corrected (lock key name, proxy limits).
Performance: `channel_state()` no longer calls `list_balances` a second time; poll loop cannot spin; quotes not re-bought on every balance change; poison-tolerant locks in `node.rs` (a panic elsewhere no longer turns every later call, and `Claim::drop` during unwinding, into a panic or abort).

## 5. Adversarial-input tests (step 2)

Property tests with `proptest` (dev-dependency only; `Cargo.lock` gains `proptest`, `rand_xorshift`, `unarray`; no shipped binary changes) plus a seeded random-mutation test in Kotlin. Invariants: no panic, no hang (each case is timed), bounded memory (strategy sizes), and the specific safety property where there is one.

| Target | Where | Safety property pinned |
|---|---|---|
| `parse_at` | `tests/adversarial.rs` | never panics; huge/nested input bounded |
| LNURL / Lightning address | `tests/adversarial.rs` | no URL ever fetched with a private/local host |
| Esplora scan responses | `tests/adversarial.rs`, `recover.rs::fuzz` | in-range indices; no panic on any status/body |
| Scan accepted results | `tests/adversarial.rs` | only sane, unique, in-range UTXOs |
| Sweep | `tests/adversarial.rs` | pays only our address, fee ≤ half, no dust, standard size |
| Attempts / notes files | `pay.rs::fuzz` | a corrupt attempts file blocks sending, `send` never called, file untouched |
| Close-record file | `channel.rs::fuzz` | arbitrary bytes/JSON read as "no records" and rewritten cleanly |
| `scantxoutset`, broadcast answer | `recover.rs::fuzz` | no panic; only our own txid accepted |
| Unread request bodies (`tiny_http`) | `crates/fortis-edge/tests/body_desync.rs` | an unread body is never parsed as another request and the connection is closed; normal keep-alive still works |
| `/ln/info` (Rust edge) | `ln.rs::fuzz` | accepted documents are within every limit the wallet enforces |
| `/ln/info` (Kotlin) | `LnInfoFuzzTest` | within limits or `JSONException`/`IllegalArgumentException`, never anything else |

Not fuzzed with a coverage-guided fuzzer (`cargo-fuzz` was not used); the corpora are the unit-test vectors plus random mutation.

## 6. Open findings — these block the beta

### 6.1 Release blockers (functional; they can lose fees or strand funds)

1. **"Move to Lightning" cannot open a channel as built.** The app sends exactly the amount to the transit address, then calls `openChannel(amount)`, which refuses unless the transit wallet holds *amount + the 25 000 sat anchor reserve* (`channel.rs::open_channel`; ldk-node enforces the same; `tests/channel.rs` asserts the refusal). The error is swallowed and the funds are swept back on the next start. Not reproduced on a device.
2. **The Move intent is fragile:** cleared before the open is attempted (`LightningSession.kt`), kept only in memory, not cleared on cancel, and unbounded above by `channelMaxSat`.
3. **Recovery is not reachable from the app.** A restored wallet whose VSS data is lost starts a fresh node and is never offered the seed-only sweep.

4. **"Close channel" silently becomes a force-close** (`LightningSession.closeAndSweep`: `runCatching { closeChannel() }.onFailure { forceCloseChannel() }`, added in commit `f2be6e7`). Any failure of the cooperative close — LSP briefly unreachable, HTLCs in flight, backup unavailable, or a second tap while a cooperative close is pending (`close_in_progress`) — broadcasts the latest commitment: the funds are timelocked for the LSP's `to_self_delay` and the fees are higher, and the dialog only says "closes your Lightning channel". `channel.rs` says a force close is for an explicit user action only. The sweep that follows in the same call will normally throw `closing`, so the user may see an error although the force close went through. Fix: surface the error and offer force close as a separate, explicitly worded action; disable Close while the state is Closing. (Reported by the adversarial reviewer and confirmed from the diff; not changed here because it was a deliberate edit of the owner's.)

### 6.2 Needs an owner decision

4. **No LSP pinning** (2.2). Options: compile the hosted LSP's node id into the app; trust-on-first-use with a "the Lightning service changed" prompt (needs a rotation story); or sign `/ln/info`.
5. **One operator holds backup and counterparty** (2.3). Accept in writing, or add an independent VSS operator / watchtower.

### 6.3 Not fixed (accepted until the owner decides otherwise)

6. The recovery scan's 404 handling (2.4) — changing it could break recovery against a server that 404s unused scripts, which cannot be checked offline; needs a run against a real Esplora.
7. Chain-source trust (2.4) and the offline-while-locked limitation (3.7) — design-level.
8. `closeAndSweep` swallows a failed close; closing a channel needs no re-authentication (2.7).
9. Slow request bodies still hold an edge worker on every route except `/ln/vss` (no read deadline in `tiny_http` or the edge; `crash_report` and the swap routes read bodies). A few dozen trickling connections can starve the pool. Pre-existing; needs a socket read timeout.
10. The VSS per-token table lock-out and free tokens (2.11); the LNURL host rule blocks all of `64:ff9b::/96`, so lookups fail on IPv6-only networks that use DNS64 (the embedded IPv4 address should be checked instead); quotes refused by the fee ceiling fall back silently to an on-chain QR (2.1).

## 7. Fault injection on the real stack (step 3) — status

**Not completed in this pass.** What exists and is run by `cargo test -p fortis-lightning -- --ignored --test-threads=1` (with `FORTIS_BTC_BITCOIND`, `FORTIS_VSS_SERVER`, the built `fortis-lsp`): VSS down at start, mid-session and at stop (`restore.rs`, `pay.rs::pay_with_vss_down_returns_backup_unavailable_and_sends_nothing`, `channel.rs::sweep_blocked_when_vss_down`, `node.rs`), a payment in flight across stop/restart (`pay.rs::payment_in_flight_survives_stop_and_restart`), a crash restart that is not "another device" (`restore.rs::own_crash_restart_is_not_another_device`), a crash that needs a takeover (`restore.rs::restore_after_crash_needs_takeover_then_recovers`), and the Exit-mode matrix (`exit.rs`).
**Still to build:** `kill -9` of the client mid-channel-open, mid-close and mid-record-write; disk full; clock jumps; VSS pause/resume during every operation class; the LSP restarted under a new node id; the edge returning garbage or an old `/ln/info`; and the post-restart assertions (no lost or doubled payment, no `None` state while a channel exists, no fund-moving action while VSS is down, balances reconcile).

## 8. Staged rollout, kill switch, and the audit recommendation (steps 6 and 7)

Order, each stage gated on the previous one passing `deploy/lightning.md`'s canary checklist: **regtest → signet/testnet LSP with small caps → mainnet canary** (`channel_max_sat` 500 000, the owner's own wallets only) **→ wider beta**. The kill switch is in place and tested in both languages (2.12); turning it on must be the *first* response to any incident, and it never blocks an exit.

**Independent reviews (step 5).**
(a) The `security-review` pass over the branch found one High issue — the `tiny_http` connection desync (2.11) — which is fixed and pinned. (b) A dedicated adversarial review by a separate reviewer (read-only, instructed to assume a mistake in every money path) reported twelve findings. Dispositions: the force-close fallback (6.1 item 4) — open, owner's decision; the `tiny_http` desync — fixed; the global VSS quota as a kill switch — default turned off; the ratio-only sweep fee cap and its misleading error — feerate ceiling lowered and error corrected; the hard-coded JIT ceiling — tightened, info-derived follow-up open; no de-duplication for LNURL/BOLT12 payments — guard added; IPv6 site-local — blocked; the limited value of the seed wipe — documented in 2.7; slow bodies and the key-table lock-out — open (6.3); seed and store id independent of the network — not applicable today because Lightning is enabled for mainnet wallets only; the takeover window and deep-link behaviour — already listed. Both reviews are by the same family of model as the author; they are not the third-party review below.
(c) The final whole-branch review by a human is still to do.

**External audit recommendation.** An independent third-party review of the money paths (key derivation, device lock and VSS gate, pay and attempt record, channel operations and sweep guards, recovery, the edge `/ln/*` routes, the LSP config, and the Android send/receive/move flows) is recommended before the beta is offered to anyone holding more than trivial amounts. Nothing in this process — tests, property tests, tooling, or self-review — replaces one.
