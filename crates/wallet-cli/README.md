# wallet-cli

`fortis` — a command-line wallet that drives [`wallet-core`](../wallet-core) against
a **Bitcoin Knots / BLAKE2b-fork** full node over JSON-RPC.

`wallet-core` owns keys, derivation, coin selection and signing; the node is the
chain backend — UTXOs, confirmations, fee estimation, broadcast. This crate is the
"platform shell" the core's design assumes, for desktop.

## Status

Send-capable. Not yet done: RBF fee bumps, PSBT import/export, the HTLC
atomic-swap flow.

- generates / restores a BIP-84 wallet; the seed is shown once and only written to
  disk if you seal it (`import-seed` / `init --seal`)
- imports the account xpub into a **watch-only** descriptor wallet on the node
- derives addresses with `wallet-core`, cross-checked against the node (`ismine`)
- reports balance and UTXOs as the node sees them
- builds, signs (`SIGHASH_UNIFIED` on the BLAKE2b chain), pre-checks with
  `testmempoolaccept`, and broadcasts payments and sweeps

## Requirements

- A running Bitcoin Knots BLAKE2b node with `server=1`. Cookie auth is automatic
  (`<datadir>/.cookie`, or `<datadir>/regtest/.cookie`); static `rpc_user` /
  `rpc_password` in `wallet.json` also work.
- The `blake2b` deployment active on the node (`fortis node` reports it).

## Use

```sh
# create (prints a 24-word phrase — write it down) or restore
fortis init --datadir "C:\Bitcoin\Knots"
fortis init --restore --datadir "C:\Bitcoin\Knots"

fortis node                     # chain, sync, BLAKE2b activation
fortis connect                  # import watch-only descriptors (--rescan for old funds)

fortis address                  # next unused receive address
fortis address --change
fortis address --peek 5
fortis balance
fortis utxos

# spending
fortis import-seed              # seal the seed at <home>/seed.enc (prompts for a password)
fortis send bc1q... 0.5         # amount in XBT; --sats for satoshis
fortis send bc1q... --sweep     # whole confirmed balance, fee deducted
fortis send bc1q... 0.5 --dry-run          # print the signed hex, don't broadcast
fortis send bc1q... 0.5 --phrase-stdin     # sign from a piped mnemonic instead of seed.enc
fortis send bc1q... 0.5 --feerate 3        # sat/vB; default is estimatesmartfee, floored at minrelay
fortis broadcast <hex>
```

`send` prints a summary (inputs, outputs, change, fee, feerate, txid) and asks you
to type `yes` before broadcasting — `--yes` skips that, `--dry-run` stops before it.

## Files

`<home>` is `$FORTIS_HOME`, else `%APPDATA%\fortis` (Windows) / `~/.fortis` (Unix).

| File | Contents |
|---|---|
| `wallet.json` | account xpub, master fingerprint, address counters, node connection — **no secrets** |
| `seed.enc` | the mnemonic sealed with XChaCha20-Poly1305 over an Argon2id password (only if you ran `import-seed` / `init --seal`) |

`$FORTIS_SEED_PASSWORD`, if set, is used instead of prompting — for scripting.

On the node, `fortis connect` creates a private-keys-disabled descriptor wallet
named `fortis-<chain>` (e.g. `fortis-xbt`).

## Layout

| File | Role |
|---|---|
| `main.rs` | CLI (clap) + command implementations |
| `config.rs` | `wallet.json` model, home-dir / cookie resolution |
| `seed.rs` | seal / unseal the mnemonic; load the master key for signing |
| `rpc.rs` | minimal JSON-RPC client (`ureq`, cookie/basic auth, wallet-scoped calls) |
| `node.rs` | chain status, watch-only wallet, descriptor import, fee estimation, UTXO reads |
| `tests/regtest_e2e.rs` | full send path against a real regtest node with BLAKE2b active (set `FORTIS_BITCOIND`) |
