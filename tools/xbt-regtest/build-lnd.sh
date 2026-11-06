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
src="$XR/src/lightning-fork"
if [ ! -d "$src/.git" ]; then
  git clone --depth 1 --branch "$XR_LND_TAG" "$XR_LND_REPO" "$src"
fi
# An existing checkout may be at an older tag: move it to $XR_LND_TAG.
tag_commit() { git -C "$src" rev-parse -q --verify "refs/tags/$XR_LND_TAG^{commit}" || true; }
if [ -z "$(tag_commit)" ] || [ "$(git -C "$src" rev-parse HEAD)" != "$(tag_commit)" ]; then
  echo "build-lnd: checkout in $src is not at $XR_LND_TAG, fetching it"
  git -C "$src" fetch --depth 1 "$XR_LND_REPO" "refs/tags/$XR_LND_TAG:refs/tags/$XR_LND_TAG"
  git -C "$src" checkout -q --detach "$(tag_commit)"
fi
if [ "$(git -C "$src" rev-parse HEAD)" != "$(tag_commit)" ]; then
  echo "build-lnd: $src is not at $XR_LND_TAG" >&2
  exit 1
fi

cd "$src"
mise exec go@1.27.3 -- go build -tags "$tags" -o "$XR/bin/lnd" ./cmd/lnd
mise exec go@1.27.3 -- go build -tags "$tags" -o "$XR/bin/lncli" ./cmd/lncli
echo "$stamp" >"$XR/bin/build-stamp"
echo "build-lnd: built $("$XR/bin/lnd" --version)"
