// Locking the web wallet when it's left: after a minute hidden, and a fresh
// password (or passkey) before revealing a phrase or sending. Pure, so the
// timing is unit-tested (node --test relock.test.js).

/** How long the tab may be hidden before the wallet locks. */
export const AUTO_LOCK_MS = 60_000;

/** Locks after the tab was hidden for at least `timeoutMs`. */
export class AutoLock {
  constructor(timeoutMs, now = Date.now) {
    this.timeoutMs = timeoutMs;
    this.now = now;
    this.leftAt = null;
  }

  wentToBackground() {
    this.leftAt = this.now();
  }

  /** True once if hidden at least `timeoutMs`; the answer is used up. */
  shouldLockOnReturn() {
    if (this.leftAt === null) return false;
    const away = this.now() - this.leftAt;
    this.leftAt = null;
    return away >= this.timeoutMs;
  }
}

/** Constant-time comparison of two secrets of the same kind — byte arrays
 *  (the app secret is a Uint8Array) or strings; false if either is missing or
 *  they differ in kind or length. */
export function sameSecret(a, b) {
  const bytes = (x) => x instanceof Uint8Array;
  const strs = (x) => typeof x === 'string';
  if (!((bytes(a) && bytes(b)) || (strs(a) && strs(b))) || a.length !== b.length) return false;
  const at = (x, i) => (bytes(x) ? x[i] : x.charCodeAt(i));
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= at(a, i) ^ at(b, i);
  return diff === 0;
}
