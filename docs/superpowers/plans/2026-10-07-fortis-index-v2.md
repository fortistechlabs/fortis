# fortis-index v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rewrite `crates/fortis-index` so a from-SegWit sync takes hours instead of ~11 days, the API serves wallets concurrently, and the index is crash-, reorg- and misconfiguration-safe — for XBT over Knots and BTC over Core.

**Architecture:** Raw blocks (`getblock <hash> 0`) fetched by parallel workers, parsed by hand (80- or 164-byte header) and reduced to P2WPKH rows. Spends are attributed from the witness (`HASH160(witness[1])`), so every sync write is a blind put/delete into RocksDB column families keyed by a 20-byte program and a 5-byte sequential TxNum. Bulk mode (no WAL, atomic flush) until within 288 blocks of tip, then durable follow mode with undo records, tip notification via `waitfornewblock`. `axum` API reads lock-free through `ArcSwap` snapshots verified against `meta.tip`.

**Tech Stack:** Rust 2021, `rocksdb` 0.25, `axum` 0.8 + `tokio` 1, `arc-swap` 1.9, `crossbeam-channel` 0.5, `bitcoin` 0.32 (workspace), `ureq` 2 (via `fortis-node`), `clap` 4.

**Spec:** `docs/superpowers/specs/2026-10-07-fortis-index-v2-design.md`

## Global Constraints

- Work on branch `index-v2` in a worktree; `main` keeps the v1 index until Task 16 passes.
- Workspace `rust-version = "1.82"`; if a new dependency needs newer, bump it in `Cargo.toml` and note it in `CHANGELOG.md`.
- Must still build on Windows (MSVC): no unix-only crates; SIGTERM handling behind `#[cfg(unix)]`, Ctrl-C everywhere.
- Holds no keys; public chain data only.
- HTTP API stays compatible with `fortis-edge` and the wallets: same routes, JSON shapes, status codes, CORS headers (`Access-Control-Allow-Origin: *`, `Allow-Methods: GET, POST, OPTIONS`, `Allow-Headers: content-type`), errors as `{"error": "<msg>"}`.
- Only P2WPKH programs are indexed. Default start height 481 824 mainnet, 0 regtest.
- `MAX_REORG_DEPTH = 288`. `SCHEMA_VERSION = 2`. TxNum encoded as 5 bytes BE, max `2^40 - 1`.
- Txids/block hashes are stored as `to_byte_array()` (internal order); hex only at the API boundary.
- Log to stderr, prefix `index:` / `mempool:` / `api:` like v1.
- Exit code 2 = fatal (do not restart); any other failure is retried by the process or by systemd.
- Every `cargo` command below runs from the worktree root.

## Review Focus

1. **The node restarts while the index is running** (cookie rotates, RPC refuses for ~30 s): the index backs off, re-reads the cookie, carries on — no spin, no exit, no corruption. → Task 5 test `cookie_is_reread_after_401`, Task 10 test `transient_source_errors_back_off_and_recover`.
2. **`kill -9` mid bulk sync, then restart:** resumes from the last flushed tip, and `verify` finds no mismatch. → Task 14 test `kill_minus_nine_during_bulk_sync_recovers`.
3. **An address with tens of thousands of outputs** (reused exchange address queried by someone): answers 400 `address too heavy` quickly and other requests stay fast. → Task 13 test `heavy_address_is_refused_without_stalling_others`.
4. **A reorg while requests are in flight**, including one that crosses the bulk→follow boundary: no request ever mixes two chain states. → Task 8 test `view_never_pairs_a_table_with_a_newer_snapshot`, Task 10 tests `reorg_of_two_blocks_matches_a_fresh_sync`, `blocks_within_288_of_tip_get_undo`.
5. **Mempool churn:** an RBF replacement or eviction makes the old tx's pending outputs disappear and its spent coins reappear as spendable. → Task 11 test `replaced_tx_releases_its_spent_coin`.

---

## File Structure (`crates/fortis-index/`)

| File | Responsibility |
|---|---|
| `src/keys.rs` | all on-disk key/value encodings; pure |
| `src/chain.rs` | `HeaderFormat` (80/164 detection) and raw block parsing; pure except `detect` |
| `src/extract.rs` | transaction → P2WPKH rows (`TxRows`), `ParsedBlock`; pure |
| `src/db.rs` | RocksDB open/options/meta guard, `apply`, `rollback`, undo, `Reader` queries, render cache |
| `src/chainstate.rs` | in-memory `BlockTable`, `View` (consistent table + snapshot) |
| `src/source.rs` | `BlockSource` / `MempoolSource` / `TxSource` traits and their RPC impls |
| `src/fetch.rs` | parallel ordered fetch → parse pipeline |
| `src/sync.rs` | `Syncer`: bulk/follow modes, reorgs, backoff |
| `src/mempool.rs` | `MempoolTracker` + immutable `MempoolView` |
| `src/render.rs` | Esplora tx JSON (confirmed cached in DB, pending in memory) |
| `src/api.rs` | `axum` router, handlers, `/scan` |
| `src/verify.rs` | `verify` subcommand against `scantxoutset` |
| `src/main.rs` | CLI, wiring, signals, exit codes |
| `tests/fixtures/xbt-976000.hex` | real 164-byte-header Knots block |
| `tests/regtest.rs` | opt-in end-to-end against real regtest nodes |

Deleted: `src/store.rs`, `src/store/tests.rs`, v1 `src/sync.rs`, v1 `src/mempool.rs`, v1 `src/api.rs` (replaced in Tasks 10–13; ported tests noted where they land).
Modified: `crates/fortis-node/src/rpc.rs` (Task 5), `crates/fortis-edge/Cargo.toml` (Task 1), `deploy/linux-home/*.service`, `crates/fortis-index/README.md` (Task 16).

---

### Task 1: Upgrade RocksDB workspace-wide

`librocksdb-sys` sets `links = "rocksdb"`, so the edge and index must share one version. 0.25 bundles a RocksDB that builds with GCC 15 (the 0.22 one needs `CXXFLAGS=-include cstdint`).

**Files:**
- Modify: `crates/fortis-index/Cargo.toml` (`rocksdb = "0.25"`), `crates/fortis-edge/Cargo.toml` (`rocksdb = "0.25"`), any call sites the upgrade breaks in `crates/fortis-edge/src/{cache,scan_cache,btc_history}.rs` and `crates/fortis-index/src/store.rs`

**Interfaces:** Produces: `rocksdb` 0.25 available to all later tasks.

- [ ] **Step 1:** Bump both `Cargo.toml` entries; run `cargo update -p rocksdb`.
- [ ] **Step 2:** Build without the local workaround: `CXXFLAGS= cargo build --release -p fortis-index -p fortis-edge`. Expected: `Finished`. Fix any API breakage (keep behaviour identical).
- [ ] **Step 3:** `CXXFLAGS= cargo test -p fortis-edge -p fortis-index`. Expected: all pass (edge 62+33+1, index existing tests).
- [ ] **Step 4:** Remove the `[env] CXXFLAGS` block from `~/.cargo/config.toml` (added 2026-10-07 for 0.22); rerun Step 2 with no env override. Expected: `Finished`.
- [ ] **Step 5:** Commit `build: rocksdb 0.25 (builds with GCC 15)`.

---

### Task 2: On-disk encodings (`keys.rs`)

**Files:** Create `src/keys.rs` (tests in-module). Add `mod keys;` to `main.rs`.

**Interfaces — Produces:**
```rust
pub type Program = [u8; 20];
pub type TxNum = u64;
pub const TXNUM_LEN: usize = 5;
pub const MAX_TXNUM: TxNum = (1 << 40) - 1;
pub fn txnum_key(n: TxNum) -> [u8; 5];              // panics if n > MAX_TXNUM
pub fn txnum_from(b: &[u8]) -> TxNum;
pub fn height_key(h: u32) -> [u8; 4];
pub fn history_key(p: &Program, n: TxNum) -> [u8; 25];
pub fn history_txnum(key: &[u8]) -> TxNum;
pub fn utxo_key(p: &Program, op: &bitcoin::OutPoint) -> [u8; 56];   // program · txid · vout BE
pub fn utxo_outpoint(key: &[u8]) -> bitcoin::OutPoint;
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct UtxoVal { pub value: u64, pub height: u32 }
impl UtxoVal { pub fn encode(&self) -> [u8; 12]; pub fn decode(b: &[u8]) -> anyhow::Result<Self>; }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct BlockRec { pub hash: bitcoin::BlockHash, pub time: u32, pub first_txnum: TxNum, pub n_txs: u32 }
impl BlockRec { pub fn encode(&self) -> [u8; 45]; pub fn decode(b: &[u8]) -> anyhow::Result<Self>; }
pub fn tip_value(h: u32, hash: &bitcoin::BlockHash) -> [u8; 36];
pub fn tip_from(b: &[u8]) -> anyhow::Result<(u32, bitcoin::BlockHash)>;
```

- [ ] **Step 1: Write the failing tests**
```rust
#[test] fn txnum_round_trips_at_the_edges() { for n in [0, 1, 255, 1 << 32, MAX_TXNUM] { assert_eq!(txnum_from(&txnum_key(n)), n); } }
#[test] #[should_panic] fn txnum_above_40_bits_panics() { txnum_key(MAX_TXNUM + 1); }
#[test] fn history_keys_sort_by_program_then_txnum() {
    let (a, b) = ([1u8; 20], [2u8; 20]);
    assert!(history_key(&a, 2) > history_key(&a, 1));
    assert!(history_key(&b, 0) > history_key(&a, MAX_TXNUM));
    assert_eq!(history_txnum(&history_key(&a, 77)), 77);
}
#[test] fn utxo_key_round_trips_the_outpoint() { /* OutPoint{txid: Txid::from_byte_array([9;32]), vout: 70000} → key → same OutPoint; key[..20] == program */ }
#[test] fn block_rec_and_utxo_val_round_trip() { /* encode→decode equality; decode of a 3-byte slice is Err */ }
#[test] fn tip_value_round_trips() { /* (976000, hash) */ }
```
- [ ] **Step 2:** `cargo test -p fortis-index keys::` → FAIL (unresolved items).
- [ ] **Step 3:** Implement the listed functions in `src/keys.rs`.
- [ ] **Step 4:** `cargo test -p fortis-index keys::` → PASS.
- [ ] **Step 5:** Commit `index-v2: key encodings`.

---

### Task 3: Raw block parsing and header format (`chain.rs`)

**Files:** Create `src/chain.rs`, `tests/fixtures/xbt-976000.hex`.

**Interfaces — Produces:**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderFormat { pub v2_from: Option<u32> }   // 164-byte headers from this height
impl HeaderFormat {
    pub const BTC: HeaderFormat = HeaderFormat { v2_from: None };
    pub fn header_len(&self, height: u32) -> usize;        // 80 or 164
    pub fn chain_id(&self) -> String;                       // "btc" | "xbt@961640"
    pub fn from_deployment_info(v: &serde_json::Value) -> Self;  // deployments.blake2b.height if present
    pub fn detect(rpc: &fortis_node::Rpc) -> anyhow::Result<Self>; // calls getdeploymentinfo
}
pub struct BlockBody { pub prev: bitcoin::BlockHash, pub time: u32, pub txs: Vec<bitcoin::Transaction> }
pub fn parse_block(bytes: &[u8], header_len: usize) -> anyhow::Result<BlockBody>;
```
`prev` = bytes 4..36, `time` = LE u32 at 68..72 in both formats; then CompactSize tx count and `Transaction::consensus_decode` per tx; leftover bytes ⇒ `Err("trailing bytes")`.

- [ ] **Step 1: Capture the fixture**
```bash
K="/opt/bitcoin-knots/current/bin/bitcoin-cli -rpcport=8432 -rpccookiefile=/var/lib/bitcoin-knots/.cookie"
$K getblock $($K getblockhash 976000) 0 > crates/fortis-index/tests/fixtures/xbt-976000.hex
```
- [ ] **Step 2: Write the failing tests**
```rust
#[test] fn parses_a_real_xbt_block_with_a_164_byte_header() {
    let raw = hex::decode(include_str!("../tests/fixtures/xbt-976000.hex").trim()).unwrap();
    let b = parse_block(&raw, 164).unwrap();
    assert_eq!(b.prev.to_string(), "00000000000000007b33968e11ae9476536a8034c838d71bf7f32b2836b4d080");
    assert_eq!(b.time, 1_791_360_298);
    assert_eq!(b.txs.len(), 45);
    assert!(b.txs[0].is_coinbase());
}
#[test] fn the_wrong_header_length_is_an_error_not_garbage() { /* same fixture with 80 → Err */ }
#[test] fn parses_a_standard_80_byte_block() { /* serialize a bitcoin::Block built in-test (2 txs) → prev, time, txids match */ }
#[test] fn trailing_bytes_are_rejected() { /* append 0x00 → Err containing "trailing" */ }
#[test] fn header_format_from_deployment_info() {
    let knots = json!({"deployments": {"blake2b": {"height": 961640, "active": true}}});
    let f = HeaderFormat::from_deployment_info(&knots);
    assert_eq!((f.header_len(961639), f.header_len(961640)), (80, 164));
    assert_eq!(f.chain_id(), "xbt@961640");
    let core = json!({"deployments": {"taproot": {"height": 709632, "active": true}}});
    assert_eq!(HeaderFormat::from_deployment_info(&core), HeaderFormat::BTC);
    assert_eq!(HeaderFormat::BTC.chain_id(), "btc");
}
```
- [ ] **Step 3:** Run → FAIL. **Step 4:** Implement. **Step 5:** Run → PASS.
- [ ] **Step 6:** Commit `index-v2: raw block parser with 164-byte XBT headers`.

---

### Task 4: P2WPKH extraction (`extract.rs`)

**Files:** Create `src/extract.rs`.

**Interfaces — Consumes:** `keys::Program`, `chain::{parse_block, HeaderFormat}`. **Produces:**
```rust
#[derive(Clone, Debug, PartialEq, Eq)] pub struct Funded { pub program: Program, pub vout: u32, pub value: u64 }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct Spent  { pub program: Program, pub prevout: bitcoin::OutPoint }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct TxRows { pub txid: bitcoin::Txid, pub funded: Vec<Funded>, pub spent: Vec<Spent> }
pub struct ParsedBlock { pub height: u32, pub hash: bitcoin::BlockHash, pub prev: bitcoin::BlockHash, pub time: u32, pub txs: Vec<TxRows> }
pub fn spend_program(input: &bitcoin::TxIn) -> Option<Program>;   // empty scriptSig && witness.len()==2 → HASH160(witness[1])
pub fn tx_rows(tx: &bitcoin::Transaction) -> Option<TxRows>;       // None if it funds and spends no program; txid computed only when Some
pub fn parse_and_extract(height: u32, hash: bitcoin::BlockHash, bytes: &[u8], fmt: &HeaderFormat) -> anyhow::Result<ParsedBlock>;
```

- [ ] **Step 1: Write the failing tests** (build keys with `bitcoin::secp256k1`; a real ECDSA signature is not needed — any 72-byte item satisfies the witness shape)
```rust
#[test] fn a_p2wpkh_spend_is_attributed_from_its_witness() { /* pk → Address::p2wpkh → program; input with witness [sig, pk.serialize()] → Some(program) */ }
#[test] fn coinbase_taproot_and_wrapped_inputs_are_not_p2wpkh_spends() {
    // coinbase (scriptSig non-empty), 1-item witness (taproot key path), P2SH-P2WPKH (non-empty scriptSig) → None each
}
#[test] fn outputs_keep_their_true_vout_and_only_p2wpkh_counts() {
    // outputs [OP_RETURN, P2WPKH(p,2000), P2PKH, P2TR] → funded == [Funded{program:p, vout:1, value:2000}]
}
#[test] fn a_tx_touching_no_program_yields_none() {}
#[test] fn the_real_xbt_block_extracts_without_error() { /* fixture → parse_and_extract(976000, hash, ..) Ok; every TxRows non-empty */ }
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `index-v2: witness-attributed P2WPKH extraction`.

---

### Task 5: RPC batch calls, fresh agents and cookie re-read (`fortis-node`)

**Files:** Modify `crates/fortis-node/src/rpc.rs` (tests in-module, using a `std::net::TcpListener` stub that answers canned HTTP).

**Interfaces — Produces:**
```rust
impl Rpc {
    pub fn call_batch(&self, calls: &[(&str, serde_json::Value)]) -> anyhow::Result<Vec<anyhow::Result<serde_json::Value>>>; // results in request order, matched by id
    pub fn fresh(&self) -> Rpc;                                    // same url/auth, own ureq::Agent (per fetch worker)
    pub fn from_cookie_file(base_url: &str, path: &std::path::Path) -> anyhow::Result<Rpc>; // on 401: re-read file once, retry once
}
```
Auth must become interior-mutable (`RwLock<String>`) so a re-read is visible to every clone made by `fresh()` afterwards.

- [ ] **Step 1: Write the failing tests**
```rust
#[test] fn batch_results_come_back_in_request_order() { /* stub replies [{"id":1,...},{"id":0,...}] → out[0] is id 0's result */ }
#[test] fn a_failed_call_inside_a_batch_is_an_err_entry_not_a_failed_batch() { /* one {"error":{...}} → out[1].is_err(), out[0].is_ok() */ }
#[test] fn cookie_is_reread_after_401() { /* stub accepts only "u:new"; file says "u:old" then is rewritten to "u:new" before call → call Ok, stub saw 2 requests */ }
```
- [ ] **Step 2:** `cargo test -p fortis-node rpc::` → FAIL. **Step 3:** Implement. **Step 4:** → PASS; also `cargo test -p fortis-edge -p wallet-cli` still pass (shared crate).
- [ ] **Step 5:** Commit `fortis-node: batch RPC, per-worker agents, cookie re-read on 401`.

---

### Task 6: Database core (`db.rs`): open, guards, apply, reads

**Files:** Create `src/db.rs` (tests in `src/db/tests.rs`). Add `thiserror = "2"`.

**Interfaces — Consumes:** `keys::*`, `extract::ParsedBlock`. **Produces:**
```rust
pub const SCHEMA_VERSION: u32 = 2;
pub const MAX_REORG_DEPTH: u32 = 288;
pub struct DbConfig { pub cache_mb: usize }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Durability { Bulk, Durable }
#[derive(Clone)] pub struct Db { /* Arc<rocksdb::DB> */ }
#[derive(Debug, thiserror::Error)] pub enum OpenError { #[error("database is for chain {found}, node is {expected}")] ChainMismatch { found: String, expected: String }, #[error("database schema {found} is not {SCHEMA_VERSION} (v1 index? use a new --db directory)")] Schema { found: String } }
impl Db {
    pub fn open(path: &std::path::Path, cfg: &DbConfig, chain_id: &str) -> anyhow::Result<Db>;
    pub fn load_blocks(&self) -> anyhow::Result<Vec<(u32, BlockRec)>>;   // ascending height
    pub fn apply(&self, blocks: &[ParsedBlock], next_txnum: TxNum, d: Durability, keep_undo: bool) -> anyhow::Result<Vec<BlockRec>>;
    pub fn flush(&self) -> anyhow::Result<()>;
    pub fn compact_background(&self);                                    // history + utxo, on its own thread
    pub fn reader(&self) -> Reader<'_>;
    pub fn render_put(&self, n: TxNum, json: &[u8]) -> anyhow::Result<()>;
}
pub struct Reader<'a> { /* snapshot */ }
impl Reader<'_> {
    pub fn tip(&self) -> anyhow::Result<Option<(u32, bitcoin::BlockHash)>>;          // meta.tip
    pub fn history(&self, p: &Program, limit: usize) -> anyhow::Result<Vec<TxNum>>;  // newest first, at most `limit` rows (bounded scan)
    pub fn utxos(&self, p: &Program, max_rows: usize) -> anyhow::Result<Vec<(bitcoin::OutPoint, UtxoVal)>>; // Err(TooHeavy) if > max_rows
    pub fn utxo(&self, p: &Program, op: &bitcoin::OutPoint) -> anyhow::Result<Option<UtxoVal>>;
    pub fn txid(&self, n: TxNum) -> anyhow::Result<Option<bitcoin::Txid>>;
    pub fn render_get(&self, n: TxNum) -> anyhow::Result<Option<Vec<u8>>>;
}
#[derive(Debug, thiserror::Error)] #[error("address too heavy")] pub struct TooHeavy;
```
Apply order per tx: puts for `funded` (history + utxo), then blind deletes for `spent` (utxo) + history put for the spending txnum; txids put; `blocks` put; `meta.tip` put — all in one `WriteBatch`. Bulk: `disable_wal(true)`; Durable: `set_sync(true)`. Options per spec (atomic_flush, 20-byte prefix extractor + prefix bloom on `history`/`utxo`, LZ4 + ZSTD bottommost, shared LRU block cache `cache_mb`, `increase_parallelism(available_parallelism)`, 256 MB write buffers on `history`/`utxo`). With `keep_undo`, before the batch: `multi_get` the UTXO values about to be deleted and write `undo[height]` (added history keys, added utxo keys, deleted `(utxo key, UtxoVal)`, txnum range).

- [ ] **Step 1: Write the failing tests** (fixture helper `blk(height, txs)` building `ParsedBlock`s from `TxRows`; programs `p1=[1;20]`, `p2=[2;20]`)
```rust
#[test] fn fund_then_spend_across_blocks() {
    // 100: A funds p1 vout1 500 · 101: B spends A:1 (program p1), funds p2 vout0 480
    // → history(p1) == [1, 0]; utxos(p1) empty; utxos(p2) == [(B:0, {480, 101})]
    //   txid(0)==A, txid(1)==B; load_blocks == [(100,{first_txnum:0,n_txs:1}), (101,{first_txnum:1,n_txs:1})]; reader.tip()==Some((101, h101))
}
#[test] fn fund_and_spend_in_one_batch_matches_two_batches() {}
#[test] fn a_spend_seen_by_two_programs_in_one_tx_is_one_history_row_each() {}
#[test] fn refuses_a_database_for_the_other_chain() { /* open "btc", close, open "xbt@961640" → downcast OpenError::ChainMismatch */ }
#[test] fn refuses_a_v1_database() { /* create rocksdb with CF "outputs" only → OpenError::Schema */ }
#[test] fn bulk_writes_survive_a_clean_close() { /* apply Bulk, drop, reopen → load_blocks len 2, utxos(p2) present */ }
#[test] fn utxos_past_max_rows_is_too_heavy() { /* 11 outputs to p1, utxos(p1, 10) → downcast TooHeavy */ }
#[test] fn unused_program_lookup_returns_empty() {}
```
- [ ] **Step 2:** `cargo test -p fortis-index db::` → FAIL. **Step 3:** Implement. **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: RocksDB schema, apply and reads`.

---

### Task 7: Rollback, undo and pruning (`db.rs`)

**Interfaces — Produces:**
```rust
#[derive(Debug, thiserror::Error)] #[error("no undo for height {height}: reorg deeper than {MAX_REORG_DEPTH}; rebuild with --start-height or restore")] pub struct NoUndo { pub height: u32 }
impl Db {
    pub fn rollback(&self, height: u32, rec: &BlockRec, new_tip: Option<(u32, bitcoin::BlockHash)>) -> anyhow::Result<()>; // one durable WriteBatch
    pub fn prune_undo(&self, below: u32) -> anyhow::Result<()>;
}
```
Rollback deletes added history/utxo keys, restores deleted UTXOs, deletes `txids` and `render` for `rec.first_txnum .. first_txnum + n_txs`, deletes `blocks[height]` and `undo[height]`, sets `meta.tip`.

- [ ] **Step 1: Write the failing tests**
```rust
#[test] fn rollback_restores_the_previous_state_exactly() {
    // apply 100 and 101 with keep_undo; render_put(1, b"x"); rollback(101) →
    // history(p1)==[0]; utxos(p1)==[(A:1,{500,100})]; utxos(p2) empty; txid(1)==None; render_get(1)==None;
    // load_blocks len 1; tip()==Some((100,h100))
}
#[test] fn rollback_without_undo_is_no_undo() { /* apply Bulk without undo, rollback → downcast NoUndo{height:101} */ }
#[test] fn prune_drops_old_undo_only() { /* undo for 100 and 101; prune_undo(101) → rollback(101) Ok, then rollback(100) is NoUndo */ }
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: undo-based rollback`.

---

### Task 8: In-memory chain and consistent views (`chainstate.rs`)

**Interfaces — Consumes:** `Db`, `Reader`, `BlockRec`. **Produces:**
```rust
#[derive(Clone, Default)] pub struct BlockTable { /* start: u32, recs: Vec<BlockRec> */ }
impl BlockTable {
    pub fn from_db(rows: Vec<(u32, BlockRec)>) -> anyhow::Result<Self>;   // heights must be contiguous
    pub fn tip(&self) -> Option<(u32, &BlockRec)>;
    pub fn get(&self, h: u32) -> Option<&BlockRec>;
    pub fn next_txnum(&self) -> TxNum;
    pub fn height_of(&self, n: TxNum) -> Option<u32>;   // block with first_txnum <= n < first_txnum+n_txs
    pub fn push(&mut self, h: u32, rec: BlockRec);       // h must be tip+1
    pub fn pop(&mut self) -> Option<(u32, BlockRec)>;
}
pub type SharedChain = std::sync::Arc<arc_swap::ArcSwap<BlockTable>>;
pub struct View<'a> { pub chain: std::sync::Arc<BlockTable>, pub reader: Reader<'a> }
pub fn view<'a>(db: &'a Db, chain: &SharedChain) -> anyhow::Result<View<'a>>;  // retry until reader.tip() == chain.tip(); Err after 1000 tries
```

- [ ] **Step 1: Write the failing tests**
```rust
#[test] fn height_of_skips_blocks_with_no_indexed_txs() { /* recs: h10 {0,2}, h11 {2,0}, h12 {2,3} → height_of(1)==10, (2)==12, (4)==12, (5)==None */ }
#[test] fn from_db_rejects_a_gap() {}
#[test] fn view_never_pairs_a_table_with_a_newer_snapshot() {
    // db at 101, chain published at 100 → view() Errs after retries;
    // spawn a thread that publishes 101 after 5 ms → view() Ok with chain tip 101
}
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement (add `arc-swap = "1.9"`). **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: block table and consistent views`.

---

### Task 9: Sources and the fetch pipeline (`source.rs`, `fetch.rs`)

**Interfaces — Consumes:** `Rpc::{call_batch, fresh}`, `parse_and_extract`, `HeaderFormat`. **Produces:**
```rust
// source.rs
pub trait BlockSource: Send + Sync {
    fn tip(&self) -> anyhow::Result<u32>;
    fn hashes(&self, from: u32, count: u32) -> anyhow::Result<Vec<bitcoin::BlockHash>>; // one batch RPC, ≤1000
    fn raw_block(&self, hash: &bitcoin::BlockHash) -> anyhow::Result<Vec<u8>>;        // getblock <hash> 0
    fn wait_for_block(&self, timeout_ms: u64) -> anyhow::Result<()>;                   // waitfornewblock
}
pub struct RpcSource { /* one Rpc per thread via thread_local or a pool of fresh() agents */ }
impl RpcSource { pub fn new(rpc: fortis_node::Rpc) -> Self; }
// fetch.rs
pub struct FetchConfig { pub workers: usize }
pub fn fetch_range(src: std::sync::Arc<dyn BlockSource>, fmt: HeaderFormat, from: u32, to: u32, cfg: &FetchConfig, stop: std::sync::Arc<std::sync::atomic::AtomicBool>)
    -> crossbeam_channel::Receiver<anyhow::Result<ParsedBlock>>;   // strictly ascending heights; first Err ends the stream
```
Channel `bounded(4 * workers)`; reorder buffer keyed by height. Hash lookup runs ahead in 1000-height batches.

- [ ] **Step 1: Write the failing tests** (`MemSource` in `source.rs` `#[cfg(test)] pub mod mem`: a `Vec` of serialized synthetic 80-byte blocks, optional per-height delay and failure height — reused by Task 10)
```rust
#[test] fn delivers_every_block_in_height_order_despite_random_worker_delays() { /* 300 blocks, 8 workers, delays 0–3 ms → heights == 0..300 */ }
#[test] fn an_error_ends_the_stream_after_the_good_prefix() { /* fail at 50 → 50 Ok then 1 Err then closed */ }
#[test] fn stop_flag_closes_the_stream_promptly() { /* set after 10 received → closed within 200 ms, < 300 delivered */ }
#[test] fn never_buffers_more_than_the_channel_bound() { /* consumer sleeps; MemSource counts in-flight raw_block calls ≤ workers + 4*workers + workers */ }
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement (add `crossbeam-channel = "0.5"`). **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: parallel ordered block fetch`.

---

### Task 10: The syncer (`sync.rs`)

**Interfaces — Consumes:** Tasks 6–9. **Produces:**
```rust
pub struct Backoff { pub min: std::time::Duration, pub max: std::time::Duration }   // default 1 s / 30 s
pub struct SyncStatus { pub node_tip: std::sync::atomic::AtomicU32, pub follow: std::sync::atomic::AtomicBool }
pub struct Syncer { pub db: Db, pub src: std::sync::Arc<dyn BlockSource>, pub fmt: HeaderFormat, pub chain: SharedChain,
                    pub start_height: u32, pub fetch: FetchConfig, pub stop_height: Option<u32>, pub backoff: Backoff,
                    pub status: std::sync::Arc<SyncStatus> }
impl Syncer {
    pub fn step(&mut self, stop: &std::sync::atomic::AtomicBool) -> anyhow::Result<u32>;  // one pass: reorg check, catch up; returns blocks applied
    pub fn run(&mut self, stop: &std::sync::atomic::AtomicBool, after_pass: &mut dyn FnMut()) -> anyhow::Result<()>; // loop until stop / stop_height; Err only for fatal
}
pub fn is_fatal(e: &anyhow::Error) -> bool;   // NoUndo | OpenError
```
Bulk while `node_tip - our_tip > MAX_REORG_DEPTH` (64 blocks per `apply`, `keep_undo=false`); otherwise one block per `apply`, `Durable`, `keep_undo=true`, then `prune_undo(tip - MAX_REORG_DEPTH)`. First entry into follow: `flush()` + `compact_background()`, log `index: caught up at <h>, following`. Reorg: while node hash at our tip ≠ ours → `rollback` + publish. Commit before publish, always. `run` sleeps via `wait_for_block(1000)` between passes and calls `after_pass` (mempool refresh) after each.

- [ ] **Step 1: Write the failing tests** (`MemSource` with mutable chain; MAX depth constant used as-is)
```rust
#[test] fn syncs_from_start_height_to_tip() { /* 1000 blocks, start 10 → chain tip 999, chain.get(10) exists, get(9) None */ }
#[test] fn blocks_within_288_of_tip_get_undo() { /* tip 1000: rollback(711) is NoUndo, rollback(1000) Ok */ }
#[test] fn reorg_of_two_blocks_matches_a_fresh_sync() { /* sync, replace blocks 999–1000 with different txs, step → every program's history/utxos equal a fresh Db synced on the new chain */ }
#[test] fn reorg_deeper_than_undo_is_fatal() { /* replace last 300 → step Err, is_fatal */ }
#[test] fn transient_source_errors_back_off_and_recover() { /* tip() fails 3×, backoff 1–5 ms → run reaches tip; ≥3 sleeps recorded */ }
#[test] fn stop_height_stops_cleanly() {}
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: syncer with bulk/follow modes and reorgs`.

---

### Task 11: Mempool overlay (`mempool.rs`)

**Interfaces — Consumes:** `tx_rows`, `TxRows`. **Produces:**
```rust
pub trait MempoolSource: Send + Sync {
    fn snapshot(&self) -> anyhow::Result<(u64, Vec<bitcoin::Txid>)>;                         // getrawmempool false true
    fn raw_txs(&self, ids: &[bitcoin::Txid]) -> anyhow::Result<Vec<Option<Vec<u8>>>>;         // batch getrawtransaction <id> 0, ≤500 per RPC
}
impl MempoolSource for RpcSource {}
#[derive(Default)] pub struct MempoolView { /* seq, txs: HashMap<Txid, Arc<TxRows>>, by_program, spent: HashSet<OutPoint>, outputs: HashMap<OutPoint,(Program,u64)> */ }
impl MempoolView {
    pub fn len(&self) -> usize;
    pub fn txids_for(&self, p: &Program) -> Vec<bitcoin::Txid>;
    pub fn utxos_for(&self, p: &Program) -> Vec<(bitcoin::OutPoint, u64)>;   // created by mempool, not spent by another mempool tx
    pub fn is_spent(&self, op: &bitcoin::OutPoint) -> bool;
    pub fn output_at(&self, op: &bitcoin::OutPoint) -> Option<(Program, u64)>;
}
#[derive(Default)] pub struct MempoolTracker { /* rows by txid, ids known to touch nothing, last seq */ }
impl MempoolTracker { pub fn refresh(&mut self, src: &dyn MempoolSource) -> anyhow::Result<Option<MempoolView>>; } // None if seq unchanged
pub type SharedMempool = std::sync::Arc<arc_swap::ArcSwap<MempoolView>>;
```

- [ ] **Step 1: Write the failing tests** (fake source with call counters)
```rust
#[test] fn a_new_tx_funding_a_program_is_a_pending_utxo() {}
#[test] fn a_chained_mempool_spend_hides_the_parent_output() {}
#[test] fn replaced_tx_releases_its_spent_coin() { /* tx1 spends confirmed C (program p); next snapshot has tx2 instead → !is_spent(C); tx1's outputs gone */ }
#[test] fn unchanged_sequence_fetches_nothing() { /* second refresh with same seq → None, raw_txs calls == 1 */ }
#[test] fn txs_touching_nothing_are_fetched_once() {}
#[test] fn a_tx_that_vanished_before_fetch_is_skipped() { /* raw_txs returns None for it → Ok, not in view */ }
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: sequence-diffed mempool overlay`.

---

### Task 12: Transaction rendering (`render.rs`)

**Interfaces — Consumes:** `View`, `MempoolView`, `spend_program`. **Produces:**
```rust
pub trait TxSource: Send + Sync { fn tx_verbose(&self, txid: &bitcoin::Txid, block: Option<&bitcoin::BlockHash>) -> anyhow::Result<serde_json::Value>; } // getrawtransaction <id> 2 [block]
impl TxSource for RpcSource {}
pub struct Renderer { /* src, db, 8 RPC slots, pending cache (bounded 20_000, keyed by txid) */ }
impl Renderer {
    pub fn new(src: std::sync::Arc<dyn TxSource>, db: Db) -> Self;
    pub fn confirmed(&self, v: &View, ns: &[TxNum]) -> anyhow::Result<Vec<serde_json::Value>>;  // order kept; DB render cache, misses ≤8 in parallel then render_put
    pub fn pending(&self, v: &View, mp: &MempoolView, ids: &[bitcoin::Txid]) -> anyhow::Result<Vec<serde_json::Value>>; // prevouts back-filled via spend_program + reader.utxo / mp.output_at
    pub fn forget_pending_not_in(&self, mp: &MempoolView);
}
pub fn esplora_tx(t: &serde_json::Value, status: serde_json::Value) -> serde_json::Value;   // ported from v1 api.rs, shape unchanged
```
Status for confirmed = `{"confirmed":true,"block_height":h,"block_time":rec.time}` from the block table (not the tx JSON).

- [ ] **Step 1: Write the failing tests** — port v1 `api.rs` tests `spk_hex_to_address_round_trips_a_p2wpkh`, `backfill_fills_a_null_mempool_prevout_from_the_confirmed_index` (now via `reader.utxo`), `backfill_leaves_a_prevout_the_node_already_gave_us_untouched`, `a_failed_fetch_fails_the_whole_call…`; add:
```rust
#[test] fn confirmed_render_is_cached_across_restarts() { /* render n=0 → 1 source call; reopen Db, new Renderer → 0 more calls, same JSON */ }
#[test] fn confirmed_keeps_row_order_with_parallel_misses() { /* 20 txnums, source sleeps random 0–3 ms */ }
#[test] fn status_block_time_comes_from_the_block_table() {}
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** → PASS.
- [ ] **Step 5:** Commit `index-v2: Esplora tx rendering with persistent cache`.

---

### Task 13: HTTP API and process wiring (`api.rs`, `main.rs`)

**Files:** Create `src/api.rs`; rewrite `src/main.rs`; delete `src/store.rs`, `src/store/`, old `src/sync.rs`, `src/mempool.rs` (already replaced), v1 code in `api.rs`. Add `axum = "0.8"`, `tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal", "net"] }`, `tower = { version = "0.5", features = ["limit"] }`.

**Interfaces — Consumes:** everything above. **Produces:**
```rust
#[derive(Clone)] pub struct AppState { pub db: Db, pub chain: SharedChain, pub mempool: SharedMempool, pub renderer: std::sync::Arc<Renderer>,
                                       pub rpc: fortis_node::Rpc, pub network: bitcoin::Network, pub status: std::sync::Arc<SyncStatus>, pub chain_id: String }
pub fn router(state: AppState) -> axum::Router;
pub const SCAN_MAX_ADDRESSES: usize = 1000; pub const SCAN_DEFAULT_HISTORY: usize = 50; pub const SCAN_MAX_HISTORY: usize = 100;
pub const TXS_HISTORY: usize = 100; pub const MAX_ROWS: usize = 10_000;
```
Routes and semantics as v1 (`api.rs` doc comments), except: `GET /` → `{"name","version","tip","node_tip","mode":"bulk"|"follow","chain","mempool"}`; heavy address → 400 `{"error":"address too heavy"}`; `view()` failure → 503. All blocking work in `tokio::task::spawn_blocking`; `ConcurrencyLimitLayer(64)`; `DefaultBodyLimit::max(2 * 1024 * 1024)`.
`main.rs`: CLI per spec (`--fetch-workers 8`, `--cache-mb 1024`, `--stop-height`; `--poll` removed); RPC from `--rpc-auth`, else `Rpc::from_cookie_file` (`--cookie-file`, or `<datadir>/.cookie` / `<datadir>/regtest/.cookie`) so a node restart needs no index restart; `HeaderFormat::detect` → `Db::open(.., chain_id)`; sync thread runs `Syncer::run` with `after_pass` = mempool refresh + `ArcSwap::store` + `renderer.forget_pending_not_in`; SIGTERM/SIGINT → stop flag → join sync thread → `db.flush()` → exit 0; fatal (`is_fatal`) → `eprintln!` + exit 2.

- [ ] **Step 1: Write the failing tests** (`tower::ServiceExt::oneshot`; state from temp `Db` + fake `TxSource` + `MempoolView`) — port v1 `scan_reports_used_addresses_and_only_unspent_outputs`, `scan_lists_each_tx_once_newest_first_even_when_it_touches_two_addresses` (order now txnum desc: `cc, bb, aa` still holds — distinct heights), `scan_caps_history_to_the_newest_n_overall`, `scan_route_rejects_bad_input_before_touching_the_index`; add:
```rust
#[tokio::test] async fn utxo_route_has_the_v1_shape() { /* body == [{"txid":..,"vout":0,"value":480,"status":{"confirmed":true,"block_height":101}}] */ }
#[tokio::test] async fn every_response_carries_cors_headers_including_errors() { /* 404 route and 400 address */ }
#[tokio::test] async fn heavy_address_is_refused_without_stalling_others() { /* 10_001 outputs to p; /address/<p>/utxo → 400 "address too heavy"; concurrent /blocks/tip/height < 100 ms */ }
#[tokio::test] async fn a_slow_node_rpc_does_not_block_other_requests() { /* TxSource sleeps 500 ms; /txs in flight; /blocks/tip/height < 100 ms */ }
#[tokio::test] async fn pending_utxos_show_unconfirmed_and_hide_mempool_spent() {}
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement; delete v1 files. **Step 4:** `cargo test -p fortis-index` → all PASS; `cargo clippy -p fortis-index -- -D warnings` clean.
- [ ] **Step 5:** Commit `index-v2: axum API and process wiring; drop v1 store`.

---

### Task 14: Regtest end-to-end (`tests/regtest.rs`)

**Files:** Create `crates/fortis-index/tests/regtest.rs`. Copy node-harness helpers from `crates/fortis-edge/tests/regtest_e2e.rs:31-200` (`free_port`, `bin`, `Node`, `start_node`, `spawn`, `poll_json`); opt-in via `FORTIS_BITCOIND` (Knots, launched with `-testactivationheight=blake2b@1` → 164-byte headers) and `FORTIS_BITCOIND_CORE` (80-byte). Each test skips with a message when its variable is unset.

**Interfaces — Consumes:** the `fortis-index` binary (`--network regtest --start-height 0`).

- [ ] **Step 1: Write the tests** (same body run once per node kind)
```rust
#[test] fn receive_spend_and_reorg_over_real_nodes() {
    // wallet A funds addr X → /address/X/utxo has confirmed:false within 3 s; mine 1 → confirmed, block_height == tip
    // spend X's coin → X utxo empty; /address/X/txs lists 2 txs; spend tx vin[0].prevout == {X, value}; fee > 0
    // invalidateblock tip; mine 2 → /blocks/tip/height == node height and history heights match node within 5 s
}
#[test] fn kill_minus_nine_during_bulk_sync_recovers() {
    // mine 600 blocks with a tx each; start index; SIGKILL after first "index: +" log line; restart;
    // poll until tip == 600; run `fortis-index verify --sample 50` → exit 0
}
```
- [ ] **Step 2:** `FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind FORTIS_BITCOIND_CORE=/opt/bitcoin-core/current/bin/bitcoind cargo test -p fortis-index --test regtest -- --nocapture --test-threads 1` → FAIL until Task 15 provides `verify` (the first test should already PASS; fix anything it exposes).
- [ ] **Step 3:** Commit `index-v2: regtest end-to-end`.

---

### Task 15: `verify` subcommand (`verify.rs`)

**Interfaces — Produces:**
```rust
#[derive(Debug, PartialEq)] pub enum Mismatch { Missing { address: String, outpoint: bitcoin::OutPoint }, Extra { address: String, outpoint: bitcoin::OutPoint }, Value { address: String, outpoint: bitcoin::OutPoint, ours: u64, node: u64 } }
pub fn diff_utxos(address: &str, ours: &[(bitcoin::OutPoint, u64)], node: &[(bitcoin::OutPoint, u64)]) -> Vec<Mismatch>;
pub fn sample_programs(r: &Reader, n: usize, rng_seed: u64) -> anyhow::Result<Vec<Program>>; // half random seeks into utxo, half into history
pub fn run(db_path: &std::path::Path, rpc: &fortis_node::Rpc, network: bitcoin::Network, sample: usize) -> anyhow::Result<Vec<Mismatch>>;
```
Opens the DB as a RocksDB **secondary** (works while the service runs), waits until its tip == node tip (re-catching up with the primary), runs one `scantxoutset start [addr(..)…]`, retries (max 3) if the node tip moved during the scan. CLI: `fortis-index verify --sample N [--db --rpc-url --cookie-file --rpc-auth --network]`; prints mismatches, exit 0 if none else 1.

- [ ] **Step 1: Write the failing tests**
```rust
#[test] fn diff_finds_missing_extra_and_value_mismatches() {}
#[test] fn sample_programs_is_deterministic_for_a_seed_and_unique() {}
```
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement + wire subcommand. **Step 4:** unit tests PASS; Task 14 command → both regtest tests PASS for Knots and Core.
- [ ] **Step 5:** Commit `index-v2: verify against scantxoutset`.

---

### Task 16: Mainnet measurement, edge compatibility, deploy

**Files:** Modify `deploy/linux-home/fortis-{xbt,btc}-index.service` (`--db /var/lib/fortis/{xbt,btc}-index-v2`, add `RestartPreventExitStatus=2`, `TimeoutStopSec=120`), `crates/fortis-index/README.md` (v2 architecture, flags, `verify`), `deploy/README.md` (Knots RPC port note, v2 paths), `CHANGELOG.md`.

- [ ] **Step 1: Edge compatibility.** `FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind cargo test -p fortis-edge --test regtest_e2e -- --nocapture` → PASS.
- [ ] **Step 2: Throughput benchmark** (Core, post-2020 blocks):
```bash
S=$(mktemp -d); time ./target/release/fortis-index --rpc-url http://127.0.0.1:8332 \
  --cookie-file /var/lib/bitcoin-core/.cookie --db $S/db --bind 127.0.0.1:18095 \
  --start-height 850000 --stop-height 860000; du -sh $S/db
```
Expected: ≥ 100 blocks/s (≤ 100 s). Record blocks/s and DB size in the commit message. If below target, profile (`perf top -p`) before changing anything and report the bottleneck rather than tuning blind.
- [ ] **Step 3: Full syncs** of both chains from 481 824 into the v2 paths (background), recording wall time; then `fortis-index verify --sample 500` against each → exit 0.
- [ ] **Step 4:** Update deploy units/README/CHANGELOG; `bash -n deploy/linux-home/install.sh`.
- [ ] **Step 5:** Commit `index-v2: deploy units, docs, measured results`; open a PR from `index-v2`.
