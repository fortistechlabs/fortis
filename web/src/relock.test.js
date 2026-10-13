import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AutoLock, sameSecret } from './relock.js';

test('the wallet locks after a minute hidden, not before', () => {
  let now = 1_000_000;
  const a = new AutoLock(60_000, () => now);
  assert.equal(a.shouldLockOnReturn(), false, 'never hidden');
  a.wentToBackground(); now += 59_999;
  assert.equal(a.shouldLockOnReturn(), false);
  a.wentToBackground(); now += 60_000;
  assert.equal(a.shouldLockOnReturn(), true);
  assert.equal(a.shouldLockOnReturn(), false, 'answered once');
});

test('secrets compare equal only when identical', () => {
  assert.equal(sameSecret('ab12', 'ab12'), true);
  assert.equal(sameSecret('ab12', 'ab13'), false);
  assert.equal(sameSecret('ab12', 'ab1'), false);
  assert.equal(sameSecret(null, 'ab12'), false);
  assert.equal(sameSecret('ab12', undefined), false);
});

test('the app secret is bytes: byte arrays compare equal only when identical', () => {
  const a = Uint8Array.from([1, 2, 3, 4]);
  assert.equal(sameSecret(a, Uint8Array.from([1, 2, 3, 4])), true);
  assert.equal(sameSecret(a, Uint8Array.from([1, 2, 3, 5])), false);
  assert.equal(sameSecret(a, Uint8Array.from([1, 2, 3])), false);
  assert.equal(sameSecret(a, null), false);
  assert.equal(sameSecret(a, '01020304'), false, 'never across types');
});
