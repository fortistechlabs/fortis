# fortis-index

An address index over a Bitcoin Knots / BLAKE2b node, served as the same Esplora
REST subset the fortis wallet's `EsploraBackend` already speaks.

Where [`fortisd`](../fortisd)'s gateway holds one account at a time (it imports an
xpub as a watch-only descriptor on the node), `fortis-index` keeps **no per-user
state** — the client derives its own addresses and scans them, so one instance
serves any number of wallets concurrently. That's what the XBT side needs to go
multi-tenant, since no public Esplora exists for the fork.

## How it works

- Follows the chain with `getblock <hash> 2` — the node decodes the fork's
  164-byte header and modified PoW, so the indexer never parses a raw block. Each
  transaction's `hex` *is* parsed (tx format is unchanged by the fork) for exact
  amounts and scripts.
- Stores `scriptPubKey → {outputs, spends, txids}` in a SQLite file (WAL mode:
  the sync loop writes, the HTTP server reads).
- Reorg-safe: each block must extend our tip or the index unwinds one block and
  retries; a reorg back past `--start-height` clears and re-syncs.
- Keeps an in-memory **mempool overlay**, refreshed each poll: unconfirmed
  outputs show up in `/address/:a/utxo` with `status.confirmed = false`, and a
  confirmed coin already spent by a pending tx is dropped so it isn't offered
  for coin selection again.
- `/address/:a/txs` stores only txid lists and rebuilds full detail on read from
  `getrawtransaction <txid> 2 <blockhash>` (no `txindex` needed on the node).

## Scope

Indexes from `--start-height` forward — the BLAKE2b fork height (961640) by
default, which covers every wallet created in the app. **Pre-fork coins are out
of scope:** a restored old seed's historical UTXOs won't appear (claiming forked
coins would need a full-history index or a one-off `scantxoutset`).

## Run

```sh
cargo run -p fortis-index -- \
  --datadir "C:\Bitcoin\Knots" --network mainnet \
  --db fortis-index.sqlite --bind 0.0.0.0:8094
# regtest:  --network regtest --datadir <regtest datadir> --start-height 0
```

RPC auth: `--datadir` / `--cookie-file` read the node's `.cookie` (rotates on
restart); `--rpc-auth user:password` uses a static `rpcauth` credential instead —
preferred for a long-running deployment. See [`deploy/`](../../deploy).

Point the wallet's explorer URL at `http://<host>:8094` (or front it with
`fortis-edge`).

## Routes

| | |
|---|---|
| `GET /blocks/tip/height` | indexed tip height (plain text) |
| `GET /address/:addr/utxo` | `[{ txid, vout, value, status:{ confirmed, block_height } }]` |
| `GET /address/:addr/txs` | last 100, Esplora tx shape (`vin[].prevout`, `vout`, `fee`, `status`) |
| `GET /v1/fees/recommended` | `{ fastestFee, halfHourFee, hourFee, economyFee, minimumFee }` from the node |
| `POST /tx` | broadcast (raw hex body → txid) |

Public data only — no auth. Bind to localhost / a trusted network, or front it
with a TLS + rate-limiting proxy.
