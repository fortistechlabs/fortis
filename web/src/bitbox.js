// Thin wrapper over the vendored @bitboxswiss/bitbox-api bundle
// (web/vendor/bitbox-api.bundle.js, built by deploy/build-bitbox-bundle.mjs —
// see that script's header for why it's bundled locally rather than loaded
// from a CDN at runtime). This is the only file that imports it; everything
// else in the app calls through here.
//
// BTC only, forever: no generic hardware wallet's firmware can produce a
// valid signature for the BLAKE2b/XBT chain — see wallet-core's psbt.rs
// module doc for why (a structurally different sighash, not just a
// different trailing flag byte). Every function here hardcodes coin 'btc'.
//
// Loaded via dynamic import() on first use, not on every page load — the
// bundle is ~345KB with no reason to pay that unless someone actually clicks
// "Connect BitBox02".

const ACCOUNT_KEYPATH = "m/84'/0'/0'";

let modPromise;
function loadModule() {
  return (modPromise ||= import('../vendor/bitbox-api.bundle.js'));
}

/** Fire-and-forget: start fetching/parsing the vendored bundle as soon as
 *  the BitBox02 UI is shown, so by the time the user actually clicks
 *  "Connect" the import() inside connectAndFetchAccount() is already
 *  resolved. Dynamic import() is async, and stacking it in front of
 *  navigator.hid.requestDevice() risks losing the click's "user activation"
 *  a browser requires before it'll show the WebHID device picker — any
 *  failure here is surfaced again by the next real call, so it's safe to
 *  swallow. */
export function preload() {
  loadModule().catch(() => {});
}

/** True for "the human said no" (or a pairing/operation timed out waiting on
 *  them), as opposed to a real connection/protocol/firmware error: covers
 *  the WebHID device-picker cancel and an on-device operation decline (the
 *  library's own `isUserAbort`), plus an on-device *pairing-code* decline
 *  (`code === 'pairing-rejected'`, which `isUserAbort` does not cover —
 *  confirmed by reading errors.js: it's a separate code from user-abort/
 *  bitbox-user-abort). All three deserve the same non-alarming "cancelled"
 *  copy rather than a generic error. */
export async function isUserDeclined(err) {
  const { isUserAbort } = await loadModule();
  return isUserAbort(err) || err?.code === 'pairing-rejected';
}

/** True when the error looks like "no device found" — WebHID found nothing
 *  or the device is already open elsewhere, or BitBoxBridge isn't running/
 *  reachable (confirmed both map to the same `could-not-open` code in
 *  errors.js, distinguished only in the message text). */
export function isNotFound(err) {
  return err?.code === 'could-not-open';
}

/** Connect (WebHID if this browser/OS supports it, else BitBoxBridge), pair,
 *  run `fn(pairedDevice)`, and always close the handle afterward — a fresh
 *  connect+pair per operation means an unplugged/asleep device is always a
 *  clean "not found" error next time, never a stale-handle bug.
 *
 *  `onPairingCode(code)` is called only when a fresh confirmation is
 *  actually needed (a previously-trusted device is cached by the library in
 *  its own localStorage entry, so most calls after the first skip straight
 *  to `waitConfirm()` with no code to show) — the caller shows it and waits;
 *  this function's promise doesn't resolve until the user confirms
 *  on-device (or declines, or it times out). */
async function withPairedDevice(onPairingCode, fn) {
  const { bitbox02ConnectAuto } = await loadModule();
  const bitbox = await bitbox02ConnectAuto();
  const pairing = await bitbox.unlockAndPair();
  const code = pairing.getPairingCode();
  if (code) onPairingCode?.(code);
  const paired = await pairing.waitConfirm();
  try {
    return await fn(paired);
  } finally {
    paired.close();
  }
}

/** Read the account-0 BIP-84 xpub + fingerprint + device info — everything
 *  needed to add this device as a wallet the same way a watch-only xpub
 *  import is added, plus what to show for it. */
export function connectAndFetchAccount(onPairingCode) {
  return withPairedDevice(onPairingCode, async (paired) => {
    const [fingerprint, xpub] = await Promise.all([
      paired.rootFingerprint(),
      paired.btcXpub('btc', ACCOUNT_KEYPATH, 'zpub', false),
    ]);
    return { fingerprint, xpub, product: paired.product(), version: paired.version() };
  });
}

/** Show the account's first receive address on the device screen for the
 *  user to visually verify on hardware they trust — proof this app derived
 *  the account it claims to, independent of anything the browser reports. */
export function confirmFirstAddress(onPairingCode) {
  return withPairedDevice(onPairingCode, (paired) =>
    paired.btcAddress('btc', `${ACCOUNT_KEYPATH}/0/0`, { simpleType: 'p2wpkh' }, true),
  );
}

/** Sign an unsigned PSBT (base64, as built by planPayment/planSweep) on the
 *  device. Returns it with the device's signature attached but still not
 *  finalized (BIP-174 partial_sigs, not final witness data) — hand the
 *  result to wallet.js's finalizeHardwareSignedPsbt. No script-config is
 *  forced: this app only ever builds P2WPKH PSBTs, which the device infers
 *  on its own from each input's witness_utxo. */
export function signPsbt(psbtBase64, onPairingCode) {
  return withPairedDevice(onPairingCode, (paired) => paired.btcSignPSBT('btc', psbtBase64, undefined, 'default'));
}
