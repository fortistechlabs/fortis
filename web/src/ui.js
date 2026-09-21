// Tiny DOM helpers — no framework.

import { currentTag, t } from './i18n.js';

export function el(tag, props = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v == null || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k === 'html') n.innerHTML = v;
    else if (k === 'value') n.value = v;
    else if (k.startsWith('on') && typeof v === 'function') n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v === true ? '' : String(v));
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}

export function mount(node) {
  document.getElementById('app').replaceChildren(node);
}

export function toast(msg, ms = 2600) {
  const t = el('div', { class: 'toast' }, msg);
  document.body.append(t);
  setTimeout(() => t.remove(), ms);
}

export async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast(t('toast_copied'));
  } catch {
    toast(t('toast_copy_failed'));
  }
}

export const SAT = 100_000_000;
export const fmt = (sat) => (Number(sat) / SAT).toFixed(8);

/** An inline QR code for `text`, `size` CSS px square. Always on a white tile
 *  (with a real quiet-zone margin) regardless of the app's theme, since a
 *  scanner needs real contrast — `vendor/qrcode.js` (global `qrcode`) does the
 *  actual encoding, this just wraps its SVG output. */
export function qrCode(text, size = 176) {
  const qr = qrcode(0, 'M'); // typeNumber 0 = auto-pick the smallest version that fits
  qr.addData(text);
  qr.make();
  const svg = qr.createSvgTag({ cellSize: 4, margin: 8, scalable: true, title: t('cd_address_qr') });
  return el('div', {
    style: `width:${size}px;height:${size}px;background:#fff;border-radius:var(--r-sm);padding:10px;margin:0 auto;box-shadow:var(--e-card);`,
    html: svg,
  });
}

/** Same as `qrCode`, but for payloads that might not fit in one QR frame (an
 *  unsigned/signed transaction, unlike a short address) — `null` instead of
 *  throwing once `vendor/qrcode.js` runs past its largest version (empirically
 *  confirmed: it throws "code length overflow" past ~2.3KB, not something to
 *  assume). Callers always show the always-present copy box alongside this,
 *  so a `null` here is a normal, expected case, not a degraded one. */
export function qrCodeOrNull(text, size = 220) {
  try {
    return qrCode(text, size);
  } catch {
    return null;
  }
}

/** Approximate USD value, device-locale grouping/punctuation, USD currency
 *  pinned regardless of locale. `< $0.01` floor for tiny nonzero values,
 *  matching the Android app's `fmtUsd`. `null`/`undefined` -> null (omit). */
export function fmtUsd(v) {
  if (v == null || !isFinite(v)) return null;
  const nf = new Intl.NumberFormat(currentTag(), { style: 'currency', currency: 'USD' });
  return v > 0 && v < 0.01 ? '<' + nf.format(0.01) : nf.format(v);
}

const reduceMotion = () =>
  typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

/** Animate `el`'s text from `fromSat` to `toSat` (both in sats), easeOutCubic. */
export function countUp(el, fromSat, toSat) {
  const from = Number(fromSat) || 0;
  const to = Number(toSat) || 0;
  if (from === to || reduceMotion()) {
    el.textContent = fmt(to);
    return;
  }
  const t0 = performance.now();
  const dur = 650;
  const tick = (now) => {
    const p = Math.min(1, (now - t0) / dur);
    const e = 1 - (1 - p) ** 3;
    el.textContent = fmt(from + (to - from) * e);
    if (p < 1) requestAnimationFrame(tick);
    else el.textContent = fmt(to);
  };
  requestAnimationFrame(tick);
}

/** Nudge the ambient background as the page scrolls. */
export function initParallax() {
  const bg = document.querySelector('.bg');
  if (!bg || reduceMotion()) return;
  let raf = 0;
  addEventListener(
    'scroll',
    () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        bg.style.transform = `translateY(${scrollY * -0.06}px)`;
        raf = 0;
      });
    },
    { passive: true },
  );
}
export const parseAmount = (s) => {
  const str = String(s).trim();
  const n = Number(str);
  // Number('') and Number('  ') are both 0, not NaN, so an empty field would
  // otherwise silently plan a real 0-value send instead of surfacing this
  // message — wasm still has the final say (it rejects a dust/zero output
  // regardless), but the friendly error is the one this function exists for.
  if (!str || !isFinite(n) || n <= 0) throw new Error(t('error_enter_amount'));
  return Math.round(n * SAT);
};

export function shortTxid(txid) {
  return txid.slice(0, 10) + '…' + txid.slice(-6);
}

/** `{key, n}` for the caller to run through `t()`/i18n — kept free of English
 *  strings itself so it isn't a hidden untranslated corner. */
export function timeAgoParts(unixSecs) {
  if (!unixSecs) return null;
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unixSecs));
  if (s < 60) return { key: 'time_just_now', n: 0 };
  if (s < 3600) return { key: 'time_minutes_ago', n: Math.floor(s / 60) };
  if (s < 86400) return { key: 'time_hours_ago', n: Math.floor(s / 3600) };
  return { key: 'time_days_ago', n: Math.floor(s / 86400) };
}

export function timeAgo(unixSecs) {
  const p = timeAgoParts(unixSecs);
  return p ? t(p.key, p.n) : '';
}
