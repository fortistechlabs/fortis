#!/usr/bin/env bash
# Start the XBT regtest environment: Knots, fortis-index, fee server, LND fork.
# Idempotent: components already running (per $XR/pids) are left alone.
set -euo pipefail
# shellcheck source=env.sh
. "$(dirname "${BASH_SOURCE[0]}")/env.sh"

mkdir -p "$XR"/{knots,fees,lnd-data,logs}
touch "$XR_PIDS"

recorded() { awk -v n="$1" '$1 == n { p = $2 } END { print p }' "$XR_PIDS"; }

running() { # name -> true if recorded pid is alive and ours
  local pid
  pid="$(recorded "$1")"
  [ -n "$pid" ] && [ -r "/proc/$pid/cmdline" ] &&
    tr '\0' ' ' <"/proc/$pid/cmdline" | grep -qF -- "$XR"
}

record() { # name pid
  local tmp
  tmp="$(grep -v "^$1 " "$XR_PIDS" || true)"
  { [ -n "$tmp" ] && echo "$tmp"; echo "$1 $2"; } >"$XR_PIDS.new"
  mv "$XR_PIDS.new" "$XR_PIDS"
}

port_busy() { ss -Hltn "sport = :$1" | grep -q .; }

check_ports() { # ports...
  local p
  for p in "$@"; do
    if port_busy "$p"; then
      echo "up: port $p is in use by a process this script did not start" >&2
      exit 1
    fi
  done
}

wait_for() { # description timeout-seconds command...
  local what="$1" t="$2" i
  shift 2
  for ((i = 0; i < t * 2; i++)); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  echo "up: timed out waiting for $what (logs in $XR/logs)" >&2
  exit 1
}

# --- Knots ---
if ! running knots; then
  check_ports "$XR_RPC_PORT" "$XR_P2P_PORT"
  cat >"$XR/knots/bitcoin.conf" <<CONF
regtest=1
server=1
txindex=1
blockfilterindex=1
fallbackfee=0.0002
testactivationheight=blake2b@$XR_LND_ACTIVATION_HEIGHT
[regtest]
rpcuser=lab
rpcpassword=lab
rpcport=$XR_RPC_PORT
port=$XR_P2P_PORT
bind=127.0.0.1
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
zmqpubrawblock=tcp://127.0.0.1:$XR_ZMQ_BLOCK_PORT
zmqpubrawtx=tcp://127.0.0.1:$XR_ZMQ_TX_PORT
listenonion=0
discover=0
CONF
  "$XR_KNOTS_BIN/bitcoind" -datadir="$XR/knots" -conf="$XR/knots/bitcoin.conf" \
    >"$XR/logs/knots.log" 2>&1 &
  record knots $!
fi
wait_for "Knots RPC" 60 xr_cli getblockcount

# --- fortis-index ---
if ! running index; then
  check_ports "$XR_INDEX_PORT"
  [ -x "$XR_REPO/target/debug/fortis-index" ] || {
    echo "up: build first: cargo build -p fortis-index" >&2
    exit 1
  }
  "$XR_REPO/target/debug/fortis-index" \
    --rpc-url "http://127.0.0.1:$XR_RPC_PORT" --rpc-auth lab:lab --network regtest \
    --db "$XR/index-db" --bind "127.0.0.1:$XR_INDEX_PORT" \
    >"$XR/logs/index.log" 2>&1 &
  record index $!
fi
wait_for "index /blocks" 60 curl -sf "127.0.0.1:$XR_INDEX_PORT/blocks"

# --- fee server ---
if ! running fees; then
  check_ports "$XR_FEE_PORT"
  echo '{"fee_by_block_target":{"1":1000,"6":1000,"144":1000}}' >"$XR/fees/fees.json"
  python3 -m http.server "$XR_FEE_PORT" --bind 127.0.0.1 --directory "$XR/fees" \
    >"$XR/logs/fees.log" 2>&1 &
  record fees $!
fi
wait_for "fee server" 20 curl -sf "127.0.0.1:$XR_FEE_PORT/fees.json"

# --- LND ---
if ! running lnd; then
  check_ports "$XR_LND_RPC_PORT" "$XR_LND_P2P_PORT"
  [ -x "$XR/bin/lnd" ] || {
    echo "up: run build-lnd.sh first" >&2
    exit 1
  }
  "$XR/bin/lnd" --lnddir="$XR/lnd-data" --noseedbackup --bitcoin.regtest --bitcoin.node=bitcoind \
    --bitcoin.blake2b-activation-height="$XR_LND_ACTIVATION_HEIGHT" \
    --fee.url="http://127.0.0.1:$XR_FEE_PORT/fees.json" \
    --bitcoind.rpchost="127.0.0.1:$XR_RPC_PORT" --bitcoind.rpcuser=lab --bitcoind.rpcpass=lab \
    --bitcoind.zmqpubrawblock="tcp://127.0.0.1:$XR_ZMQ_BLOCK_PORT" \
    --bitcoind.zmqpubrawtx="tcp://127.0.0.1:$XR_ZMQ_TX_PORT" \
    --rpclisten="127.0.0.1:$XR_LND_RPC_PORT" --norest --listen="127.0.0.1:$XR_LND_P2P_PORT" \
    --alias=lf-xbt --accept-keysend --protocol.wumbo-channels \
    >"$XR/logs/lnd.log" 2>&1 &
  record lnd $!
fi

# --- mine to 101 ---
xr_cli -rpcwallet=lab getwalletinfo >/dev/null 2>&1 ||
  xr_cli loadwallet lab >/dev/null 2>&1 ||
  xr_cli createwallet lab >/dev/null
height="$(xr_cli getblockcount)"
if [ "$height" -lt 101 ]; then
  addr="$(xr_cli -rpcwallet=lab getnewaddress)"
  xr_cli -rpcwallet=lab generatetoaddress "$((101 - height))" "$addr" >/dev/null
fi

index_height() { curl -sf "127.0.0.1:$XR_INDEX_PORT/blocks/tip/height"; }
index_at_tip() { [ "$(index_height)" = "$(xr_cli getblockcount)" ]; }
wait_for "index to reach the Knots tip" 60 index_at_tip
wait_for "lncli getinfo" 90 xr_lncli getinfo

echo "up: ready. knots height $(xr_cli getblockcount), index height $(index_height)"
