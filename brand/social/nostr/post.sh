#!/usr/bin/env bash
# Publish one of the drafts here as the fortistechlabs Nostr account.
#   ./post.sh 1-launch.txt
# Shows the post, asks for the nsec (hidden; never on the command line or in
# history), refuses any key that isn't fortistechlabs', asks once more, sends.
set -euo pipefail
cd "$(dirname "$0")"

EXPECTED=043b092bf277ffb2f6334657e96dbff2110726b258412f8c8417bf5d9b1eff1e # npub1qsasj2l…dwthtf
RELAYS=(wss://relay.damus.io wss://nos.lol wss://relay.primal.net wss://relay.nostr.band wss://relay.snort.social)

f=${1:?usage: ./post.sh <draft.txt>}
[[ -f $f ]] || { echo "no such draft: $f" >&2; exit 1; }
command -v nak >/dev/null || { echo "nak is not installed" >&2; exit 1; }

# Hashtags in the text become "t" tags, so the post shows up under them.
tags=()
while read -r t; do tags+=(-t "t=$t"); done < <(grep -oE '#[A-Za-z0-9_]+' "$f" | cut -c2- | tr 'A-Z' 'a-z' | sort -u)

echo "──────── $f ────────"; cat "$f"; echo "────────────────────"

read -rsp 'nsec: ' NOSTR_SECRET_KEY; echo
export NOSTR_SECRET_KEY
trap 'unset NOSTR_SECRET_KEY' EXIT
pub=$(printf %s "$NOSTR_SECRET_KEY" | nak key public 2>/dev/null || true)
if [[ $pub != "$EXPECTED" ]]; then
  echo "That key is not the fortistechlabs account — nothing sent." >&2; exit 1
fi

read -rp "Post this as fortistechlabs to ${#RELAYS[@]} relays? [y/N] " ok
[[ $ok == y || $ok == Y ]] || { echo "Not sent."; exit 0; }

nak event -k 1 -c "@$f" "${tags[@]}" "${RELAYS[@]}"
