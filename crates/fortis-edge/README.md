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

### Swaps

`--swap` turns on peer-to-peer BTC ⇄ XBT atomic swaps under `/swap/*` (off by
default: every `/swap/*` route then 404s). The edge matches offers, relays
messages between the two phones, and runs a **watchtower**. It never holds keys or
coins: each phone funds its own HTLC and signs its own spends.

- **Price.** NeoxEX `BTCB2_BTC`: the depth-weighted mid over the top 0.05 BTC of
  each side. The edge refuses to quote (`503`) when the spread is over 2%, the
  data is over 60 s old, the pair is paused, or the mid is over 2% from the last
  trade. Unit: `price_sat_per_xbt`, the BTC sats paid for 1 XBT.
- **Commission.** 50 bps per side, paid on the chain each user sends. It is an
  extra output to the `--service-fee-address` in the HTLC funding transaction.
  HTLC redeems and refunds are exempt from the `/tx` fee check. To qualify,
  every input must spend a recorded swap funding outpoint on that chain, with
  that leg's exact HTLC witness script. The edge rebuilds that script from the
  keys and hashlock relayed for the match.
- **Watchtower.** Each pass (every 60 s), it:
  - enforces the accept and funding deadlines; three missed deadlines in 24 h bar
    an install token from new offers for 24 h;
  - starts the participant's funding clock once the XBT HTLC has 6
    confirmations;
  - broadcasts the refunds and redeems that phones hand it with
    `POST /swap/watch`. A presigned XBT redeem is finished with the preimage taken
    from the initiator's BTC redeem.
- **Single instance only.** Offers, matches, relay messages and watch jobs live
  in one local RocksDB (`--swap-db`). The watchtower runs in-process. Don't run
  two `--swap` edges against the same users.
- **Rollout order.** The watchtower and the phones read HTLC outputs through
  `fortis-index`'s `GET /txout/{txid}/{vout}`. Deploy a `fortis-index` with
  `/txout` behind the edge's XBT upstream **before** enabling `--swap`: without
  it, every XBT HTLC check fails (the route 404s), so no participant clock
  starts and no XBT refund or redeem is sent.

Flags:

| flag | default | |
|---|---|---|
| `--swap` | off | enable `/swap/*` and the watchtower; needs `--service-fee-address` and `--network bitcoin` or `regtest` |
| `--swap-db <dir>` | `<home>/fortis-edge-swap-rocksdb` | the swap store |
| `--swap-fee-bps` | `50` | commission per side; the protocol fixes it at 50 (phones reject other terms) |
| `--swap-max-sat` | `1000000` | largest BTC leg of one swap; the minimum is 80 000 |
| `--neoxex-url` | `https://neoxa.exchange` | the price source |

Two hidden flags exist **for tests only**:
- `--swap-tick-secs` (default 60) sets the watchtower interval.
- `--swap-clock-offset-secs` (default 0) runs the watchtower's clock ahead of the
  wall clock.

Both are used by `tests/swap_e2e.rs`. Never set them in production.

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

Swap routes (with `--swap`). Every `/swap/*` request needs a valid install
token. Every call about an offer or a match also needs its owner secret, sent
as `X-Swap-Secret`; the secret decides the caller's role.

| | |
|---|---|
| `GET /swap/quote` | `{price_sat_per_xbt, fee_bps, min_btc_sat, max_btc_sat, open:{xbt_to_btc, btc_to_xbt}}`; `503` when NeoxEX can't be quoted |
| `POST /swap/offers` | `{direction: "xbt_to_btc"\|"btc_to_xbt", amount_sat, max_slippage_bps: 50\|100\|200, expires_in?, private?, client_id?}` → the offer view plus `owner_secret` (and a `share_code` if private). A public offer is matched FIFO at once, with partial fills. `429` after three missed deadlines. `client_id` (≤ 64 hex digits) makes the POST idempotent: a repeat with the same token and `client_id` returns the same offer and owner secret, without re-matching |
| `GET /swap/offers/{id}` | the offer view: `{offer_id, direction, amount_sat, status, match_id, matches, share_code, …}` |
| `DELETE /swap/offers/{id}` | cancel; `409` unless still `open` |
| `POST /swap/take/{code}` | take a private offer whole → `{match_id, owner_secret, offer_id}` |
| `GET /swap/matches/{id}` | `{terms, role, status, participant_fund_by, xbt_funding, btc_funding}` |
| `POST /swap/matches/{id}/messages` | relay `{type: "accept"\|"decline"\|"keys"\|"hashlock"\|"funded", …}` → `{seq}`. Two of these are set once: `keys` (`{xbt, btc}` compressed pubkeys, per role) and `hashlock` (`{hex}`, initiator only); an identical repeat is a no-op and a different one is a `409`. `funded` is `{chain, txid, vout}` and is accepted only on the sender's own chain, in turn |
| `GET /swap/matches/{id}/messages?after=N` | long-poll (25 s) → `{messages:[{seq, from, body}]}`; `from` is `initiator`, `participant` or `edge` (a `deadline` message) |
| `POST /swap/watch` | hand the watchtower spends of the match's recorded HTLC: `{match_id, chain, kind: "refund"\|"redeem", htlc_outpoint:{txid, vout}, not_before, variants:[hex]}`. For a redeem that waits on the preimage, also send `sigs` (one per variant), `pubkey` and `witness_script`. Refund your own HTLC; redeem the counterparty's |

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

`tests/swap_e2e.rs` runs five swap scenarios end to end over HTTP, with two
`wallet-core` users driving the swap the way the phone's engine does:
1. the happy path, with the commission on both chains;
2. the participant goes offline and the watchtower redeems for them;
3. the participant never funds and the watchtower refunds the initiator;
4. a skewed price, which both users decline;
5. HTLC spends pass the `/tx` fee check, but an unrelated fee-less transaction
   does not.

The setup:
- XBT is a Knots BLAKE2b regtest node behind `fortis-index`.
- BTC is a stock Bitcoin Core regtest node (`-txindex=1`) behind an in-test
  Esplora shim.
- A stub NeoxEX serves the price.

Opt-in: the scenarios run only when both variables are set. The `harness_*`
tests check the shim (with only `FORTIS_BTC_BITCOIND` set), the stub and the
driver (with neither set) on their own.

```sh
cargo build --workspace
FORTIS_BITCOIND=…/knots/bitcoind FORTIS_BTC_BITCOIND=…/core/bitcoind \
  cargo test -p fortis-edge --test swap_e2e -- --nocapture
```

## Not yet

Token revocation (would need a blocklist), distributed rate limiting / cache
(in-memory, per instance), built-in TLS, request-size limits, structured request
logging.
