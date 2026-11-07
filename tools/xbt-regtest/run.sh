#!/usr/bin/env bash
# The XBT release gate: a fresh regtest environment (up.sh), the on-chain
# suite (crates/xbt-regtest, every scenario), then down.sh, which runs on any
# exit. The exit code is the suite's.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=env.sh
. "$here/env.sh"
cd "$XR_REPO"
export PATH="$HOME/.cargo/bin:$PATH"

cargo build -p fortis-index -p fortis-lsp -p xbt-regtest
"$here/build-lnd.sh"
# The app node's VSS (app_node_xbt): LDK's vss-server, no-op auth (tests only). Run
# from $XR/bin so down.sh can tell it is ours (its command line names $XR).
vss_bin="${FORTIS_VSS_SERVER:-$(crates/fortis-lightning/scripts/build-vss-server.sh)}"

# Start from nothing: the scenarios assume a new chain, LND wallet and nodes.
"$here/down.sh"
case "$XR" in
  */fortis-xbt-regtest) ;;
  *) echo "run: refusing to wipe unexpected XR=$XR" >&2; exit 1 ;;
esac
rm -rf "$XR"/{knots,index-db,lnd-data,fees,logs,ldk-p,ldk-s,ldk-s.stale,lsp,vss}
rm -rf "$XR"/ldk-* "$XR"/app-*
mkdir -p "$XR/bin"
cp -f "$vss_bin" "$XR/bin/vss-server"

trap '"$here/down.sh"' EXIT
"$here/up.sh"

export XR XR_REPO XR_KNOTS_BIN XR_RPC_PORT XR_INDEX_PORT XR_LND_RPC_PORT XR_LND_P2P_PORT XR_LDK_S_PORT \
  XR_LSP_PORT XR_LSP_ADMIN_PORT XR_VSS_PORT XR_VSS_PG_PORT
status=0
cargo run -q -p xbt-regtest -- all 2>&1 | tee "$XR/suite.log" || status=$?
echo "run: suite exit $status (log $XR/suite.log; node logs in $XR/ldk-p, $XR/ldk-s, $XR/ldk-p2, $XR/lsp, $XR/app-a, $XR/app-b, $XR/vss, $XR/logs)"
exit "$status"
