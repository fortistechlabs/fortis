#!/usr/bin/env bash
# Stop the regtest environment. Touches only PIDs recorded in $XR/pids whose
# /proc/<pid>/cmdline contains $XR. Never uses pkill/killall.
set -euo pipefail
# shellcheck source=env.sh
. "$(dirname "${BASH_SOURCE[0]}")/env.sh"

[ -f "$XR_PIDS" ] || { echo "down: nothing recorded"; exit 0; }

ours() { # pid -> true if alive and its command line mentions $XR
  [ -r "/proc/$1/cmdline" ] && tr '\0' ' ' <"/proc/$1/cmdline" | grep -qF -- "$XR"
}

wait_gone() { # pid seconds
  local i
  for ((i = 0; i < $2 * 10; i++)); do
    [ -d "/proc/$1" ] || return 0
    sleep 0.1
  done
  return 1
}

# Reverse start order: lnd, fees, index, knots.
mapfile -t lines < <(tac "$XR_PIDS")
for line in "${lines[@]}"; do
  name="${line%% *}"
  pid="${line##* }"
  if ! ours "$pid"; then
    echo "down: $name ($pid) not running"
    continue
  fi
  if [ "$name" = knots ]; then
    xr_cli stop >/dev/null 2>&1 || true
  else
    kill -TERM "$pid" 2>/dev/null || true
  fi
  if ! wait_gone "$pid" 30; then
    if ours "$pid"; then
      echo "down: $name ($pid) ignoring stop, sending KILL"
      kill -KILL "$pid" 2>/dev/null || true
    fi
  fi
  echo "down: stopped $name ($pid)"
done
rm -f "$XR_PIDS"
