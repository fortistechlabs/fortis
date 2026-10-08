# fortis-index v2 — design

**Status:** proposed · 2026-10-07
**Replaces:** `crates/fortis-index` storage, sync and HTTP layers. The Esplora-subset
API the edge and wallets speak is kept byte-compatible except where noted.

## Why

Measured on the current index (comments in `sync.rs`): ~0.53 blocks/s, i.e.
~10–11 days from SegWit activation (481 824) to tip, per chain. Causes:

| cause | evidence |
|---|---|
| `getblock <hash> 2` JSON ingest | block 970 000: 14.5 MB JSON in 0.25 s vs 3.6 MB hex in 0.03 s for verbosity 0 (8×), then re-parsed as JSON + hex |
| a read per input during sync (`outputs` lookup) and a read-modify-write of `OutputRecord.spent` | `store.rs::apply_blocks` |
| `fsync` on every batch, including the initial bulk load | `write_opts_synced` |
| single-threaded HTTP (`tiny_http`, one request at a time) holding a mempool lock | `api.rs::serve` |
| mempool cold start = one `getrawtransaction … 2` (~12 ms) per mempool tx, rebuilt and cloned each 10 s poll | `mempool.rs`, `main.rs::indexer_loop` |

## Goals

1. Initial sync from 481 824 to tip in **hours, not days** on this host (16 cores,
   NVMe, local node). Target: ≥ 100 blocks/s sustained on post-2020 blocks; the
   final task measures it.
2. Stable: crash-safe at any instant (kill -9, power loss in follow mode),
   reorg-safe, never spins on errors, refuses a database built for the other chain.
3. Efficient: database materially smaller than v1 (v1 > 115 GB); unused-address
   lookups (most of a 1 000-address scan) cost a bloom-filter check, not a disk seek.
4. Concurrent API: many wallets at once; slow node RPCs never block other requests.
5. One binary for both chains: XBT over Knots (BLAKE2b fork, 164-byte headers from
   961 640) and BTC over Core.

## Non-goals

Address types other than P2WPKH (the wallet derives only BIP-84). Pagination beyond
the current "newest 100". Electrum protocol. Serving pre-481 824 history.

## Key insight: spends need no lookup

A P2WPKH spend has an empty `scriptSig` and exactly two witness items, and consensus
requires `HASH160(witness[1]) == program`. So for every input with an empty scriptSig
and a 2-item witness, `program = HASH160(witness[1])` identifies the spent address
with **no read**. False positives (e.g. a P2WSH spend whose witness script is the 2nd
item) yield a program nobody can own — harmless rows. False negatives are impossible
for real P2WPKH spends. Result: every sync write is a blind put or blind delete.

## Data model (RocksDB 0.25, one DB, column families)

`TxNum` = sequential number assigned (in chain order) to each transaction that funds
or spends at least one P2WPKH program; encoded as 5 bytes big-endian (max 2^40).
`Program` = the 20-byte P2WPKH witness program.

| CF | key | value | notes |
|---|---|---|---|
| `meta` | `b"schema"`, `b"chain"`, `b"tip"` | u32 / chain id / height u32 | `chain` = `"btc"` or `"xbt@<fork height>"`; mismatch on open = refuse |
| `blocks` | height u32 BE | hash 32 · time u32 · first_txnum 5 · n_txs u32 | whole CF loaded into memory at start (~25 MB) |
| `txids` | txnum 5 | txid 32 | sequential keys — append-only pattern |
| `history` | program 20 · txnum 5 | — | funding *and* spending txs; prefix-bloom on 20 bytes |
| `utxo` | program 20 · txid 32 · vout u32 BE | value u64 · height u32 | put on fund, **blind delete** on spend |
| `undo` | height u32 | encoded `Undo` | only for blocks within `MAX_REORG_DEPTH` (288) of the node tip |
| `render` | txnum 5 | Esplora tx JSON | persistent cache of rendered confirmed txs |

Height of a txnum = binary search over the in-memory `blocks` table's `first_txnum`.
History order = txnum descending (chain order, newest first; within one block, later
tx first — v1 used txid ascending within a height; the wallet does not depend on it).

## Sync

- **Fetch pipeline:** a hash stage (JSON-RPC batch of up to 1 000 `getblockhash`),
  `--fetch-workers` (default 8) workers each with its own `ureq::Agent` doing
  `getblock <hash> 0`, decoding hex, parsing (manual header of 80 or 164 bytes,
  then `bitcoin` crate tx decoding; trailing bytes = error) and extracting rows; a
  reorder buffer delivers `ParsedBlock`s to the single writer strictly in height
  order through a bounded channel (memory bound ≈ 4 × workers blocks).
- **Bulk mode** (node tip − our tip > `MAX_REORG_DEPTH`): WAL disabled,
  `atomic_flush = true`, ~64 blocks per `WriteBatch`, no undo records. Each batch
  also writes `meta.tip`, so whatever RocksDB has flushed is a consistent prefix;
  after a crash, sync resumes from the flushed `meta.tip`.
- **Follow mode** (within `MAX_REORG_DEPTH`): WAL on, `sync = true`, one block per
  batch, `Undo` written (keys added + UTXOs deleted with their values, read via
  `multi_get` before the delete). Undo older than tip − 288 is pruned.
- On entering follow mode for the first time: flush, then `compact_range` on
  `history` and `utxo` in the background.
- **Tip notification:** `waitfornewblock` (1 s timeout) long-poll — no ZMQ dependency.
- **Reorg:** before applying, if node's hash at our tip ≠ ours, roll back one block
  at a time (using `undo`) until hashes agree. Needing undo that doesn't exist (reorg
  deeper than 288) is a fatal error naming the height — never silent corruption.
- **Errors:** transient (RPC down, timeout) → exponential backoff 1 s → 30 s, reset on
  success; with cookie auth a 401 re-reads the cookie file once before failing, so a
  node restart needs no index restart. Fatal (missing undo, chain mismatch, schema
  mismatch) → exit code 2, which the systemd units mark `RestartPreventExitStatus=2`.
- **Shutdown:** SIGTERM/SIGINT → finish the current batch, flush, exit 0.
- **Header format:** `getdeploymentinfo` → `deployments.blake2b.height` if present ⇒
  164-byte headers from that height, else 80. Prev-hash at bytes 4..36, time at
  68..72 in both formats (verified against Knots block 976 000).
- **Chain guard:** `meta.chain` stores the profile; opening an XBT DB against Core (or
  vice versa) is refused at startup.

## Consistency for readers

The writer publishes the in-memory block table through `ArcSwap` (`chain`), always
**commit first, then publish** (apply and rollback alike; every batch writes
`meta.tip = height · hash`). A reader loads `chain`, takes a RocksDB snapshot, and
accepts the pair only if the snapshot's `meta.tip` equals the table's tip; otherwise
it retries (the window is the microseconds between commit and publish; after 1 000
tries it answers 503). Every request therefore sees one exact chain state with no
locks — including across a rollback that reassigns txnums.

Rollback and its undo restore are one `WriteBatch`, which also deletes the `txids`
and `render` rows of the rolled-back txnums.

## Mempool

`getrawmempool false true` (txids + sequence) after each block-sync pass and at
least every 2 s; new txids fetched raw (`getrawtransaction <txid> 0`) by JSON-RPC
batch of 500, extracted with the same `tx_rows` as blocks. Snapshot published via
`ArcSwap<MempoolView>`; requests never hold a lock. Pending-tx JSON is rendered
lazily (`getrawtransaction <txid> 2`, missing prevouts back-filled from `utxo` /
mempool outputs) and cached per txid while the tx stays in the mempool.

## API

`axum` 0.8 on `tokio`; blocking DB/RPC work in `spawn_blocking`; global concurrency
limit 64, node-RPC semaphore 8, request body cap 2 MB. Routes unchanged:
`GET /`, `GET /blocks/tip/height`, `GET /address/:a/utxo`, `GET /address/:a/txs`,
`GET /v1/fees/recommended`, `POST /tx`, `POST /scan`. CORS headers unchanged.
`GET /` adds `node_tip`, `mode` (`bulk`|`follow`), `chain`. An address with more
than 10 000 UTXO or history rows scanned answers 400 `address too heavy`.

## CLI

Kept: `--rpc-url --datadir --cookie-file --rpc-auth --network --db --bind
--start-height`. Removed: `--poll`. Added: `--fetch-workers` (8), `--cache-mb`
(1024), `--stop-height` (benchmark/testing), and subcommand
`verify --sample N` (compare UTXOs of N sampled addresses against the node's
`scantxoutset`).

## Migration

New on-disk format (`schema = 2`) in a new directory; v1 databases are refused with
a message. The Windows v1 XBT index is not reusable — the v2 sync replaces it.
`deploy/linux-home` units move to `/var/lib/fortis/{xbt,btc}-index-v2`.
