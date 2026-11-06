#!/usr/bin/env bash
# Build paulscode's LND fork (BLAKE2b) into $XR/bin/{lnd,lncli}. Skips if
# built with the same tag and build tags.
set -euo pipefail
# shellcheck source=env.sh
. "$(dirname "${BASH_SOURCE[0]}")/env.sh"

# offersrpc: the fork's BOLT 12 offers (lncli offer ...), used by the suite.
tags="signrpc walletrpc chainrpc invoicesrpc routerrpc peersrpc offersrpc"
stamp="$XR_LND_TAG $tags"
if [ -x "$XR/bin/lnd" ] && [ -x "$XR/bin/lncli" ] &&
  [ "$(cat "$XR/bin/build-stamp" 2>/dev/null)" = "$stamp" ]; then
  echo "build-lnd: binaries exist in $XR/bin, skipping"
  exit 0
fi

mkdir -p "$XR/src" "$XR/bin"
if [ ! -d "$XR/src/lightning-fork/.git" ]; then
  git clone --depth 1 --branch "$XR_LND_TAG" "$XR_LND_REPO" "$XR/src/lightning-fork"
fi

cd "$XR/src/lightning-fork"
mise exec go@1.27.3 -- go build -tags "$tags" -o "$XR/bin/lnd" ./cmd/lnd
mise exec go@1.27.3 -- go build -tags "$tags" -o "$XR/bin/lncli" ./cmd/lncli
echo "$stamp" >"$XR/bin/build-stamp"
echo "build-lnd: built $("$XR/bin/lnd" --version)"
