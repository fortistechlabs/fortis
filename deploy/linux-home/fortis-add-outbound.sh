#!/usr/bin/env bash
# Add one or more [[outbound]] channels to the XBT LSP config, restart it, and optionally switch
# XBT Lightning on for clients. Dry run unless --apply is given.
#
#   sudo ./fortis-add-outbound.sh [--apply] [--enable] [--config FILE] [--info FILE] \
#        <pubkey>@<host>:<port>=<sat> [<pubkey>@<host>:<port>=<sat> ...]
#
# Example (placeholders, use real values):
#   sudo ./fortis-add-outbound.sh --apply 02aa…66hex…@node1.example.org:9735=150000 03bb…@203.0.113.5:9735=100000
#
# Rules (from crates/fortis-lsp/src/outbound.rs): each sat is 20,000..100,000,000; a peer may be listed
# once; the LSP opens each target at most once per run, only when the on-chain wallet is synced and
# covers the channel, and leaves a peer alone if any channel to it already exists. The entries are
# appended at the END of the file: in TOML everything after an [[outbound]] header belongs to it.
set -euo pipefail

CONF=/etc/fortis/lsp-xbt.toml
INFO=/etc/fortis/ln-info-xbt.json
UNIT=fortis-lsp-xbt
APPLY=0 ENABLE=0 NORESTART=0
peers=()

while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --enable) ENABLE=1 ;;
    --no-restart) NORESTART=1 ;;      # for tests
    --config) CONF=$2; shift ;;
    --info) INFO=$2; shift ;;
    -h|--help) sed -n 2,16p "$0"; exit 0 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) peers+=("$1") ;;
  esac
  shift
done
[ ${#peers[@]} -gt 0 ] || { echo "give at least one <pubkey>@<host>:<port>=<sat>" >&2; exit 2; }
[ -r "$CONF" ] || { echo "cannot read $CONF (run with sudo)" >&2; exit 1; }

block=""
declare -A seen
for p in "${peers[@]}"; do
  [[ "$p" =~ ^(0[23][0-9a-fA-F]{64})@([A-Za-z0-9.-]+|\[[0-9a-fA-F:]+\]):([0-9]{1,5})=([0-9]+)$ ]] \
    || { echo "bad peer spec: $p  (want <66-hex pubkey>@<host>:<port>=<sat>)" >&2; exit 2; }
  id=${BASH_REMATCH[1],,} host=${BASH_REMATCH[2]} port=${BASH_REMATCH[3]} sat=${BASH_REMATCH[4]}
  case "$host" in *.onion) echo "$p: ldk-node cannot dial .onion addresses (it resolves them as plain TCP, and the LSP has no Tor proxy). Use a clearnet host, or a local TCP forward to the onion service (e.g. 127.0.0.1:19735)." >&2; exit 2 ;; esac
  { [ "$port" -ge 1 ] && [ "$port" -le 65535 ]; } || { echo "bad port in $p" >&2; exit 2; }
  { [ "$sat" -ge 20000 ] && [ "$sat" -le 100000000 ]; } || { echo "sat must be 20000..100000000 in $p" >&2; exit 2; }
  [ -z "${seen[$id]:-}" ] || { echo "peer $id given twice" >&2; exit 2; }
  seen[$id]=1
  grep -qi "$id" "$CONF" && { echo "peer $id is already in $CONF — edit it by hand instead" >&2; exit 2; }
  block+=$'\n[[outbound]]\n'"peer = \"$id@$host:$port\""$'\n'"sat = $sat"$'\n'
done

grep -q '^alias *=' "$CONF" && grep -q '^announce_addresses *=' "$CONF" \
  || { echo "$CONF needs alias and announce_addresses (outbound channels are announced)" >&2; exit 1; }

NEW=$(mktemp); trap 'rm -f "$NEW"' EXIT
cat "$CONF" > "$NEW"; [ -n "$(tail -c1 "$NEW")" ] && echo >> "$NEW"
printf '%s' "$block" >> "$NEW"

# The result must be valid TOML with exactly the new entries (plus any old ones) in `outbound`.
python3 -I - "$NEW" "${#peers[@]}" <<'PY'
import sys, tomllib
d = tomllib.load(open(sys.argv[1], "rb"))
ob = d.get("outbound", [])
assert len(ob) >= int(sys.argv[2]), "outbound entries missing"
for o in ob:
    assert set(o) == {"peer", "sat"}, f"unexpected keys in an outbound table: {sorted(o)}"
print(f"config parses; {len(ob)} outbound channel(s) in total")
PY

echo "--- change to $CONF ---"
diff -u "$CONF" "$NEW" || true
if [ "$ENABLE" = 1 ]; then echo "--- would set \"enabled\": true in $INFO and clear its message ---"; fi

if [ "$APPLY" != 1 ]; then echo; echo "Dry run. Re-run with --apply to write it and restart $UNIT."; exit 0; fi

[ "$(id -u)" = 0 ] || [ "$NORESTART" = 1 ] || { echo "--apply needs root" >&2; exit 1; }
BAK="$CONF.bak-$(date -u +%Y%m%d-%H%M%S)"
cp -a "$CONF" "$BAK"
cat "$NEW" > "$CONF"                     # keeps the file's owner and mode
echo "wrote $CONF (backup: $BAK)"

if [ "$ENABLE" = 1 ]; then
  python3 -I - "$INFO" <<'PY'
import json, sys, os, tempfile
p = sys.argv[1]
d = json.load(open(p))
d["enabled"] = True
d.pop("message", None)
st = os.stat(p)
fd, tmp = tempfile.mkstemp(dir=os.path.dirname(p))
with os.fdopen(fd, "w") as f: json.dump(d, f, indent=2); f.write("\n")
os.chmod(tmp, st.st_mode & 0o7777); os.replace(tmp, p)   # one atomic swap: the edge reloads on mtime/size
print("ln-info: enabled = true")
PY
fi

if [ "$NORESTART" = 1 ]; then echo "(not restarting)"; exit 0; fi
systemctl restart "$UNIT"
sleep 3
systemctl is-active --quiet "$UNIT" || { echo "$UNIT did not start — restore: cp -a $BAK $CONF && systemctl restart $UNIT" >&2; journalctl -u "$UNIT" -n 15 --no-pager >&2; exit 1; }
echo "$UNIT is running. Watch:  journalctl -u $UNIT -f   (look for 'opening announced channel' → 'pending' → 'ready')"
echo "Then:  curl -s 127.0.0.1:9746/metrics | grep -E 'fortis_lsp_(channels|outbound_sat)'"
