#!/usr/bin/env bash
# Install the fortis backend on the Linux home machine (Bitcoin Core + Knots on
# this host, managed by ~/Work/bitcoin-nodes). Safe to re-run.
#
#   cargo build --release -p fortis-index -p fortis-edge     # as your user, first
#   sudo deploy/linux-home/install.sh [--index-from DIR] [--cloudflared-from DIR]
#
# The v2 indexes live in /var/lib/fortis/{xbt,btc}-index-v2 and sync from
# scratch on first start (a v1 index directory is refused, never imported).
#
#   --index-from DIR        copy v2 indexes already synced elsewhere (DIR/xbt,
#                           DIR/btc — e.g. by running fortis-index by hand) into
#                           place instead of syncing again; stop those first
#
#   --cloudflared-from DIR  the Windows %USERPROFILE%\.cloudflared folder
#                           (config.yml + <tunnel-id>.json) — reuses the same
#                           tunnel and DNS name
set -euo pipefail

here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
repo=$(cd "$here/../.." && pwd)
index_from= cf_from=
while (( $# )); do
  case $1 in
    --index-from) index_from=$2; shift 2 ;;
    --cloudflared-from) cf_from=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ $EUID -eq 0 ]] || { echo "run with sudo" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

step "Binaries"
for b in fortis-index fortis-edge; do
  [[ -x $repo/target/release/$b ]] || { echo "missing $repo/target/release/$b — run cargo build --release first" >&2; exit 1; }
  install -m 0755 "$repo/target/release/$b" "/usr/local/bin/$b"
done

step "User"
id fortis &>/dev/null || useradd --system --home-dir /var/lib/fortis --shell /usr/bin/nologin fortis
usermod -aG bitcoin-core,bitcoin-knots fortis
install -d -o fortis -g fortis -m 0750 /var/lib/fortis /var/lib/fortis/edge /var/log/fortis

if [[ -n $index_from ]]; then
  for c in xbt btc; do
    [[ -d $index_from/$c ]] || continue
    step "Importing v2 $c index from $index_from/$c"
    systemctl stop "fortis-$c-index" 2>/dev/null || true
    dst=/var/lib/fortis/$c-index-v2
    [[ -e $dst ]] && mv "$dst" "$dst.old.$(date +%s)"
    cp -a "$index_from/$c" "$dst"
    chown -R fortis:fortis "$dst"
  done
fi

step "Units"
install -d -m 0755 /etc/fortis
[[ -e /etc/fortis/edge.env ]] || install -m 0644 "$here/edge.env" /etc/fortis/edge.env
for u in fortis-xbt-index fortis-btc-index fortis-edge; do
  install -m 0644 "$here/$u.service" "/etc/systemd/system/$u.service"
done
systemctl daemon-reload
systemctl enable fortis-xbt-index fortis-btc-index fortis-edge
systemctl restart fortis-xbt-index fortis-btc-index fortis-edge

if [[ -n $cf_from ]]; then
  step "Cloudflare tunnel"
  command -v cloudflared >/dev/null || pacman -S --needed --noconfirm cloudflared
  install -d -m 0755 /etc/cloudflared
  # Already in place (e.g. --cloudflared-from /etc/cloudflared): copying it onto
  # itself would truncate config.yml, so only (re)start the service.
  if [[ $(readlink -f "$cf_from") != /etc/cloudflared ]]; then
    for f in "$cf_from"/*.json "$cf_from"/cert.pem; do
      [[ -e $f ]] && install -m 0600 "$f" /etc/cloudflared/
    done
    # Rewrite the Windows credentials-file path to the Linux one; ingress stays as-is.
    sed -E 's|^(credentials-file:).*[\\/]([^\\/]+\.json)\s*$|\1 /etc/cloudflared/\2|' \
      "$cf_from/config.yml" > /etc/cloudflared/config.yml
  fi
  grep -q 'localhost:8098\|127.0.0.1:8098' /etc/cloudflared/config.yml \
    || echo "warning: config.yml ingress doesn't point at localhost:8098 — check it" >&2
  cloudflared --config /etc/cloudflared/config.yml tunnel ingress validate
  [[ -e /etc/systemd/system/cloudflared.service ]] || cloudflared service install
  systemctl enable --now cloudflared
  systemctl restart cloudflared
fi

step "Status"
sleep 3
systemctl --no-pager --lines=5 status fortis-xbt-index fortis-btc-index fortis-edge || true
for p in 8094 8095 8098; do printf '%s  ' "$p"; curl -s --max-time 3 "http://127.0.0.1:$p/" || echo "(not answering yet)"; echo; done
