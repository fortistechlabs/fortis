#!/usr/bin/env bash
# Weekly backup of the Fortis LSPs' keys, accounts, channel data, VSS and configs to the
# encrypted USB drive mounted at $FORTIS_BACKUP_MOUNT. Runs as root (see fortis-backup.service).
#
# The drive is LUKS-encrypted and is unlocked by hand, so this job never unlocks or mounts it:
# if it is not mounted the run FAILS (visible in `systemctl --failed` and the journal) and
# nothing is written. A backup lands in <mount>/weekly/<UTC timestamp>/ via a .partial directory,
# so an interrupted run never leaves something that looks complete.
set -euo pipefail
umask 077

MOUNT="${FORTIS_BACKUP_MOUNT:-/mnt/fortis-backup}"
KEEP="${FORTIS_BACKUP_KEEP:-8}"
STATE=/var/lib/fortis
ETC=/etc/fortis
MIN_FREE_KB=$((512 * 1024))

fail() { echo "fortis-backup: $*" >&2; command -v logger >/dev/null && logger -p user.err -t fortis-backup "$*"; exit 1; }

mountpoint -q "$MOUNT" || fail "$MOUNT is not mounted — plug in and unlock the backup drive, then run: systemctl start fortis-backup"
[ "$(findmnt -n -o FSTYPE "$MOUNT")" = ext4 ] || fail "$MOUNT is not the ext4 backup drive"
avail=$(df -Pk "$MOUNT" | awk 'NR==2{print $4}')
[ "$avail" -ge "$MIN_FREE_KB" ] || fail "less than 512 MiB free on $MOUNT"

TS=$(date -u +%Y%m%dT%H%M%SZ)
ROOT="$MOUNT/weekly"
DEST="$ROOT/$TS"
PART="$DEST.partial"
mkdir -p "$ROOT"
rm -rf "$ROOT"/*.partial            # leftovers of an interrupted run
mkdir -p "$PART"

# The BTC LSP lives in $STATE/lsp, the XBT LSP in $STATE/lsp-xbt (only if it exists).
for n in lsp lsp-xbt; do
  [ -d "$STATE/$n" ] || { [ "$n" = lsp ] && fail "$STATE/lsp is missing" || continue; }
  mkdir -p "$PART/$n"
  cp -a "$STATE/$n/keys_seed" "$STATE/$n/admin.token" "$PART/$n/"
  # Online backup: WAL-safe, the LSP keeps running. Never plain-cp these.
  for f in accounts ldk_node_data; do
    sqlite3 "$STATE/$n/$f.sqlite" ".backup '$PART/$n/$f.sqlite'"
  done
done

# VSS (Postgres) — dumped last so it is as close as possible to the channel snapshots.
runuser -u postgres -- pg_dump -Fc vss > "$PART/vss.dump"

# Configs and secrets.
mkdir -p "$PART/etc-fortis"
for f in lsp.toml lsp-xbt.toml edge.env ln-info.json ln-info-xbt.json fcm-service-account.json lsp-rpc-password msmtprc; do
  [ -e "$ETC/$f" ] && cp -a "$ETC/$f" "$PART/etc-fortis/"
done
[ -e "$STATE/edge/fortis-edge.secret" ] && cp -a "$STATE/edge/fortis-edge.secret" "$PART/etc-fortis/"

# Verify before declaring it complete.
for db in "$PART"/*/accounts.sqlite "$PART"/*/ldk_node_data.sqlite; do
  [ "$(sqlite3 "$db" 'PRAGMA integrity_check')" = ok ] || fail "integrity check failed for $db"
done
pg_restore -l "$PART/vss.dump" > /dev/null || fail "vss.dump does not read back"
[ -s "$PART/lsp/keys_seed" ] || fail "BTC keys_seed missing or empty"

( cd "$PART" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS )
sync
mv "$PART" "$DEST"
sync

# Keep the newest $KEEP weekly backups. Only directories named like a timestamp are pruned;
# the hand-made backups in $MOUNT/manual are never touched.
ls -1d "$ROOT"/20[0-9][0-9][0-9][0-9][0-9][0-9]T[0-9][0-9][0-9][0-9][0-9][0-9]Z 2>/dev/null \
  | sort | head -n -"$KEEP" | xargs -r rm -rf

echo "fortis-backup: ok — $DEST ($(du -sh "$DEST" | cut -f1))"
