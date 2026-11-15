#!/usr/bin/env bash
# Read-only health check of the XBT Lightning backends (deploy/lightning.md §11-12).
# Run it ON THE HOME HOST. It changes nothing and prints no secrets (no tokens, seeds,
# keys or file contents - only yes/no, versions, heights and public ids).
#
#   bash deploy/linux-home/check-xbt-lightning.sh
#   EDGE_URL=https://api.fortistechlabs.com bash deploy/linux-home/check-xbt-lightning.sh
#
# Needs: curl, jq (optional but recommended), sudo (for /etc/fortis and /var/lib/fortis;
# without it those checks are skipped).
set -u

EDGE_URL=${EDGE_URL:-https://api.fortistechlabs.com}
XBT_INDEX=${XBT_INDEX:-http://127.0.0.1:8094}
XBT_ADMIN=${XBT_ADMIN:-http://127.0.0.1:9746}
BTC_ADMIN=${BTC_ADMIN:-http://127.0.0.1:9736}
XBT_PORT=${XBT_PORT:-9835}

pass=0; warn=0; fail=0
ok()   { printf '  [ OK ] %s\n' "$*"; pass=$((pass+1)); }
bad()  { printf '  [FAIL] %s\n' "$*"; fail=$((fail+1)); }
wrn()  { printf '  [warn] %s\n' "$*"; warn=$((warn+1)); }
info() { printf '         %s\n' "$*"; }
sec()  { printf '\n== %s\n' "$*"; }

have() { command -v "$1" >/dev/null 2>&1; }
if sudo -n true 2>/dev/null; then SUDO="sudo -n"; else SUDO=""; fi
rd() { $SUDO cat "$1" 2>/dev/null; }          # read a root-owned file, quietly
have jq || echo "note: jq not found - some checks are reduced"

sec "1. Services"
for u in fortis-xbt-index fortis-lsp-xbt; do
  if systemctl is-active --quiet "$u" 2>/dev/null; then ok "$u is running"; else bad "$u is NOT running (sudo systemctl status $u)"; fi
done
for u in fortis-edge fortis-lsp; do
  if systemctl is-active --quiet "$u" 2>/dev/null; then ok "$u is running"; else wrn "$u is not running (or has another unit name)"; fi
done

sec "2. XBT index (Esplora) at $XBT_INDEX"
tip=$(curl -fsS --max-time 5 "$XBT_INDEX/blocks/tip/height" 2>/dev/null || true)
if [[ "$tip" =~ ^[0-9]+$ ]]; then
  ok "index answers, tip height $tip"
  if have knots-cli; then
    node=$(knots-cli getblockcount 2>/dev/null || true)
    if [[ "$node" =~ ^[0-9]+$ ]]; then
      d=$(( node - tip )); d=${d#-}
      if (( d <= 2 )); then ok "index is in step with Knots (node $node)"; else bad "index tip $tip vs Knots $node: still syncing"; fi
    else wrn "knots-cli did not answer; could not compare heights"; fi
  else wrn "knots-cli not on PATH; could not compare with the node"; fi
else
  bad "index did not answer on $XBT_INDEX/blocks/tip/height"
fi

sec "3. XBT LSP (admin $XBT_ADMIN)"
lsp_id=""
if body=$(curl -fsS --max-time 5 "$XBT_ADMIN/info" 2>/dev/null); then
  ok "LSP admin answers"
  if have jq; then lsp_id=$(printf '%s' "$body" | jq -r '.node_id // empty' 2>/dev/null); fi
  [ -n "$lsp_id" ] && info "node_id $lsp_id" || wrn "no node_id in /info"
else
  bad "LSP admin did not answer on $XBT_ADMIN/info"
fi
if m=$(curl -fsS --max-time 5 "$XBT_ADMIN/metrics" 2>/dev/null); then
  printf '%s\n' "$m" | grep -E '^fortis_lsp_(channels|custody_sat|onchain|balance|peers)' | sed 's/^/         /' | head -12
else wrn "no metrics from $XBT_ADMIN/metrics"; fi
if ss -ltn 2>/dev/null | grep -q ":$XBT_PORT "; then ok "port $XBT_PORT is listening locally"; else bad "nothing listens on port $XBT_PORT"; fi
if have nc; then
  if nc -z -w 5 lsp.fortistechlabs.com "$XBT_PORT" 2>/dev/null; then ok "lsp.fortistechlabs.com:$XBT_PORT reachable from here"
  else wrn "lsp.fortistechlabs.com:$XBT_PORT not reachable from here - test it from MOBILE DATA (the runbook requires that)"; fi
fi

sec "4. LSP config /etc/fortis/lsp-xbt.toml"
cfg=$(rd /etc/fortis/lsp-xbt.toml)
if [ -z "$cfg" ]; then
  wrn "cannot read /etc/fortis/lsp-xbt.toml (no sudo, or it does not exist)"
else
  grep -Eq '^\s*chain\s*=\s*"xbt"' <<<"$cfg" && ok 'chain = "xbt"' || bad 'chain is not "xbt"'
  grep -Eq '^\s*esplora_url\s*=' <<<"$cfg" && ok "esplora_url set" || bad "esplora_url missing"
  grep -Eq '^\s*xbt_gossip_floor_height\s*=\s*961640' <<<"$cfg" && ok "gossip floor 961640" || wrn "xbt_gossip_floor_height is not 961640"
  if grep -Eq '^\s*(bitcoind_rpc|rpc_user|rpc_password_file|rgs_url)\s*=' <<<"$cfg"; then bad "forbidden BTC-only key present (bitcoind_rpc / rpc_* / rgs_url)"; else ok "no BTC-only keys"; fi
  grep -Eq '^\s*fcm_service_account\s*=' <<<"$cfg" && ok "fcm_service_account set (push notifications)" || bad "fcm_service_account missing - sleeping phones get no push"
  btc=$(rd /etc/fortis/lsp.toml)
  for k in custody_max_sat account_max_sat; do
    a=$(grep -E "^\s*$k\s*=" <<<"$cfg" | head -1 | tr -d ' '); b=$(grep -E "^\s*$k\s*=" <<<"$btc" | head -1 | tr -d ' ')
    if [ -z "$a" ]; then bad "$k missing in lsp-xbt.toml"
    elif [ -n "$b" ] && [ "$a" != "$b" ]; then wrn "$k differs from the BTC LSP ($a vs $b)"
    else ok "$a"; fi
  done
fi

sec "5. LSP data and seed backup (no contents shown)"
seed=/var/lib/fortis/lsp-xbt/keys_seed
if $SUDO test -f "$seed" 2>/dev/null; then
  ok "keys_seed exists ($($SUDO stat -c '%a %U' "$seed" 2>/dev/null))"
  info "I cannot verify an OFF-HOST backup: confirm you have one before funding (runbook 11.3 step 5)"
else wrn "keys_seed not found / not readable (no sudo?)"; fi

sec "6. /ln/info file and edge flags"
li=$(rd /etc/fortis/ln-info-xbt.json)
if [ -z "$li" ]; then bad "/etc/fortis/ln-info-xbt.json missing or unreadable"
elif have jq; then
  en=$(jq -r '.enabled' <<<"$li" 2>/dev/null); nid=$(jq -r '.node_id' <<<"$li" 2>/dev/null); port=$(jq -r '.port' <<<"$li" 2>/dev/null)
  info "enabled=$en port=$port"
  if [ -n "$lsp_id" ] && [ "$nid" = "$lsp_id" ]; then ok "node_id matches the running LSP"; else bad "node_id in ln-info-xbt.json does not match the LSP ($nid)"; fi
  [ "$port" = "$XBT_PORT" ] && ok "port $XBT_PORT" || wrn "ln-info port is $port, expected $XBT_PORT"
  [ "$en" = "false" ] && ok "enabled=false (correct until the canary has passed)" || wrn "enabled=$en - users are exposed; the canary must have passed"
else wrn "jq missing: not parsing ln-info-xbt.json"; fi
env=$(rd /etc/fortis/edge.env)
if [ -n "$env" ]; then
  grep -q -- '--ln-info-xbt-file' <<<"$env" && ok "edge has --ln-info-xbt-file" || bad "edge.env lacks --ln-info-xbt-file"
  grep -q -- '--lsp-xbt-admin-url' <<<"$env" && ok "edge has --lsp-xbt-admin-url" || bad "edge.env lacks --lsp-xbt-admin-url"
else wrn "cannot read /etc/fortis/edge.env"; fi

sec "7. Public edge $EDGE_URL"
tok=$(curl -fsS --max-time 8 -X POST "$EDGE_URL/register" 2>/dev/null | { have jq && jq -r '.token // empty' || cat; } 2>/dev/null)
if [ -n "$tok" ]; then
  ok "edge answers /register"
  if resp=$(curl -fsS --max-time 8 -H "Authorization: Bearer $tok" "$EDGE_URL/ln/info" 2>/dev/null) && have jq; then
    x=$(jq -c '.xbt // empty' <<<"$resp")
    if [ -n "$x" ]; then
      ok "/ln/info carries the xbt object"
      info "enabled=$(jq -r '.xbt.enabled' <<<"$resp") message=$(jq -r '.xbt.message' <<<"$resp")"
      [ -n "$lsp_id" ] && [ "$(jq -r '.xbt.node_id' <<<"$resp")" = "$lsp_id" ] && ok "public node_id matches the LSP" || wrn "public node_id differs from the LSP (or LSP id unknown)"
    else bad "/ln/info has no xbt object - the edge is not serving XBT yet"; fi
  else wrn "could not read /ln/info (or jq missing)"; fi
else bad "edge did not answer /register at $EDGE_URL"; fi
unset tok

sec "8. Recent LSP errors"
if have journalctl; then
  errs=$($SUDO journalctl -u fortis-lsp-xbt -n 300 --no-pager 2>/dev/null | grep -Ei 'error|panic|fail' || true)
  n=$(printf '%s' "$errs" | grep -c . || true)
  if [ "${n:-0}" -eq 0 ]; then ok "no error lines in the last 300 journal entries"; else wrn "$n error-like lines; the last 5:"; printf '%s\n' "$errs" | tail -5 | cut -c1-200 | sed 's/^/         /'; fi
fi

printf '\n== Summary: %d ok, %d warnings, %d failed\n' "$pass" "$warn" "$fail"
echo "Not checked here: LSP funding and outbound channels (runbook 11.6), off-host seed backup, mobile-data reachability, the outside XBT node for the canary."
[ "$fail" -eq 0 ]
