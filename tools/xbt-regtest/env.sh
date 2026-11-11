# Shared settings for the XBT regtest environment. Source this; do not run it.
# shellcheck shell=bash

XR="${XBT_REGTEST_DIR:-$HOME/.cache/fortis-xbt-regtest}"
XR_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

XR_KNOTS_BIN=/opt/bitcoin-knots/current/bin
XR_RPC_PORT=18543
XR_P2P_PORT=18544
XR_ZMQ_BLOCK_PORT=28541
XR_ZMQ_TX_PORT=28542
XR_INDEX_PORT=18094
XR_FEE_PORT=18095
XR_LND_RPC_PORT=10109
XR_LND_P2P_PORT=19735
# Node S (the LSP) of the suite, crates/xbt-regtest; node P does not listen.
XR_LDK_S_PORT=19737
# fortis-lsp (chain = "xbt"), started by the suite's lsp_xbt_jit scenario: peers, admin/metrics.
# (Production's XBT LSP is on 9835 / 9746; these are regtest-only.)
XR_LSP_PORT=19835
XR_LSP_ADMIN_PORT=19746
# The app node's VSS (vss-server, no-op auth) and its private PostgreSQL, started by the
# suite's app_node_xbt scenario. (Production's fortis-vss and PostgreSQL are never touched.)
XR_VSS_PORT=19780
XR_VSS_PG_PORT=19781
XR_LND_ACTIVATION_HEIGHT=20
XR_LND_TAG=v0.21.6-beta-blake2b.23
XR_LND_REPO=https://github.com/paulscode/lightning-fork

# Ports the environment owns (up.sh checks its own; the suite checks the LSP's and VSS's).
XR_PORTS=("$XR_RPC_PORT" "$XR_P2P_PORT" "$XR_INDEX_PORT" "$XR_FEE_PORT" "$XR_LND_RPC_PORT" "$XR_LND_P2P_PORT"
  "$XR_LSP_PORT" "$XR_LSP_ADMIN_PORT" "$XR_VSS_PORT" "$XR_VSS_PG_PORT")

XR_PIDS="$XR/pids"

xr_cli() {
  "$XR_KNOTS_BIN/bitcoin-cli" -datadir="$XR/knots" -regtest \
    -rpcport="$XR_RPC_PORT" -rpcuser=lab -rpcpassword=lab "$@"
}

xr_lncli() {
  "$XR/bin/lncli" --lnddir="$XR/lnd-data" --network=regtest \
    --rpcserver="127.0.0.1:$XR_LND_RPC_PORT" "$@"
}
