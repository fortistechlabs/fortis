import { test } from 'node:test';
import assert from 'node:assert/strict';
import { lightningEligible, reconcile } from './lnlogic.js';

const soft = { watchOnly: false, hwType: null, network: 'mainnet', chain: 'btc', backend: { kind: 'edge', url: 'https://e' } };

test('only software wallets on mainnet with the hosted edge get Lightning', () => {
  assert.equal(lightningEligible(soft), true);
  assert.equal(lightningEligible({ ...soft, chain: 'xbt' }), true);
  assert.equal(lightningEligible({ ...soft, watchOnly: true }), false);
  assert.equal(lightningEligible({ ...soft, hwType: 'bitbox02' }), false);
  assert.equal(lightningEligible({ ...soft, network: 'regtest' }), false);
  assert.equal(lightningEligible({ ...soft, backend: { kind: 'esplora', url: 'https://x' } }), false);
  assert.equal(lightningEligible({ ...soft, backend: null }), false);
  assert.equal(lightningEligible(null), false);
  assert.equal(lightningEligible({ ...soft, chain: 'ltc' }), false);
});

const e = (kind, amountMsat, feeMsat = 0) => ({ atSecs: 1, kind, amountMsat, feeMsat, paymentHash: null });

test('history is itemised and adds up to the balance', () => {
  const r = reconcile([
    e('received', 10_000_000),
    e('sent', -3_001_000, 1_000), // the debit includes the routing fee
    e('internal', 500_000),
    e('internal', -200_000),
    e('withdrawn', -2_000_000, 5_000),
    e('converted', -1_000_000, 10_000),
  ], 4_299_000, false);
  assert.deepEqual(r.byKind, {
    received: 10_000_000, sent: -3_001_000, internal: 300_000, withdrawn: -2_000_000, converted: -1_000_000,
  });
  assert.equal(r.feesMsat, 16_000);
  assert.equal(r.totalMsat, 4_299_000);
  assert.equal(r.unexplainedMsat, 0);
  assert.equal(r.olderMsat, 0);
});

test('a difference is shown, never hidden', () => {
  assert.equal(reconcile([e('received', 1000)], 1500, false).unexplainedMsat, 500);
  assert.equal(reconcile([e('received', 1000)], 400, false).unexplainedMsat, -600);
});

test('when the list is cut off the rest is older activity, not an error', () => {
  const r = reconcile([e('received', 1000)], 5000, true);
  assert.equal(r.olderMsat, 4000);
  assert.equal(r.unexplainedMsat, 0);
});

test('an unknown kind is kept, not dropped', () => {
  const r = reconcile([e('mystery', 700)], 700, false);
  assert.equal(r.byKind.mystery, 700);
  assert.equal(r.unexplainedMsat, 0);
});

import { planSend, planWithdraw, receiveErrorKey, judgePayment, fmtSatMsat, checkFetchedInvoice } from './lnlogic.js';

const NOW = 1_800_000_000;
const inv = (o = {}) => ({ amountMsat: 121_000, createdSecs: NOW - 10, expirySecs: 3600, network: 'bitcoin', ...o });

test('a payable invoice shows its amount and the most the route may charge', () => {
  assert.deepEqual(planSend({ chain: 'btc', kind: 'bolt11', info: inv(), freeMsat: 200_000, nowSecs: NOW }),
    { ok: true, amountMsat: 121_000, feeCapMsat: 1_210, totalMsat: 122_210 });
});

test('the 2 sat balance can pay 1 sat, not 2 sat', () => {
  const amountless = inv({ amountMsat: null });
  assert.equal(planSend({ chain: 'btc', kind: 'bolt11', info: amountless, typedMsat: 1_000, freeMsat: 2_000, nowSecs: NOW }).ok, true);
  assert.equal(planSend({ chain: 'btc', kind: 'bolt11', info: amountless, typedMsat: 2_000, freeMsat: 2_000, nowSecs: NOW }).error, 'insufficient');
});

test('refusals happen before any signed call', () => {
  const err = (p) => planSend({ chain: 'btc', nowSecs: NOW, freeMsat: 10_000_000, ...p }).error;
  assert.equal(err({ kind: 'bolt11', info: inv({ createdSecs: NOW - 4000 }) }), 'expired');
  assert.equal(err({ kind: 'bolt11', info: inv({ network: 'testnet' }) }), 'wrong_network');
  assert.equal(err({ kind: 'bolt11', info: inv({ amountMsat: null }) }), 'amount_required');
  assert.equal(err({ kind: 'bolt11', info: inv({ amountMsat: null }), typedMsat: 0 }), 'amount_required');
  assert.equal(err({ kind: 'bolt11', info: inv({ amountMsat: null }), typedMsat: 1500 }), 'amount_required', 'whole sats only');
  assert.equal(err({ kind: 'bolt11', info: inv({ amountMsat: 99_000_000_000 }) }), 'insufficient');
  // An invoice with its own amount cannot be changed by what was typed.
  assert.equal(planSend({ chain: 'btc', kind: 'bolt11', info: inv(), typedMsat: 5_000_000, freeMsat: 10_000_000, nowSecs: NOW }).amountMsat, 121_000);
});

test('an offer takes its own amount or the typed one; an lnurl link its limits', () => {
  assert.equal(planSend({ chain: 'btc', kind: 'offer', info: { amountMsat: 50_000 }, freeMsat: 1_000_000, nowSecs: NOW }).amountMsat, 50_000);
  assert.equal(planSend({ chain: 'btc', kind: 'offer', info: { amountMsat: null }, typedMsat: 70_000, freeMsat: 1_000_000, nowSecs: NOW }).amountMsat, 70_000);
  const link = { minMsat: 1_000, maxMsat: 5_000_000 };
  assert.equal(planSend({ chain: 'btc', kind: 'lnurl', info: link, typedMsat: 999_000_000, freeMsat: 1e12, nowSecs: NOW }).error, 'out_of_range');
  assert.equal(planSend({ chain: 'btc', kind: 'lnurl', info: link, typedMsat: 0, freeMsat: 1e12, nowSecs: NOW }).error, 'amount_required');
  assert.equal(planSend({ chain: 'btc', kind: 'lnurl', info: link, typedMsat: 2_000_000, freeMsat: 1e12, nowSecs: NOW }).ok, true);
});

test('a withdrawal predicts the server exactly', () => {
  // The LSP's own cases (account_api.rs tests): amount, feerate -> payout, fee.
  assert.deepEqual(planWithdraw({ amountSat: 100_000, feerateSatVb: 2, freeMsat: 100_000_000 }), { ok: true, payoutSat: 99_100, feeSat: 900 });
  assert.deepEqual(planWithdraw({ amountSat: 20_000, feerateSatVb: 3, freeMsat: 20_000_000 }), { ok: true, payoutSat: 19_250, feeSat: 750 });
  assert.deepEqual(planWithdraw({ amountSat: 400_000, feerateSatVb: 10, freeMsat: 1e12 }), { ok: true, payoutSat: 396_000, feeSat: 4_000 });
  assert.equal(planWithdraw({ amountSat: 900, feerateSatVb: 2, freeMsat: 1e9 }).error, 'too_small');
  assert.equal(planWithdraw({ amountSat: 100_000, feerateSatVb: 2, freeMsat: 99_999_000 }).error, 'insufficient');
  assert.equal(planWithdraw({ amountSat: 100_000, feerateSatVb: 501, freeMsat: 1e12 }).error, 'feerate');
  assert.equal(planWithdraw({ amountSat: 100_000, feerateSatVb: 0, freeMsat: 1e12 }).error, 'feerate');
  assert.equal(planWithdraw({ amountSat: 1.5, feerateSatVb: 2, freeMsat: 1e12 }).error, 'amount');
});

test('errors map to message keys', () => {
  assert.equal(receiveErrorKey({ kind: 'custody_full' }), 'wln_err_custody_full');
  assert.equal(receiveErrorKey({ kind: 'insufficient' }), 'wln_err_insufficient');
  assert.equal(receiveErrorKey({ kind: 'auth', reason: 'expired' }), 'wln_err_clock');
  assert.equal(receiveErrorKey({ kind: 'auth', reason: 'too_far_ahead' }), 'wln_err_clock');
  assert.equal(receiveErrorKey({ kind: 'auth', reason: 'replayed' }), 'wln_err_auth');
  assert.equal(receiveErrorKey({ kind: 'network' }), 'wln_err_network');
  assert.equal(receiveErrorKey({ kind: 'duplicate' }), 'wln_err_duplicate');
  assert.equal(receiveErrorKey({ kind: 'rejected' }), 'wln_err_rejected');
  assert.equal(receiveErrorKey(new Error('x')), 'wln_err_network');
});

test('an invoice or offer for the other chain is refused before anything is held', () => {
  const p = (chain, info, kind = 'bolt11') => planSend({ chain, kind, info, freeMsat: 10_000_000, nowSecs: NOW });
  assert.equal(p('btc', inv({ isXbt: true })).error, 'wrong_chain');
  assert.equal(p('xbt', inv({ isXbt: false })).error, 'wrong_chain');
  assert.equal(p('xbt', inv({ isXbt: true })).ok, true);
  assert.equal(p('btc', inv()).ok, true, 'undefined means BTC');
  assert.equal(p('btc', { amountMsat: 5_000, isXbt: true }, 'offer').error, 'wrong_chain');
  assert.equal(p('xbt', { amountMsat: 5_000, isXbt: true }, 'offer').ok, true);
});

test('an invoice fetched from an LNURL service is checked like a pasted one', () => {
  const ok = { network: 'bitcoin', createdSecs: NOW - 5, expirySecs: 600, isXbt: false };
  assert.equal(checkFetchedInvoice('btc', ok, NOW), null);
  assert.equal(checkFetchedInvoice('btc', { ...ok, isXbt: true }, NOW), 'wrong_chain');
  assert.equal(checkFetchedInvoice('xbt', ok, NOW), 'wrong_chain');
  assert.equal(checkFetchedInvoice('btc', { ...ok, network: 'testnet' }, NOW), 'wrong_network');
  assert.equal(checkFetchedInvoice('btc', { ...ok, createdSecs: NOW - 700 }, NOW), 'expired');
});

test('a payment is called paid only when the ledger says so', () => {
  const sent = (hash) => ({ kind: 'sent', paymentHash: hash, amountMsat: -2000, feeMsat: 0, atSecs: 1 });
  assert.equal(judgePayment({ internal: true, paymentHash: 'aa' }, { heldMsat: 0 }, []), 'paid');
  assert.equal(judgePayment({ internal: false, paymentHash: 'aa' }, { heldMsat: 2000 }, []), 'pending');
  assert.equal(judgePayment({ internal: false, paymentHash: 'aa' }, { heldMsat: 0 }, [sent('bb'), sent('aa')]), 'paid');
  assert.equal(judgePayment({ internal: false, paymentHash: 'aa' }, { heldMsat: 2000 }, [sent('aa')]), 'paid');
  // The hold is gone and nothing was debited for this payment: the route failed and the hold was released.
  assert.equal(judgePayment({ internal: false, paymentHash: 'aa' }, { heldMsat: 0 }, [sent('bb')]), 'failed');
});

test('amounts keep their milli-sats: nothing rounds away', () => {
  assert.equal(fmtSatMsat(0), '0');
  assert.equal(fmtSatMsat(2000), '2');
  assert.equal(fmtSatMsat(1500), '1.5');
  assert.equal(fmtSatMsat(-500), '-0.5');
  assert.equal(fmtSatMsat(-1500), '-1.5');
  assert.equal(fmtSatMsat(1_234_567_000), '1,234,567');
  assert.equal(fmtSatMsat(1_001), '1.001');
  assert.equal(fmtSatMsat(1_010), '1.01');
  // entries +1,500 and -500 against a balance of 1,000 msat read +1.5, -0.5 and 1.
  assert.equal([fmtSatMsat(1500), fmtSatMsat(-500), fmtSatMsat(1000)].join(' '), '1.5 -0.5 1');
});
