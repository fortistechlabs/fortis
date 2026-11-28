import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FortisAccount, AccountError, newNonce, buildAccountRequest, edgeTransport } from './lnaccount.js';

const NODE = '02' + '11'.repeat(32);
const signer = () => {
  const s = { signed: null, nodeId: () => NODE, sign: (body) => ((s.signed = body), 'zbase32sig') };
  return s;
};
const account = (reply, sent = {}) =>
  new FortisAccount(async (body, sig) => ((sent.body = body), (sent.sig = sig), reply), () => 1_800_000_000);

test('a request is signed and sent byte for byte, with a 2 minute expiry and a 32 character nonce', async () => {
  const sent = {};
  const s = signer();
  const b = await account({ status: 200, text: '{"balance_msat":5000,"held_msat":1000,"max_msat":50000000}' }, sent).balance(s);
  assert.deepEqual(b, { balanceMsat: 5000, heldMsat: 1000, maxMsat: 50_000_000, freeMsat: 4000 });
  assert.equal(sent.body, s.signed, 'the signed bytes are the sent bytes');
  assert.equal(sent.sig, 'zbase32sig');
  const o = JSON.parse(sent.body);
  assert.equal(o.op, 'balance');
  assert.equal(o.node_id, NODE);
  assert.equal(o.expires_at, 1_800_000_120);
  assert.equal(o.nonce.length, 32);
  assert.match(o.nonce, /^[A-Za-z0-9_-]{32}$/);
  assert.deepEqual(Object.keys(o), ['node_id', 'op', 'params', 'nonce', 'expires_at']);
});

test('nonces are not repeated', () => {
  assert.equal(new Set(Array.from({ length: 200 }, newNonce)).size, 200);
});

test('free balance never goes below zero', async () => {
  const b = await account({ status: 200, text: '{"balance_msat":100,"held_msat":900,"max_msat":1}' }).balance(signer());
  assert.equal(b.freeMsat, 0);
});

const kindOf = async (status, text) => {
  try { await account({ status, text }).balance(signer()); } catch (e) { return e; }
  assert.fail('should have thrown');
};

test('server errors map to their meaning', async () => {
  const full = await kindOf(409, '{"error":"custody_full","min_channel_sat":11000}');
  assert.ok(full instanceof AccountError);
  assert.equal(full.kind, 'custody_full');
  assert.equal(full.minChannelSat, 11000);
  assert.equal((await kindOf(402, '{"error":"insufficient"}')).kind, 'insufficient');
  const auth = await kindOf(401, '{"error":"auth","reason":"replayed"}');
  assert.equal(auth.kind, 'auth');
  assert.equal(auth.reason, 'replayed');
  assert.equal((await kindOf(502, '{}')).kind, 'network');
  assert.equal((await kindOf(500, 'not json')).kind, 'network');
  assert.equal((await kindOf(409, '{"error":"name_taken"}')).kind, 'name_taken');
  assert.equal((await kindOf(409, '{"error":"has_name"}')).kind, 'has_name');
  assert.equal((await kindOf(409, '{"error":"duplicate_payment"}')).kind, 'duplicate');
  const bad = await kindOf(400, '{"error":"name_invalid","reason":"reserved"}');
  assert.equal(bad.kind, 'name_invalid');
  assert.equal(bad.reason, 'reserved');
  const other = await kindOf(404, '{"error":"unknown_op"}');
  assert.equal(other.kind, 'rejected');
  assert.equal(other.message, 'unknown_op');
});

test('a transport that throws is a network error', async () => {
  const a = new FortisAccount(async () => { throw new TypeError('Failed to fetch'); });
  await assert.rejects(a.balance(signer()), (e) => e.kind === 'network');
});

test('each operation sends its op name and parameters', async () => {
  const sent = {};
  const ok = (text) => account({ status: 200, text }, sent);
  const op = () => JSON.parse(sent.body);
  const s = signer();
  assert.equal(await ok('{"invoice":"lnbc1"}').invoice(s, 5000, 'hi'), 'lnbc1');
  assert.deepEqual([op().op, op().params], ['invoice', { amount_msat: 5000, description: 'hi' }]);
  assert.deepEqual(await ok('{"internal":true,"payment_hash":"ab"}').pay(s, 'lnbc2', null), { internal: true, paymentHash: 'ab' });
  assert.deepEqual(op().params, { invoice: 'lnbc2' });
  await ok('{"internal":false,"payment_hash":"cd"}').pay(s, 'lnbc2', 7000);
  assert.deepEqual(op().params, { invoice: 'lnbc2', amount_msat: 7000 });
  assert.equal(await ok('{"payment_id":"pid"}').payOffer(s, 'lno1x', 9000), 'pid');
  assert.deepEqual([op().op, op().params], ['pay_offer', { offer: 'lno1x', amount_msat: 9000 }]);
  assert.equal(await ok('{"status":"pending"}').payStatus(s, 'pid'), 'pending');
  assert.deepEqual([op().op, op().params], ['pay_status', { payment_id: 'pid' }]);
  assert.equal(await ok('{"offer":"lno1y"}').offer(s), 'lno1y');
  assert.equal(op().op, 'offer');
  assert.equal(await ok('{"name":"donal"}').myName(s, 'xbt'), 'donal');
  assert.deepEqual([op().op, op().params], ['my_name', { chain: 'xbt' }]);
  assert.equal(await ok('{"name":null}').myName(s, 'btc'), null);
  assert.equal(await ok('{"name":"dbtc"}').claimName(s, 'btc', 'dbtc'), 'dbtc');
  assert.deepEqual([op().op, op().params], ['claim_name', { chain: 'btc', name: 'dbtc' }]);
  assert.equal(await ok('{"released":"dbtc"}').releaseName(s, 'btc'), 'dbtc');
  assert.equal(await ok('{"released":null}').releaseName(s, 'btc'), null);
  assert.equal(op().op, 'release_name');
  assert.equal(await ok('{"txid":"tx"}').withdraw(s, 100_000, 'bc1qaddr', 3), 'tx');
  assert.deepEqual([op().op, op().params], ['withdraw', { amount_msat: 100_000, address: 'bc1qaddr', feerate_sat_vb: 3 }]);
  const h = await ok('[{"at_secs":5,"kind":"received","amount_msat":2000,"fee_msat":0,"payment_hash":"ph"},{"at_secs":6,"kind":"sent","amount_msat":-3000,"fee_msat":1000,"payment_hash":null}]').history(s);
  assert.deepEqual([op().op, op().params], ['history', { limit: 50 }]);
  assert.deepEqual(h, [
    { atSecs: 5, kind: 'received', amountMsat: 2000, feeMsat: 0, paymentHash: 'ph' },
    { atSecs: 6, kind: 'sent', amountMsat: -3000, feeMsat: 1000, paymentHash: null },
  ]);
});

test('the edge transport posts to the chain\'s URL with the token and signature, and refreshes a stale token once', async () => {
  const calls = [];
  let token = 'old';
  const fetchImpl = async (url, init) => {
    calls.push({ url, auth: init.headers.authorization, sig: init.headers['x-fortis-sig'], body: init.body });
    if (init.headers.authorization === 'Bearer old') return { status: 401, text: async () => '' };
    return { status: 200, text: async () => '{"ok":1}' };
  };
  const mk = (chain) => edgeTransport({ base: 'https://edge.example/', chain, getToken: () => token, refreshToken: async () => (token = 'new'), fetchImpl });
  assert.deepEqual(await mk('btc')('{"a":1}', 'sig1'), { status: 200, text: '{"ok":1}' });
  assert.equal(calls[0].url, 'https://edge.example/ln/account');
  assert.equal(calls[1].url, 'https://edge.example/ln/account');
  assert.equal(calls[1].auth, 'Bearer new');
  assert.equal(calls[1].sig, 'sig1');
  assert.equal(calls[1].body, '{"a":1}');
  assert.equal(calls.length, 2);
  await mk('xbt')('{}', 's');
  assert.equal(calls[2].url, 'https://edge.example/ln/account?chain=xbt');
});

test('a 401 that names a reason is not retried with a new token', async () => {
  let n = 0;
  const fetchImpl = async () => (n++, { status: 401, text: async () => '{"error":"auth","reason":"expired"}' });
  const t = edgeTransport({ base: 'https://e', chain: 'btc', getToken: () => 't', refreshToken: async () => 't2', fetchImpl });
  assert.equal((await t('{}', 's')).status, 401);
  assert.equal(n, 1);
});

test('buildAccountRequest keeps the documented key order', () => {
  assert.equal(
    buildAccountRequest(NODE, 'balance', {}, 'n'.repeat(32), 5),
    `{"node_id":"${NODE}","op":"balance","params":{},"nonce":"${'n'.repeat(32)}","expires_at":5}`,
  );
});

test('only a stale token is refreshed, not a missing signature or a named refusal', async () => {
  for (const body of ['missing X-Fortis-Sig', '{"error":"auth","reason":"expired"}']) {
    let n = 0, refreshed = 0;
    const fetchImpl = async () => (n++, { status: 401, text: async () => body });
    const t = edgeTransport({ base: 'https://e', chain: 'btc', getToken: () => 't', refreshToken: async () => (refreshed++, 't2'), fetchImpl });
    assert.equal((await t('{}', 's')).status, 401);
    assert.deepEqual([n, refreshed], [1, 0], body);
  }
});
