# XBT Lightning, Plan B: server side and index spends — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a second `fortis-lsp` instance for XBT behind the edge, and make
the indexes answer "who spent this channel output" for any channel age, on BTC
and XBT, without timing out a phone's sync.

**Architecture:**

- **fortis-index:** gains a persistent `spends` column family. It maps an
  outpoint to its spender, for segwit script-path inputs from a configured
  height onward.
  - It is filled while indexing, backfilled once, and undone on reorgs.
  - Outspend lookups answer from it, so they have no 2016-block limit and no
    50-block scan per request.
- **fortis-lsp:** gains a `chain` setting. With `chain = "xbt"`:
  - its chain source is our XBT index over Esplora;
  - it collects routing data peer to peer;
  - the LDK fork runs in XBT mode, which accepts splices from any peer.
- **fortis-edge:** serves an `xbt` object in `/ln/info`, and forwards
  `/ln/account?chain=xbt` to the XBT LSP.
- **Deployment:** done with templates and a runbook.

**Tech Stack:**

- Rust: fortis-index (RocksDB), fortis-lsp (ldk-node 0.7.1 plus the vendored
  XBT LDK), fortis-edge (tiny_http).
- systemd, and nftables on the GCP relay.

**Spec:** `docs/superpowers/specs/2026-11-06-xbt-lightning-design.md` §2,
`vendor/XBT-PATCHES.md` ("Plan B / production follow-ups").

**Follow-on:** Plan C, the app.

## Global Constraints

- **Work location:** a new worktree `~/Work/fortis-xbt-server`, on branch
  `feat/xbt-lightning-server` from `main` (7fed930 or later). Never push.
  Don't touch production services, apart from the deploy steps in Task 8,
  which the operator runs.
- **XBT LSP:**
  - `/etc/fortis/lsp-xbt.toml`, unit `fortis-lsp-xbt.service`, data in
    `/var/lib/fortis/lsp-xbt` (its own `keys_seed`).
  - Admin and metrics on `127.0.0.1:9746`; peers on `0.0.0.0:9835`.
  - Announced as `lsp.fortistechlabs.com:9835`, through the relay (DNAT 9835).
- **XBT LSP settings:**
  - chain source `http://127.0.0.1:8094` (the XBT index);
  - gossip peer to peer, never RGS;
  - gossip floor 961640;
  - `custody_max_sat = 0` by default.
- **`/ln/info`:** the `xbt` object has the same fields as the BTC document,
  plus `"chain": "xbt"`. Ship it with `enabled: false`. Old apps must keep
  working, since unknown fields are ignored.
- **`chain` values:** `"btc"` (the default) or `"xbt"`; anything else is an
  error. The account body stays exactly as the phone signed it, so `chain`
  goes in the query string, not the body.
- **Spends index heights (`--spends-from-height`):**
  - XBT mainnet `961640`.
  - BTC mainnet `973000`, provided Task 8 Step 1's check confirms every live
    LSP channel was funded at or above it.
  - Regtest `0`.
- **Vendored LDK changes:** every one gets a `// FORTIS XBT:` marker and a row
  in `vendor/XBT-PATCHES.md`. `tools/xbt-regtest/run.sh` must still pass
  (12/12) after Task 4.
- **Commit messages** end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_019QuZCvQdMzZS7AMNP6Hiqf
  ```
- **This host runs production services:** `bitcoin-knots`, `bitcoin-core`,
  both `fortis-*-index` units, `fortis-lsp`, `fortis-edge` and `fortis-vss`.
  Tests and regtest use their own ports and directories. Never use `pkill`,
  `killall` or `systemctl` in tests.

## Review Focus

1. **A channel funded below `--spends-from-height` and closed now:** the lookup
   must still answer correctly, through the old scan path. A wrong "unspent"
   is the funds-loss case. Tested in Task 2.
2. **A spend in a block the node has but the index hasn't processed yet:** the
   lookup must not report "unspent". Tested in Task 2.
3. **A reorg that removes the spending block:** the entry is deleted, and the
   outpoint reads as unspent or in the mempool again. Tested in Task 1.
4. **Restart part-way through the backfill:** it resumes from the stored
   watermark without gaps, and lookups above the watermark don't trust the
   column family. Tested in Task 2.
5. **`/ln/account?chain=xbt` while the XBT LSP is down or not configured:**
   a clear 404 or 502, and the BTC account is never answered by mistake.
   Tested in Task 6.

---

### Task 1: fortis-index — the `spends` column family (write and reorg)

**Files:**
- Modify: `crates/fortis-index/src/db.rs` (column families, block
  commit/disconnect, meta keys).
- Create: `crates/fortis-index/src/spendcf.rs`, which holds the selection rule
  and the encoding.

**Interfaces:**
- Produces, in `spendcf.rs`:
  - `pub fn recorded(input: &TxIn) -> bool`. True unless one of these holds:
    - the witness is empty;
    - the witness is exactly 2 items and item[1] is 33 bytes (P2WPKH);
    - the witness is exactly 1 item of 64 or 65 bytes (P2TR key path).
  - `pub struct SpendRec { pub txid: Txid, pub vin: u32, pub height: u32 }`
    with `encode() -> [u8; 40]` and `decode(&[u8]) -> Option<SpendRec>`.
  - `pub fn key(op: &OutPoint) -> [u8; 36]`: the txid bytes, then the vout as
    big-endian.
- Produces, in `db.rs`:
  - `CF_SPENDS = "spends"`;
  - `Db::spend_of(&self, op: &OutPoint) -> Result<Option<SpendRec>>`;
  - `Db::spends_from(&self) -> Option<u32>`, the configured start height;
  - `Db::spends_watermark(&self) -> Option<u32>`, the highest height whose
    spends are complete.
- Block commit at height `h`, when `spends_from` is `Some(f)` and `h >= f`:
  - put each recorded input of each tx;
  - in the same write batch, if the watermark equals `h - 1` or `f - 1`, set
    the meta `spends_watermark = h`.
- Disconnect at height `h`:
  - delete that block's recorded inputs;
  - set `spends_watermark = h - 1` if it was `h`.
  - Use the same undo path the other column families use; the undo record
    already has the block's inputs.

- [ ] **Step 1: Write the failing tests** (in `spendcf.rs` and `db.rs` tests)
  - `recorded_skips_p2wpkh_taproot_keypath_and_legacy`: four inputs, one per
    case, plus a 4-item `[empty, sig, sig, script]` 2-of-2 that must be
    recorded.
  - `spend_rec_roundtrip`
  - `commit_records_spends_from_start_height`: commit blocks 10, 11 and 12
    with `spends_from = 11`. The block-10 input is absent, the block-11 and
    block-12 inputs are present with the right txid, vin and height, and the
    watermark is 12.
  - `disconnect_removes_spends` (Review Focus 3): commit 11 and 12, then
    disconnect 12. The block-12 spend is gone, the block-11 spend is still
    there, and the watermark is 11.
  - `no_spends_when_disabled`: `spends_from = None`, so nothing is written.
- [ ] **Step 2: Run them and see them fail**

  Run: `$HOME/.cargo/bin/cargo test -p fortis-index spend`
- [ ] **Step 3: Implement the Interfaces above**
  - The column family uses the existing "big" options.
  - Opening an existing database must create the new column family. Check
    `create_missing_column_families`.
  - Don't bump the schema version: an old database opens with an empty
    `spends` and no watermark.
- [ ] **Step 4: Run `cargo test -p fortis-index`** and see it all pass.
- [ ] **Step 5: Commit**

  `fortis-index: spends column family (outpoint -> spender) from a start height`

### Task 2: fortis-index — `--spends-from-height`, backfill, outspend from the column family

**Files:**
- Modify:
  - `crates/fortis-index/src/main.rs`: the flag, which is passed to the db
    config.
  - `crates/fortis-index/src/sync.rs`: the backfill.
  - `crates/fortis-index/src/spends.rs`: the lookup order.

**Interfaces:**
- Consumes: Task 1's `Db::{spend_of, spends_from, spends_watermark}`,
  `spendcf::recorded`, `SpendRec`.
- Produces:
  - **CLI:** `--spends-from-height <u32>`, optional; leaving it out disables
    the feature, which is the current behaviour.
  - **Backfill:** `sync::backfill_spends(db, node, from, to)`. It processes
    heights `max(from, watermark + 1)..=to`, where `to` is the index tip, in
    batches of 100 blocks. It advances the watermark per batch, so a restart
    resumes. It runs in a background thread at startup, and normal indexing
    is not blocked.
  - **Lookup:** `SpendIndex::outspend`, called with the funding height `fh`,
    the index tip `it` and the watermark `w`:
    1. Mempool or the node's spender index (`known_spender`): spent,
       unconfirmed. That's the existing behaviour.
    2. If `spends_from = Some(f)`, `fh >= f` and `w >= fh`:
       - a column-family hit → `Spent` with its block (hash read through the
         height);
       - a miss and `unspent(op)` → `Unspent`;
       - a miss and spent: scan only the blocks `w + 1 ..= node tip` with the
         existing per-request scan and no 2016-block cap.
       - **Never** answer `Unspent` while the node says the outpoint is spent
         (Review Focus 2).
    3. Otherwise, the existing scan path, unchanged (swaps, old outputs).

- [ ] **Step 1: Write the failing tests** (in `spends.rs` tests, using the
  existing fake `SpendNode`, and a db fixture)
  - `cf_hit_answers_without_scanning`: funded at 100, spent at 5,100 (past the
    old 2016 limit), recorded in the column family. Expect `Spent` with block
    height 5,100, and the fake node's `fetches() == 0`.
  - `cf_miss_unspent`: in range, no record, the node says it's unspent.
    Expect `Unspent`, with 0 block fetches.
  - `spent_above_watermark_scans_only_new_blocks` (Review Focus 2): watermark
    at 200, the spend at 202 is in the node only. Expect `Spent` at 202, with
    block fetches only for heights over 200.
  - `funded_below_start_uses_old_scan` (Review Focus 1): `spends_from = 1000`,
    funded at 900, spent at 950. The old path finds it.
  - `backfill_resumes_from_watermark` (Review Focus 4): backfill stopped
    after the first batch (the fake node errors on height `from + 100`).
    Restart the backfill. All spends from `from..=to` are present, and none is
    written twice or missing.
  - `lookup_above_watermark_during_backfill`: funded above the watermark
    while the backfill is mid-way. It takes path 3 (or the scan of path 2), and
    the column family is never trusted for it.
- [ ] **Step 2: Run them and see them fail**

  Run: `cargo test -p fortis-index spends::`
- [ ] **Step 3: Implement the Interfaces above.** Log one line per backfill
  batch: `spends backfill: <h>/<to>`.
- [ ] **Step 4: Run `cargo test -p fortis-index`** and see it all pass.
- [ ] **Step 5: Regtest check.** Add `--spends-from-height 0` to the index line
  in `tools/xbt-regtest/up.sh`, then run `tools/xbt-regtest/run.sh`.

  Expected: `RESULT: PASS (12 PASS of 12 steps)`, and `run.log` has no "S:
  sync_wallets timed out" retry lines. If the retry never fires, remove
  `sync_retrying_timeouts` from `crates/xbt-regtest/src/env.rs` and its row in
  XBT-PATCHES.md, then run again.
- [ ] **Step 6: Update and commit.** Update `vendor/XBT-PATCHES.md` "Plan B /
  production follow-ups" items (a) and (b) to "resolved by the spends index
  (Plan B Task 1–2)". Commit:

  `fortis-index: answer outspends from the spends index (no 2016-block limit); backfill`

### Task 3: LDK — the LSP accepts splices from any peer

**Files:**
- Modify:
  - `vendor/lightning-0.2.7/src/util/config.rs` (`XbtConfig`)
  - `vendor/lightning-0.2.7/src/ln/channelmanager.rs` (`splice_channel`,
    `internal_splice_init` allow-list check)
  - `vendor/ldk-node/src/config.rs`
  - `vendor/ldk-node/bindings/ldk_node.udl`
  - `vendor/lightning-0.2.7/src/ln/xbt_tests.rs`
  - `vendor/XBT-PATCHES.md`

**Interfaces:**
- Produces: `XbtConfig.splice_all_peers: bool`, default `false`, in both
  LDK's and ldk-node's `XbtConfig`.
  - When it's `true`, the allow-list check passes for every peer.
  - When it's `false`, behaviour is unchanged.
  - Why: an LSP's splice peers are its phones, which change all the time. The
    LND fork can't splice, and the LSP only starts splices towards phones.

- [ ] **Step 1: Write the failing test.**
  `xbt_splice_all_peers_accepts_unlisted`: an empty `splice_peers` and
  `splice_all_peers = true`. Expect both a splice-in started by the unlisted
  peer and an outbound `splice_channel` to succeed. The existing
  `xbt_splice_refused_for_unlisted_peer` must still pass.
- [ ] **Step 2: Run it and see it fail.** Use the command in XBT-PATCHES.md,
  "Running the tests".
- [ ] **Step 3: Implement the Interfaces above.** Add `// FORTIS XBT:` markers
  and a row in XBT-PATCHES.md.
- [ ] **Step 4: Run the tests.** Expected: the lightning suite at 1256 plus 1,
  ldk-node at 50, and `cargo check --workspace --all-targets` clean.
- [ ] **Step 5: Commit**

  `lightning/ldk-node: XbtConfig.splice_all_peers for an LSP`

### Task 4: fortis-lsp — the `chain` setting and its XBT wiring

**Files:**
- Modify:
  - `crates/fortis-lsp/src/config.rs` (`Raw`, `LspConfig`, validation, tests)
  - `crates/fortis-lsp/src/main.rs` (builder wiring near the
    `set_chain_source_bitcoind_rpc` call, about line 224, and RGS, about
    line 240)
  - `crates/fortis-lsp/src/push.rs` (`wake` payload)
- Create: `deploy/linux-home/lsp-xbt.toml.example`

**Interfaces:**
- **New keys:**
  - `chain` (`"btc"`, the default, or `"xbt"`)
  - `esplora_url`
  - `xbt_gossip_floor_height` (default `961640`)
- **Validation rules** (`from_toml` errors):
  - `chain = "xbt"` requires `esplora_url`;
  - `chain = "xbt"` forbids `rgs_url`;
  - with `chain = "xbt"`, `bitcoind_rpc`, `rpc_user` and `rpc_password_file`
    may be omitted;
  - `chain = "btc"` keeps today's requirements;
  - any other `chain` value is an error naming the allowed values.
- **Produces:** `pub enum ChainSource { Bitcoind { host, port, user, password_file }, Esplora(String) }`
  and `pub fn LspConfig::chain_source(&self) -> ChainSource`.
  - In `main.rs`, `Esplora` calls `set_chain_source_esplora(url, None)`.
  - If `chain` is xbt:
    - set `Config.xbt = Some(XbtConfig { gossip_floor_height, splice_peers: vec![], splice_all_peers: true })`;
    - use P2P gossip (`set_gossip_source_p2p`).
- **Push:** the `wake` data gains `"chain": "btc"|"xbt"` from the config. The
  title and body text are unchanged.

- [ ] **Step 1: Write the failing tests** (`config.rs` and `push.rs` tests)
  - `xbt_requires_esplora_url`
  - `xbt_rejects_rgs_url`
  - `xbt_allows_missing_bitcoind_keys`
  - `chain_defaults_to_btc`
  - `unknown_chain_rejected` (the error text contains `"btc"` and `"xbt"`)
  - `xbt_gossip_floor_default_961640`
  - `example_xbt_config_parses`: `include_str!` of
    `deploy/linux-home/lsp-xbt.toml.example` parses. Its values are
    `chain = "xbt"`, listen `0.0.0.0:9835`, `metrics_listen = 127.0.0.1:9746`,
    `esplora_url = http://127.0.0.1:8094`, `data_dir = /var/lib/fortis/lsp-xbt`,
    announce `lsp.fortistechlabs.com:9835`, alias `"Fortis XBT"`,
    `custody_max_sat = 0`, and the same fee/limit values as the BTC example.
  - `wake_payload_names_chain`: the data JSON has `"chain":"xbt"` for an XBT
    config.
- [ ] **Step 2: Run them and see them fail**

  Run: `cargo test -p fortis-lsp config:: push::`
- [ ] **Step 3: Implement the Interfaces above.** Also write the example file.
- [ ] **Step 4: Run `cargo test -p fortis-lsp`** and
  `cargo check --workspace --all-targets`.
- [ ] **Step 5: Commit**

  `fortis-lsp: chain = "xbt" (Esplora chain source, P2P gossip, XBT LDK mode)`

### Task 5: Regtest — fortis-lsp as the XBT LSP

**Files:**
- Modify:
  - `crates/xbt-regtest/src/{env.rs,scenarios.rs,main.rs}`
  - `tools/xbt-regtest/{env.sh,run.sh,README.md}`

**Interfaces:**
- Consumes: Task 4's config keys, and the `fortis-lsp` binary from
  `cargo build -p fortis-lsp`.
- Produces: a scenario `lsp_xbt_jit`, run after the existing ones.
  - **Setup:** `fortis-lsp` starts with a generated regtest config: chain xbt,
    the regtest index URL, listen and admin on new regtest ports added to
    `XR_PORTS`, its data under `$XR/lsp`, gossip floor 20. It is funded from
    Knots, and LND opens a channel to it.
  - **A new phone node P2** (ldk-node, XBT, `splice_peers = [lsp]`) runs with
    the LSPS2 client pointed at it:
    1. P2 gets a JIT invoice from the LSP, and LND pays it. The funding is
       `[0x21]`-signed where it's ours, and confirmed.
    2. P2 pays an LND invoice.
    3. P2 splices in 20,000 sat. The shared input is `[0x21, 0x21]`.
    4. A cooperative close, `[0x21, 0x21]`.
  - **On any failure,** the suite prints the LSP's last 30 log lines.

- [ ] **Step 1: Add the scenario.** Check bytes with `check::confirmed_with`,
  and include the LSP's fees in `sweeps_settle` accounting, or use a
  scenario-local balance check.
- [ ] **Step 2: Run `tools/xbt-regtest/run.sh`.** Expected:
  `RESULT: PASS (13 PASS of 13 steps)`. Leave the environment stopped.
- [ ] **Step 3: Commit**

  `xbt-regtest: fortis-lsp (chain = "xbt") as the XBT LSP`

### Task 6: fortis-edge — `xbt` in `/ln/info`, `/ln/account?chain=xbt`

**Files:**
- Modify:
  - `crates/fortis-edge/src/main.rs` (args, `State`, `ln_account_route`, about
    line 1370)
  - `crates/fortis-edge/src/ln.rs` (`LnInfoSource`, the `/ln/info` body)
- Create: `deploy/linux-home/ln-info-xbt.json.example`

**Interfaces:**
- **New arguments:** `--ln-info-xbt-file <path>` and
  `--lsp-xbt-admin-url <url>`.
- **`GET /ln/info`:**
  - The BTC document is unchanged. When the XBT file is set and valid, it
    gains `"xbt": { ...the XBT file's fields..., "chain": "xbt" }`.
  - An invalid or missing XBT file leaves out `xbt` (it isn't an error for
    BTC) and is logged once per change, the same "last good version" rule the
    BTC file already follows.
- **`POST /ln/account`:**
  - `?chain=xbt` forwards to `--lsp-xbt-admin-url`; that's 404 if unset.
  - No query, or `?chain=btc`, forwards to `--lsp-admin-url`, as today.
  - Any other `chain` is 400 with `"unknown chain"`.
  - The body and `X-Fortis-Sig` are relayed unchanged.
  - An unreachable upstream is 502 (Review Focus 5).
- **Example file:** `ln-info-xbt.json.example` is the BTC example's shape with
  - `"port": 9835`,
  - `"enabled": false`,
  - `"message": "XBT Lightning is coming soon"`,
  - `"node_id": "<fill in from fortis-lsp-xbt node_id>"`,
  - `"network": "bitcoin"`.

- [ ] **Step 1: Write the failing tests** (edge tests, using the existing
  test-server helpers for these routes)
  - `ln_info_includes_xbt_object`
  - `ln_info_without_xbt_file_unchanged`
  - `ln_info_bad_xbt_file_keeps_btc`
  - `account_chain_xbt_forwards_to_xbt_admin`: a fake upstream records the
    request. Assert the body and signature are unchanged and the BTC upstream
    is not called.
  - `account_chain_xbt_404_when_unset`
  - `account_unknown_chain_400`
  - `account_chain_xbt_upstream_down_502`
- [ ] **Step 2: Run them and see them fail**

  Run: `cargo test -p fortis-edge ln_`
- [ ] **Step 3: Implement the Interfaces above.**
- [ ] **Step 4: Run `cargo test -p fortis-edge`.**
- [ ] **Step 5: Commit**

  `fortis-edge: /ln/info xbt object; /ln/account?chain=xbt -> XBT LSP`

### Task 7: Deployment templates and runbook

**Files:**
- Create: `deploy/linux-home/fortis-lsp-xbt.service`. Copy
  `fortis-lsp.service`, with these changes: `--config /etc/fortis/lsp-xbt.toml`,
  `After=`/`Wants=` `fortis-xbt-index.service`, and no bitcoin-core
  dependency.
- Modify:
  - `deploy/linux-home/fortis-xbt-index.service`: add
    `--spends-from-height 961640`.
  - `deploy/linux-home/fortis-btc-index.service`: add
    `--spends-from-height 973000`.
  - `deploy/linux-home/edge.env`: commented lines for `--ln-info-xbt-file`
    and `--lsp-xbt-admin-url http://127.0.0.1:9746`.
  - `deploy/relay/lnrelay.nft`: DNAT and masquerade for 9835, next to the
    9735 rules.
  - `deploy/relay/README.md`: the firewall rule `tcp:9835`, and
    `nc -vz <VPS_IP> 9835`.
  - `deploy/lightning.md`: a new section, "XBT LSP", with the operator steps
    in Task 8's order.

- [ ] **Step 1: Write the files.**
- [ ] **Step 2: Verify.** Run
  `systemd-analyze verify deploy/linux-home/fortis-lsp-xbt.service` (it may
  warn only about the missing binary path) and `nft -c -f` on a copy of
  `lnrelay.nft`, if `nft` is available without root; otherwise say so.
- [ ] **Step 3: Commit**

  `deploy: XBT LSP unit, spends-from heights, relay 9835, runbook`

### Task 8: Operator rollout (run by the user; the plan only checks results)

These steps are in `deploy/lightning.md` "XBT LSP". The implementer doesn't run
any of them: they need sudo, the GCP console and funds. The plan's job ends with
the runbook and these checks.

1. **Check the BTC spends height before deploying the index:**
   `fortis-lsp --config /etc/fortis/lsp.toml` channel listing (or
   `/metrics`). Every channel's funding height must be ≥ 973000. If one
   isn't, lower `--spends-from-height` to below the oldest.
2. **Deploy the indexes:** install the new `fortis-index` and restart both
   index units. Wait for `spends backfill: done` in each journal.
3. **Deploy and set up the XBT LSP:**
   - install `fortis-lsp` and `/etc/fortis/lsp-xbt.toml`;
   - restart the BTC `fortis-lsp`, since it shares the binary, and check it
     still comes up with the same `node_id` and channels;
   - enable `fortis-lsp-xbt`;
   - back up `/var/lib/fortis/lsp-xbt/keys_seed` off the host;
   - read its `node_id`.
4. **Open the relay port:** relay `nft` for 9835, and GCP firewall `tcp:9835`.
   From mobile data, `nc -vz lsp.fortistechlabs.com 9835` must succeed.
5. **Turn on the edge:** write `/etc/fortis/ln-info-xbt.json` from the
   example, with `enabled: false`, and add the two edge arguments. Restart the
   edge. Check that `curl -s https://<edge>/ln/info | jq .xbt` shows the
   object.
6. **Fund and connect:** fund the XBT LSP and open outbound channels to XBT
   Lightning nodes (`[[outbound]]` in `lsp-xbt.toml`).
7. **Wait for the app:** setting `enabled: true` waits for Plan C's
   real-phone canary.

---

## Self-review notes

- **Spec coverage:** §2 is covered:
  - the LSP instance (Tasks 4 and 7);
  - Esplora chain source, P2P gossip and floor (Task 4);
  - ports and relay (Task 7);
  - limits per file (example config in Task 4);
  - edge `xbt` object and chain routing (Task 6);
  - VSS shared (no change needed: store ids are node ids);
  - push names the chain (Task 4).
- **Not here:** async-path and transfer-quote routes don't exist on the edge
  today; transfers and offers go through `/ln/account`, so `?chain=` covers
  them. The index difficulty fix shipped in 66812b9.
- **Plan A follow-ups:** resolved in Tasks 1–2 (the outspend limit and the
  sync timeout).
- **Spec deviation, recorded:** the spec says the LSP needs a splice
  allow-list. An LSP can't list its phones in advance, so Task 3 adds
  `splice_all_peers`. The LSP never starts a splice towards an LND peer, and
  the LND fork can't start one.
