# XBT regtest environment

A private regtest XBT chain (Knots with BLAKE2b active at height 20), the
Fortis index as its Esplora server, a static fee file server and paulscode's
LND fork. State lives in `$XBT_REGTEST_DIR` (default `~/.cache/fortis-xbt-regtest`).

    cargo build -p fortis-index     # up.sh uses target/debug/fortis-index
    tools/xbt-regtest/build-lnd.sh  # clones and builds the fork once (needs mise + go 1.27.3)
    tools/xbt-regtest/up.sh         # starts everything, mines to height 101; safe to re-run
    tools/xbt-regtest/down.sh       # stops only the PIDs in $XR/pids
    tools/xbt-regtest/run.sh        # the release gate: wipes state, up, the suite, down

Source `env.sh` for `$XR`, `xr_cli` (bitcoin-cli) and `xr_lncli` (lncli).

| Service | Address |
|---|---|
| Knots RPC (lab / lab) | 127.0.0.1:18543 (P2P 18544, ZMQ 28541 block, 28542 tx) |
| Index (Esplora) | 127.0.0.1:18094 |
| Fee file | 127.0.0.1:18095/fees.json |
| LND gRPC / P2P | 127.0.0.1:10109 / 127.0.0.1:19735 |
| Suite node S (ldk-node, the LSP) | 127.0.0.1:19737 |

`up.sh` refuses to start a component whose port is held by a process it did
not start. `down.sh` verifies each PID's command line contains `$XR` first.
Logs are in `$XR/logs`.

`run.sh` deletes the chain, index, LND and node state under `$XR` (not the
LND build) before it starts, and stops the environment on any exit. The
suite's output is also written to `$XR/suite.log`; the ldk-node logs are in
`$XR/ldk-p` and `$XR/ldk-s`.
