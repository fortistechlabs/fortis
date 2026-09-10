# wallet-wasm

`wasm-bindgen` bindings over [`wallet-core`](../wallet-core). The master key lives in
wasm linear memory; JS holds opaque handles and passes hex / JSON.

`cargo check -p wallet-wasm` builds on the host. Producing the `.wasm` needs a bit
more toolchain (below).

## Building the package

One-time:

```
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

`secp256k1-sys` compiles C to wasm, so **clang + llvm-ar must be on PATH**:

- Windows: `winget install LLVM.LLVM`, then in the shell you build from
  `set CC=clang` / `set AR=llvm-ar` (or the `CC_wasm32_unknown_unknown` /
  `AR_wasm32_unknown_unknown` vars) if `cc` doesn't find them.
- macOS: `xcode-select --install`.
- Linux: `apt install clang`.

Then:

```
wasm-pack build crates/wallet-wasm --target web --out-dir pkg
```

Output is an ES module (`pkg/wallet_wasm.js`) + `.d.ts` + the `.wasm`. Point the web
trading app's bundler at `pkg/`.

## API

| Export | |
|---|---|
| `generateMnemonic(Uint8Array(32)) → string` | 24-word phrase from platform entropy |
| `new Wallet(mnemonic, passphrase)` | holds the key |
| `wallet.accountXpub(chain, account) → string` | for `WalletView` |
| `wallet.masterFingerprint() → hex` | descriptor key-origin (for `fortisd` `POST /v1/connect`) |
| `wallet.swapPubkey(chain, account, swapIndex) → hex` | per-swap pubkey |
| `wallet.signSwap(chain, account, swapIndex, sighashHex) → hex` | HTLC leg signature |
| `wallet.signFundingTx(chain, account, txHex, selected) → txHex` | signs a tx's P2WPKH inputs (funding *or* a plain payment) |
| `new WalletView(chain, accountXpub)` | watch-only |
| `view.setNextIndices(nextReceive, nextChange)` / `view.nextIndices() → {nextReceive,nextChange}` | resume / read derivation counters across reloads |
| `view.nextReceiveAddress() / nextChangeAddress() → {address, script_pubkey_hex}` | BIP-84 bech32, advances a counter |
| `view.addressAt(branch, index) → {address, script_pubkey_hex}` | a specific address, no advance (branch 0 receive, 1 change) |
| `view.planPayment(utxos, [{address, amount_sat}], feerate, minConf) → plan` | coin selection for an arbitrary payment |
| `view.planSweep(utxos, destAddress, feerate, minConf) → plan` | send the whole confirmed balance, fee deducted |
| `view.planHtlcFunding(utxos, htlcSpkHex, valueSat, feerate, minConf) → plan` | coin selection for a swap-leg funding output |
| `Htlc.build(chain, hashlockHex, redeemPkHex, refundPkHex, locktime, valueSat)` | one leg |
| `htlc.scriptPubkeyHex / witnessScriptHex` | |
| `htlc.findOutput(fundingTxHex, minValueSat) → {vout, value_sat}` | verify a counterparty's funding |
| `htlc.redeemTx / refundTx(prevoutTxid, prevoutVout, payoutSpkHex, feeSat) → txHex` | unsigned |
| `htlc.spendSighash(txHex, inputIndex) → hex` | to feed `wallet.signSwap` |
| `htlc.finalizeRedeem / finalizeRefund(txHex, inputIndex, sigHex, pkHex, [preimageHex]) → txHex` | attach witness |
| **seed sealing** | |
| `sealMnemonic(mnemonic, kek32, nonce24) → hex` / `unsealMnemonic(hex, kek32) → mnemonic` | passkey path (KEK from WebAuthn PRF) |
| `sealMnemonicWithPassword(mnemonic, password, salt, nonce24) → hex` / `unsealMnemonicWithPassword(hex, password, salt) → mnemonic` | password path (Argon2id in wasm; KEK never reaches JS) |
| **`new SwapSession(params)`** | client half of one swap; `params` = `JsSwapParams` (below) |
| `session.state` / `session.role` / `session.revealedPreimage` | getters |
| `session.ourContract() → {script_pubkey_hex, witness_script_hex, locktime, value_sat}` | build our leg |
| `session.verifyTheirFunding(fundingTxHex) → {txid, vout}` | check counterparty funding |
| `session.buildRefund / buildRedeem(txid, vout, payoutSpkHex, feeSat) → {tx_hex, sighash_hex}` | unsigned |
| `session.finalizeRefund(txHex, sigHex, refundPkHex) → txHex` | |
| `session.finalizeRedeem(txHex, sigHex, redeemPkHex, preimageHex) → txHex` | |
| `session.onCounterpartyFunded(confs)` / `onCounterpartyRedeemed(preimageHex)` / `onOurFundingBroadcast(txid)` / `onOurOutputConfirmed(confs)` / `onTimelockExpired()` → state | advance the machine |

`chain` is `"btc"` or `"xbt"`. `"xbt"` spends sign with `SIGHASH_UNIFIED`
(`ALL | UNIFIED = 0x21`, Knots PR #357) automatically.

### `JsSwapParams`

```ts
{
  swap_id_hex, role: "initiator" | "participant",
  send_chain: "btc" | "xbt", recv_chain: "btc" | "xbt",
  send_amount_sat, recv_amount_sat, hashlock_hex,
  our_pubkey_send_hex, their_pubkey_send_hex,
  our_pubkey_recv_hex, their_pubkey_recv_hex,
  our_contract_locktime, their_contract_locktime,   // absolute CLTV, unix secs
  required_incoming_confs
}
```

## End-to-end: fund and refund a BTC HTLC leg

```ts
import init, { generateMnemonic, Wallet, WalletView, Htlc } from "./pkg/wallet_wasm.js";
await init();

// --- wallet ---
const entropy = crypto.getRandomValues(new Uint8Array(32));
const phrase = generateMnemonic(entropy);          // show + confirm backup
const wallet = new Wallet(phrase, "");
const xpub = wallet.accountXpub("btc", 0);
const view = new WalletView("btc", xpub);

const fundHere = view.nextReceiveAddress();          // send testnet BTC here

// --- build our HTLC leg (we send BTC; matched params come from the platform) ---
const SWAP = 7;
const ourPk = wallet.swapPubkey("btc", 0, SWAP);
const htlc = Htlc.build("btc", hashlockHex, theirPkHex, ourPk, refundUnixSecs, 250_000);

// --- fund it ---
const plan = view.planHtlcFunding(utxos, htlc.scriptPubkeyHex, 250_000, feerate, 2);
const fundingHex = wallet.signFundingTx("btc", 0, plan.tx_hex, plan.selected);
// broadcast fundingHex; note its txid + the HTLC output vout

// --- refund path, after the timelock, if the swap stalls ---
const refund = htlc.refundTx(fundingTxid, htlcVout, myPayoutSpkHex, 300);
const sh = htlc.spendSighash(refund, 0);
const sig = wallet.signSwap("btc", 0, SWAP, sh);
const refundHex = htlc.finalizeRefund(refund, 0, sig, ourPk);
// broadcast refundHex once nLockTime passes
```

## Full swap with `SwapSession` (initiator, sends XBT for BTC)

```ts
import init, {
  Wallet, WalletView, SwapSession, sealMnemonicWithPassword,
} from "./pkg/wallet_wasm.js";
await init();

const wallet = new Wallet(phrase, "");
const nonce = crypto.getRandomValues(new Uint8Array(24));
localStorage.blob = sealMnemonicWithPassword(phrase, pw, salt, nonce);   // backup at rest

const SWAP = 12;
const session = new SwapSession({
  swap_id_hex, role: "initiator",
  send_chain: "xbt", recv_chain: "btc",
  send_amount_sat: 2_000_000, recv_amount_sat: 1_000_000, hashlock_hex,
  our_pubkey_send_hex: wallet.swapPubkey("xbt", 0, SWAP),
  their_pubkey_send_hex, 
  our_pubkey_recv_hex: wallet.swapPubkey("btc", 0, SWAP),
  their_pubkey_recv_hex,
  our_contract_locktime, their_contract_locktime, required_incoming_confs: 100,
});

// 1. fund our XBT HTLC
const c = session.ourContract();
const view = new WalletView("xbt", wallet.accountXpub("xbt", 0));
const plan = view.planHtlcFunding(xbtUtxos, c.script_pubkey_hex, 2_000_000, feerate, 100);
const fundingHex = wallet.signFundingTx("xbt", 0, plan.tx_hex, plan.selected);   // SIGHASH_UNIFIED
// broadcast; tell the platform the funding txid

// 2. once their BTC funding is buried deep enough, verify + redeem it
const { txid, vout } = session.verifyTheirFunding(theirFundingTxHex);
const r = session.buildRedeem(txid, vout, myBtcPayoutSpkHex, 300);
const sig = wallet.signSwap("btc", 0, SWAP, r.sighash_hex);
const redeemHex = session.finalizeRedeem(r.tx_hex, sig, wallet.swapPubkey("btc", 0, SWAP), preimageHex);
// broadcast redeemHex — this reveals the preimage on Bitcoin; the counterparty then redeems our XBT

// refund path if it stalls: session.buildRefund(...) → signSwap("xbt",...) → session.finalizeRefund(...)
```

`plan` (from `planPayment` / `planSweep` / `planHtlcFunding`) is a plain object:
`{ tx_hex, fee_sat, change_sat | null, selected: [...] }` — pass `selected`
straight to `wallet.signFundingTx`.

## Consumers

[`web/`](../../web) is the browser wallet built on this module (keys in wasm, seed
sealed in IndexedDB, chain data via a `fortisd` gateway).

## Next

`wallet-ffi` (UniFFI, mobile shells), the `swap-orchestrator` service + watchtower.
