# Deploying the fortis backend

```
wallet ──HTTPS──▶ Caddy / Tailscale ──▶ fortis-edge :8098 ─┬─▶ fortis-index :8094 ──▶ Knots (XBT) RPC
   (per-install token)                  (tokens, rate-limit,  │
                                         cache, CORS, metrics) └─▶ Esplora upstream (BTC)
```

The Bitcoin **nodes stay where they are** — the edge and index just need to
reach the Knots node's RPC. Nothing here holds keys.

## 1. Prepare the node

Give the Knots node a static RPC credential so a long-running service survives
node restarts (a `.cookie` rotates on every restart). In `bitcoin.conf`:

```ini
rpcauth=fortis:<salt>$<hash>     # generate with Bitcoin Core's share/rpcauth/rpcauth.py
rpcbind=127.0.0.1               # keep RPC on localhost
rpcallowip=127.0.0.1
```

Restart the node. The `user:password` (the plaintext the script printed, not the
hash) is what `--rpc-auth` takes.

## 2. Run the two services

### Native binaries — Windows or Linux, no Docker

```sh
cargo build --release -p fortis-index -p fortis-edge

# terminal 1 — the XBT index
./target/release/fortis-index \
  --network mainnet --rpc-url http://127.0.0.1:8332 \
  --rpc-auth fortis:YOURPASS \
  --db fortis-index.sqlite --bind 127.0.0.1:8094
# first run indexes from the fork height (961640) — a few minutes

# terminal 2 — the edge
./target/release/fortis-edge \
  --bind 127.0.0.1:8098 \
  --xbt-upstream http://127.0.0.1:8094 \
  --btc-upstream https://mempool.space/api \
  --btc-price-url 'https://api.kraken.com/0/public/Ticker?pair=XBTUSD' \
  --xbt-price-url https://mempool.kilombino.com/api/v1/prices \
  --btc-upstream-rate 5 --btc-haskoin-url https://api.haskoin.com/btc \
  --require-token --trust-forwarded-for \
  --crash-log /var/log/fortis/crashes.ndjson
  # --service-fee-address <addr>   optional: advertise + enforce a % fee to <addr>.
  #   The public fortistechlabs.com edge runs without it (no fee).
```

On Linux, `deploy/systemd/*.service` run these under `systemd` with sandboxing —
copy to `/etc/systemd/system/`, edit the `--rpc-auth` / `--allow-origin`, then
`systemctl enable --now fortis-index fortis-edge`.

### Docker Compose

```sh
cp deploy/.env.example deploy/.env      # fill in XBT_RPC_AUTH etc.
docker compose -f deploy/docker-compose.yml up -d --build
```

The containers reach the host's node at `host.docker.internal`. `index` is not
published; `edge` is bound to `127.0.0.1:8098` for a front proxy to pick up.

## 3. Make it reachable

Keep `fortis-edge --bind 127.0.0.1:8098` in every case — the front door is one
of the following, never the port directly.

### Cloudflare Tunnel + your own domain — the home-machine path (in use)

A public `https://api.<domain>` with a real cert, no open ports, home IP hidden.
See [`cloudflared/config.example.yml`](cloudflared/config.example.yml):

```sh
winget install Cloudflare.cloudflared        # or the platform package
cloudflared tunnel login
cloudflared tunnel create fortis
cloudflared tunnel route dns fortis api.example.com
# write ~/.cloudflared/config.yml (ingress -> http://localhost:8098)
cloudflared tunnel run fortis                 # test
cloudflared service install                   # run on boot
```

Cloudflare sets `X-Forwarded-For`, so the edge runs with `--trust-forwarded-for`
for real client IPs — safe here because the tunnel is the only way in.

### Tailscale — private, your own devices only

```sh
tailscale serve --bg https / http://127.0.0.1:8098   # https on your tailnet
```

Point the app at `https://<machine>.<tailnet>.ts.net`. No DNS, no public
exposure. Good for solo dogfooding; `tailscale funnel` makes it public without a
domain if you need that.

### VPS + Caddy — the production path

On a Linux VPS running the node + services, point `api.<domain>` at it and
`caddy run --config deploy/Caddyfile` (or `--profile tls` in compose) — Caddy
gets a Let's Encrypt cert and reverse-proxies `:443 → 127.0.0.1:8098`.

Lock `--allow-origin` to your wallet's origin(s) once the web app has a home.

## 4. Point the wallet at it

In the app's backend picker, choose **fortis (hosted)** and enter the URL from
step 3 (`https://api.fortis.example`, or the Tailscale one). The app does
`POST /register` for a per-install token and sends it as `Authorization: Bearer`
from then on. Update the default in `web/src/app.js` (`DEFAULT_EDGE`) and
`android/.../Screens.kt` (`DEFAULT_EDGE`) once the URL is fixed.

## Operating

- `GET /metrics` on the edge — Prometheus counters (requests, registrations,
  cache hits, rate-limit / auth rejections, upstream errors).
- `GET /` on the edge and the index — health + which chains are served.
- The index is safe to restart any time (it resumes from its SQLite tip);
  a `--rpc-auth` credential means it reconnects cleanly after a node restart too.

## Not covered yet

Multiple edge replicas behind a load balancer (the rate-limiter and cache are
per-instance), token revocation, `fortis-notify` (push notifications).
