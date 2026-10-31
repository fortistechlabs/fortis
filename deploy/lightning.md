# Deploying Lightning (`fortis-lsp` + `vss-server`)

```
Android wallet (fortis-lightning / ldk-node 0.7.1)
  ├── HTTPS ──▶ Cloudflare Tunnel ──▶ fortis-edge :8098
  │                                     ├── GET  /ln/info ──▶ /etc/fortis/ln-info.json
  │                                     └── POST /ln/vss/* ─▶ vss-server 127.0.0.1:8080 ──▶ PostgreSQL 127.0.0.1:5432
  └── TCP :9735 (BOLT 8 Lightning P2P) ─────────────────────▶ fortis-lsp 0.0.0.0:9735   ──▶ Bitcoin Core RPC 127.0.0.1:8332
```

Phase 1 runs `fortis-lsp` and `vss-server` alongside `fortis-edge` and Bitcoin Core on the Linux home host (or on a VPS if inbound TCP 9735 is not reachable at home).

---

## 1. Reachability prerequisite (TCP 9735)

Cloudflare's HTTP tunnel (`cloudflared`) does **not** carry raw Lightning P2P traffic (BOLT 8 Noise over TCP). Clients must reach `fortis-lsp` directly on port `9735/TCP`.

1. **Local listener check:**
   ```sh
   ss -ltn | grep 9735
   ```
   Expected: `LISTEN 0 ... 0.0.0.0:9735` (or `[::]:9735`).

2. **Router / firewall setup:**
   - **IPv4:** forward external TCP port `9735` on the router to the host's LAN IP `:9735`, and allow `9735/tcp` in the host firewall (`ufw allow 9735/tcp` or `nftables`). Requires a public, non-CGNAT IPv4 address.
   - **IPv6:** if the ISP delegates a global `/64`, allow inbound `9735/tcp` to the host's stable IPv6 address in the router's IPv6 firewall and set `listen = "[::]:9735"` in `/etc/fortis/lsp.toml`.

3. **External reachability check:**
   From an outside network (e.g. a phone on mobile data or a remote machine):
   ```sh
   nc -vz <PUBLIC_HOST_OR_IP> 9735
   # or test a real BOLT 8 handshake from another Lightning node:
   # lightning-cli connect <NODE_ID>@<PUBLIC_HOST_OR_IP>:9735
   ```
   **VPS trigger:** If the ISP uses CGNAT on IPv4 and blocks inbound IPv6 (or port 9735 cannot be reached reliably from mobile networks), move `fortis-lsp` to a small Linux VPS (see [§9 Moving to a VPS](#9-moving-to-a-vps)).

---

## 2. PostgreSQL + `vss-server` on localhost

`vss-server` stores client-encrypted channel backups and the per-wallet device lock (`fortis/device-lock`). It listens only on `127.0.0.1:8080` (`IPAddressAllow=localhost` in `fortis-vss.service`) and is reached exclusively through `fortis-edge`'s `POST /ln/vss/<call>` proxy, which enforces the per-install Bearer token, path allowlist (`getObject`, `putObjects`, `deleteObject`, `listKeyVersions`), 4 MiB body cap, rate limit, a 256 MiB/hour write quota per install token, an optional global write quota (`--vss-global-quota-bytes-per-hour`, off by default: tokens are free, so a few of them could use a shared budget up and lock every real user out of VSS; watch `fortis_edge_vss_put_bytes_total` instead), and a cap on concurrent VSS requests so slow uploads cannot take every edge worker.

1. **Build `vss-server` (pinned tag `v0.1.0-alpha.0`, commit `88a5703496386465556d920dccf49512296c53d0`):**
   ```sh
   crates/fortis-lightning/scripts/build-vss-server.sh
   ```
   *(Because `vss-server` is bound to `127.0.0.1:8080` behind `fortis-edge`'s token gate and systemd `IPAddressAllow=localhost`, the local build uses `noop_authorizer`; client isolation is enforced by the 256-bit HMAC store ID derived on-device from the BIP-85 Lightning seed.)*

2. **Create the PostgreSQL role, database, and `vss_db` table:**
   ```sh
   sudo -u postgres psql <<'SQL'
   CREATE ROLE fortis WITH LOGIN PASSWORD 'fortis';
   CREATE DATABASE vss OWNER fortis;
   \c vss
   CREATE TABLE IF NOT EXISTS vss_db (
     user_token VARCHAR(120) NOT NULL,
     store_id VARCHAR(120) NOT NULL,
     key VARCHAR(600) NOT NULL,
     value BYTEA NULL,
     version BIGINT NOT NULL,
     created_at TIMESTAMPTZ,
     last_updated_at TIMESTAMPTZ,
     PRIMARY KEY (user_token, store_id, key)
   );
   GRANT ALL PRIVILEGES ON TABLE vss_db TO fortis;
   SQL
   ```

3. **Verify `fortis-vss.service`:**
   ```sh
   curl -s http://127.0.0.1:8080/metrics | head -n 5
   ```

---

## 3. `fortis-lsp` and `/etc/fortis/lsp.toml`

1. **Bitcoin Core RPC credentials:**
   Generate an `rpcauth=` line for `fortis-lsp` in Bitcoin Core's `bitcoin.conf`, and write the plaintext password (first line only, mode `0600`, owned by `fortis:fortis`) to `/etc/fortis/lsp-rpc-password`:
   ```sh
   sudo install -m 0600 -o fortis -g fortis /dev/null /etc/fortis/lsp-rpc-password
   sudo sh -c 'echo "YOUR_RPC_PASSWORD" > /etc/fortis/lsp-rpc-password'
   ```

2. **`/etc/fortis/lsp.toml` fields** (template in [`deploy/linux-home/lsp.toml.example`](linux-home/lsp.toml.example)):
   - `network`: `"bitcoin"` (mainnet), `"testnet"`, `"signet"`, or `"regtest"`.
   - `listen`: `"0.0.0.0:9735"` (Lightning P2P socket).
   - `bitcoind_rpc`: `"127.0.0.1:8332"` (`host:port`).
   - `rpc_user` / `rpc_password_file`: RPC username and path to the `0600` password file.
   - `data_dir`: `"/var/lib/fortis/lsp"` (holds `keys_seed` with mode `0400`, SQLite store, and `logs/`).
   - `open_fee_ppm`: `10000` (1% JIT channel-open fee).
   - `open_fee_min_sat`: `1000` (1,000 sat minimum JIT channel-open fee; keep near 2× next-block funding tx fee).
   - `min_payment_size_msat` / `max_payment_size_msat`: outer payment bounds (`10_000_000` / `500_000_000` msat). `fortis-lsp` automatically narrows these so every JIT channel lands inside `channel_min_sat..=channel_max_sat` (`51_000..=250_000` sat with defaults).
   - `channel_size_factor`: `2.0` (JIT channel size = `(payment - fee) × factor`).
   - `channel_min_sat` / `channel_max_sat`: `100000` / `500000` sat (`500_000` sat canary cap).
   - `token`: optional LSPS2 token string (if set, must match `lsps2_token` in `/etc/fortis/ln-info.json`).
   - `low_liquidity_sat`: `2000000` (logs a warning on stderr every 60s when on-chain balance drops below 0.02 BTC).
   - `metrics_listen`: `"127.0.0.1:9736"` (`GET /metrics` and `GET /info`).

3. **Install and start:**
   ```sh
   cargo build --release -p fortis-index -p fortis-edge -p fortis-lsp
   crates/fortis-lightning/scripts/build-vss-server.sh
   sudo deploy/linux-home/install.sh --with-lightning
   ```

4. **Read the LSP's node ID, funding address, and effective bounds:**
   ```sh
   curl -s http://127.0.0.1:9736/info | jq .
   journalctl -u fortis-lsp -n 20 --no-pager
   ```

---

## 4. Funding the LSP and opening outbound channels

1. **Fund the LSP on-chain wallet with ~0.05 BTC (5,000,000 sat):**
   Send 0.05 BTC to the `onchain_address` printed by `fortis-lsp` on startup. Once confirmed, `curl -s http://127.0.0.1:9736/metrics | grep fortis_lsp_onchain_sat` will report `5000000` and the `low_liquidity_sat` warning will clear.

2. **Open 1–2 outbound channels to well-connected routing nodes:**
   So clients can pay arbitraryBOLT 11/12 recipients across the wider network (and receive from outside wallets like Phoenix), open 1–2 public or private channels (e.g. 2,000,000 sat each to ACINQ `03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f@3.33.236.230:9735` or LQWD), leaving ~1,000,000–2,000,000 sat on-chain for JIT channel opens to Fortis users.

---

## 5. `/etc/fortis/ln-info.json` and `fortis-edge` flags

Create `/etc/fortis/ln-info.json` using the `node_id` and effective bounds from `curl -s http://127.0.0.1:9736/info`:

```json
{
  "node_id": "<66-HEX-PUBKEY-FROM-LSP-INFO>",
  "host": "lsp.fortistechlabs.com",
  "port": 9735,
  "lsps2_token": null,
  "fee": {
    "ppm": 10000,
    "min_sat": 1000
  },
  "limits": {
    "min_payment_sat": 51000,
    "max_payment_sat": 250000,
    "channel_min_sat": 100000,
    "channel_max_sat": 500000
  },
  "enabled": true,
  "message": null,
  "network": "bitcoin"
}
```

Enable the Lightning routes in `/etc/fortis/edge.env`:

```ini
EXTRA_ARGS=--ln-info-file /etc/fortis/ln-info.json --vss-upstream http://127.0.0.1:8080/vss
```

Then restart `fortis-edge`:
```sh
sudo systemctl restart fortis-edge
```

`fortis-edge` hot-reloads `/etc/fortis/ln-info.json` whenever its mtime/size changes (if an edit is invalid, it keeps the last valid snapshot and logs a warning).

**Remote kill switch:** Setting `"enabled": false` (with an optional `"message": "Lightning maintenance in progress"`) in `/etc/fortis/ln-info.json` immediately stops clients from requesting new JIT receive invoices or opening new channels, while keeping existing channel payments, force-close, and on-chain recovery sweeps working.

---

## 6. Nightly off-host backups

Back up both the LSP state and the VSS PostgreSQL database nightly to an encrypted off-host target (e.g. `restic` or `rsync` over Tailscale/SSH):

```sh
#!/usr/bin/env bash
# /etc/cron.daily/fortis-lightning-backup
set -euo pipefail
umask 077
 DEST="/var/backups/fortis-ln/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$DEST"

# 1. VSS PostgreSQL dump (atomic snapshot)
sudo -u postgres pg_dump -Fc vss > "$DEST/vss.dump"

# 2. LSP keys_seed and SQLite store (sqlite3 online backup avoids torn WAL reads)
cp -a /var/lib/fortis/lsp/keys_seed "$DEST/keys_seed"
sqlite3 /var/lib/fortis/lsp/ldk_node_data.sqlite ".backup '$DEST/ldk_node_data.sqlite'"
cp -a /etc/fortis/lsp.toml /etc/fortis/ln-info.json "$DEST/"

# 3. Sync off-host and prune local snapshots older than 14 days
find /var/backups/fortis-ln -maxdepth 1 -mindepth 1 -type d -mtime +14 -exec rm -rf {} +
```

> **Critical:** Never restore an older `ldk_node_data.sqlite` on a running LSP that has advanced channel states since the snapshot, or the counterparty may broadcast a justice transaction. `keys_seed` is static and safe to back up once + nightly.

---

## 7. Lost-VSS disaster recovery (Task 1 finding)

Because `ldk-node 0.7.1`'s `build_with_vss_store` requires VSS at startup and cannot reconstruct live off-chain channel state from the recovery phrase alone:

1. **If VSS is temporarily unreachable:**
   - Clients fail closed with `BackupUnavailable` (*"Lightning backup unavailable, try again shortly."*) before any payment or channel action can diverge state.
   - Restore `vss-server` / PostgreSQL from the latest `pg_dump` (only if no newer channel updates occurred after the dump) or bring the existing PostgreSQL instance back up.

2. **If the VSS database is permanently lost AND the user also lost their phone:**
   - **Step A (LSP closes the channel):** On `fortis-lsp`, cooperative-close or force-close the user's channel. Because `fortis-lightning` uses standard BIP-85 entropy (`m/83696968'/128169'/64'/0'`) for LDK's internal BDK wallet (`m/84'/0'/0'/{0,1}/*`), `option_static_remotekey` sends the user's settled channel balance directly to an on-chain address derived from their 12/24-word recovery phrase.
   - **Step B (User sweeps on-chain transit funds from phrase alone):** When the user restores their phrase in Fortis Wallet (or via `fortis_lightning::recover_transit_wallet`), the seed-only recovery sweeper (`recover.rs`) scans `m/84'/0'/0'/{0,1}/*` (gap limit 200) against Esplora / Bitcoin Core and sweeps all settled channel-close and transit outputs into the main BTC wallet's next receive address — **without needing VSS or `ldk-node` to start**.
   - *(Limitation: HTLCs that were actively in flight at the exact moment of a double loss — both phone storage and VSS database lost simultaneously — require the channel monitor and cannot be claimed by the seed-only sweeper.)*

---

## 8. Closing idle channels

Channels with zero activity and negligible balance for 6–12 months tie up LSP capital. In Phase 1 (`MIN_CHANNEL_LIFETIME_BLOCKS = 4320`, ~30 days), idle channels are closed manually by the operator after the promised 30-day lifetime has elapsed. When the user next unlocks Fortis with Lightning enabled, `LightningSession` detects the settled close output in `onchainTransitSat` and automatically sweeps it to their main on-chain BTC wallet.

---

## 9. Moving `fortis-lsp` to a VPS

If home reachability on TCP 9735 is unreliable or capacity grows:

1. **Lower TTL and pause new opens:** Set `"enabled": false` and `"message": "Moving Lightning service, back shortly"` in `/etc/fortis/ln-info.json` on the edge.
2. **Stop `fortis-lsp` cleanly on the home host:**
   ```sh
   sudo systemctl stop fortis-lsp
   ```
   Verify in `journalctl -u fortis-lsp` that `fortis-lsp` exited with status `0` after stopping cleanly.
3. **Copy `/var/lib/fortis/lsp` and `/etc/fortis/lsp.toml` to the VPS:**
   ```sh
   rsync -aHAX --numeric-ids /var/lib/fortis/lsp/ root@vps:/var/lib/fortis/lsp/
   ```
   Disable `fortis-lsp` on the home host (`sudo systemctl disable fortis-lsp`) and rename `/var/lib/fortis/lsp` to `/var/lib/fortis/lsp.migrated` so the home host can **never** start the same `keys_seed` again.
4. **Start `fortis-lsp` on the VPS** (pointing at the VPS's Bitcoin Core RPC). Verify `node_id` matches.
5. **Update `/etc/fortis/ln-info.json` on the edge:** set `"host"` to the VPS's public IP/hostname and `"enabled": true`. Existing channels automatically reconnect to the new `host:port` on the next app unlock because `LightningSession` refetches `/ln/info` on every start.

---

## 10. Mainnet canary checklist

Before enabling Lightning (beta) for general users, run the mainnet canary with `channel_max_sat = 500000` using `.dev` and `.dev2` debug builds on the Pixel (never overwrite the store app):

- [ ] **LSP live on mainnet (`channel_max_sat = 500000`):** `curl -s http://127.0.0.1:9736/info` reports `channel_max_sat: 500000`, `min_payment_size_msat: 51000000`, `max_payment_size_msat: 250000000`; `GET /ln/info` on `api.fortistechlabs.com` matches.
- [ ] **Enable Lightning on `.dev`:** *Settings → Lightning (beta)* toggle on a software mainnet BTC wallet transitions `Off → Starting → Ready` (*"LSP connected · backup OK"*).
- [ ] **First receive via JIT channel (50,000+ sat from Phoenix):** Enter `55,000 sat` on Receive; verify the fee quote line (*"Receiving 55,000 sat · fee 1,000 · you get 54,000"*) and unified BIP21 QR (`bitcoin:...?amount=...&lightning=lnbc...`); pay from Phoenix; verify JIT channel opens at 0-conf, toast *"Received ⚡ 54,000 sat"* appears, and Home hero shows combined total + *On-chain · Lightning* split.
- [ ] **Pay a real outside invoice by QR scan:** Scan a BOLT 11 invoice on Phoenix from `.dev`; verify confirm sheet shows amount, description, expiry, and routing fee cap (`max(1%, 50 sat)`), biometric prompt gates send, payment succeeds, and History lists `⚡ send` with fee and description.
- [ ] **Pay `.dev` → `.dev2`:** Enable Lightning on `.dev2`, generate a receive QR, pay from `.dev` through `fortis-lsp`.
- [ ] **Restore `.dev2` from recovery phrase:** Wipe `.dev2` app data, restore the same 12/24-word recovery phrase, enable Lightning; verify channel state and balance restore from VSS without error.
- [ ] **Two-device takeover:** Unlock `.dev` after `.dev2` has taken the VSS lock; verify `.dev` stops with *"Lightning is active on another device."* and tapping *"Use Lightning here instead"* (after confirmation dialog) transfers the lock back to `.dev`, stopping `.dev2` on its next VSS check.
- [ ] **`lightning:` deep link from browser:** Tap a `lightning:lnbc...` link in Chrome/Firefox on the Pixel; verify it opens `.dev` directly on the Send tab with the invoice prefilled.
- [ ] **LSP restart under the same node ID:** `sudo systemctl restart fortis-lsp`; verify `.dev` reconnects automatically and can send/receive.
- [ ] **Review Focus 5 (LSP closes channel while app is closed):** Force-close or coop-close the channel from the LSP while `.dev` is force-stopped; once confirmed on-chain, open `.dev` and verify the balance is never shown as 0 — settled close funds appear in `onchainTransitSat` and are automatically swept to the main BTC wallet.

### Canary run log
- Status: Pending owner mainnet LSP funding and Pixel `.dev`/`.dev2` execution.
