# fortis-index

A P2WPKH address index over a Bitcoin Core node (BTC) or a Bitcoin Knots /
BLAKE2b node (XBT), served as the same Esplora REST subset the fortis wallet's
`EsploraBackend` already speaks.

Where [`fortisd`](../fortisd)'s gateway holds one account at a time (it imports an
xpub as a watch-only descriptor on the node), `fortis-index` keeps **no per-user
state** — the client derives its own addresses and scans them, so one instance
serves any number of wallets concurrently.

Design and rationale: [`docs/superpowers/specs/2026-10-07-fortis-index-v2-design.md`](../../docs/superpowers/specs/2026-10-07-fortis-index-v2-design.md).

## How it works

- **Raw blocks, parsed here.** `getblock <hash> 0` is fetched by
  `--fetch-workers` parallel workers and parsed by hand: 80-byte headers, or
  164-byte headers from the BLAKE2b fork height, which is read from the node's
  `getdeploymentinfo` (`blake2b.height`) at startup. A reorder stage hands blocks
  to a single writer strictly in height order.
- **P2WPKH only.** The wallet derives exclusively BIP-84 P2WPKH addresses, so
  nothing else is stored. A P2WPKH spend names its address in its witness
  (`HASH160(witness[1])`), so every write is a blind put or delete — sync never
  reads the database.
- **RocksDB, column families** `meta · blocks · txids · history · utxo · undo ·
  render`, keyed by the 20-byte program and a sequential 5-byte transaction
  number. Unused-address lookups cost a prefix-bloom check, not a seek.
- **Bulk, then follow.** Far from the node tip it writes 64 blocks per batch
  without a WAL; within 288 blocks it writes each block durably with an undo
  record and rolls back on reorgs. A reorg deeper than the undo window is fatal
  (exit 2), never silent corruption.
- **Crash-safe.** Every batch records its tip; after a crash (even `kill -9`) the
  index resumes from the last flushed tip.
- **Mempool overlay.** It diffs the node's mempool by sequence number, so only new
  transactions are fetched. Pending outputs show with `status.confirmed = false`,
  and a confirmed coin spent by a pending transaction is hidden.
- **Concurrent API.** It runs on `axum`/`tokio`. Each request reads one consistent
  chain state without locks, and slow node RPCs (transaction detail) run off the
  request path, bounded to 8 at a time. Confirmed transaction JSON is cached in
  the DB.
- **Guarded.** It refuses a database built for the other chain or by v1 (exit 2).
  With cookie auth, a node restart (rotated `.cookie`) is picked up without
  restarting the index.

## Scope

It indexes from `--start-height` forward: **SegWit activation, block 481824, by
default** (0 on regtest). This includes a restored seed's pre-fork Bitcoin history
on XBT. P2WPKH didn't exist before SegWit, so earlier blocks can't hold any fortis
wallet history. The node must be unpruned from the start height.

## Run

```sh
cargo build --release -p fortis-index
# BTC over Core
./target/release/fortis-index --rpc-url http://127.0.0.1:8332 \
  --cookie-file /var/lib/bitcoin-core/.cookie --db btc-index-v2 --bind 127.0.0.1:8095
# XBT over Knots (RPC :8432 on the Linux home host)
./target/release/fortis-index --rpc-url http://127.0.0.1:8432 \
  --cookie-file /var/lib/bitcoin-knots/.cookie --db xbt-index-v2 --bind 127.0.0.1:8094
# regtest:  --network regtest --start-height 0
```

| flag | default | |
|---|---|---|
| `--rpc-url` | `127.0.0.1:8332` (`:18443` regtest) | node RPC |
| `--cookie-file` / `--datadir` / `--rpc-auth` | `<datadir>/.cookie` | RPC auth; a cookie is re-read on HTTP 401 |
| `--network` | `mainnet` | or `regtest` |
| `--db` | `fortis-index-rocksdb` | RocksDB directory (a v1 directory is refused) |
| `--bind` | `127.0.0.1:8094` | HTTP API |
| `--start-height` | 481824 (0 regtest) | first block indexed |
| `--fetch-workers` | 8 | parallel block fetchers |
| `--cache-mb` | 1024 | RocksDB block cache |
| `--stop-height` | — | index up to this height, then exit 0 (benchmarks/tests) |

Exit codes: `0` stopped (SIGTERM/SIGINT finish the current batch and flush), `2`
fatal (wrong chain or schema in `--db`, or a reorg deeper than 288 blocks; don't
restart), anything else is retryable.

### Verify

```sh
fortis-index verify --sample 500 --db xbt-index-v2 \
  --rpc-url http://127.0.0.1:8432 --cookie-file /var/lib/bitcoin-knots/.cookie
```

This samples N indexed addresses and compares their UTXOs with the node's
`scantxoutset`. It opens the DB as a RocksDB secondary, so it's safe to run while
the service is running, and it waits until the index has reached the node tip.
It prints any mismatches and exits 0 only when there are none.

## Routes

| | |
|---|---|
| `GET /` | `{ name, version, tip, node_tip, mode: "bulk"\|"follow", chain, mempool }` |
| `GET /blocks/tip/height` | indexed tip height (plain text) |
| `GET /address/:addr/utxo` | `[{ txid, vout, value, status:{ confirmed, block_height } }]` |
| `GET /address/:addr/txs` | pending, then the newest 100 confirmed; Esplora tx shape (`vin[].prevout`, `vout`, `fee`, `status`) |
| `POST /scan` | `{"addresses":[…≤1000], "history":≤100}` → `{ tip, used, utxos, txs, failed }` |
| `GET /v1/fees/recommended` | `{ fastestFee, halfHourFee, hourFee, economyFee, minimumFee }` from the node |
| `POST /tx` | broadcast (raw hex body → txid) |

An address with more than 10 000 UTXOs answers `400 {"error":"address too heavy"}`.
`503` means the index is between two states (retry). CORS is open (`*`).

Public data only — no auth. Bind to localhost / a trusted network, or front it
with `fortis-edge`.

## Tests

`cargo test -p fortis-index` runs the unit tests. The end-to-end tests over real
regtest nodes are opt-in:

```sh
FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind \
FORTIS_BITCOIND_CORE=/opt/bitcoin-core/current/bin/bitcoind \
  cargo test -p fortis-index --test regtest -- --nocapture --test-threads 1
```
