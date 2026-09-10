// Thin wrapper over the wallet-wasm module. The seed and every private key stay
// inside wasm linear memory — this file only moves strings and plain objects.

import init, {
  generateMnemonic,
  setNetwork,
  Wallet,
  WalletView,
  sealMnemonicWithPassword,
  unsealMnemonicWithPassword,
} from '../pkg/wallet_wasm.js';

let ready;
export function ensureWasm() {
  return (ready ||= init());
}

const rand = (n) => crypto.getRandomValues(new Uint8Array(n));
const toHex = (u8) => [...u8].map((b) => b.toString(16).padStart(2, '0')).join('');
const fromHex = (h) => new Uint8Array(h.match(/../g).map((x) => parseInt(x, 16)));

/** `words` is 12 or 24. `extra` (optional Uint8Array from `EntropyPool.bytes()`)
 *  is folded into the CSPRNG bytes inside wasm — it can only strengthen the seed. */
export function newMnemonic(extra, words = 24) {
  return generateMnemonic(rand(32), extra && extra.length ? extra : undefined, words);
}

/** Validate a phrase (+ passphrase) by constructing a wallet; throws on bad input. */
export function validateMnemonic(chain, network, mnemonic, passphrase) {
  setNetwork(network);
  const w = new Wallet(mnemonic.trim(), passphrase || '');
  const xpub = w.accountXpub(chain, 0);
  w.free();
  return xpub;
}

/** Seal `{mnemonic, passphrase}` under a password. Returns fields to persist. */
export function seal(mnemonic, passphrase, password) {
  const salt = rand(16);
  const nonce = rand(24);
  const payload = JSON.stringify({ m: mnemonic.trim(), p: passphrase || '' });
  return {
    sealed: sealMnemonicWithPassword(payload, password, salt, nonce),
    salt: toHex(salt),
  };
}

/** Reverse `seal`. Throws "wrong password or corrupt data" on a bad password. */
export function unseal(sealed, saltHex, password) {
  const json = unsealMnemonicWithPassword(sealed, password, fromHex(saltHex));
  const { m, p } = JSON.parse(json);
  return { mnemonic: m, passphrase: p || '' };
}

/** A live signing session. Holds the wasm `Wallet` + `WalletView` handles. */
export class Session {
  constructor(chain, network, mnemonic, passphrase) {
    setNetwork(network);
    this.chain = chain;
    this.wallet = new Wallet(mnemonic, passphrase || '');
    this.xpub = this.wallet.accountXpub(chain, 0);
    this.fingerprint = this.wallet.masterFingerprint();
    this.view = new WalletView(chain, this.xpub);
  }

  setIndices(nextReceive, nextChange) {
    this.view.setNextIndices(nextReceive >>> 0, nextChange >>> 0);
  }
  indices() {
    return this.view.nextIndices(); // { next_receive, next_change }
  }
  addressAt(branch, index) {
    return this.view.addressAt(branch >>> 0, index >>> 0); // { address, script_pubkey_hex }
  }
  receiveAddress(index) {
    return this.addressAt(0, index);
  }
  /** Throws "… is not a valid address" if `address` doesn't parse on this
   *  network. Cheap — call before any network I/O so a bad address isn't
   *  masked by a later "no coins" error. */
  checkAddress(address) {
    return this.view.checkAddress(address);
  }
  planPayment(utxos, outputs, feerate, minConf, opReturnHex, serviceFee, feeFromAmount = false) {
    return this.view.planPayment(
      utxos, outputs, BigInt(feerate), minConf >>> 0, opReturnHex || undefined, serviceFee || undefined,
      !!feeFromAmount,
    );
  }
  planSweep(utxos, destAddress, feerate, minConf, serviceFee) {
    return this.view.planSweep(utxos, destAddress, BigInt(feerate), minConf >>> 0, serviceFee || undefined);
  }
  sign(planTxHex, selected) {
    return this.wallet.signFundingTx(this.chain, 0, planTxHex, selected);
  }
  free() {
    try {
      this.view.free();
      this.wallet.free();
    } catch {
      /* already freed */
    }
  }
}
