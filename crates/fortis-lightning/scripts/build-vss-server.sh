#!/usr/bin/env bash
# Builds LDK's vss-server (pinned tag) for the fortis-lightning regtest tests,
# with the no-op authorizer (tests only — never deploy this build), and
# prints the binary path. Source and build live under the workspace target/.
# Usage: FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh)
set -euo pipefail
TAG=v0.1.0-alpha.0            # commit 88a5703496386465556d920dccf49512296c53d0
REV=88a5703496386465556d920dccf49512296c53d0
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SRC="$ROOT/target/tools/vss-server-src"
BIN="$SRC/target/release/vss-server"
if [[ ! -d "$SRC/.git" ]]; then
  mkdir -p "$ROOT/target/tools"
  git clone -q --depth 1 --branch "$TAG" https://github.com/lightningdevkit/vss-server.git "$SRC" >&2
fi
[[ "$(git -C "$SRC" rev-parse HEAD)" == "$REV" ]] || { echo "vss-server checkout is not $REV" >&2; exit 1; }
if [[ ! -x "$BIN" ]]; then
  (cd "$SRC" && RUSTFLAGS="--cfg noop_authorizer" cargo build --release --no-default-features -p vss-server >&2)
fi
echo "$BIN"
