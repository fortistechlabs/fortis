import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EsploraBackend, unlistedSat } from './esplora.js';

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

test('_batchScan preserves degraded flag and used addresses on balances()', async () => {
  const session = {
    chain: 'btc',
    addressAt(branch, index) {
      return { address: `bc1q_${branch}_${index}`, script_pubkey_hex: '0014' + String(index).padStart(40, '0') };
    },
  };
  const backend = new EsploraBackend('https://edge.test/btc', session, 'mainnet', { token: 't' });
  backend._postScan = async () => ({
    tip: 850000,
    degraded: true,
    used: ['bc1q_0_0'],
    utxos: [],
    txs: [],
  });
  const bal = await backend.balances();
  assert.equal(bal.confirmed_sat, 0);
  assert.equal(bal.degraded, true);
  assert.equal(backend._snap.degraded, true);
  assert.equal(backend._snap.used.has('bc1q_0_0'), true);
});

test('broadcast invalidation does not re-seed _watchSet from IndexedDB', async () => {
  const session = {
    chain: 'btc',
    addressAt(branch, index) {
      return { address: `bc1q_${branch}_${index}`, script_pubkey_hex: '0014' + String(index).padStart(40, '0') };
    },
  };
  const backend = new EsploraBackend('https://explorer.test/api', session, 'mainnet');
  // Simulate initial cold start that already checked IndexedDB once.
  backend._seededFromIdb = true;
  backend._watchSetCache = [{ address: 'bc1q_0_0', spk: '00', branch: 0, index: 0, txs: [], ok: true }];
  backend._watchSetCacheAt = Date.now();

  let networkWalkCalls = 0;
  backend.get = async (path) => {
    if (path.endsWith('/txs')) {
      networkWalkCalls++;
      return [];
    }
    return '850000';
  };
  backend._fetch = async () => ({
    ok: true,
    status: 200,
    text: async () => 'a'.repeat(64),
  });

  await backend.broadcast('02000000');
  assert.equal(backend._watchSetCache, null);
  assert.equal(backend._seededFromIdb, true);

  const ws = await backend._watchSet();
  assert.ok(networkWalkCalls > 0, 'expected a fresh network walk after broadcast');
  assert.ok(ws.length > 0);
});
