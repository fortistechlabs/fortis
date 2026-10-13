import { test } from 'node:test';
import assert from 'node:assert/strict';
import { unlistedSat } from './esplora.js';

// An address dusted past the index's per-address cap: its largest coins are
// listed, the rest summed under `unlisted` — and the balance counts them.
test('unlisted coins add to the balance', () => {
  const r = { unlisted: [{ address: 'bc1qa', count: 12000, value: 3600000 }, { address: 'bc1qb', count: 1, value: 5 }] };
  assert.equal(unlistedSat(r, new Set(['bc1qa', 'bc1qb'])), 3600005);
});

test('no unlisted field is nothing unlisted', () => {
  assert.equal(unlistedSat({ utxos: [] }, new Set(['bc1qa'])), 0);
});

test('an address that was not asked about is refused', () => {
  assert.throws(() => unlistedSat({ unlisted: [{ address: 'bc1qz', count: 1, value: 1 }] }, new Set(['bc1qa'])));
});
