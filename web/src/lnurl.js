// Paying an LNURL-pay link or a Lightning address (LUD-06 / LUD-16) from a browser: find out what
// the payment target is, ask the service for an invoice for an exact amount, and refuse the invoice
// unless it is for that amount and its description hash is sha256 of the service's metadata.

export class LnurlError extends Error {
  /** kind: cors | http | bad_response | out_of_range | hash_mismatch | amount_mismatch */
  constructor(kind, message) {
    super(message || kind);
    this.kind = kind;
  }
}

// --- bech32 (BIP-173) for `lnurl1…` links -----------------------------------------------------
const CHARSET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
const GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
function polymod(values) {
  let chk = 1;
  for (const v of values) {
    const top = chk >>> 25;
    chk = ((chk & 0x1ffffff) << 5) ^ v;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk ^= GEN[i];
  }
  return chk >>> 0;
}
const hrpExpand = (hrp) => [...[...hrp].map((c) => c.charCodeAt(0) >> 5), 0, ...[...hrp].map((c) => c.charCodeAt(0) & 31)];
function convertBits(data, from, to, pad) {
  let acc = 0, bits = 0;
  const out = [];
  const max = (1 << to) - 1;
  for (const v of data) {
    if (v >> from) return null;
    acc = (acc << from) | v;
    bits += from;
    while (bits >= to) { bits -= to; out.push((acc >> bits) & max); }
    acc &= (1 << bits) - 1;
  }
  if (pad) { if (bits) out.push((acc << (to - bits)) & max); }
  else if (bits >= from || ((acc << (to - bits)) & max)) return null;
  return out;
}
/** Encodes bytes as a lower-case bech32 string. */
export function bech32Encode(hrp, bytes) {
  const data = convertBits(bytes, 8, 5, true);
  const mod = polymod([...hrpExpand(hrp), ...data, 0, 0, 0, 0, 0, 0]) ^ 1;
  const check = Array.from({ length: 6 }, (_, i) => (mod >>> (5 * (5 - i))) & 31);
  return hrp + '1' + [...data, ...check].map((d) => CHARSET[d]).join('');
}
/** Decodes a bech32 string (any case, no length limit) to `{hrp, bytes}`, or null. */
function bech32Decode(str) {
  if (str !== str.toLowerCase() && str !== str.toUpperCase()) return null;
  const s = str.toLowerCase();
  const pos = s.lastIndexOf('1');
  if (pos < 1 || pos + 7 > s.length) return null;
  const hrp = s.slice(0, pos);
  const data = [];
  for (const c of s.slice(pos + 1)) {
    const i = CHARSET.indexOf(c);
    if (i < 0) return null;
    data.push(i);
  }
  if (polymod([...hrpExpand(hrp), ...data]) !== 1) return null;
  const bytes = convertBits(data.slice(0, -6), 5, 8, false);
  return bytes ? { hrp, bytes: Uint8Array.from(bytes) } : null;
}

// --- targets ----------------------------------------------------------------------------------
const MAX_INPUT = 16 * 1024;

/** What a pasted string is: `bolt11`, `offer`, `lnurl` (value: the https URL), `address` (value: {user, domain}) or `unknown`. */
export function parseTarget(input) {
  let t = String(input ?? '').trim();
  if (!t || t.length > MAX_INPUT) return { kind: 'unknown', value: null };
  if (/^lightning:/i.test(t)) t = t.slice(10).trim();
  const lower = t.toLowerCase();
  if (lower.startsWith('lnurlp://')) {
    const rest = t.slice(9);
    const host = rest.split(/[/?#]/)[0];
    if (!host) return { kind: 'unknown', value: null };
    return { kind: 'lnurl', value: `${host.toLowerCase().endsWith('.onion') ? 'http' : 'https'}://${rest}` };
  }
  if (lower.startsWith('lnurl1')) {
    const d = bech32Decode(t);
    if (!d || d.hrp !== 'lnurl') return { kind: 'unknown', value: null };
    let url;
    try { url = new TextDecoder('utf-8', { fatal: true }).decode(d.bytes).trim(); } catch { return { kind: 'unknown', value: null }; }
    return /^https?:\/\//i.test(url) ? { kind: 'lnurl', value: url } : { kind: 'unknown', value: null };
  }
  if (lower.startsWith('lno1')) return { kind: 'offer', value: lower };
  if (lower.startsWith('ln') && lower.includes('1') && !lower.includes('@')) return { kind: 'bolt11', value: lower };
  if (t.includes('@')) {
    const [user, domain, ...extra] = lower.split('@');
    const userOk = user && /^[a-z0-9._+-]+$/.test(user);
    const domainOk = domain && domain.includes('.') && domain.split('.').every((l) => l && !l.startsWith('-') && !l.endsWith('-') && /^[a-z0-9-]+$/.test(l));
    if (!extra.length && userOk && domainOk) return { kind: 'address', value: { user, domain } };
  }
  return { kind: 'unknown', value: null };
}

// --- the service ------------------------------------------------------------------------------
async function getJson(url, fetchImpl) {
  let res;
  try {
    res = await fetchImpl(url, { headers: { accept: 'application/json' }, signal: AbortSignal.timeout(10_000) });
  } catch (e) {
    // A browser reports a blocked cross-origin request as a plain network failure.
    throw new LnurlError('cors', e?.message);
  }
  if (!res.ok) throw new LnurlError('http', `HTTP ${res.status}`);
  let o;
  try { o = JSON.parse(await res.text()); } catch { throw new LnurlError('bad_response', 'not JSON'); }
  if (!o || typeof o !== 'object') throw new LnurlError('bad_response', 'not an object');
  if (String(o.status).toUpperCase() === 'ERROR') throw new LnurlError('bad_response', o.reason || 'error');
  return o;
}

function metadataText(metadata) {
  try {
    const row = JSON.parse(metadata).find((r) => Array.isArray(r) && r[0] === 'text/plain');
    return row ? String(row[1]) : '';
  } catch { return ''; }
}

const isHttps = (u) => { try { return new URL(u).protocol === 'https:'; } catch { return false; } };

/** Asks the service what it accepts: `{callback, minMsat, maxMsat, metadata, description}`. */
export async function resolvePay(target, fetchImpl = (...a) => fetch(...a)) {
  const url = target.kind === 'address'
    ? `https://${target.value.domain}/.well-known/lnurlp/${encodeURIComponent(target.value.user)}`
    : target.value;
  if (!isHttps(url)) throw new LnurlError('bad_response', 'service is not https');
  const o = await getJson(url, fetchImpl);
  if (o.tag !== 'payRequest') throw new LnurlError('bad_response', 'not a pay link');
  if (typeof o.callback !== 'string' || !isHttps(o.callback)) throw new LnurlError('bad_response', 'bad callback');
  const minMsat = Number(o.minSendable), maxMsat = Number(o.maxSendable);
  if (!Number.isSafeInteger(minMsat) || !Number.isSafeInteger(maxMsat) || minMsat < 1 || maxMsat < minMsat) {
    throw new LnurlError('bad_response', 'bad limits');
  }
  if (typeof o.metadata !== 'string') throw new LnurlError('bad_response', 'no metadata');
  return { callback: o.callback, minMsat, maxMsat, metadata: o.metadata, description: metadataText(o.metadata) };
}

/** The BOLT11 invoice for exactly `amountMsat`, or an error. Never returns an invoice for another
 *  amount or whose description hash is not sha256 of the metadata the user was shown. */
export async function fetchInvoice(pay, amountMsat, { fetchImpl = (...a) => fetch(...a), sha256Hex, decodeInvoice }) {
  if (!Number.isSafeInteger(amountMsat) || amountMsat < pay.minMsat || amountMsat > pay.maxMsat) {
    throw new LnurlError('out_of_range', 'amount outside what the service accepts');
  }
  const u = new URL(pay.callback);
  u.searchParams.set('amount', String(amountMsat));
  const o = await getJson(u.toString(), fetchImpl);
  if (typeof o.pr !== 'string' || !o.pr) throw new LnurlError('bad_response', 'no invoice');
  let info;
  try { info = decodeInvoice(o.pr); } catch { throw new LnurlError('bad_response', 'unreadable invoice'); }
  if (info.amountMsat !== amountMsat) throw new LnurlError('amount_mismatch', 'invoice is for another amount');
  if (!info.descriptionHash || info.descriptionHash !== (await sha256Hex(pay.metadata))) {
    throw new LnurlError('hash_mismatch', 'invoice does not match what was offered');
  }
  return o.pr;
}
