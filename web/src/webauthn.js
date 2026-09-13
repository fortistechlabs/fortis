// WebAuthn PRF: a platform-authenticator-backed secret for quick app unlock.
// Best-effort progressive enhancement — the app password is always the
// source of truth and always still works; this just skips typing it on a
// device/browser that supports the PRF extension. Support is inconsistent
// across browsers today, so every call here can fail or return null and the
// caller must fall back to the password path without surfacing an error.

const RP_NAME = 'fortis wallet';
const TIMEOUT_MS = 60_000;

export function prfPossible() {
  return typeof PublicKeyCredential !== 'undefined' && !!window.isSecureContext;
}

function toB64(bytes) {
  return btoa(String.fromCharCode(...new Uint8Array(bytes)));
}
function fromB64(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}

async function evalPrf(credentialIdB64, prfSalt) {
  const assertion = await navigator.credentials.get({
    publicKey: {
      challenge: crypto.getRandomValues(new Uint8Array(32)),
      allowCredentials: [{ id: fromB64(credentialIdB64), type: 'public-key' }],
      userVerification: 'required',
      extensions: { prf: { eval: { first: prfSalt } } },
      timeout: TIMEOUT_MS,
    },
  });
  const first = assertion?.getClientExtensionResults()?.prf?.results?.first;
  return first ? new Uint8Array(first) : null;
}

/** Register a platform credential and derive its PRF secret. Returns
 *  `{credentialId, prfSalt, secret}` (all needed to wrap the app secret), or
 *  `null` if this browser/device doesn't support PRF — never throws. */
export async function registerPrf() {
  if (!prfPossible()) return null;
  try {
    const cred = await navigator.credentials.create({
      publicKey: {
        rp: { name: RP_NAME },
        user: { id: crypto.getRandomValues(new Uint8Array(16)), name: 'fortis', displayName: 'fortis wallet' },
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        pubKeyCredParams: [
          { type: 'public-key', alg: -7 }, // ES256
          { type: 'public-key', alg: -257 }, // RS256
        ],
        authenticatorSelection: { authenticatorAttachment: 'platform', userVerification: 'required', residentKey: 'preferred' },
        extensions: { prf: {} },
        timeout: TIMEOUT_MS,
      },
    });
    if (!cred?.getClientExtensionResults()?.prf?.enabled) return null;
    const credentialId = toB64(cred.rawId);
    const prfSaltBytes = crypto.getRandomValues(new Uint8Array(32));
    const secret = await evalPrf(credentialId, prfSaltBytes);
    if (!secret) return null;
    return { credentialId, prfSalt: toB64(prfSaltBytes), secret };
  } catch {
    return null;
  }
}

/** Re-derive the same PRF secret for an already-registered credential.
 *  Throws on rejection/failure — the caller shows "use password instead". */
export async function unlockPrf(credentialId, prfSaltB64) {
  const secret = await evalPrf(credentialId, fromB64(prfSaltB64));
  if (!secret) throw new Error('quick unlock failed');
  return secret;
}
