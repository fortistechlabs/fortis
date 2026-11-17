#!/usr/bin/env bash
# Post notes to Nostr with `nak`.
#
#   ./post.sh 1-announcement.txt                  # dry run: shows the signed event, sends nothing
#   ./post.sh --send 1-announcement.txt 2-liquidity.txt   # asks, then publishes to the relays
#
# The secret key (nsec) is read without echo, kept only in this process, and never written down.
# Hashtags in the text (#xbt) are also added as "t" tags. The signer's identity is shown first.
set -euo pipefail

RELAYS=(wss://relay.damus.io wss://nos.lol wss://relay.primal.net wss://relay.nostr.band wss://relay.zapstore.dev)
EXPECT_NPUB="npub1qsasj2ljwllm9a3nget7jmdl7ggswf4jtpqjlryyz7l4mxc7lu0qdwthtf"   # the Fortis / Zapstore identity

send=0; files=()
for a in "$@"; do
  case "$a" in
    --send) send=1 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) files+=("$a") ;;
  esac
done
[ "${#files[@]}" -gt 0 ] || { echo "usage: $0 [--send] note.txt [note2.txt ...]" >&2; exit 2; }
for f in "${files[@]}"; do [ -r "$f" ] || { echo "can't read $f" >&2; exit 2; }; done
command -v nak >/dev/null || { echo "nak not found" >&2; exit 2; }

# NEVER sign without an explicit key: nak's default is a key specific to this machine.
read -rsp 'nsec (hidden): ' NOSTR_SECRET_KEY; echo
export NOSTR_SECRET_KEY
trap 'unset NOSTR_SECRET_KEY' EXIT
case "$NOSTR_SECRET_KEY" in nsec1*|[0-9a-fA-F]*) ;; *) echo "that doesn't look like an nsec or hex key" >&2; exit 2 ;; esac

pub=$(nak key public "$NOSTR_SECRET_KEY")
npub=$(nak encode npub "$pub")
echo "signing as: $npub"
if [ "$npub" != "$EXPECT_NPUB" ]; then
  echo "WARNING: this is NOT the Fortis identity ($EXPECT_NPUB)"
  read -rp "post as this identity anyway? [y/N] " yn
  [ "$yn" = "y" ] || { echo "stopped"; exit 1; }
fi

for f in "${files[@]}"; do
  echo; echo "=== $f ($(wc -c < "$f") bytes)"
  tags=()
  while read -r h; do tags+=(-t "t=$(echo "${h#\#}" | tr 'A-Z' 'a-z')"); done < <(grep -o '#[A-Za-z0-9_]\+' "$f" | sort -u)
  if [ "$send" = 1 ]; then
    nak event --confirm -c "@$f" "${tags[@]}" "${RELAYS[@]}"
  else
    echo "(dry run: signed, not sent. Add --send to publish.)"
    nak event -c "@$f" "${tags[@]}"
  fi
done
