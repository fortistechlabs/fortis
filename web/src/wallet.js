// Thin wrapper over the wallet-wasm module. The seed and every private key stay
// inside wasm linear memory — this file only moves strings and plain objects.

import init, {
  generateMnemonic,
  setNetwork,
  Wallet,
  WalletView,
  sealMnemonic,
  unsealMnemonic,
  sealMnemonicWithPassword,
  unsealMnemonicWithPassword,
  finalizeHardwareSignedPsbt as wasmFinalizeHardwareSignedPsbt,
} from '../pkg/wallet_wasm.js';

let ready;
export function ensureWasm() {
  return (ready ||= init());
}

/** `NETWORK` is a wasm-global (thread_local), not per-`Wallet`/`WalletView` —
 *  call this before touching a given wallet's session/backend whenever more
 *  than one wallet may be held in memory on different networks (mainnet +
 *  regtest side by side). */
export function activateNetwork(network) {
  setNetwork(network);
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

/** Finalize a PSBT that already carries a hardware signer's signature (e.g. a
 *  BitBox02's `btcSignPSBT` response — BIP-174 `partial_sigs`, not final
 *  witness data) into broadcast-ready hex. No signing session/seed needed —
 *  every signature is re-verified against the PSBT's own witness_utxo, never
 *  trusted just because it's present. Throws for any chain other than "btc"
 *  (see wallet-core's psbt module doc for why). */
export function finalizeHardwareSignedPsbt(chain, psbtBase64) {
  return wasmFinalizeHardwareSignedPsbt(chain, psbtBase64);
}

/** A fresh random 32-byte app secret — the one thing that, combined with a
 *  wallet's own `salt`, unseals its mnemonic. Wrapped at rest under a
 *  password and/or a WebAuthn-PRF secret (see webauthn.js); never stored raw. */
export function newAppSecret() {
  return rand(32);
}

/** Seal a wallet's mnemonic under the app secret (fed through the same
 *  Argon2id-then-XChaCha20-Poly1305 path as a plain password — one seal code
 *  path per wallet regardless of which lock mode unlocked `appSecret`). */
export function sealWithAppSecret(mnemonic, passphrase, appSecret) {
  return seal(mnemonic, passphrase, toHex(appSecret));
}
export function unsealWithAppSecret(sealed, saltHex, appSecret) {
  return unseal(sealed, saltHex, toHex(appSecret));
}

/** Wrap/unwrap the app secret itself under the user's app password. */
export function wrapAppSecretWithPassword(appSecret, password) {
  const salt = rand(16);
  const nonce = rand(24);
  return { wrapped: sealMnemonicWithPassword(toHex(appSecret), password, salt, nonce), salt: toHex(salt) };
}
export function unwrapAppSecretWithPassword(wrapped, saltHex, password) {
  return fromHex(unsealMnemonicWithPassword(wrapped, password, fromHex(saltHex)));
}

/** Wrap/unwrap the app secret under a raw 32-byte key (a WebAuthn-PRF
 *  secret). */
export function wrapAppSecretWithKey(appSecret, kek) {
  const nonce = rand(24);
  return { wrapped: sealMnemonic(toHex(appSecret), kek, nonce) };
}
export function unwrapAppSecretWithKey(wrapped, kek) {
  return fromHex(unsealMnemonic(wrapped, kek));
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
    // Lets planPayment()/planSweep() also return a PSBT a paired offline
    // signing session (this same wallet's seed, on a device that never
    // touches the network) can import, review, and sign.
    this.view.setMasterFingerprint(this.fingerprint);
  }

  /** A read-only session from just an xpub — no `Wallet`, no private key,
   *  ever. `wallet_core::WalletView` (what this wraps) stores only a public
   *  `Xpub` and derives addresses via secp256k1's verification-only context
   *  — there is no signing method anywhere on the type, so this is
   *  incapable of signing by construction, not just by convention.
   *
   *  `fingerprint` (optional): the signing wallet's master fingerprint, if
   *  this watch-only import captured one — without it, planPayment()/
   *  planSweep() simply never get a psbt_base64 (see wallet-core's
   *  `FundingPlan::psbt_base64` doc), not an error. */
  static watchOnly(chain, network, xpub, fingerprint) {
    const s = Object.create(Session.prototype);
    setNetwork(network);
    s.chain = chain;
    s.wallet = null;
    s.xpub = xpub;
    s.fingerprint = fingerprint || null;
    s.view = new WalletView(chain, xpub);
    if (s.fingerprint) s.view.setMasterFingerprint(s.fingerprint);
    return s;
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
  /** Defense in depth — the real gate is that a watch-only wallet never
   *  shows a Send tab, so this should never actually be reached. */
  sign(planTxHex, selected) {
    if (!this.wallet) throw new Error('watch-only — cannot sign');
    return this.wallet.signFundingTx(this.chain, 0, planTxHex, selected);
  }
  /** Review an unsigned PSBT (base64) imported for offline signing — same
   *  shape as a live planPayment()/planSweep() result (fee_sat, change_sat,
   *  destinations, selected), reviewed entirely from what's embedded in the
   *  PSBT itself, no network call. Only a signing session (one holding the
   *  seed) can call this — same guard as sign(). */
  reviewImportedPsbt(psbtBase64) {
    if (!this.wallet) throw new Error('watch-only — cannot sign');
    return this.wallet.reviewPsbt(this.chain, 0, psbtBase64);
  }
  /** Sign every input of an imported unsigned PSBT and return the finalized,
   *  broadcast-ready transaction hex — hand this straight to a *watch-only*
   *  session's backend.broadcast(), unchanged. */
  signImportedPsbt(psbtBase64) {
    if (!this.wallet) throw new Error('watch-only — cannot sign');
    return this.wallet.signPsbt(this.chain, 0, psbtBase64);
  }
  free() {
    try {
      this.view.free();
      this.wallet?.free();
    } catch {
      /* already freed */
    }
  }
}
