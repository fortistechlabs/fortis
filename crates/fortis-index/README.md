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
  the sync loop writes, the HTTP server reads) — **P2WPKH outputs only**. The
  wallet derives exclusively BIP-84 P2WPKH addresses (`Address::p2wpkh`,
  `wallet-core/src/wallet.rs`), so nothing else (OP_RETURN, legacy P2PKH,
  P2SH, Taproot/inscriptions, bare multisig) can ever be a fortis wallet
  address — storing it is pure waste. Same filter applies to the in-memory
  mempool overlay below.
- Reorg-safe: each block must extend our tip or the index unwinds one block and
  retries; a reorg back past `--start-height` clears and re-syncs.
- Keeps an in-memory **mempool overlay**, refreshed each poll: unconfirmed
  outputs show up in `/address/:a/utxo` with `status.confirmed = false`, and a
  confirmed coin already spent by a pending tx is dropped so it isn't offered
  for coin selection again.
- `/address/:a/txs` stores only txid lists and rebuilds full detail on read from
  `getrawtransaction <txid> 2 <blockhash>` (no `txindex` needed on the node).

## Scope

Indexes from `--start-height` forward — **SegWit activation, block 481824, by
default** — so a restored old seed's pre-fork Bitcoin history (inherited by the
XBT chain at the hard fork) is included, not just activity since the fork.
Genesis-to-SegWit blocks are skipped on purpose, not just as a speed
optimization: P2WPKH (the only address type this wallet ever derives) didn't
exist before SegWit, so no fortis wallet address can possibly have history
there — indexing that range can never find anything. The node still needs the
complete, unpruned pre-fork block history from 481824 forward, and the initial
catch-up is a real one-time cost. Pass `--start-height 961640` (the BLAKE2b
fork height) to skip pre-fork blocks entirely and index even faster when
pre-fork coins don't matter (e.g. regtest, or a deployment that only ever
expects post-fork wallets).

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
