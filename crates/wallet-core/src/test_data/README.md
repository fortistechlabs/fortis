# Vendored test data

## `unified_sighash.json`

`SIGHASH_UNIFIED` reference vectors, copied verbatim from Bitcoin Knots PR #357
(`privkeyio/bitcoin` branch `hf-sighash-opt-in`,
`src/test/data/unified_sighash.json`). MIT-licensed, © The Bitcoin Knots developers.

Schema (first array element is the header):
`[scriptCode, rawTx, inIdx, hashType, scriptType, [[amountSat, spkHex], ...], sighashHex]`

`scriptType`: 0 = bare/P2SH, 1 = segwit v0, 2 = taproot key path, 3 = tapscript.
`wallet-core` implements 0 and 1; the 24 taproot vectors are skipped by the test.

Re-fetch if the PR updates:
`curl -o unified_sighash.json https://raw.githubusercontent.com/privkeyio/bitcoin/hf-sighash-opt-in/src/test/data/unified_sighash.json`
