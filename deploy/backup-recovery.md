# Recovering the Fortis LSPs from the USB backup

How to get the BTC and XBT LSPs (and the edge/VSS around them) back from the encrypted backup drive. Backups are made by `linux-home/fortis-backup.sh` (weekly, §6.1 of [lightning.md](lightning.md)) and by hand into `manual/`. **This procedure has not been restore-tested yet. Do a dry run into a scratch directory (§5) before you need it.**

No secrets are written in this file. The drive passphrase must be stored somewhere other than this host.

## 0. What is on the drive

```
<mount>/weekly/<UTC timestamp>/          one folder per weekly run (newest 8 kept), with SHA256SUMS
<mount>/manual/…                         hand-made copies, never pruned
  lsp/      keys_seed  admin.token  accounts.sqlite  ldk_node_data.sqlite      BTC LSP
  lsp-xbt/  keys_seed  admin.token  accounts.sqlite  ldk_node_data.sqlite      XBT LSP
  vss.dump                                                                    VSS (Postgres, `pg_dump -Fc vss`)
  etc-fortis/  lsp.toml lsp-xbt.toml edge.env ln-info*.json fcm-service-account.json
               lsp-rpc-password msmtprc fortis-edge.secret
```

## 1. Unlock and check the drive

```sh
sudo cryptsetup open /dev/sdX fortis-backup        # passphrase; find the device with lsblk (USB, ~29 GB)
sudo mkdir -p /mnt/fortis-backup && sudo mount /dev/mapper/fortis-backup /mnt/fortis-backup
B=/mnt/fortis-backup/weekly/<newest folder>
sudo sh -c "cd $B && sha256sum -c SHA256SUMS"      # every line must say OK
sudo sqlite3 $B/lsp/accounts.sqlite 'PRAGMA integrity_check'    # repeat for each .sqlite: must print ok
sudo pg_restore -l $B/vss.dump | head              # must list a table of contents
```

If the desktop auto-unlocks the stick, use the `/dev/mapper/luks-…` name it created instead of opening it again. A failed check on the newest folder means use the next older one; do not mix files from different folders (§3).

## 2. Decide what you lost

| Situation | Go to |
|---|---|
| Host died or disk lost, LSP seeds and data gone | §3 full restore |
| One LSP's database corrupt, host fine | §4.1 |
| Only the seed survives (no channel data at all) | §4.2 |
| Only `accounts.sqlite` lost | §4.3 |
| Drive passphrase lost | §4.4 |

## 3. Full restore onto a fresh host

**Rule: restore everything from one folder.** `ldk_node_data.sqlite`, `vss.dump` and `accounts.sqlite` describe the same moment. Mixing a newer file with an older one is what makes a counterparty able to broadcast an old channel state (a justice transaction against you) or leaves customer balances wrong.

1. Install the host as in §1–§3 of [lightning.md](lightning.md) (packages, the `fortis` user, `install.sh --with-lightning`) but **do not start any `fortis-*` service yet**.
2. Restore the secrets and configs (from `etc-fortis/`): `lsp.toml`, `lsp-xbt.toml`, `edge.env`, `ln-info*.json`, `fcm-service-account.json`, `lsp-rpc-password` into `/etc/fortis/`, and `fortis-edge.secret` into `/var/lib/fortis/edge/`. Keep the original owners and modes (the LSP configs are `root:fortis 0640`, the RPC password and edge secret `fortis 0600`). `msmtprc` goes where the support-mail unit reads it.
3. Restore the LSP state, for each of `lsp` and `lsp-xbt`:
   ```sh
   sudo install -d -m 700 -o fortis -g fortis /var/lib/fortis/<n>
   sudo cp -a $B/<n>/keys_seed $B/<n>/admin.token $B/<n>/accounts.sqlite $B/<n>/ldk_node_data.sqlite /var/lib/fortis/<n>/
   sudo chown fortis:fortis /var/lib/fortis/<n>/*; sudo chmod 400 /var/lib/fortis/<n>/keys_seed; sudo chmod 600 /var/lib/fortis/<n>/{admin.token,accounts.sqlite,ldk_node_data.sqlite}
   ```
   `keys_seed` must be the exact file: it is the node's identity (the same `node_id`) and the key to every on-chain output.
4. Restore VSS into a new empty `vss` database from the **same folder**, then start `fortis-vss` (the Postgres setup is in lightning.md):
   ```sh
   sudo -u postgres createdb vss          # or recreate it empty
   sudo -u postgres pg_restore -d vss --no-owner $B/vss.dump
   ```
5. Start in this order, watching `journalctl -u <unit> -f` between each: `fortis-vss`, bitcoind/Knots and the indexes (let them sync), `fortis-lsp`, `fortis-lsp-xbt`, `fortis-edge`.
6. Check: `curl -s 127.0.0.1:9736/info` (BTC) and `:9746/info` (XBT) report the **same `node_id` as before**; `…/metrics | grep fortis_lsp_onchain_sat` shows the expected on-chain balance; channels appear as the LSP reconnects. Keep new Lightning activity switched off until this is right: set `"enabled": false` in `/etc/fortis/ln-info.json` (§5 of lightning.md) while you verify.

**If the restore is older than the live channels** (the host kept running after the snapshot and channels moved on), do not start the LSP normally. The counterparties hold newer state; an LSP that opens with old state can lose the whole channel. Use §4.2 (recover from the seed alone and let channels force-close) instead.

## 4. Specific cases

### 4.1 One LSP database corrupt, host fine
Stop that LSP, move the bad file aside, restore from the newest good folder as in §3 step 3, restore VSS from the same folder, start, verify. Same warning: if channels advanced since that snapshot, do not run the LSP on the old state; go to §4.2.

### 4.2 Only the seed survives
The seed (`keys_seed`, 64 bytes) controls the funds. Without the channel database the LSP cannot keep channels open, but on-chain outputs and force-closed balances are recoverable from the seed alone: `crates/fortis-lightning` has a seed-only scan and sweep of the static scripts (`recover.rs`: keys 0..999 for P2WPKH and the anchor outputs). Contact the counterparties to force-close their channels with you, wait for the closes to confirm, then sweep. Treat this as a manual, supervised operation; outputs in pending HTLCs are not recovered by that sweep. I have not run this against a real closure.

### 4.3 Only `accounts.sqlite` lost
Restore it from the newest folder. Restoring an old copy rolls customer balances back to that time. Before turning custody back on, reconcile it against channel state and against payments made since (held payments and withdrawals in the LSP logs), and tell affected users if balances differ.

### 4.4 Drive passphrase lost
The LUKS container cannot be opened and the data is unrecoverable. This is why the passphrase lives off this host. Take a header backup once, and store it apart from the drive: `sudo cryptsetup luksHeaderBackup /dev/sdX --header-backup-file <file>`.

## 5. Practice restore (do this before you need it)

Restore a weekly folder into a scratch directory, never over live state:

```sh
S=$(mktemp -d) && sudo cp -a $B/. $S/
sudo sh -c "cd $S && sha256sum -c SHA256SUMS"
for d in $S/*/accounts.sqlite $S/*/ldk_node_data.sqlite; do sudo sqlite3 $d 'PRAGMA integrity_check'; done
sudo -u postgres createdb vss_restore_test && sudo -u postgres pg_restore -d vss_restore_test --no-owner $S/vss.dump
sudo -u postgres psql -d vss_restore_test -c '\dt'; sudo -u postgres dropdb vss_restore_test
```

Record the date and the result of the test here when you do it: not yet tested.

## 6. After any restore

- Replace anything that was on a lost or stolen host: admin tokens, the FCM key, the edge secret, the RPC password.
- Take a fresh backup (`sudo systemctl start fortis-backup`) once the host is healthy, and re-test the next restore.
- Review `journalctl -u fortis-lsp -u fortis-lsp-xbt` for `channel … ready` and no repeated errors before re-enabling custody.
