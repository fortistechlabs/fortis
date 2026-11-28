// Lightning address names (name@fortis.cash): the same rules, in the same order, as the server's
// (android/.../LightningAddressName.kt). The server is the final word; this only explains early.

export const LN_ADDRESS_DOMAIN = 'fortis.cash';

const RESERVED = new Set([
  'abuse', 'admin', 'administrator', 'api', 'billing', 'bitcoin', 'btc', 'contact', 'dns', 'ftp', 'fortis', 'help',
  'hostmaster', 'imap', 'info', 'lightning', 'lnurlp', 'mail', 'no-reply', 'noreply', 'postmaster', 'pop', 'root',
  'sales', 'satoshi', 'security', 'smtp', 'staff', 'status', 'support', 'team', 'wallet', 'webmaster', 'www', 'xbt',
]);

const isAlnum = (c) => (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
const isNameChar = (c) => isAlnum(c) || c === '.' || c === '_' || c === '-';

/** What the user typed, as it is checked and sent: trimmed and lower-cased. */
export const normalizeName = (input) => input.trim().toLowerCase();

/** The first rule `name` breaks (characters, length, edges, double_dot, reserved), or null. */
export function checkName(name) {
  const chars = [...name];
  if (!chars.every(isNameChar)) return 'characters';
  if (chars.length < 3 || chars.length > 32) return 'length';
  if (!isAlnum(chars[0]) || !isAlnum(chars[chars.length - 1])) return 'edges';
  if (name.includes('..')) return 'double_dot';
  if (RESERVED.has(name)) return 'reserved';
  return null;
}

/** Whether a name is free, from the public `/.well-known/lnurlp/<name>` answer: a payRequest means taken,
 *  an LNURL ERROR (HTTP 200) means free, anything else is unknown (the claim is the final word). */
export function parseAvailability(status, body) {
  let o;
  try { o = JSON.parse(body); } catch { return 'unknown'; }
  if (status >= 200 && status < 300 && o?.tag === 'payRequest') return 'taken';
  if (status === 200 && o?.status === 'ERROR') return 'available';
  return 'unknown';
}
