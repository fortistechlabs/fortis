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

2. **Create the PostgreSQL role and an empty database** (`vss-server` creates and migrates the `vss_db` table itself on first start; a table created by hand as `postgres` is owned by the wrong role and makes its migrations fail):
   ```sh
   sudo -u postgres psql <<'SQL'
   CREATE ROLE fortis WITH LOGIN PASSWORD 'fortis';
   CREATE DATABASE vss OWNER fortis;
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
   - `data_dir`: `"/var/lib/fortis/lsp"` (holds `keys_seed` with mode `0400`, SQLite store, and ldk-node's log `ldk_node.log`).
   - `open_fee_ppm`: `10000` (1% JIT channel-open fee).
   - `open_fee_min_sat`: `1000` (1,000 sat minimum JIT channel-open fee; keep near 2× next-block funding tx fee).
   - `min_payment_size_msat` / `max_payment_size_msat`: outer payment bounds (`10_000_000` / `500_000_000` msat). `fortis-lsp` automatically narrows these so every JIT channel lands inside `channel_min_sat..=channel_max_sat` (`51_000..=250_000` sat with defaults).
   - `channel_size_factor`: `2.0` (JIT channel size = `(payment - fee) × factor`).
   - `channel_min_sat` / `channel_max_sat`: `100000` / `500000` sat (`500_000` sat canary cap).
   - `token`: optional LSPS2 token string (if set, must match `lsps2_token` in `/etc/fortis/ln-info.json`).
   - `low_liquidity_sat`: `2000000` (logs a warning on stderr every 60s when on-chain balance drops below 0.02 BTC). Use `300000` for the 0.005 BTC canary (§4).
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

Sized for the **canary** (one or two test phones). For a live service with many users, raise these, and raise `low_liquidity_sat` with them.

1. **Fund the LSP on-chain wallet with ~0.005 BTC (500,000 sat):**
   - Send 0.005 BTC to the `onchain_address` printed by `fortis-lsp` on startup (`journalctl -u fortis-lsp | grep onchain_address`).
   - Back up `/var/lib/fortis/lsp/keys_seed` off the host **before** funding.
   - Set `low_liquidity_sat = 300000` in `/etc/fortis/lsp.toml` and restart `fortis-lsp`, or the default (2,000,000 sat) logs a low-liquidity warning every 60s.
   - Once confirmed, `curl -s http://127.0.0.1:9736/metrics | grep fortis_lsp_onchain_sat` reports `500000`.

   Where the 500,000 sat goes:

   | Use | Sat |
   |---|---|
   | One outbound channel to a routing node | 100,000–150,000 |
   | JIT channels for test phones (100,000–500,000 each; ~100,000–300,000 for two) | 100,000–300,000 |
   | Anchor reserve (25,000 per channel, held back by ldk-node) | ~75,000 |
   | On-chain fees for opens | the rest |

   A retry budget is 0.01 BTC.

2. **Open one outbound channel to a well-connected routing node** (100,000–150,000 sat), so Fortis users can pay BOLT 11/12 recipients across the wider network and outside payers can route to the LSP. `fortis-lsp` opens it itself from `/etc/fortis/lsp.toml`:
   ```toml
   alias = "Fortis"
   announce_addresses = ["34.31.126.17:9735"]   # the public relay (deploy/relay), not `listen`

   [[outbound]]
   peer = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f@3.33.236.230:9735"   # ACINQ
   sat = 150000
   ```
   Then `sudo systemctl restart fortis-lsp` and watch `journalctl -u fortis-lsp -f`:
   - `opening announced channel of 150000 sat to …` then `channel … pending` and, after confirmation, `channel … ready`.
   - `waits for funds` means spendable on-chain is below the channel size + 25,000 sat anchor reserve; it opens by itself once the balance covers it.
   - `open to … failed: …` retries every 10 minutes. A peer that refuses *after* the open started shows up as `channel … closed: CounterpartyForceClosed { peer_msg: … }`; the peer's first reason (e.g. `invalid funding_amount=150000 sat (min=400000 sat)`) is only in ldk-node's log: `sudo grep -i "Got Err message" /var/lib/fortis/lsp/ldk_node.log | tail`. Change the entry and restart to try again (a refused open costs nothing: no funding transaction is broadcast).
   - **ACINQ's minimum is 400,000 sat** (seen 2026-11-03). At the 0.005 BTC canary size that leaves too little for JIT channels: pick a hub with a lower minimum, or fund more.

   The channel is announced (public); JIT channels to Fortis users stay private. Each target is opened at most once per run: if the peer closes the channel, the LSP does not reopen it until the next restart, so a misbehaving peer cannot make it spend fees in a loop. Leave the rest of the funds on-chain for JIT channel opens.

3. **Inbound liquidity.** A channel the LSP opens starts with all funds on the LSP's side: outbound capacity, no inbound. Paying out to outside invoices works from the start. Receiving from outside wallets (Phoenix and others) needs balance on the *peer's* side of an LSP channel, which appears only after the LSP has sent payments out through it. In the canary, test sending before receiving, and see whether the first outside receive fails. If it does, send a small payment out through the channel first, or ask the peer to open a channel to the LSP.

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
  "min_payment_sat": 51000,
  "max_payment_sat": 250000,
  "channel_min_sat": 100000,
  "channel_max_sat": 500000,
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

# 2b. XBT LSP (if /var/lib/fortis/lsp-xbt exists): its own seed and store
if [ -d /var/lib/fortis/lsp-xbt ]; then
  mkdir -p "$DEST/lsp-xbt"
  cp -a /var/lib/fortis/lsp-xbt/keys_seed "$DEST/lsp-xbt/keys_seed"
  sqlite3 /var/lib/fortis/lsp-xbt/ldk_node_data.sqlite ".backup '$DEST/lsp-xbt/ldk_node_data.sqlite'"
  cp -a /etc/fortis/lsp-xbt.toml /etc/fortis/ln-info-xbt.json "$DEST/lsp-xbt/" 2>/dev/null || true
fi

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
   - **Step B (User sweeps on-chain transit funds from phrase alone):** When the user restores the Lightning wallet from its phrase in Fortis Wallet (or via `fortis_lightning::recover_transit_wallet`), the seed-only recovery sweeper (`recover.rs`) scans `m/84'/0'/0'/{0,1}/*` (gap limit 200) against Esplora / Bitcoin Core and sweeps all settled channel-close and transit outputs into the first software mainnet BTC wallet's next receive address — **without needing VSS or `ldk-node` to start**.
   - *(Limitation: HTLCs that were actively in flight at the exact moment of a double loss — both phone storage and VSS database lost simultaneously — require the channel monitor and cannot be claimed by the seed-only sweeper.)*

---

## 8. Closing idle channels

Channels with zero activity and negligible balance for 6–12 months tie up LSP capital. In Phase 1 (`MIN_CHANNEL_LIFETIME_BLOCKS = 4320`, ~30 days), idle channels are closed manually by the operator after the promised 30-day lifetime has elapsed. When the user next unlocks the app (the Lightning wallet's node starts), `LightningSession` detects the settled close output in `onchainTransitSat` and automatically sweeps it to the first software mainnet BTC wallet.

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

Before offering the Lightning wallet to general users, run the mainnet canary with `channel_max_sat = 500000` using `.dev` and `.dev2` debug builds on the Pixel (never overwrite the store app):

- [ ] **Phones that ran the old per-BTC-wallet Lightning toggle:** this build no longer starts a node for a BTC wallet, so that node's channel and funds are not shown after the update. Either, *before* installing this build, close the channel and sweep on the old build (Settings → close channel; wait for the funds in the BTC wallet); or, *after* installing it, restore that BTC wallet's recovery phrase (and its passphrase, if any) **as the Lightning wallet** (*Add wallet → Lightning → Restore*). The node seed comes from the phrase and passphrase alone, so this is the same node and the same VSS store. Expect *"Lightning is active on another device."* (the old device id lived in the old node's data directory, `files/ln/<btcWalletId>`); tap *"Use Lightning here instead"* to take over, then close the channel or keep using it. Optionally delete `files/ln/<btcWalletId>` afterwards; the leftover `ln_enabled_*` / `ln_move_*` preferences are harmless orphans.
- [ ] **Backend prerequisites:** `keys_seed` backed up off-host; LSP funded with ~0.005 BTC (§4) and `low_liquidity_sat = 300000`; one outbound channel open (§4); `lsp.fortistechlabs.com:9735` reachable from mobile data (direct, or through the relay in `deploy/relay/`); `GET /ln/info` on `api.fortistechlabs.com` returns 401 without an install token (not 404).
- [ ] **LSP live on mainnet (`channel_max_sat = 500000`):** `curl -s http://127.0.0.1:9736/info` reports `channel_max_sat: 500000`, `min_payment_size_msat: 51000000`, `max_payment_size_msat: 250000000`; `GET /ln/info` on `api.fortistechlabs.com` matches.
- [ ] **Create a Lightning wallet on `.dev`:** *Add wallet → Lightning* (new recovery phrase) reaches *"LSP connected · backup OK"*.
- [ ] **First receive via JIT channel (51,000+ sat from Phoenix):** In the Lightning wallet, enter `55,000 sat` on Receive; verify the fee quote line (*"Receiving 55,000 sat · fee 1,000 · you get 54,000"*) and that the QR is a BOLT 11 invoice (`lightning:lnbc...`), not a BIP21 `bitcoin:` QR; pay from Phoenix; verify JIT channel opens at 0-conf, toast *"Received ⚡ 54,000 sat"* appears, and the Lightning wallet's balance shows `54,000 sat`.
- [ ] **Pay a real outside invoice by QR scan:** Scan a BOLT 11 invoice on Phoenix from `.dev`; verify confirm sheet shows amount, description, expiry, and routing fee cap (`max(1%, 50 sat)`), biometric prompt gates send, payment succeeds, and History lists `⚡ send` with fee and description.
- [ ] **Pay `.dev` → `.dev2`:** Create a Lightning wallet on `.dev2`, generate a receive QR, pay from `.dev` through `fortis-lsp`.
- [ ] **Restore `.dev2` from recovery phrase:** Wipe `.dev2` app data, restore the Lightning wallet from its recovery phrase (the one shown for the Lightning wallet; for a node first run by the old per-BTC-wallet toggle, that BTC wallet's phrase, see the first item); verify channel state and balance restore from VSS without error.
- [ ] **Two-device takeover:** Unlock `.dev` after `.dev2` has taken the VSS lock; verify `.dev` stops with *"Lightning is active on another device."* and tapping *"Use Lightning here instead"* (after confirmation dialog) transfers the lock back to `.dev`, stopping `.dev2` on its next VSS check.
- [ ] **`lightning:` deep link from browser:** Tap a `lightning:lnbc...` link in Chrome/Firefox on the Pixel; verify it opens `.dev` directly on the Send tab with the invoice prefilled.
- [ ] **LSP restart under the same node ID:** `sudo systemctl restart fortis-lsp`; verify `.dev` reconnects automatically and can send/receive.
- [ ] **Review Focus 5 (LSP closes channel while app is closed):** Force-close or coop-close the channel from the LSP while `.dev` is force-stopped; once confirmed on-chain, open `.dev` and verify the balance is never shown as 0 — settled close funds appear in `onchainTransitSat` and are automatically swept to the first software mainnet BTC wallet.

### Canary run log
- Status: Backend deployed (edge `/ln/info` and `/ln/vss`, VSS, LSP); relay `lsp.fortistechlabs.com` → e2-micro → home LSP verified for TCP. Pending: LSP funding (0.005 BTC canary) and Pixel `.dev`/`.dev2` execution. Note: test sending before receiving from an outside wallet (§4, inbound liquidity). Force close and sweeps need the edge with the Lightning fee exemption (`crates/fortis-edge/src/ln_fee.rs`); before it, the edge refused the phone's own broadcasts (402).

---

## 11. XBT LSP

A second `fortis-lsp` instance for XBT (Bitcoin BLAKE2b chain): its own unit `fortis-lsp-xbt`, config `/etc/fortis/lsp-xbt.toml`, data `/var/lib/fortis/lsp-xbt` (its own `keys_seed`, so its own `node_id`), admin and metrics on `127.0.0.1:9746`, peers on `0.0.0.0:9835` (announced as `lsp.fortistechlabs.com:9835` through the relay). It reads the chain from the XBT index (`http://127.0.0.1:8094`), gossips peer to peer (never RGS), and runs Fortis accounts (custody) with the same limits as the BTC LSP.

New in this rollout:

- `fortis-index --spends-from-height <h>` builds the spends index (who spent which output; served at `/tx/<txid>/outspend/<vout>`): XBT `961640`, BTC `973000`.
- `fortis-lsp` keys `chain = "xbt"`, `esplora_url` and `xbt_gossip_floor_height` (example: [`linux-home/lsp-xbt.toml.example`](linux-home/lsp-xbt.toml.example)).
- `fortis-edge` flags `--ln-info-xbt-file` and `--lsp-xbt-admin-url http://127.0.0.1:9746` (example: [`linux-home/ln-info-xbt.json.example`](linux-home/ln-info-xbt.json.example)).

Do the steps in order. Each one is safe to stop after. `install.sh` restarts several units at once, so it is not used here: install by hand as shown.

Build first, as your user: `cargo build --release -p fortis-index -p fortis-edge -p fortis-lsp`.

### 11.1 Check the BTC spends height (before touching an index)

The unit file in this repo carries `--spends-from-height 973000` for BTC. Every live BTC LSP channel must have been funded at or above that height, or its close would not be indexed. The phones' Fortis channels are only with this LSP, and every one of them (LSP to phone, LSP outbound) was funded after the BTC LSP node was created, so it is enough that block 973000 is older than the node:

```sh
# (a) when the node was created (keys_seed is written once, at creation):
#     birth time (%w; '-' if the filesystem has none) and modification time (%y)
sudo stat -c '%w  %y' /var/lib/fortis/lsp/keys_seed
# (b) the time of block 973000 on Bitcoin Core (the "time" field)
core-cli getblockheader \
  $(core-cli getblockhash 973000)
date -d @<time>
```

Take the creation time as the **earlier** of the two times from (a) (only the mtime if the birth time is `-`). If `keys_seed` was ever restored from a backup or moved from another host, these times are those of the copy, not of the node: use the date the node was first created instead (from your notes or the first backup).

Proceed only if the block time from (b) is at least **2 hours** earlier than that creation time (a block's timestamp can run up to 2 hours ahead of real time). Otherwise pick a lower height whose block time is (repeat (b) with that height), and put it in `--spends-from-height` in `deploy/linux-home/fortis-btc-index.service` before 11.2. A lower value is always safe; it only makes the backfill longer.

### 11.2 Deploy the indexes

1. **Both index nodes need `-txindex`** (the spends lookups fetch the spending transactions). Confirm for each node before going on; `txindex` must report `"synced": true`:
   ```sh
   # core-cli / knots-cli wrap bitcoin-cli for each node (no sudo needed once your account is in the node group)
   knots-cli getindexinfo   # XBT (Knots)
   core-cli getindexinfo    # BTC (Core)
   ```
   On both nodes the output must list `txindex` with `"synced": true`. If either lacks it or is still syncing, stop here and fix the node first.
2. **Dry run (recommended).** Run the new `fortis-index` against a copy of the XBT index DB (with its own ports and data directory) and watch it backfill before running it on the live DBs. A fresh DB, such as the one `tools/xbt-regtest` builds, starts its spends index at height 0 and never backfills, so it does not exercise this. At the least, watch the first batches in step 4 and run the spot check in step 5.
3. **Install and restart.** First compare the repo's unit files with the installed ones, in case the host has local edits; carry any you want to keep into the repo files before installing:
   ```sh
   diff /etc/systemd/system/fortis-xbt-index.service deploy/linux-home/fortis-xbt-index.service
   diff /etc/systemd/system/fortis-btc-index.service deploy/linux-home/fortis-btc-index.service
   ```
   The only differences should be the new `--spends-from-height` arguments. Then:
   ```sh
   sudo install -m 0755 target/release/fortis-index /usr/local/bin/fortis-index
   sudo install -m 0644 deploy/linux-home/fortis-xbt-index.service deploy/linux-home/fortis-btc-index.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl restart fortis-xbt-index fortis-btc-index
   ```
4. **Watch the backfill** in each journal (`journalctl -u fortis-xbt-index -f`, `-u fortis-btc-index -f`): one `spends backfill: <h>/<to>` line per 100-block batch, then `spends backfill: done`. A batch that is refused is logged with its reason. Each batch briefly pauses block indexing while it flushes, so the index tip lags a little until `done`. Wait for `done` in both journals.
5. **Spot check** with a closed channel you know, for example an old P2WSH funding transaction: `curl -s 127.0.0.1:8094/tx/<funding txid>/outspend/<vout>` must report `"spent": true` with the closing transaction. Do it once for XBT (8094) and, if you have a closed BTC channel above 973000, for BTC (8095).

Cautions:

- **Start the new index binary first, before running the `verify` tool.** `verify` opens the DB as a RocksDB secondary and fails on a DB the new binary has not opened once.
- **Do not downgrade `fortis-index` once spends are enabled.** The old undo decoding rejects the new trailing spends section. Keep the old binary only for the window before the restart in step 3 (to roll back, reinstall it and restart before the new one has run; after that, the way back is restoring a DB copy or resyncing).

### 11.3 Deploy and set up the XBT LSP

1. **Install the binary.** `fortis-lsp` is one binary for both LSPs:
   ```sh
   sudo install -m 0755 target/release/fortis-lsp /usr/local/bin/fortis-lsp
   ```
2. **Restart the BTC LSP**, since it shares the binary, and check it came up as before: the same `node_id` and channels.
   ```sh
   # note both BEFORE the restart
   curl -s http://127.0.0.1:9736/info | jq .node_id
   curl -s http://127.0.0.1:9736/metrics | grep fortis_lsp_channels
   sudo systemctl restart fortis-lsp
   # AFTER: node_id identical, fortis_lsp_channels the same count
   curl -s http://127.0.0.1:9736/info | jq .node_id
   curl -s http://127.0.0.1:9736/metrics | grep fortis_lsp_channels
   ```
   The BTC config must not contain `esplora_url`, and the XBT config is rejected if it contains `bitcoind_rpc`, `rpc_user`, `rpc_password_file` or `rgs_url`. Do not copy `lsp.toml` to make the XBT one.
3. **Install the XBT config and unit:**
   ```sh
   sudo install -m 0640 -o root -g fortis deploy/linux-home/lsp-xbt.toml.example /etc/fortis/lsp-xbt.toml
   sudo install -m 0644 deploy/linux-home/fortis-lsp-xbt.service /etc/systemd/system/
   sudo install -d -o fortis -g fortis -m 0700 /var/lib/fortis/lsp-xbt
   sudo systemctl daemon-reload
   ```
   Review `/etc/fortis/lsp-xbt.toml`: `chain = "xbt"`, `esplora_url = "http://127.0.0.1:8094"`, `xbt_gossip_floor_height = 961640`, and `custody_max_sat` / `account_max_sat` equal to the BTC LSP's (`sudo grep -E 'custody_max_sat|account_max_sat' /etc/fortis/lsp.toml`; the example has the published 300000 / 50000). Set `fcm_service_account` to the same key file as in `/etc/fortis/lsp.toml` (`sudo grep fcm_service_account /etc/fortis/lsp.toml`): push notifications to phones need it, and without it held payments are not announced to a sleeping phone.
4. **Start it:** `sudo systemctl enable --now fortis-lsp-xbt`, then `journalctl -u fortis-lsp-xbt -f`. It needs `fortis-xbt-index` running and synced.
5. **Back up `/var/lib/fortis/lsp-xbt/keys_seed` off the host before funding** (the nightly script in §6 also copies `lsp-xbt`, but take this first copy by hand before funding).
6. **Read the `node_id`:** `curl -s http://127.0.0.1:9746/info | jq .node_id`. You need it in 11.5.

Lightning on XBT uses only P2WSH channels (no taproot channels). If taproot channels are ever enabled, the spends index's skip rule must be revisited first.

### 11.4 Open the relay port

1. On the VPS, install the updated `deploy/relay/lnrelay.nft` as `/etc/wireguard/lnrelay.nft` (it now also forwards 9835) and reload it with `sudo nft delete table ip lnrelay && sudo nft -f /etc/wireguard/lnrelay.nft` (the same as `vps-wg0.conf`'s PostDown/PostUp; loading the file without deleting the table first appends duplicate rules). Do not restart `wg-quick@wg0` for this: it briefly drops the live BTC tunnel.
2. In the GCP console, add a VPC firewall rule, ingress, source 0.0.0.0/0, `tcp:9835`. nftables alone is not enough.
3. From mobile data (not the home network): `nc -vz lsp.fortistechlabs.com 9835` must succeed.

### 11.5 Turn on the edge (still disabled)

1. Create `/etc/fortis/ln-info-xbt.json` from [`linux-home/ln-info-xbt.json.example`](linux-home/ln-info-xbt.json.example): `sudo install -m 0644 deploy/linux-home/ln-info-xbt.json.example /etc/fortis/ln-info-xbt.json` (same mode as the BTC file), then edit it: fill in `node_id` from 11.3, keep `"enabled": false`.
2. In `/etc/fortis/edge.env`, append to `EXTRA_ARGS`: `--ln-info-xbt-file /etc/fortis/ln-info-xbt.json --lsp-xbt-admin-url http://127.0.0.1:9746`.
3. `sudo systemctl restart fortis-edge`.
4. Check that the object appears: `curl -s https://<edge>/ln/info | jq .xbt` (with an install token, as for the BTC document). Old apps ignore the unknown field.

Pushes from the XBT LSP carry `"chain":"xbt"`, which only the new app understands.

### 11.6 Fund and connect

Back up the seed (11.3 step 5) first. Send XBT to the `onchain_address` in `journalctl -u fortis-lsp-xbt`, then add outbound channels to well-connected XBT Lightning nodes in `/etc/fortis/lsp-xbt.toml` as in §4:

```toml
[[outbound]]
peer = "<node_id>@<host>:<port>"
sat = 150000
```

and `sudo systemctl restart fortis-lsp-xbt`. Watch `journalctl -u fortis-lsp-xbt -f` as in §4.

### 11.7 Wait for the app

Do not set `"enabled": true` in `ln-info-xbt.json` until the app that understands XBT Lightning (Plan C) is released and its real-phone canary has passed. Custody runs with the BTC LSP's limits from the start; the XBT custody total adds to the BTC one, so check the operator's combined exposure (`fortis_lsp_custody_sat` on 127.0.0.1:9736 and :9746).

---

## 12. XBT Lightning canary (real phone)

Run this on a real phone against the production XBT LSP (§11) while `"enabled": false` is still set in `/etc/fortis/ln-info-xbt.json`. Use the `.dev` debug build (never overwrite the store app). Do the steps in order; do not skip ahead to step 9.

**Debug override.** With `enabled: false` a release app refuses XBT Lightning (it starts the node in exit-only mode and the Receive and Send screens show the `message`). So that this canary can run before anyone is exposed, the `.dev` and `.dev2` debug builds ignore `enabled: false` for XBT only (`effectiveLnEnabled` in `LightningSession.kt`): they start the node, take over and let you receive, move and pay as if it were enabled. A missing or invalid `xbt` object still counts as unreachable. Release builds and the BTC wallet follow `/ln/info` exactly. A debug build therefore behaves differently from release for XBT until step 9; the canary result only carries over to release once `enabled` is true, so repeat a quick receive on a release build after step 9.

1. **Prerequisites.** §11 is done with `"enabled": false`: `curl -s -H "Authorization: Bearer $TOKEN" https://<edge>/ln/info | jq .xbt` shows the XBT object with `enabled: false`; `fortis-lsp-xbt` is running and synced; `lsp.fortistechlabs.com:9835` is reachable from mobile data; the XBT LSP is funded and has outbound channels (§11.6). (Get a token with `TOKEN=$(curl -s -X POST https://<edge>/register | jq -r .token)`; tokens are free, and `/ln/info` returns 401 without one.) You also need an outside XBT Lightning node to pay from and to (for example the LND fork), and an XBT on-chain wallet with some balance in the app (step 5 needs one; without it the app refuses with *"Add an XBT wallet to move Lightning (XBT) funds on-chain."*).
2. **Build and install the `.dev` APK** (as in `android/README.md`, "Swap testing"): `cd android && ./gradlew installDebug`. It installs as `com.fortistechlabs.wallet.dev` ("fortis dev"); `-Pdev2` builds `.dev2` as in §10. Debug builds (`BuildConfig.DEBUG`) offer *Add wallet → Lightning (XBT)* even while `xbt.enabled` is false; release builds hide it until step 9.
3. **Create the wallet and check the takeover.** *Add wallet → Lightning (XBT)* (new recovery phrase); wait for *"LSP connected · backup OK"*. Write the phrase down. Install `.dev2` (`./gradlew installDebug -Pdev2`) and restore the same phrase there as a Lightning (XBT) wallet: it must show *"Lightning is active on another device."*; tap *"Use Lightning here instead"* and confirm. Then check `.dev` stops on its next VSS check, and that the balance and channel restore on `.dev2`. Continue on whichever install holds the lock.
4. **JIT receive.**
   - With the app open: on Receive enter an amount above `channel_min_sat` (100,000 sat by default in `ln-info-xbt.json`; at or above `min_payment_sat`), check the fee quote line, and pay the invoice from the outside XBT Lightning node. The channel opens at 0-conf, the toast *"Received ⚡ <amount> sat"* appears, the Receive screen shows *"Received +<amount> ✓"*, and the balance updates.
   - With the app closed (force-stop it): create a second invoice first, close the app, pay it. The phone must get a push titled *"Incoming XBT Lightning payment"* (later *"XBT Lightning payment received"*). Tap it with the app swiped away: it opens the Lightning (XBT) wallet, not the BTC one, and the payment completes. Also tap it with the app in the background (not killed); check this explicitly. Without `fcm_service_account` in `/etc/fortis/lsp-xbt.toml` (§11.3 step 3) no push arrives.
5. **Move funds** (*Move* tab):
   - **In:** *Move to Lightning* from the XBT on-chain wallet. It is ready to spend after 1 confirmation.
   - **Out, part of it:** *Move to on-chain*, to the XBT on-chain wallet; it arrives once the transaction confirms.
   - **Move all:** drag the slider to the maximum, or leave under 3,000 sat in the channel (the app then moves everything). The app shows *"Moving everything closes your Lightning channel."*; confirm, and the funds arrive in the XBT on-chain wallet when the close confirms.
6. **Pay outside.** Pay an XBT invoice from the outside node, and an XBT BOLT 12 offer (the outside node's), from the Lightning (XBT) wallet. Check the confirm sheet (amount, fee cap), the biometric prompt, success, and the History entries.
7. **Wrong chain.** In the Lightning (XBT) wallet paste a BTC invoice (and a BTC offer): it shows *"This is a BTC Lightning invoice"* (*"…offer"*) with a *"Pay from your Lightning wallet"* button. In the BTC Lightning wallet paste an XBT invoice: *"This is an XBT Lightning invoice"* with *"Pay from your Lightning (XBT) wallet"*. Neither may attempt a payment.
8. **Offer receive with the app closed.** Custody is on (same limits as BTC), so the Offer card shows on the Lightning (XBT) wallet once the node is up and the LSP has returned the offer. Close the app, pay the offer from the outside node, and check the push ("XBT Lightning payment received" or "Incoming XBT Lightning payment") and that the Fortis balance credits when the app is opened. If the card is missing, check `custody_max_sat` in `/etc/fortis/lsp-xbt.toml` (above 0) and restart `fortis-lsp-xbt` after changing it.
9. **Enable.** Once steps 3 to 7 pass, set `"enabled": true` in `/etc/fortis/ln-info-xbt.json`. `fortis-edge` polls the file every second and swaps in a valid change without a restart (`crates/fortis-edge/src/ln.rs`; the journal logs `ln info: reloaded ... (enabled=true ...)`; an invalid file is ignored and the last good one kept). Check `curl -s -H "Authorization: Bearer $TOKEN" https://<edge>/ln/info | jq .xbt.enabled`. The debug builds were already running XBT Lightning; release builds now offer *Lightning (XBT)* the next time the app reads `/ln/info`.

**Before enabling, all must be true:**

- [ ] Steps 3 to 7 passed on a real phone (on the `.dev` build, which ignores `enabled: false`; nothing but this checklist gates real users).
- [ ] `/var/lib/fortis/lsp-xbt/keys_seed` is backed up off the host (§11.3 step 5, §6).
- [ ] The XBT LSP has outbound channels open (§11.6), with enough capacity for the channels users will open.
- [ ] `lsp.fortistechlabs.com:9835` is reachable from mobile data (§11.4).
- [ ] `fcm_service_account` is set in `/etc/fortis/lsp-xbt.toml`.
- [ ] `custody_max_sat` / `account_max_sat` in `lsp-xbt.toml` match `lsp.toml`, and step 8 passed.

**Rollback:** set `"enabled": false` in `/etc/fortis/ln-info-xbt.json` (reloads within seconds, no restart). Release installs stop being offered XBT Lightning and existing release wallets go to exit-only mode on their next start; their channels are untouched. Debug builds are unaffected (see the debug override).

### XBT canary run log
- Status: not yet run.
