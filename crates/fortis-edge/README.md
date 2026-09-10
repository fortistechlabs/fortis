# fortis-edge

The public front for the fortis wallet backends. It sits in front of a
[`fortis-index`](../fortis-index) instance (XBT) and an Esplora upstream (BTC — a
public explorer, or a [`fortisd --esplora-proxy`](../fortisd)) and adds what a
backend exposed to many wallets needs.

```
wallet ──HTTPS──▶ reverse proxy (TLS) ──▶ fortis-edge ─┬─▶ /xbt/*  fortis-index
                                                       └─▶ /btc/*  Esplora upstream
```

- **Per-install tokens** — `POST /register` mints an anonymous
  `<id>.<hmac>` token. Verified statelessly with the shared secret (no DB
  lookup), so instances scale out horizontally. `--require-token` enforces it on
  `/{xbt,btc}/*`.
- **Rate limiting** — token-bucket per token (or per client IP when untokened);
  a stricter bucket on `/register` per IP. **Outbound**, `--btc-upstream-rate`
  (default 5/s, `0` off) paces `/btc/address/*` so a wallet's ~40-address gap
  scan doesn't get 429'd by mempool.space — requests queue rather than fail.
- **Batch prewarm** — `POST /btc/prewarm` (a JSON array of the wallet's
  addresses) pulls the whole set from a Haskoin Store (`--btc-haskoin-url`,
  default `https://api.haskoin.com/btc`) in two calls, reshapes it to the
  Esplora `/address/{a}/{utxo,txs}` bodies, and fills the cache — so the client's
  per-address scan is entirely local and `--btc-upstream` is only a fallback.
- **Response caching** — short TTLs (tip 5 s, fees 30 s, prices 60 s; address
  5 s for XBT, 60 s for the paced BTC path) collapse a burst of wallet polls
  into one upstream hit. `POST /tx` is never cached.
- **CORS** (`--allow-origin`) and **`/metrics`** (Prometheus counters:
  requests, registrations, cache hits, rate-limit / auth rejections, upstream
  errors).

TLS is expected from a reverse proxy (Caddy / nginx) in front — the edge speaks
plain HTTP.

## Run

```sh
cargo run -p fortis-edge -- \
  --bind 0.0.0.0:8098 \
  --xbt-upstream http://127.0.0.1:8094 \
  --btc-upstream https://mempool.space/api \
  --require-token --trust-forwarded-for \
  --crash-log /var/log/fortis/crashes.ndjson
```

`--crash-log <file>` enables `POST /crash`, which appends one JSON object per
line (`{ts, ip, report}`) — the app's uncaught-exception reporter posts there.
Without the flag `/crash` is 404. Rotate the file yourself (logrotate / a cron).

`--btc-rpc-url <url>` (+ `--btc-rpc-auth user:pass` or `--btc-rpc-cookie <file>`)
broadcasts `POST /btc/tx` through a local Bitcoin Core / Knots node's
`sendrawtransaction` instead of `--btc-upstream`. A pruned node is fine. Use it
so replay-protected sends (an oversized `OP_RETURN`) reach the network even when
the public Esplora won't relay them — the node itself still has to accept them
(Core 30+, or `-datacarriersize` raised). Address / history / fee reads stay on
`--btc-upstream`.

`--service-fee-address <addr>` turns on the service fee: `GET /pricing`
advertises `{address, bps, floor_sat, cap_sat}` (the client reads it and adds the
percentage output) and `POST /<chain>/tx` is rejected `402` unless the
transaction pays at least `--service-fee-floor-sat` (default 400 — keep it above
the fee address's dust limit, 294 for bech32 P2WPKH) to that address.
`--service-fee-bps` (default 100 = 1%) and `--service-fee-cap-sat` (default 0 =
uncapped) are advertised only. `--network` (default `bitcoin`)
validates the address. Unset → no fee, `/pricing` 404s.

`--btc-price-url` / `--xbt-price-url <url>` set the USD price source for
`GET /{chain}/v1/prices`. The edge fetches the URL, pulls a positive USD spot out
of a mempool `{ "USD": … }` body **or** a Kraken-style `Ticker`
(`result.<pair>.c[0]` — last trade), and returns `{ "USD": <n> }`. Cached 60 s.
Examples: `https://api.kraken.com/0/public/Ticker?pair=XBTUSD` for BTC,
`https://mempool.kilombino.com/api/v1/prices` for XBT (a `fortis-index` has no
feed). Unset → `/btc/v1/prices` proxies to `--btc-upstream/v1/prices`;
`/xbt/v1/prices` 404s and the wallet shows no fiat value.
`--xbt-price-upstream <base>` is a deprecated alias that appends `/v1/prices`.

The wallet points its XBT explorer URL at `https://<host>/xbt` and its BTC one at
`https://<host>/btc`, sending `Authorization: Bearer <token>` (or `?token=<t>`).

## Routes

| | |
|---|---|
| `POST /register` | `{ "token": "<id>.<hmac>" }` |
| `GET \| POST /xbt/<esplora path>` | → XBT upstream |
| `GET \| POST /btc/<esplora path>` | → BTC upstream |
| `GET /{btc,xbt}/v1/prices` | `{ "USD": <n> }` from `--{chain}-price-url` (60 s cache); BTC falls back to the Esplora upstream, XBT 404s |
| `POST /btc/prewarm` | `["addr",…]` → batch-load from Haskoin, fill the address cache, `{ "warmed": <n> }` (404 if `--btc-haskoin-url` empty) |
| `POST /crash` | `204`; appends the body to `--crash-log` (else `404`) |
| `GET /pricing` | `{address, bps, floor_sat, cap_sat}` when a service fee is set (else `404`) |
| `GET /metrics` | Prometheus text |
| `GET /` | health + which chains are served |

## Test

`tests/regtest_e2e.rs` drives a wallet's whole flow through
`fortis-edge → fortis-index → a regtest Knots BLAKE2b node`: register a token,
derive + fund an address, see the confirmed UTXO through the edge, build and
`SIGHASH_UNIFIED`-sign a payment with `wallet-core`, broadcast via `POST
/xbt/tx`, watch the change output appear unconfirmed (mempool overlay) then
confirm, and check the rate limiter rejects a burst. Opt-in:

```sh
cargo build --workspace
FORTIS_BITCOIND="…/bitcoind.exe" cargo test -p fortis-edge --test regtest_e2e
```

## Not yet

Token revocation (would need a blocklist), distributed rate limiting / cache
(in-memory, per instance), built-in TLS, request-size limits, structured request
logging.
