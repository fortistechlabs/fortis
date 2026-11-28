// Pure rules of the Lightning tab (no DOM, no i18n), kept apart so they can be tested in node.

import { routingFeeCapMsat } from './lnmath.js';

/** Lightning on the web is for software wallets on mainnet that use the hosted edge (the account lives
 *  behind it): not watch-only, not hardware, not regtest. */
export function lightningEligible(w) {
  if (!w) return false;
  if (w.watchOnly || w.hwType) return false;
  if (w.network !== 'mainnet') return false;
  if (w.chain !== 'btc' && w.chain !== 'xbt') return false;
  return w.backend?.kind === 'edge';
}

/** Itemises account history and compares it with the balance. A debit's amount already includes its
 *  fee (`feeMsat` says how much of it), so the entries' amounts sum to the balance. `truncated` is true
 *  when the list was cut at the limit: the remainder is "older activity". Any other difference is
 *  `unexplainedMsat`, which the screen must show. */
export function reconcile(entries, balanceMsat, truncated) {
  const byKind = {};
  let totalMsat = 0;
  let feesMsat = 0;
  for (const e of entries) {
    byKind[e.kind] = (byKind[e.kind] || 0) + e.amountMsat;
    totalMsat += e.amountMsat;
    feesMsat += e.feeMsat || 0;
  }
  const diff = balanceMsat - totalMsat;
  return {
    byKind,
    totalMsat,
    feesMsat,
    olderMsat: truncated ? diff : 0,
    unexplainedMsat: truncated ? 0 : diff,
  };
}

const wholeSats = (msat) => Number.isSafeInteger(msat) && msat > 0 && msat % 1000 === 0;

/** What a payment would be, decided before any signed call. `kind`: bolt11 | offer | lnurl; `info` is the
 *  decoded invoice/offer (`decodeInvoice`/`decodeOffer`) or the LNURL limits `{minMsat, maxMsat}`;
 *  `typedMsat` what the user typed (whole sats); `freeMsat` the account's free balance. The account holds
 *  the amount plus the most the route may charge. Errors: wrong_network, expired, amount_required,
 *  out_of_range, insufficient, wrong_chain (an invoice/offer of the other chain: BTC and XBT invoices are
 *  both `bitcoin` and differ only in the `option_blake2b` bit, `isXbt`). */
export function planSend({ chain, kind, info, typedMsat, freeMsat, nowSecs }) {
  let amountMsat;
  let fixed = false; // the target names its own amount: that is what is paid, whatever was typed
  if (kind === 'bolt11') {
    if (info.network !== 'bitcoin') return { ok: false, error: 'wrong_network' };
    if (!!info.isXbt !== (chain === 'xbt')) return { ok: false, error: 'wrong_chain' };
    if (info.createdSecs + info.expirySecs <= nowSecs) return { ok: false, error: 'expired' };
    fixed = info.amountMsat != null;
    amountMsat = fixed ? info.amountMsat : typedMsat;
  } else if (kind === 'offer') {
    if (!!info.isXbt !== (chain === 'xbt')) return { ok: false, error: 'wrong_chain' };
    fixed = info.amountMsat != null;
    amountMsat = fixed ? info.amountMsat : typedMsat;
  } else if (kind === 'lnurl') {
    amountMsat = typedMsat;
  } else {
    return { ok: false, error: 'unsupported' };
  }
  const valid = fixed ? Number.isSafeInteger(amountMsat) && amountMsat > 0 : wholeSats(amountMsat);
  if (!valid) return { ok: false, error: 'amount_required' };
  if (kind === 'lnurl' && (amountMsat < info.minMsat || amountMsat > info.maxMsat)) return { ok: false, error: 'out_of_range' };
  const feeCapMsat = routingFeeCapMsat(amountMsat);
  const totalMsat = amountMsat + feeCapMsat;
  if (totalMsat > freeMsat) return { ok: false, error: 'insufficient' };
  return { ok: true, amountMsat, feeCapMsat, totalMsat };
}

const WITHDRAW_TX_VBYTES = 200;
const MAX_FEERATE_SAT_VB = 500;
const DUST_SAT = 546;

/** A withdrawal of `amountSat` (everything leaving the account), exactly as fortis-lsp computes it
 *  (`withdrawal` in account_api.rs): the network fee plus the operator's 0.5 % (at least 150 sat). */
export function planWithdraw({ amountSat, feerateSatVb, freeMsat }) {
  if (!Number.isSafeInteger(amountSat) || amountSat <= 0) return { ok: false, error: 'amount' };
  if (!Number.isSafeInteger(feerateSatVb) || feerateSatVb < 1 || feerateSatVb > MAX_FEERATE_SAT_VB) return { ok: false, error: 'feerate' };
  const feeSat = WITHDRAW_TX_VBYTES * feerateSatVb + Math.max(Math.floor(amountSat / 200), 150);
  const payoutSat = amountSat - feeSat;
  if (payoutSat < DUST_SAT) return { ok: false, error: 'too_small' };
  if (amountSat * 1000 > freeMsat) return { ok: false, error: 'insufficient' };
  return { ok: true, payoutSat, feeSat };
}

/** An invoice an LNURL service returned is checked like a pasted one (network, chain, expiry) before it is
 *  paid. The error key (as in `planSend`) or null. */
export function checkFetchedInvoice(chain, info, nowSecs) {
  if (info.network !== 'bitcoin') return 'wrong_network';
  if (!!info.isXbt !== (chain === 'xbt')) return 'wrong_chain';
  if (info.createdSecs + info.expirySecs <= nowSecs) return 'expired';
  return null;
}

/** Whether a started payment is done. The LSP answers `pay` as soon as the payment is *started*; success
 *  shows as a `sent` ledger entry for its hash, failure as the hold being released with no such entry.
 *  `bal` must be read BEFORE `hist`: the ledger settles a payment (entry + hold release) in one transaction,
 *  so a balance showing no hold means any entry is already in the history read after it. Returns
 *  'paid' | 'pending' | 'failed'. */
export function judgePayment(result, bal, hist) {
  if (result.internal) return 'paid';
  if (hist.some((e) => e.kind === 'sent' && e.paymentHash === result.paymentHash)) return 'paid';
  return bal.heldMsat > 0 ? 'pending' : 'failed';
}

/** msat as sat with the milli-sat remainder kept ("1.5"), so a screen never shows a rounded amount that
 *  does not add up. */
export function fmtSatMsat(msat, locale = 'en') {
  const neg = msat < 0;
  const abs = Math.abs(msat);
  const whole = Math.floor(abs / 1000);
  const rem = abs % 1000;
  const intPart = new Intl.NumberFormat(locale).format(whole);
  const frac = rem ? '.' + String(rem).padStart(3, '0').replace(/0+$/, '') : '';
  return `${neg ? '-' : ''}${intPart}${frac}`;
}

/** The message key for a failed account call (the screens pass it through `t()`). */
export function receiveErrorKey(e) {
  switch (e?.kind) {
    case 'custody_full': return 'wln_err_custody_full';
    case 'insufficient': return 'wln_err_insufficient';
    case 'duplicate': return 'wln_err_duplicate';
    case 'auth': return e.reason === 'expired' || e.reason === 'too_far_ahead' ? 'wln_err_clock' : 'wln_err_auth';
    case 'rejected': return 'wln_err_rejected';
    default: return 'wln_err_network';
  }
}
