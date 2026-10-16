// The currency fiat values are shown in. Mirrors the Android app's
// data/Fiat.kt: USD (the price feeds' own) plus every currency in the ECB
// daily reference rates fortis-edge converts with (`fx.rs`). One the edge
// doesn't send just shows no value — never another currency's.

const LS_KEY = 'fortis_fiat';

export const FIAT_CODES = [
  'USD', 'EUR', 'GBP', 'CHF', 'JPY', 'CAD', 'AUD', 'NZD', 'SEK', 'NOK', 'DKK', 'ISK',
  'PLN', 'CZK', 'HUF', 'RON', 'TRY', 'BRL', 'MXN', 'CNY', 'HKD', 'SGD', 'KRW', 'INR',
  'IDR', 'MYR', 'PHP', 'THB', 'ILS', 'ZAR',
];

const EURO = ['AT', 'BE', 'HR', 'CY', 'EE', 'FI', 'FR', 'DE', 'GR', 'IE', 'IT', 'LV', 'LT', 'LU', 'MT',
  'NL', 'PT', 'SK', 'SI', 'ES', 'AD', 'MC', 'SM', 'VA', 'ME', 'XK'];
const BY_REGION = {
  ...Object.fromEntries(EURO.map((r) => [r, 'EUR'])),
  US: 'USD', GB: 'GBP', CH: 'CHF', LI: 'CHF', JP: 'JPY', CA: 'CAD', AU: 'AUD', NZ: 'NZD', SE: 'SEK',
  NO: 'NOK', DK: 'DKK', GL: 'DKK', FO: 'DKK', IS: 'ISK', PL: 'PLN', CZ: 'CZK', HU: 'HUF', RO: 'RON',
  TR: 'TRY', BR: 'BRL', MX: 'MXN', CN: 'CNY', HK: 'HKD', SG: 'SGD', KR: 'KRW', IN: 'INR', ID: 'IDR',
  MY: 'MYR', PH: 'PHP', TH: 'THB', IL: 'ILS', ZA: 'ZAR',
};

/** The region's currency for a BCP47 tag ("de-AT" → EUR; "ja" → JPY via its
 *  likely region), when it's one we have; else USD. */
export function defaultFiat(tag) {
  try {
    const region = new Intl.Locale(tag).maximize().region;
    return BY_REGION[region] || 'USD';
  } catch {
    return 'USD';
  }
}

// The choice for this page, for when storage is unavailable (private window).
let session = null;

/** The chosen currency, or null when following the browser's region. */
export function chosenFiat() {
  try {
    const c = localStorage.getItem(LS_KEY);
    if (FIAT_CODES.includes(c)) return c;
  } catch {
    /* fall back to this page's choice */
  }
  return session;
}

/** Choose the display currency; null = follow the browser's region. */
export function setFiat(code) {
  try {
    if (code && FIAT_CODES.includes(code)) localStorage.setItem(LS_KEY, code);
    else localStorage.removeItem(LS_KEY);
  } catch {
    /* private window: the choice lasts for this page only */
  }
  session = code && FIAT_CODES.includes(code) ? code : null;
}

const browserTag = () => (typeof navigator !== 'undefined' && navigator.languages?.[0]) || 'en-US';

/** The currency in effect: the chosen one, else the browser region's. */
export function currentFiat() {
  return chosenFiat() || defaultFiat(browserTag());
}

/** `/v1/prices` → { code: price per whole coin }. Anything that isn't a
 *  3-letter upper-case code with a positive finite number is dropped; no USD
 *  means no usable feed (null). */
export function parsePrices(r) {
  if (!r || typeof r !== 'object') return null;
  const out = {};
  for (const [k, v] of Object.entries(r)) {
    if (/^[A-Z]{3}$/.test(k) && typeof v === 'number' && isFinite(v) && v > 0) out[k] = v;
  }
  return out.USD ? out : null;
}

/** The currency's short symbol in the UI language ("€", "$", "kr", "zł"), or
 *  "" when it has none apart from its code (CHF). */
export function fiatSymbol(code, tag) {
  try {
    const s = new Intl.NumberFormat(tag, { style: 'currency', currency: code, currencyDisplay: 'narrowSymbol' })
      .formatToParts(0).find((p) => p.type === 'currency')?.value || '';
    return s === code ? '' : s;
  } catch {
    return '';
  }
}

/** The currency's name in the UI language ("Euro"), or the code. */
export function fiatName(code, tag) {
  try {
    return new Intl.DisplayNames([tag], { type: 'currency' }).of(code) || code;
  } catch {
    return code;
  }
}
