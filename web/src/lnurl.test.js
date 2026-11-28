import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { parseTarget, resolvePay, fetchInvoice, LnurlError, bech32Encode } from './lnurl.js';

const sha256Hex = async (s) => createHash('sha256').update(s).digest('hex');
const META = '[["text/plain","pay donal"]]';
const jsonRes = (o, status = 200) => ({ ok: status < 300, status, text: async () => JSON.stringify(o) });
const payJson = { tag: 'payRequest', callback: 'https://fortis.cash/lnurlp/callback/donal', minSendable: 1000, maxSendable: 50_000_000, metadata: META };

test('targets are recognised', () => {
  assert.deepEqual(parseTarget('lightning:LNBC25M1abc'), { kind: 'bolt11', value: 'lnbc25m1abc' });
  assert.deepEqual(parseTarget(' lno1abc '), { kind: 'offer', value: 'lno1abc' });
  assert.deepEqual(parseTarget('Donal@Fortis.Cash'), { kind: 'address', value: { user: 'donal', domain: 'fortis.cash' } });
  assert.deepEqual(parseTarget('lnurlp://example.com/pay/x'), { kind: 'lnurl', value: 'https://example.com/pay/x' });
  const url = 'https://example.com/.well-known/lnurlp/bob';
  assert.deepEqual(parseTarget(bech32Encode('lnurl', new TextEncoder().encode(url)).toUpperCase()), { kind: 'lnurl', value: url });
  for (const s of ['', '   ', 'bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq', 'hello', 'a@b', '@x.com', 'a b@x.com']) {
    assert.equal(parseTarget(s).kind, 'unknown', s);
  }
});

test('an address is looked up at its domain\'s well-known URL', async () => {
  let url;
  const pay = await resolvePay({ kind: 'address', value: { user: 'donal', domain: 'fortis.cash' } }, async (u) => ((url = u), jsonRes(payJson)));
  assert.equal(url, 'https://fortis.cash/.well-known/lnurlp/donal');
  assert.deepEqual(pay, { callback: payJson.callback, minMsat: 1000, maxMsat: 50_000_000, metadata: META, description: 'pay donal' });
});

test('lookup failures are named', async () => {
  const t = { kind: 'address', value: { user: 'a', domain: 'x.com' } };
  const kind = async (f) => { try { await resolvePay(t, f); } catch (e) { return e instanceof LnurlError ? e.kind : 'other'; } return 'none'; };
  assert.equal(await kind(async () => { throw new TypeError('Failed to fetch'); }), 'cors');
  assert.equal(await kind(async () => jsonRes({}, 500)), 'http');
  assert.equal(await kind(async () => jsonRes({ status: 'ERROR', reason: 'no such address' })), 'bad_response');
  assert.equal(await kind(async () => jsonRes({ ...payJson, tag: 'withdrawRequest' })), 'bad_response');
  assert.equal(await kind(async () => jsonRes({ ...payJson, callback: 'http://insecure.example/cb' })), 'bad_response');
  assert.equal(await kind(async () => jsonRes({ ...payJson, minSendable: 9, maxSendable: 5 })), 'bad_response');
  assert.equal(await kind(async () => ({ ok: true, status: 200, text: async () => 'not json' })), 'bad_response');
});

const pay = { callback: 'https://fortis.cash/lnurlp/callback/donal', minMsat: 1000, maxMsat: 50_000_000, metadata: META };
const goodInvoice = async () => ({ amountMsat: 5_000_000, descriptionHash: await sha256Hex(META) });
const deps = (info, fetchImpl) => ({ fetchImpl, sha256Hex, decodeInvoice: () => info });

test('the invoice for an amount must match that amount and the metadata hash', async () => {
  let url;
  const f = async (u) => ((url = u), jsonRes({ pr: 'lnbc1xyz' }));
  const pr = await fetchInvoice(pay, 5_000_000, deps(await goodInvoice(), f));
  assert.equal(pr, 'lnbc1xyz');
  assert.equal(url, 'https://fortis.cash/lnurlp/callback/donal?amount=5000000');
  // A callback that already has a query keeps it.
  await fetchInvoice({ ...pay, callback: 'https://x.com/cb?k=v' }, 5_000_000, deps(await goodInvoice(), f));
  assert.equal(url, 'https://x.com/cb?k=v&amount=5000000');
});

test('a tampered or mismatched invoice is refused', async () => {
  const f = async () => jsonRes({ pr: 'lnbc1xyz' });
  const kind = async (amount, info) => { try { await fetchInvoice(pay, amount, deps(info, f)); } catch (e) { return e.kind; } return 'none'; };
  assert.equal(await kind(5_000_000, { amountMsat: 5_000_000, descriptionHash: 'ab'.repeat(32) }), 'hash_mismatch');
  assert.equal(await kind(5_000_000, { amountMsat: 5_000_000, descriptionHash: null }), 'hash_mismatch');
  assert.equal(await kind(5_000_000, { amountMsat: 4_999_000, descriptionHash: await sha256Hex(META) }), 'amount_mismatch');
  assert.equal(await kind(5_000_000, { amountMsat: null, descriptionHash: await sha256Hex(META) }), 'amount_mismatch');
  assert.equal(await kind(999, await goodInvoice()), 'out_of_range');
  assert.equal(await kind(50_000_001, await goodInvoice()), 'out_of_range');
  assert.equal(await kind(1.5, await goodInvoice()), 'out_of_range');
});

test('a callback error or missing invoice is a bad response', async () => {
  for (const body of [{ status: 'ERROR', reason: 'nope' }, {}, { pr: 5 }]) {
    await assert.rejects(fetchInvoice(pay, 5_000_000, deps(await goodInvoice(), async () => jsonRes(body))), (e) => e.kind === 'bad_response');
  }
  await assert.rejects(fetchInvoice(pay, 5_000_000, deps(await goodInvoice(), async () => { throw new TypeError('x'); })), (e) => e.kind === 'cors');
});
