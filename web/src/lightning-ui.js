// The Lightning tab: the Fortis account (a balance the LSP holds, keyed by this wallet's node id —
// the same account the phone app has for the same recovery phrase). Signing happens in wasm; this
// file builds screens and calls `FortisAccount`. Rules live in lnlogic.js / lnmath.js / lnname.js.

import { el, copy, toast, qrCode } from './ui.js';
import { t, currentTag } from './i18n.js';
import { FortisAccount, edgeTransport } from './lnaccount.js';
import { reconcile, planSend, planWithdraw, receiveErrorKey, judgePayment, fmtSatMsat, checkFetchedInvoice } from './lnlogic.js';
import { sendAllSat } from './lnmath.js';
import { LN_ADDRESS_DOMAIN, normalizeName, checkName, parseAvailability } from './lnname.js';
import { parseTarget, resolvePay, fetchInvoice, LnurlError } from './lnurl.js';
import { decodeInvoice, decodeOffer } from './wallet.js';

export { lightningEligible } from './lnlogic.js';

const HISTORY_LIMIT = 200; // the LSP's maximum
const REFRESH_MS = 20_000;
const OFFER_POLL_MS = 2_000;
const OFFER_POLL_MAX = 60; // 2 minutes
const PAY_POLL_MAX = 60; // 2 minutes

let ctx = null;
const states = new Map(); // walletId -> screen state
const flights = new Set(); // walletIds with a payment/withdrawal in flight (no double sends)
let timer = null;

/** `c`: { idle() (called when the last in-flight request ends), session(w), wallet(id), confirm(cb) (re-ask the password/passkey), askConfirm(title, body, onYes),  rerender(), fiat(sat), feerate(), txUrl(w, txid), token: {get(w), refresh(w)},
 *  isActive(walletId) }. */
export function initLightning(c) {
  ctx = c;
}

/** A payment or withdrawal is in flight: the app must not auto-lock under it (a lock frees the key it is
 *  still using and forgets its result; the app locks when the last one ends). */
export function lightningBusy() {
  return flights.size > 0;
}

function endFlight(id) {
  flights.delete(id);
  if (!flights.size) ctx.idle?.();
}

/** Forget the screens' state (the app locked, a wallet was removed). In-flight markers stay until their
 *  request ends, so a payment started before a lock cannot be started again after unlocking. */
export function resetLightning() {
  states.clear();
  if (timer) clearInterval(timer);
  timer = null;
}

const CHAIN_LABEL = { btc: 'BTC', xbt: 'XBT' };
const sat = (msat) => Math.floor(msat / 1000);
const fmtSat = (n) => new Intl.NumberFormat(currentTag()).format(n);
/** msat as sat keeping the milli-sat remainder, so shown amounts always add up. */
const fm = (msat) => fmtSatMsat(msat, currentTag());

function stateOf(w) {
  let s = states.get(w.id);
  if (!s) {
    s = {
      view: 'account', loaded: false, loading: false, error: null,
      bal: null, hist: [], truncated: false, name: undefined, offer: null,
      addr: { input: '', hint: '', avail: null }, inv: { amount: '', desc: '', invoice: null, err: '' },
      send: { input: '', typed: '', step: 'input', plan: null, target: null, info: null, pay: null, err: '', result: null, kindText: '' },
      wd: { address: '', amount: '', fee: '', err: '', done: null, ticket: 0 },
    };
    states.set(w.id, s);
  }
  return s;
}

function accountFor(w) {
  const session = ctx.session(w);
  const signer = { nodeId: () => session.lnNodeId(), sign: (body) => session.lnSign(body) };
  const account = new FortisAccount(edgeTransport({
    base: w.backend.url,
    chain: w.chain,
    getToken: () => ctx.token.get(w),
    refreshToken: () => ctx.token.refresh(w),
  }));
  return { account, signer };
}

function errText(e, extra) {
  if (e instanceof LnurlError) return t(`wln_lnurl_${e.kind}`);
  const key = receiveErrorKey(e);
  return key === 'wln_err_custody_full' ? t(key, fmtSat(extra?.capSat ?? 0)) : t(key);
}

/* ---------------------------------------------------------------- loading */

async function load(w, { quiet = false } = {}) {
  const s = stateOf(w);
  if (s.loading) return;
  s.loading = true;
  if (!quiet) { s.error = null; ctx.rerender(); }
  try {
    const { account, signer } = accountFor(w);
    // Balance first, then history: the ledger settles a payment (entry + hold) in one transaction, so history
    // read after the balance is never older than it. A payment landing between the two reads would still
    // look like an "unexplained" difference, so a difference is read once more before it is shown.
    const fetchBoth = async () => {
      const bal = await account.balance(signer);
      const hist = await account.history(signer, HISTORY_LIMIT);
      return { bal, hist };
    };
    let { bal, hist } = await fetchBoth();
    if (reconcile(hist, bal.balanceMsat, hist.length >= HISTORY_LIMIT).unexplainedMsat !== 0) ({ bal, hist } = await fetchBoth());
    // The address lookup is not worth hiding the balance for: if it fails the address card says so.
    let name;
    try { name = await account.myName(signer, w.chain); } catch { name = undefined; }
    const gainedMsat = s.bal ? bal.balanceMsat - s.bal.balanceMsat : 0;
    s.bal = bal;
    s.hist = hist;
    s.truncated = hist.length >= HISTORY_LIMIT;
    s.name = name;
    s.loaded = true;
    s.error = null;
    if (gainedMsat > 0 && quiet) toast(t('wln_received_toast', fm(gainedMsat)));
  } catch (e) {
    s.error = e;
  } finally {
    s.loading = false;
    ctx.rerender();
  }
}

function ensureLoaded(w) {
  const s = stateOf(w);
  if (!s.loaded && !s.loading && !s.error) queueMicrotask(() => load(w));
  if (!timer) {
    timer = setInterval(() => {
      const cur = [...states.keys()].find((id) => ctx.isActive(id));
      if (!cur) { clearInterval(timer); timer = null; return; }
      const wallet = ctx.wallet(cur);
      if (wallet && !flights.has(cur) && !document.activeElement?.closest?.('[data-ln-input]')) load(wallet, { quiet: true });
    }, REFRESH_MS);
  }
}

/* ---------------------------------------------------------------- tab */

export function renderLightningTab(w) {
  const s = stateOf(w);
  ensureLoaded(w);
  const views = [['account', 'wln_view_account'], ['receive', 'tab_receive'], ['send', 'tab_send'], ['withdraw', 'wln_view_withdraw']];
  const nav = el('div', { class: 'seg' }, views.map(([id, key]) =>
    el('button', { class: s.view === id ? 'on' : '', onclick: () => { s.view = id; ctx.rerender(); } }, t(key))));
  let body;
  if (!s.loaded) {
    body = s.error
      ? el('div', { class: 'card stack' }, el('div', { class: 'err' }, errText(s.error)),
          el('button', { class: 'primary wide', onclick: () => { s.error = null; load(w); } }, t('wln_retry')))
      : el('div', { class: 'card center hint' }, t('wln_loading'));
  } else if (s.view === 'receive') body = receiveView(w, s);
  else if (s.view === 'send') body = sendView(w, s);
  else if (s.view === 'withdraw') body = withdrawView(w, s);
  else body = accountView(w, s);
  return el('div', { 'data-ln': '1', style: 'display:flex;flex-direction:column;gap:.9rem' }, nav, body);
}

/* ---------------------------------------------------------------- account: balance + activity */

const KIND_KEYS = { received: 'wln_k_received', sent: 'wln_k_sent', withdrawn: 'wln_k_withdrawn', converted: 'wln_k_converted' };
function kindLabel(e) {
  if (e.kind === 'internal') return t(e.amountMsat >= 0 ? 'wln_k_internal_in' : 'wln_k_internal_out');
  return t(KIND_KEYS[e.kind] || 'wln_k_other');
}

function accountView(w, s) {
  const b = s.bal;
  const free = sat(b.freeMsat);
  const usd = ctx.fiat(free);
  const r = reconcile(s.hist, b.balanceMsat, s.truncated);
  const row = (label, msat) => el('div', { class: 'kv' }, el('span', {}, label),
    el('span', {}, `${msat > 0 ? '+' : ''}${fm(msat)} sat`));
  const items = s.hist.map((e) => {
    const pos = e.amountMsat > 0;
    const when = new Date(e.atSecs * 1000).toLocaleString(currentTag());
    return el('div', { class: 'item' },
      el('div', {},
        el('div', { class: `amt ${pos ? 'pos' : 'neg'}` }, `${pos ? '+' : ''}${fm(e.amountMsat)} sat`),
        el('div', { class: 'meta' }, `${kindLabel(e)} · ${when}${e.feeMsat ? ` · ${t('wln_fee_included', fm(e.feeMsat))}` : ''}`)));
  });
  return el('div', { class: 'stack', style: 'gap:.9rem' },
    el('div', { class: 'card stack' },
      el('h2', {}, t('wln_title', CHAIN_LABEL[w.chain])),
      el('div', { class: 'ln-bal' }, el('span', { class: 'num' }, fm(b.freeMsat)), el('span', { class: 'unit' }, ' sat')),
      usd ? el('div', { class: 'fiat' }, t('fiat_approx', usd)) : null,
      b.heldMsat ? el('div', { class: 'hint' }, t('wln_held', fm(b.heldMsat))) : null,
      el('div', { class: 'hint' }, t('wln_cap', fmtSat(sat(b.maxMsat)))),
      el('div', { class: 'hint' }, t('wln_account_note')),
      s.error ? el('div', { class: 'err' }, errText(s.error)) : null,
      el('button', { class: 'ghost wide', disabled: s.loading, onclick: () => load(w) }, s.loading ? t('wln_loading') : t('wln_refresh'))),
    el('div', { class: 'card hist' },
      el('h2', {}, t('wln_history_title')),
      items.length ? items : el('div', { class: 'hint' }, t('wln_history_empty')),
      el('div', { class: 'stack', style: 'margin-top:.6rem' },
        r.olderMsat ? row(t('wln_sum_older'), r.olderMsat) : null,
        r.unexplainedMsat ? el('div', { class: 'kv err' }, el('span', {}, t('wln_sum_unexplained')),
          el('span', {}, `${r.unexplainedMsat > 0 ? '+' : ''}${fm(r.unexplainedMsat)} sat`)) : null,
        row(t('wln_sum_total'), b.balanceMsat))));
}

/* ---------------------------------------------------------------- receive */

function receiveView(w, s) {
  return el('div', { class: 'stack', style: 'gap:.9rem' }, addressCard(w, s), invoiceCard(w, s), offerCard(w, s));
}

function addressCard(w, s) {
  const chain = CHAIN_LABEL[w.chain];
  if (s.name === undefined) {
    return el('div', { class: 'card stack' },
      el('h2', {}, t('wln_addr_title', chain)),
      el('div', { class: 'err' }, t('wln_addr_unknown')),
      el('button', { class: 'ghost wide', disabled: s.loading, onclick: () => load(w) }, t('wln_retry')));
  }
  if (s.name) {
    const address = `${s.name}@${LN_ADDRESS_DOMAIN}`;
    const share = typeof navigator !== 'undefined' && navigator.share
      ? el('button', { onclick: () => navigator.share({ text: address }).catch(() => {}) }, t('wln_addr_share'))
      : null;
    return el('div', { class: 'card stack' },
      el('h2', {}, t('wln_addr_title', chain)),
      el('div', { onclick: () => copy(address), style: 'cursor:pointer' }, qrCode(address)),
      el('div', { class: 'addr-box mono', style: 'cursor:pointer', onclick: () => copy(address) }, address),
      el('div', { class: 'row' }, el('button', { onclick: () => copy(address) }, t('action_copy')), share),
      el('div', { id: 'wln-addr-err', class: 'err' }),
      el('button', { class: 'ghost wide', onclick: () => releaseName(w, s) }, t('wln_addr_release')));
  }
  const a = s.addr;
  return el('div', { class: 'card stack', 'data-ln-input': '1' },
    el('h2', {}, t('wln_addr_title', chain)),
    el('div', { class: 'hint' }, t('wln_addr_none', LN_ADDRESS_DOMAIN)),
    el('label', {}, t('wln_addr_name')),
    el('div', { class: 'row', style: 'align-items:center;gap:.3rem' },
      el('input', { id: 'wln-name', value: a.input, autocapitalize: 'none', spellcheck: 'false', autocomplete: 'off',
        oninput: (e) => onNameInput(w, s, e.target.value) }),
      el('span', { class: 'hint' }, `@${LN_ADDRESS_DOMAIN}`)),
    el('div', { id: 'wln-name-hint', class: 'hint' }, a.hint),
    el('div', { id: 'wln-addr-err', class: 'err' }),
    el('button', { class: 'primary wide', onclick: () => claimName(w, s) }, t('wln_addr_claim')));
}

let availTimer = null;
function setHint(text, bad = false) {
  const n = document.getElementById('wln-name-hint');
  if (n) { n.textContent = text; n.className = bad ? 'err' : 'hint'; }
}
function onNameInput(w, s, raw) {
  s.addr.input = raw;
  const name = normalizeName(raw);
  clearTimeout(availTimer);
  if (!name) { s.addr.hint = ''; return setHint(''); }
  const problem = checkName(name);
  if (problem) { s.addr.hint = t(`wln_name_err_${problem}`); return setHint(s.addr.hint, true); }
  s.addr.hint = '';
  setHint('');
  availTimer = setTimeout(async () => {
    try {
      const res = await fetch(`https://${LN_ADDRESS_DOMAIN}/.well-known/lnurlp/${encodeURIComponent(name)}`, { signal: AbortSignal.timeout(8_000) });
      const verdict = parseAvailability(res.status, await res.text());
      if (normalizeName(s.addr.input) !== name) return; // typed on meanwhile
      if (verdict === 'taken') setHint(t('wln_name_taken'), true);
      else if (verdict === 'available') setHint(t('wln_name_available'));
    } catch { /* unknown: the claim is the final word */ }
  }, 400);
}

function claimName(w, s) {
  const name = normalizeName(s.addr.input);
  const err = document.getElementById('wln-addr-err');
  const problem = name ? checkName(name) : 'length';
  if (problem) { if (err) err.textContent = t(`wln_name_err_${problem}`); return; }
  ctx.confirm(async () => {
    try {
      const { account, signer } = accountFor(w);
      s.name = await account.claimName(signer, w.chain, name);
      s.addr = { input: '', hint: '', avail: null };
      toast(t('wln_addr_claimed', `${s.name}@${LN_ADDRESS_DOMAIN}`));
    } catch (e) {
      s.addr.hint = e.kind === 'name_taken' ? t('wln_name_taken')
        : e.kind === 'has_name' ? t('wln_name_has')
        : e.kind === 'name_invalid' && e.reason ? t(`wln_name_err_${e.reason}`)
        : errText(e);
    }
    ctx.rerender();
  });
}

function releaseName(w, s) {
  ctx.askConfirm(t('wln_addr_release_title', `${s.name}@${LN_ADDRESS_DOMAIN}`), t('wln_addr_release_body'), () => {
    ctx.confirm(async () => {
      try {
        const { account, signer } = accountFor(w);
        await account.releaseName(signer, w.chain);
        s.name = null;
      } catch (e) { toast(errText(e)); }
      ctx.rerender();
    });
  });
}

function invoiceCard(w, s) {
  const i = s.inv;
  const body = [
    el('h2', {}, t('wln_inv_title')),
    el('label', {}, t('wln_inv_amount')),
    el('input', { id: 'wln-inv-amount', value: i.amount, inputmode: 'numeric', oninput: (e) => (i.amount = e.target.value) }),
    el('label', {}, t('wln_inv_desc')),
    el('input', { id: 'wln-inv-desc', value: i.desc, maxlength: '200', oninput: (e) => (i.desc = e.target.value) }),
    el('div', { id: 'wln-inv-err', class: 'err' }, i.err),
    el('button', { class: 'primary wide', onclick: () => createInvoice(w, s) }, t('wln_inv_create')),
  ];
  if (i.invoice) {
    body.push(
      el('div', { onclick: () => copy(i.invoice), style: 'cursor:pointer' }, qrCode(i.invoice.toUpperCase())),
      el('div', { class: 'addr-box mono', style: 'cursor:pointer;word-break:break-all', onclick: () => copy(i.invoice) }, i.invoice),
      el('div', { class: 'row' }, el('button', { onclick: () => copy(i.invoice) }, t('action_copy'))));
  }
  return el('div', { class: 'card stack', 'data-ln-input': '1' }, body);
}

async function createInvoice(w, s) {
  const i = s.inv;
  const n = Number(i.amount);
  if (!Number.isSafeInteger(n) || n < 1) { i.err = t('wln_inv_amount_required'); i.invoice = null; return ctx.rerender(); }
  try {
    const { account, signer } = accountFor(w);
    i.invoice = await account.invoice(signer, n * 1000, i.desc.trim());
    i.err = '';
  } catch (e) {
    i.invoice = null;
    i.err = errText(e, { capSat: s.bal ? sat(s.bal.maxMsat) : 0 });
  }
  ctx.rerender();
}

function offerCard(w, s) {
  return el('div', { class: 'card stack' },
    el('h2', {}, t('wln_offer_title')),
    s.offer
      ? [el('div', { onclick: () => copy(s.offer), style: 'cursor:pointer' }, qrCode(s.offer.toUpperCase())),
         el('div', { class: 'addr-box mono', style: 'cursor:pointer;word-break:break-all', onclick: () => copy(s.offer) }, s.offer),
         el('div', { class: 'row' }, el('button', { onclick: () => copy(s.offer) }, t('action_copy')))]
      : [el('div', { class: 'hint' }, t('wln_offer_hint')),
         el('button', { class: 'ghost wide', onclick: async () => {
           try { const { account, signer } = accountFor(w); s.offer = await account.offer(signer); } catch (e) { toast(errText(e)); }
           ctx.rerender();
         } }, t('wln_offer_get'))]);
}

/* ---------------------------------------------------------------- send */

function sendView(w, s) {
  const d = s.send;
  const free = s.bal.freeMsat;
  const all = sendAllSat(free);
  // Ticked "Send all": the amount is the most one payment can carry, and follows the balance; when nothing can be sent the tick goes.
  if (d.sendAll) { if (all > 0) d.typed = String(all); else d.sendAll = false; }
  if (flights.has(w.id) && d.step === 'input') return inFlightCard();
  if (d.step === 'done') {
    return el('div', { class: 'card stack' },
      el('h2', {}, t('tab_send')),
      el('div', { class: d.result.ok !== false ? 'hint' : 'err' }, d.result.text),
      el('button', { class: 'primary wide', onclick: () => { s.send = { input: '', typed: '', step: 'input', plan: null, target: null, info: null, pay: null, err: '', result: null }; load(w); } }, t('action_done')));
  }
  if (d.step === 'review' || d.step === 'paying' || d.step === 'waiting') {
    const p = d.plan;
    const busy = d.step !== 'review';
    const r = (k, v) => el('div', { class: 'kv' }, el('span', {}, k), el('span', {}, v));
    return el('div', { class: 'card stack' },
      el('h2', {}, t('wln_send_confirm_title')),
      d.kindText ? r(t('wln_pay_to'), d.kindText) : null,
      r(t('wln_pay_amount'), `${fm(p.amountMsat)} sat`),
      r(t('wln_pay_fee_cap'), `${fm(p.feeCapMsat)} sat`),
      r(t('wln_pay_total'), `${fm(p.totalMsat)} sat`),
      el('div', { class: 'hint' }, t('wln_pay_fee_note')),
      el('div', { class: 'err' }, d.err),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', disabled: busy, onclick: () => { d.step = 'input'; d.err = ''; ctx.rerender(); } }, t('action_cancel')),
        el('button', { class: 'primary', disabled: busy, onclick: () => startPay(w, s) }, d.step === 'waiting' ? t('wln_pay_waiting') : busy ? t('wln_pay_paying') : t('wln_pay_pay'))));
  }
  return el('div', { class: 'card stack', 'data-ln-input': '1' },
    el('h2', {}, t('tab_send')),
    el('label', {}, t('wln_send_to')),
    el('div', { class: 'hint' }, t('wln_send_domain_hint', LN_ADDRESS_DOMAIN)),
    el('input', { id: 'wln-send-to', value: d.input, autocapitalize: 'none', spellcheck: 'false', autocomplete: 'off',
      oninput: (e) => (d.input = e.target.value.trim()) }),
    el('label', {}, t('wln_send_amount')),
    all > 0
      ? el('label', { class: 'row', style: 'align-items:center;gap:.5rem' },
          el('input', { id: 'wln-send-all', type: 'checkbox', style: 'width:auto;flex:0', checked: d.sendAll,
            onchange: (e) => { d.sendAll = e.target.checked; if (d.sendAll) d.typed = String(all); ctx.rerender(); } }),
          el('span', {}, t('wln_send_all', fmtSat(all))))
      : null,
    all > 0 ? el('div', { class: 'hint' }, t('wln_send_all_note')) : null,
    el('input', { id: 'wln-send-amount', value: d.typed, inputmode: 'numeric',
      oninput: (e) => {
        d.typed = e.target.value; d.sendAll = false;
        const box = document.getElementById('wln-send-all'); if (box) box.checked = false; // no redraw on typing: untick what is on screen
      } }),
    el('div', { class: 'hint' }, t('wln_send_amount_hint')),
    el('div', { class: 'err' }, d.err),
    el('button', { class: 'primary wide', onclick: () => reviewSend(w, s) }, t('action_review')));
}

function inFlightCard() {
  return el('div', { class: 'card stack' }, el('div', { class: 'hint' }, t('wln_pay_inflight')));
}

const typedMsat = (typed) => {
  const n = Number(String(typed).trim());
  return Number.isSafeInteger(n) && n > 0 ? n * 1000 : null;
};

async function reviewSend(w, s) {
  const d = s.send;
  d.err = '';
  const target = parseTarget(d.input);
  const typed = typedMsat(d.typed);
  const nowSecs = Math.floor(Date.now() / 1000);
  const free = s.bal.freeMsat;
  let kind = target.kind, info, kindText = '';
  try {
    if (kind === 'bolt11') {
      info = decodeInvoice(target.value);
      info = { ...info, amountMsat: info.amountMsat ?? null };
      kindText = info.description || '';
    } else if (kind === 'offer') {
      info = decodeOffer(target.value);
      info = { ...info, amountMsat: info.amountMsat ?? null };
      kindText = info.description || '';
    } else if (kind === 'lnurl' || kind === 'address') {
      if (typed == null) { d.err = t('wln_send_err_amount_required'); return ctx.rerender(); }
      d.pay = await resolvePay(target);
      info = { minMsat: d.pay.minMsat, maxMsat: d.pay.maxMsat };
      kind = 'lnurl';
      kindText = kind === 'lnurl' && target.kind === 'address' ? `${target.value.user}@${target.value.domain}` : d.pay.description;
    } else {
      d.err = t('wln_send_err_unsupported');
      return ctx.rerender();
    }
  } catch (e) {
    d.err = e instanceof LnurlError ? t(`wln_lnurl_${e.kind}`) : t('wln_send_err_unreadable');
    return ctx.rerender();
  }
  const plan = planSend({ chain: w.chain, kind, info, typedMsat: typed, freeMsat: free, nowSecs });
  if (!plan.ok) { d.err = t(`wln_send_err_${plan.error}`); return ctx.rerender(); }
  Object.assign(d, { step: 'review', plan, target, info, kindText: kindText || '', kind });
  ctx.rerender();
}

function startPay(w, s) {
  const d = s.send;
  if (flights.has(w.id) || d.step !== 'review') return;
  ctx.confirm(() => doPay(w, s));
}

const sha256Hex = async (str) => [...new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(str)))]
  .map((b) => b.toString(16).padStart(2, '0')).join('');

async function doPay(w, s) {
  const d = s.send;
  if (flights.has(w.id) || d.step !== 'review') return; // one payment per wallet at a time
  flights.add(w.id);
  d.step = 'paying';
  d.err = '';
  ctx.rerender();
  // `ok`: true = paid; false = not paid; null = not known yet (the screen then says what to check).
  const finish = (ok, text) => { d.step = 'done'; d.result = { ok, text }; ctx.rerender(); };
  let sent = false; // the signed pay request has been handed to the network
  try {
    const { account, signer } = accountFor(w);
    const nowSecs = () => Math.floor(Date.now() / 1000);
    if (d.kind === 'offer') {
      sent = true;
      const id = await account.payOffer(signer, d.target.value, d.info.amountMsat == null ? d.plan.amountMsat : null);
      let unknowns = 0;
      for (let i = 0; i < OFFER_POLL_MAX; i++) {
        await new Promise((r) => setTimeout(r, OFFER_POLL_MS));
        let st;
        try { st = await account.payStatus(signer, id); } catch (e) {
          if (e.kind === 'auth') return finish(null, `${t('wln_pay_unknown')} ${errText(e)}`);
          continue;
        }
        if (st === 'succeeded') return finish(true, t('wln_pay_ok'));
        if (st === 'failed') return finish(false, t('wln_pay_failed'));
        unknowns = st === 'unknown' ? unknowns + 1 : 0;
        if (unknowns >= 3) return finish(null, t('wln_pay_unknown'));
      }
      return finish(null, t('wln_pay_unknown'));
    }
    let invoice = d.target.value;
    let amountForPay = d.info.amountMsat == null ? d.plan.amountMsat : null;
    if (d.kind === 'lnurl') {
      invoice = await fetchInvoice(d.pay, d.plan.amountMsat, { sha256Hex, decodeInvoice });
      const bad = checkFetchedInvoice(w.chain, decodeInvoice(invoice), nowSecs());
      if (bad) return finish(false, `${t(`wln_send_err_${bad}`)} ${t('wln_pay_failed')}`);
      amountForPay = null;
    }
    sent = true;
    const result = await account.pay(signer, invoice, amountForPay);
    // The LSP answers as soon as the payment is *started*. Paid means the ledger says so.
    d.step = 'waiting';
    ctx.rerender();
    for (let i = 0; i < PAY_POLL_MAX; i++) {
      let verdict;
      try {
        const bal = await account.balance(signer); // balance first, then history (see judgePayment)
        const hist = await account.history(signer, 20);
        verdict = judgePayment(result, bal, hist);
      } catch (e) {
        if (e.kind === 'auth') return finish(null, `${t('wln_pay_unknown')} ${errText(e)}`);
        verdict = 'pending';
      }
      if (verdict === 'paid') return finish(true, t('wln_pay_ok'));
      if (verdict === 'failed') return finish(false, t('wln_pay_failed'));
      await new Promise((r) => setTimeout(r, OFFER_POLL_MS));
    }
    return finish(null, t('wln_pay_unknown'));
  } catch (e) {
    if (e.kind === 'duplicate') return finish(null, t('wln_pay_duplicate'));
    if (sent && (e.kind === 'network' || !e.kind)) return finish(null, t('wln_pay_maybe'));
    if (e.kind === 'insufficient') return finish(false, t('wln_send_err_insufficient'));
    return finish(false, e instanceof LnurlError ? t(`wln_lnurl_${e.kind}`) : `${t('wln_pay_failed')} ${errText(e)}`);
  } finally {
    endFlight(w.id);
  }
}

/* ---------------------------------------------------------------- withdraw */

function withdrawView(w, s) {
  const d = s.wd;
  const free = s.bal.freeMsat;
  const allSat = sat(free);
  if (flights.has(w.id) && !d.done) return inFlightCard();
  const fees = ctx.feerate() || {};
  if (!d.fee) d.fee = String(Math.max(1, Math.ceil(Number(fees['6']) || 2)));
  if (d.done) {
    const url = ctx.txUrl(w, d.done);
    return el('div', { class: 'card stack' },
      el('h2', {}, t('wln_view_withdraw')),
      el('div', { class: 'hint' }, t('wln_wd_done')),
      el('div', { class: 'addr-box mono', style: 'word-break:break-all;cursor:pointer', onclick: () => (url ? window.open(url, '_blank', 'noopener') : copy(d.done)) }, d.done),
      el('button', { class: 'primary wide', onclick: () => { s.wd = { address: '', amount: '', fee: '', err: '', done: null, ticket: 0 }; load(w); } }, t('action_done')));
  }
  const plan = planWithdraw({ amountSat: Number(d.amount) || 0, feerateSatVb: Number(d.fee) || 0, freeMsat: free });
  return el('div', { class: 'card stack', 'data-ln-input': '1' },
    el('h2', {}, t('wln_view_withdraw')),
    el('div', { class: 'hint' }, t('wln_wd_hint')),
    el('label', {}, t('wln_wd_address')),
    el('input', { id: 'wln-wd-address', value: d.address, autocapitalize: 'none', spellcheck: 'false', autocomplete: 'off',
      oninput: (e) => (d.address = e.target.value.trim()) }),
    el('label', {}, t('wln_wd_amount')),
    el('input', { id: 'wln-wd-amount', value: d.amount, inputmode: 'numeric', oninput: (e) => { d.amount = e.target.value; updateWdPreview(w, s); } }),
    allSat > 0 ? el('button', { class: 'ghost wide', onclick: () => { d.amount = String(allSat); ctx.rerender(); } }, t('wln_wd_all', fmtSat(allSat))) : null,
    el('label', {}, t('wln_wd_fee')),
    el('input', { id: 'wln-wd-fee', value: d.fee, inputmode: 'numeric', oninput: (e) => { d.fee = e.target.value; updateWdPreview(w, s); } }),
    el('div', { id: 'wln-wd-preview', class: 'hint' }, wdPreview(plan, d)),
    el('div', { id: 'wln-wd-err', class: 'err' }, d.err),
    el('button', { class: 'primary wide', disabled: flights.has(w.id) || s.loading, onclick: () => startWithdraw(w, s) }, t('wln_wd_go')));
}

function wdPreview(plan, d) {
  if (!d.amount) return '';
  if (!plan.ok) return t(`wln_wd_err_${plan.error}`);
  return t('wln_wd_preview', fmtSat(plan.payoutSat), fmtSat(plan.feeSat));
}
function updateWdPreview(w, s) {
  const d = s.wd;
  const plan = planWithdraw({ amountSat: Number(d.amount) || 0, feerateSatVb: Number(d.fee) || 0, freeMsat: s.bal.freeMsat });
  const n = document.getElementById('wln-wd-preview');
  if (n) n.textContent = wdPreview(plan, d);
}

function startWithdraw(w, s) {
  const d = s.wd;
  const check = () => planWithdraw({ amountSat: Number(d.amount) || 0, feerateSatVb: Number(d.fee) || 0, freeMsat: s.bal.freeMsat });
  const setErr = (m) => { d.err = m; const n = document.getElementById('wln-wd-err'); if (n) n.textContent = m; };
  const plan = check();
  if (!plan.ok) return setErr(t(`wln_wd_err_${plan.error}`));
  try { ctx.session(w).checkAddress(d.address); } catch { return setErr(t('wln_wd_err_address')); }
  setErr('');
  if (flights.has(w.id)) return;
  // Only the newest click's re-ask may go through: a double click (or a passkey prompt that falls back to
  // the password sheet) leaves several asks open, and each must not send its own withdrawal.
  const ticket = ++d.ticket;
  ctx.confirm(async () => {
    if (ticket !== d.ticket || d.done || flights.has(w.id)) return;
    // The form and balance may have changed while the password sheet was open: decide again.
    const again = check();
    if (!again.ok) return setErr(t(`wln_wd_err_${again.error}`));
    flights.add(w.id);
    ctx.rerender();
    let unsure = false;
    try {
      const { account, signer } = accountFor(w);
      d.done = await account.withdraw(signer, Number(d.amount) * 1000, d.address, Number(d.fee));
      d.err = '';
      d.amount = ''; // a late second confirmation finds nothing to send
    } catch (e) {
      unsure = e.kind === 'network';
      d.err = unsure ? t('wln_wd_maybe') : errText(e);
    } finally {
      endFlight(w.id);
    }
    // After a lost answer the balance on screen is stale: read it before another withdrawal can be planned.
    if (unsure) await load(w);
    else ctx.rerender();
  });
}
