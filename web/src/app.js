// fortis web wallet — controller.
//
// Phases:  onboard → app-lock setup → backend picker → locked ⇄ shell(home/wallet/settings)
// Keys never leave wasm. Every wallet is unsealed on demand under one in-memory
// `appSecret`, itself unlocked by an app password and/or a WebAuthn-PRF secret.

import { loadRaw, saveState, wipeState, finishMigration, MAX_WALLETS } from './store.js';
import { Gateway } from './gateway.js';
import { EsploraBackend, edgeRegister } from './esplora.js';
import {
  ensureWasm, newMnemonic, validateMnemonic, unseal, Session, activateNetwork,
  newAppSecret, sealWithAppSecret, unsealWithAppSecret,
  wrapAppSecretWithPassword, unwrapAppSecretWithPassword,
  wrapAppSecretWithKey, unwrapAppSecretWithKey,
} from './wallet.js';
import { EntropyPool } from './entropy.js';
import { registerPrf, unlockPrf, prfPossible } from './webauthn.js';
import { storedTheme, setTheme, initThemeWatcher } from './theme.js';
import {
  init as i18nInit, t, tPlural, setLocale, overrideTag, localeLabel, SUPPORTED_LOCALES,
} from './i18n.js';
import {
  el, mount, toast, copy, fmt, fmtUsd, qrCode, qrCodeOrNull, parseAmount, shortTxid, timeAgo, countUp,
  initParallax, SAT,
} from './ui.js';

const UNIT = { xbt: 'XBT', btc: 'BTC' };
const DEFAULT_ESPLORA = { xbt: 'https://mempool.guide/api', btc: 'https://blockstream.info/api' };
const EXPLORER = { btc: 'https://blockstream.info', xbt: 'https://mempool.guide' };
// The hosted fortis-edge. Override for a local instance (http://127.0.0.1:8098).
const DEFAULT_EDGE = 'https://api.fortistechlabs.com';
// Supplementary entropy target — see the comment above renderGen().
const TARGET_BITS = 128;
const MAX_WALLET_NAME_LEN = 29;

let state = null; // persisted v2 config: {v, wallets:[...], selected, lock} | null
let appSecret = null; // Uint8Array(32) once unlocked, else null
let pendingLegacy = null; // a v1 record awaiting the one-time migration prompt
let bootError = false;

const sessions = new Map(); // walletId -> Session (wasm), unsealed lazily, freed on removal
const backends = new Map(); // walletId -> Gateway | EsploraBackend

let ui = { screen: 'main', nav: 'home', tab: 'receive' };
const overview = { balances: new Map(), usd: {} }; // walletId->sat, chain->usd (Home list)
let overviewScanning = false;
let detail = { status: null, balances: null, history: [], feerate: {}, usd: null, _shownBal: undefined };
let detailScanning = false;

/** Blank the wallet-detail view — call whenever `state.selected` changes.
 *  Without this, the previously-open wallet's balance/history stay on
 *  screen (this object is otherwise only reset on `onLock()`) until the
 *  newly-selected wallet's own refresh() resolves, which for a slow chain
 *  can take a while and looks like the wrong wallet's data. */
function resetDetail() {
  detail = { status: null, balances: null, history: [], feerate: {}, usd: null, _shownBal: undefined };
}
let detailPoll = null;
let overviewPoll = null;

start();

async function start() {
  try {
    await ensureWasm();
  } catch {
    bootError = true;
    return mount(el('div', { class: 'screen' },
      el('h1', {}, 'fortis'),
      el('p', { class: 'err' }, 'could not load the wallet engine (wallet_wasm). Build it with:'),
      el('pre', { class: 'card mono' }, 'wasm-pack build crates/wallet-wasm --target web --out-dir ../../web/pkg')));
  }
  initThemeWatcher();
  await i18nInit();
  const raw = await loadRaw();
  if (raw.kind === 'v2') state = raw.state;
  else if (raw.kind === 'legacy') pendingLegacy = migrateLegacyShape(raw.record);
  initParallax();
  render();
}

// v1 stored one flat record; some had gateway_url/gateway_token instead of `backend`.
function migrateLegacyShape(r) {
  if (r.chain === 'btcb2') r.chain = 'xbt';
  if (r.gateway_url && !r.backend) r.backend = { kind: 'gateway', url: r.gateway_url, token: r.gateway_token };
  return r;
}

function currentWallet() {
  return state?.wallets.find((w) => w.id === state.selected) || null;
}
function nextWalletName() {
  const n = (state?.wallets?.length || 0) + 1;
  return n === 1 ? 'Wallet' : `Wallet ${n}`;
}

/** Trims and checks a user-entered wallet name against the same 1..29 char
 *  rule at every entry point (create, restore, rename). Returns the trimmed
 *  name, or throws a translated, field-ready error message. */
function readWalletName(id) {
  const name = val(id).trim();
  if (!name) throw new Error(t('error_wallet_name_required'));
  if (name.length > MAX_WALLET_NAME_LEN) throw new Error(t('error_wallet_name_too_long', MAX_WALLET_NAME_LEN));
  return name;
}

/** Lazily unseal a wallet's session and keep it alive for the unlocked-app
 *  lifetime. Also (re)activates the wasm-global network and syncs the
 *  address-derivation counters, so this is the single choke point every
 *  screen/action should go through before touching a wallet. */
function ensureSession(wallet) {
  activateNetwork(wallet.network);
  let session = sessions.get(wallet.id);
  if (!session) {
    if (wallet.watchOnly) {
      session = Session.watchOnly(wallet.chain, wallet.network, wallet.xpub, wallet.fingerprint);
    } else {
      const { mnemonic, passphrase } = unsealWithAppSecret(wallet.sealed, wallet.salt, appSecret);
      session = new Session(wallet.chain, wallet.network, mnemonic, passphrase);
    }
    sessions.set(wallet.id, session);
  }
  session.setIndices(wallet.next_receive, wallet.next_change);
  return session;
}

function buildBackend(wallet) {
  const b = wallet.backend;
  if (!b) return null;
  if (b.kind === 'esplora' || b.kind === 'edge') {
    const session = ensureSession(wallet);
    if (b.kind === 'edge') {
      const auth = {
        token: b.token,
        refresh: async () => {
          b.token = await edgeRegister(b.url);
          await saveState(state);
          return b.token;
        },
      };
      const root = b.url.replace(/\/+$/, '');
      return new EsploraBackend(`${root}/${wallet.chain}`, session, wallet.network, auth, `${root}/pricing`);
    }
    return new EsploraBackend(b.url, session, wallet.network);
  }
  return new Gateway(b.url, b.token);
}
function ensureBackend(wallet) {
  let backend = backends.get(wallet.id);
  if (!backend) {
    backend = buildBackend(wallet);
    if (backend) backends.set(wallet.id, backend);
  }
  return backend;
}

function render() {
  stopPolling();
  if (bootError) return;
  if (pendingLegacy) return renderMigrate();
  if (!state) return renderOnboard();
  // Watch-only wallets have nothing to seal, so a device holding only those
  // can have state with no lock at all — only demand unlocking when a lock
  // actually exists.
  if (state.lock && !appSecret) return renderLocked();
  if (ui.addingWallet) return renderOnboard();
  renderShell(true);
}

/* ------------------------------------------------------------------ onboard */

// Same mark as web/icon.svg / site/icon.svg / the Android launcher icon
// (ic_launcher_foreground.xml + ic_launcher_background.xml) — the one
// canonical brand mark everywhere else.
const SHIELD = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="512" y2="512" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#0E1424"/><stop offset="1" stop-color="#080A12"/>
    </linearGradient>
    <linearGradient id="sh" x1="150" y1="60" x2="360" y2="470" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#8FBCFF"/><stop offset="1" stop-color="#4A73D0"/>
    </linearGradient>
    <clipPath id="gate"><path d="M44,81 V51 a10,10 0 0 1 20,0 V81 Z"/></clipPath>
  </defs>
  <rect width="512" height="512" rx="112" fill="url(#bg)"/>
  <g transform="translate(-25 -33) scale(5.3)">
    <path d="M22,22 h8 v8 h6 v-8 h8 v8 h6 v-8 h8 v8 h6 v-8 h8 v8 h6 v-8 h8 V58 C86,72 73,82 54,89 C35,82 22,72 22,58 Z"
          fill="url(#sh)" stroke="#233B6A" stroke-width="1.3" stroke-linejoin="round"/>
    <path d="M44,81 V51 a10,10 0 0 1 20,0 V81 Z" fill="#0B0D18"/>
    <g clip-path="url(#gate)" stroke="#8FBCFF" stroke-width="2">
      <path d="M49,49 V82 M54,47 V82 M59,49 V82"/>
      <path d="M43,56 H65 M43,64 H65 M43,72 H65"/>
    </g>
  </g>
</svg>`;

function brand(tagline) {
  return el('div', { class: 'brand' },
    el('div', { html: SHIELD }),
    el('div', { class: 'name' }, 'fortis'),
    tagline ? el('p', { class: 'center', style: 'max-width:22rem' }, tagline) : null);
}

function renderOnboard() {
  if (ui.screen === 'gen') return renderGen();
  if (ui.screen === 'create') return renderCreate();
  if (ui.screen === 'restore') return renderRestore();
  if (ui.screen === 'watch') return renderWatchImport();
  if (ui.screen === 'lock-setup') return renderLockSetup();
  mount(el('div', { class: 'screen' },
    el('div', { class: 'spacer' }),
    brand(ui.addingWallet ? undefined : t('onboard_tagline')),
    el('div', { class: 'spacer' }),
    el('button', { class: 'primary wide', onclick: () => go('gen') }, t('onboard_create')),
    el('button', { class: 'ghost wide', onclick: () => go('restore') }, t('onboard_restore')),
    el('button', { class: 'ghost wide', onclick: () => go('watch') }, t('onboard_watch')),
    ui.addingWallet ? el('button', { class: 'ghost wide', onclick: onCancelAddWallet }, t('action_cancel')) : null,
    el('div', { class: 'spacer' })));
}
function onCancelAddWallet() {
  ui.addingWallet = false;
  ui.screen = 'main';
  render();
}

// Supplementary entropy: crypto.getRandomValues already gives 256 bits, but a
// wallet is worth hedging the CSPRNG. Whatever is gathered here is *mixed*
// with it in wasm — it can only strengthen the seed — so this step is skippable.
function renderGen() {
  const pool = ui.entropyPool || (ui.entropyPool = new EntropyPool());
  ui.genWords ||= 24;
  if (!ui.jitterStarted) {
    ui.jitterStarted = true;
    pool.collectJitter(400).then(() => { if (ui.screen === 'gen') paintBar(); });
  }

  const wordsSeg = el('div', { class: 'seg' },
    [12, 24].map((n) => el('button', {
      class: ui.genWords === n ? 'on' : '',
      onclick: () => { ui.genWords = n; renderGen(); },
    }, tPlural('word_count', n))));

  const hint = el('div', { class: 'hint' }, t('gen_pad_hint_web'));
  const pad = el('div', {
    style:
      'position:relative;overflow:hidden;height:150px;border:1px dashed var(--hair);border-radius:var(--r-lg);' +
      'display:flex;align-items:center;justify-content:center;' +
      'touch-action:none;user-select:none;cursor:crosshair',
  }, hint);
  let drawing = false;
  const trail = [];
  const MAX_DOTS = 250;
  const sample = (e) => {
    if (!drawing) return;
    const r = pad.getBoundingClientRect();
    const x = e.clientX - r.left;
    const y = e.clientY - r.top;
    pool.addPointer(x, y, e.timeStamp);
    if (hint.isConnected) hint.remove();
    const dot = el('div', { style:
      `position:absolute;left:${x - 2}px;top:${y - 2}px;width:4px;height:4px;border-radius:50%;` +
      'background:var(--accent);box-shadow:0 0 4px var(--accent);pointer-events:none;' });
    pad.appendChild(dot);
    trail.push(dot);
    if (trail.length > MAX_DOTS) trail.shift().remove();
    paintBar();
  };
  pad.addEventListener('pointerdown', (e) => { drawing = true; pad.setPointerCapture(e.pointerId); sample(e); });
  pad.addEventListener('pointermove', sample);
  pad.addEventListener('pointerup', () => (drawing = false));

  const bar = el('div', { style: 'height:8px;border-radius:999px;background:var(--glass-1);overflow:hidden' },
    el('div', { id: 'entbar', style: 'height:100%;width:0%;background:linear-gradient(90deg,var(--accent),var(--accent-2));transition:width .1s' }));
  const label = el('div', { id: 'entlabel', class: 'hint' }, '');

  mount(el('div', { class: 'screen' },
    el('h2', {}, t('gen_title')),
    el('p', {}, t('gen_body_web')),
    el('label', {}, t('field_phrase_length')),
    wordsSeg,
    el('div', { class: 'hint' }, `${t('gen_words_bits_note')} ${t('gen_words_hint')}`),
    pad,
    bar,
    label,
    el('div', { id: 'err', class: 'err' }),
    el('div', { class: 'row' },
      el('button', { class: 'ghost', onclick: () => { ui.entropyPool = null; ui.jitterStarted = false; go('main'); } }, t('action_back')),
      el('button', { class: 'primary', onclick: onGenerate }, t('gen_generate')))));
  paintBar();
}

function paintBar() {
  const pool = ui.entropyPool;
  if (!pool) return;
  const bar = document.getElementById('entbar');
  const label = document.getElementById('entlabel');
  if (!bar) return;
  const pct = Math.min(100, Math.round((pool.bits / TARGET_BITS) * 100));
  bar.style.width = pct + '%';
  label.textContent = pct >= 100 ? t('gen_entropy_full') : t('gen_entropy_partial', pool.bits);
}

async function onGenerate() {
  const err = document.getElementById('err');
  err.textContent = '…';
  try {
    ui.draftMnemonic = newMnemonic(ui.entropyPool ? ui.entropyPool.bytes() : undefined, ui.genWords || 24);
    ui.entropyPool = null;
    ui.jitterStarted = false;
    go('create');
  } catch (e) {
    err.textContent = String(e.message || e);
  }
}

function chainPicker(current = 'xbt') {
  return el('select', { id: 'chain' },
    el('option', { value: 'xbt', selected: current === 'xbt' }, 'Bitcoin XBT'),
    el('option', { value: 'btc', selected: current === 'btc' }, 'Bitcoin BTC'));
}

function renderCreate() {
  const mnemonic = ui.draftMnemonic || (ui.draftMnemonic = newMnemonic(undefined, 24));
  const words = mnemonic.split(/\s+/);
  mount(el('div', { class: 'screen' },
    el('h2', {}, t('create_phrase_title')),
    el('p', {}, t('create_phrase_body', words.length)),
    el('ol', { class: 'words card' }, words.map((wd, i) => el('li', {}, el('b', {}, i + 1), wd))),
    el('label', {}, t('field_wallet_name')),
    el('input', { id: 'name', value: nextWalletName(), maxlength: MAX_WALLET_NAME_LEN, autocomplete: 'off' }),
    el('label', {}, t('field_chain')), chainPicker(),
    el('label', {}, t('field_passphrase')),
    el('input', { id: 'bip39pass', type: 'password', autocomplete: 'off' }),
    el('div', { class: 'hint' }, t('create_passphrase_hint')),
    el('label', { class: 'row', style: 'align-items:center;gap:.5rem' },
      el('input', { id: 'ack', type: 'checkbox', style: 'width:auto;flex:0' }),
      el('span', {}, t('create_ack'))),
    el('div', { id: 'err', class: 'err' }),
    el('div', { class: 'row' },
      el('button', { id: 'backBtn', class: 'ghost', onclick: () => { ui.draftMnemonic = null; go('main'); } }, t('action_back')),
      el('button', { id: 'createBtn', class: 'primary', onclick: onCreate }, t('action_continue')))));
}

async function onCreate() {
  const btn = document.getElementById('createBtn');
  if (btn.disabled) return; // guards against a mashed button firing this repeatedly
  const err = document.getElementById('err');
  if (!document.getElementById('ack').checked) return (err.textContent = t('error_confirm_phrase'));
  let name;
  try {
    name = readWalletName('name');
  } catch (e) {
    return (err.textContent = e.message);
  }
  const chain = val('chain');
  const passphrase = val('bip39pass');
  err.textContent = '';
  setBusy(btn, document.getElementById('backBtn'), t('action_creating'));
  try {
    await finishNewWallet(chain, 'mainnet', ui.draftMnemonic, passphrase, name);
  } catch (e) {
    err.textContent = String(e.message || e);
    clearBusy(btn, document.getElementById('backBtn'), t('action_continue'));
  }
}

/** Disable both buttons and swap the primary one's label for a spinner —
 *  wallet creation/restore is a multi-second Argon2id + network round trip,
 *  and without this a mashed button fires the async handler N times before
 *  the first call ever finishes, each one pushing its own duplicate wallet. */
function setBusy(primaryBtn, otherBtn, label) {
  primaryBtn.disabled = true;
  if (otherBtn) otherBtn.disabled = true;
  primaryBtn.replaceChildren(el('span', { class: 'spinner' }), document.createTextNode(' ' + label));
}
function clearBusy(primaryBtn, otherBtn, label) {
  primaryBtn.disabled = false;
  if (otherBtn) otherBtn.disabled = false;
  primaryBtn.textContent = label;
}

function renderRestore() {
  mount(el('div', { class: 'screen' },
    el('h2', {}, t('restore_title')),
    el('label', {}, t('restore_body')),
    el('textarea', { id: 'phrase', rows: 3, autocapitalize: 'none', autocomplete: 'off', spellcheck: 'false' }),
    el('label', {}, t('field_passphrase')),
    el('input', { id: 'passphrase', type: 'password', autocomplete: 'off' }),
    el('label', {}, t('field_wallet_name')),
    el('input', { id: 'name', value: nextWalletName(), maxlength: MAX_WALLET_NAME_LEN, autocomplete: 'off' }),
    el('label', {}, t('field_chain')), chainPicker(),
    el('div', { id: 'err', class: 'err' }),
    el('div', { class: 'row' },
      el('button', { id: 'backBtn', class: 'ghost', onclick: () => go('main') }, t('action_back')),
      el('button', { id: 'restoreBtn', class: 'primary', onclick: onRestore }, t('action_restore')))));
}

async function onRestore() {
  const btn = document.getElementById('restoreBtn');
  if (btn.disabled) return; // guards against a mashed button firing this repeatedly
  const err = document.getElementById('err');
  const phrase = val('phrase').trim().replace(/\s+/g, ' ');
  const passphrase = val('passphrase');
  const chain = val('chain');
  const network = 'mainnet';
  if (!phrase) return (err.textContent = t('error_enter_phrase'));
  let name;
  try {
    name = readWalletName('name');
  } catch (e) {
    return (err.textContent = e.message);
  }
  err.textContent = '';
  try {
    validateMnemonic(chain, network, phrase, passphrase); // throws on bad words
  } catch (e) {
    return (err.textContent = /invalid mnemonic/i.test(String(e)) ? t('error_invalid_phrase') : String(e.message || e));
  }
  setBusy(btn, document.getElementById('backBtn'), t('action_restoring'));
  try {
    await finishNewWallet(chain, network, phrase, passphrase, name);
  } catch (e) {
    err.textContent = String(e.message || e);
    clearBusy(btn, document.getElementById('backBtn'), t('action_restore'));
  }
}

function renderWatchImport() {
  mount(el('div', { class: 'screen' },
    el('h2', {}, t('watch_title')),
    el('label', {}, t('watch_body')),
    el('textarea', { id: 'xpub', rows: 3, autocapitalize: 'none', autocomplete: 'off', spellcheck: 'false' }),
    el('label', {}, t('field_wallet_name')),
    el('input', { id: 'name', value: nextWalletName(), maxlength: MAX_WALLET_NAME_LEN, autocomplete: 'off' }),
    el('label', {}, t('field_chain')), chainPicker(),
    el('label', {}, t('field_master_fingerprint')),
    el('input', { id: 'fingerprint', autocapitalize: 'none', autocomplete: 'off', spellcheck: 'false',
      placeholder: t('watch_fingerprint_hint') }),
    el('div', { id: 'err', class: 'err' }),
    el('div', { class: 'row' },
      el('button', { id: 'backBtn', class: 'ghost', onclick: () => go('main') }, t('action_back')),
      el('button', { id: 'watchBtn', class: 'primary', onclick: onWatchImport }, t('action_watch')))));
}

/** Base58Check-decode just far enough to read a BIP-32 extended key's depth
 *  byte — for a friendlier check than the generic "invalid xpub" error wasm
 *  gives, in the one specific case worth calling out by name: a *master*
 *  key (depth 0) is a structurally valid xpub, so it passes wasm's own
 *  parser fine, but it silently derives addresses from a completely
 *  non-standard path (`m/0/N` instead of `m/84'/coin'/account'/0/N`) that no
 *  real wallet ever uses — the result looks like a working wallet with
 *  permanently empty history, with no error to explain why. wasm's parser
 *  (called right after this) is still the real validity check for
 *  everything else — checksum, curve point, network. Every real xpub starts
 *  with a nonzero version byte, so the usual base58 leading-zero-byte edge
 *  case never applies here. */
function xpubDepth(s) {
  const ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
  try {
    let num = 0n;
    for (const c of s) {
      const idx = ALPHABET.indexOf(c);
      if (idx === -1) return null;
      num = num * 58n + BigInt(idx);
    }
    let hex = num.toString(16);
    if (hex.length % 2) hex = '0' + hex;
    const bytes = hex.match(/../g) || [];
    return bytes.length > 4 ? parseInt(bytes[4], 16) : null; // [version(4) depth(1) ...]
  } catch {
    return null;
  }
}

async function onWatchImport() {
  const btn = document.getElementById('watchBtn');
  if (btn.disabled) return; // guards against a mashed button firing this repeatedly
  const err = document.getElementById('err');
  const xpub = val('xpub');
  const chain = val('chain');
  const fingerprint = val('fingerprint').trim().toLowerCase();
  if (!xpub) return (err.textContent = t('error_enter_xpub'));
  if (xpubDepth(xpub) === 0) return (err.textContent = t('error_xpub_is_master_key'));
  if (fingerprint && !/^[0-9a-f]{8}$/.test(fingerprint)) return (err.textContent = t('error_invalid_fingerprint'));
  let name;
  try {
    name = readWalletName('name');
  } catch (e) {
    return (err.textContent = e.message);
  }
  err.textContent = '';
  try {
    Session.watchOnly(chain, 'mainnet', xpub).free(); // throws on a malformed xpub
  } catch (e) {
    return (err.textContent = /invalid account xpub/i.test(String(e)) ? t('error_invalid_xpub') : String(e.message || e));
  }
  setBusy(btn, document.getElementById('backBtn'), t('action_creating'));
  try {
    await finishWatchWallet(chain, name, xpub, fingerprint || null);
  } catch (e) {
    err.textContent = String(e.message || e);
    clearBusy(btn, document.getElementById('backBtn'), t('action_watch'));
  }
}

/** A watch-only wallet has nothing to seal — no mnemonic, no app-lock
 *  interaction at all, even as a device's very first wallet. `fingerprint`
 *  (optional): the signing wallet's master fingerprint, from its Settings
 *  "Copy fingerprint" action — without it, this wallet can still watch a
 *  balance/history but can never export a transaction for offline signing
 *  (see wallet-core's `FundingPlan::psbt_base64` doc). */
async function finishWatchWallet(chain, name, xpub, fingerprint) {
  const wallet = {
    id: crypto.randomUUID(), name, chain, network: 'mainnet', watchOnly: true, xpub, fingerprint,
    next_receive: 0, next_change: 0, backend: null,
  };
  await autoConnectBackend(wallet);
  state = state
    ? { ...state, wallets: [...state.wallets, wallet], selected: wallet.id }
    : { v: 2, wallets: [wallet], selected: wallet.id, lock: null };
  await saveState(state);
  ui.addingWallet = false;
  ui.screen = 'main';
  ui.nav = 'wallet';
  ui.tab = 'receive';
  render();
}

/** Every wallet connects to the hosted edge automatically — mainnet only,
 *  no picker, no self-hosted-node option. If the hosted edge can't be
 *  reached, fall back to a public explorer silently (matching the Android
 *  app's documented behavior) rather than leaving the wallet with nothing:
 *  this is an automatic resilience step, not a user-facing choice. */
async function autoConnectBackend(wallet) {
  try {
    const token = await edgeRegister(DEFAULT_EDGE);
    wallet.backend = { kind: 'edge', url: DEFAULT_EDGE, token };
  } catch {
    wallet.backend = { kind: 'esplora', url: DEFAULT_ESPLORA[wallet.chain] || DEFAULT_ESPLORA.xbt };
  }
}

/** A wallet only ever ends up on `kind: 'esplora'` because `autoConnectBackend`
 *  tried the hosted edge and it wasn't reachable at the time — there's no UI
 *  path left to pick esplora on purpose. So it's always worth trying to
 *  upgrade back once the edge recovers, rather than leaving the wallet
 *  stranded on a public explorer (weaker rate limits, no service-fee
 *  handling) until the user happens to hit Settings -> Reconnect.
 *
 *  Runs as an ordinary async interval, not a Worker thread — this is a
 *  single-threaded JS app and every wasm session/backend handle already
 *  lives on the main thread, so "non-blocking" here means what it always
 *  means in JS: an async function driven by a timer, which naturally yields
 *  to the event loop on every `await` instead of freezing the UI. A `retrying`
 *  guard (mirroring detailScanning/overviewScanning) stops one slow cycle
 *  from overlapping the next. Deliberately skips `kind: 'gateway'` wallets —
 *  those are a deliberate self-hosted-node choice from before the backend
 *  picker was removed, not a fallback, and should never be silently swapped
 *  onto the hosted service. */
let retryingEdge = false;
async function retryEdgeUpgrades() {
  if (retryingEdge || !state || !appSecret) return;
  retryingEdge = true;
  try {
    let upgraded = false;
    for (const w of state.wallets) {
      if (w.backend?.kind !== 'esplora') continue;
      try {
        const token = await edgeRegister(DEFAULT_EDGE);
        w.backend = { kind: 'edge', url: DEFAULT_EDGE, token };
        backends.delete(w.id);
        upgraded = true;
      } catch {
        /* edge still unreachable — try again next tick */
      }
    }
    if (upgraded) {
      await saveState(state);
      renderShellIfIdle();
    }
  } finally {
    retryingEdge = false;
  }
}
setInterval(retryEdgeUpgrades, 10_000);

/** First wallet ever -> app-lock setup first. Otherwise (adding wallet #2+,
 *  already unlocked) seal straight under the in-memory `appSecret`. */
async function finishNewWallet(chain, network, mnemonic, passphrase, name) {
  // A signing wallet always needs an app-lock to seal its mnemonic under —
  // check for "no lock exists yet", not "no state exists yet": a device can
  // already hold `state` full of watch-only wallets (which need no lock at
  // all) by the time its first signing wallet is added.
  if (!state?.lock) {
    ui.pendingWallet = { chain, network, mnemonic, passphrase, name };
    ui.screen = 'lock-setup';
    return render();
  }
  const { sealed, salt } = sealWithAppSecret(mnemonic, passphrase, appSecret);
  const id = crypto.randomUUID();
  const wallet = { id, name: name || nextWalletName(), chain, network, sealed, salt, next_receive: 0, next_change: 0, backend: null };
  await autoConnectBackend(wallet);
  state.wallets.push(wallet);
  state.selected = id;
  resetDetail();
  await saveState(state);
  ui.addingWallet = false;
  ui.draftMnemonic = null;
  ui.screen = 'main';
  ui.nav = 'wallet';
  ui.tab = 'receive';
  render();
}

/* ------------------------------------------------------------------ app-lock setup (first wallet) */

function renderLockSetup() {
  const prfOk = prfPossible();
  mount(el('div', { class: 'screen' },
    el('h2', {}, t('biometric_setup_title')),
    el('p', {}, t('lock_choice_password_note')),
    el('label', {}, t('field_app_password')),
    el('input', { id: 'pw', type: 'password', autocomplete: 'new-password' }),
    el('label', {}, t('field_confirm_password')),
    el('input', { id: 'pw2', type: 'password', autocomplete: 'new-password' }),
    prfOk ? el('label', { class: 'row', style: 'align-items:center;gap:.5rem' },
      el('input', { id: 'prf', type: 'checkbox', style: 'width:auto;flex:0' }),
      el('span', {}, t('lock_choice_use_prf'))) : null,
    prfOk ? el('div', { class: 'hint' }, t('lock_choice_prf_note')) : null,
    el('div', { id: 'err', class: 'err' }),
    el('div', { class: 'row' },
      el('button', { id: 'backBtn', class: 'ghost', onclick: () => { ui.screen = 'create'; render(); } }, t('action_back')),
      el('button', { id: 'lockSetupBtn', class: 'primary', onclick: onLockSetupSubmit }, t('action_continue')))));
}

async function onLockSetupSubmit() {
  const btn = document.getElementById('lockSetupBtn');
  if (btn.disabled) return; // guards against a mashed button firing this repeatedly
  const err = document.getElementById('err');
  const pw = val('pw');
  const pw2 = val('pw2');
  if (pw.length < 8) return (err.textContent = t('lock_password_hint', 8));
  if (pw !== pw2) return (err.textContent = t('lock_password_mismatch'));
  err.textContent = '';
  setBusy(btn, document.getElementById('backBtn'), t('action_creating'));
  try {
    const secret = newAppSecret();
    const { wrapped, salt } = wrapAppSecretWithPassword(secret, pw);
    let prf = null;
    if (document.getElementById('prf')?.checked) {
      const reg = await registerPrf();
      if (reg) prf = { credentialId: reg.credentialId, prfSalt: reg.prfSalt, wrapped: wrapAppSecretWithKey(secret, reg.secret).wrapped };
    }
    const { chain, network, mnemonic, passphrase, name } = ui.pendingWallet;
    const { sealed, salt: wSalt } = sealWithAppSecret(mnemonic, passphrase, secret);
    const id = crypto.randomUUID();
    const wallet = { id, name: name || nextWalletName(), chain, network, sealed, salt: wSalt, next_receive: 0, next_change: 0, backend: null };
    await autoConnectBackend(wallet);
    // Append onto whatever's already there instead of overwriting — a device
    // can already hold watch-only wallets (which need no lock) by the time
    // its first signing wallet triggers this setup.
    state = { v: 2, wallets: [...(state?.wallets ?? []), wallet], selected: id, lock: { password: { wrapped, salt }, prf } };
    appSecret = secret;
    await saveState(state);
    ui.pendingWallet = null;
    ui.draftMnemonic = null;
    // Was never reachable while true before watch-only wallets existed (you
    // can't be "adding an additional wallet" before a device has its first
    // one) — now it can be, e.g. a watch-only-only device adding its first
    // signing wallet. Left set, render() would loop back to onboarding
    // instead of the shell.
    ui.addingWallet = false;
    ui.screen = 'main';
    ui.nav = 'wallet';
    ui.tab = 'receive';
    render();
  } catch (e) {
    err.textContent = String(e.message || e);
    clearBusy(btn, document.getElementById('backBtn'), t('action_continue'));
  }
}

/* ------------------------------------------------------------------ v1 -> v2 migration */

function renderMigrate() {
  mount(el('div', { class: 'screen' },
    el('div', { class: 'spacer' }),
    brand(),
    el('div', { class: 'card stack' },
      el('p', {}, t('lock_password_prompt')),
      el('input', { id: 'pw', type: 'password', autocomplete: 'current-password', onkeydown: (e) => e.key === 'Enter' && onMigrateSubmit() }),
      el('div', { id: 'err', class: 'err' }),
      el('button', { class: 'primary wide', onclick: onMigrateSubmit }, t('action_unlock'))),
    el('div', { class: 'spacer' })));
}

async function onMigrateSubmit() {
  const err = document.getElementById('err');
  const pw = val('pw');
  err.textContent = '…';
  try {
    const { mnemonic, passphrase } = unseal(pendingLegacy.sealed, pendingLegacy.salt, pw);
    const secret = newAppSecret();
    const { wrapped, salt } = wrapAppSecretWithPassword(secret, pw);
    const { sealed, salt: wSalt } = sealWithAppSecret(mnemonic, passphrase, secret);
    const id = crypto.randomUUID();
    const wallet = {
      id, name: 'Wallet', chain: pendingLegacy.chain, network: pendingLegacy.network,
      sealed, salt: wSalt,
      next_receive: pendingLegacy.next_receive || 0, next_change: pendingLegacy.next_change || 0,
      backend: pendingLegacy.backend || null,
    };
    const newState = { v: 2, wallets: [wallet], selected: id, lock: { password: { wrapped, salt }, prf: null } };
    await finishMigration(newState);
    state = newState;
    appSecret = secret;
    pendingLegacy = null;
    render();
  } catch {
    err.textContent = t('error_wrong_password');
  }
}

/* ------------------------------------------------------------------ locked */

function renderLocked() {
  if (state.lock.prf && prfPossible() && !ui.prfAutoTried) {
    ui.prfAutoTried = true;
    attemptPrfUnlock();
  }
  mount(el('div', { class: 'screen' },
    el('div', { class: 'spacer' }),
    brand(),
    el('div', { class: 'card stack' },
      el('label', {}, t('lock_password_prompt')),
      el('input', { id: 'pw', type: 'password', autocomplete: 'current-password', onkeydown: (e) => e.key === 'Enter' && onUnlockPassword() }),
      el('div', { id: 'err', class: 'err' }),
      el('button', { class: 'primary wide', onclick: onUnlockPassword }, t('action_unlock')),
      state.lock.prf && prfPossible()
        ? el('button', { class: 'ghost wide', onclick: () => { ui.prfAutoTried = false; render(); } }, t('action_try_quick_unlock'))
        : null),
    el('div', { class: 'spacer' }),
    el('button', { class: 'ghost danger', onclick: onWipe }, t('wipe_action'))));
}

async function attemptPrfUnlock() {
  try {
    const secret = await unlockPrf(state.lock.prf.credentialId, state.lock.prf.prfSalt);
    appSecret = unwrapAppSecretWithKey(state.lock.prf.wrapped, secret);
    render();
  } catch {
    /* silent — the password field remains available */
  }
}

function onUnlockPassword() {
  const err = document.getElementById('err');
  try {
    appSecret = unwrapAppSecretWithPassword(state.lock.password.wrapped, state.lock.password.salt, val('pw'));
    render();
  } catch {
    err.textContent = t('error_wrong_password');
  }
}

function onWipe() {
  ui.dialog = {
    kind: 'confirm', title: t('wipe_action'), body: t('wipe_confirm'),
    confirmLabel: t('action_remove'), danger: true,
    onConfirm: async () => {
      for (const s of sessions.values()) s.free();
      sessions.clear();
      backends.clear();
      await wipeState();
      appSecret = null;
      state = null;
      pendingLegacy = null;
      overview.balances.clear();
      overview.usd = {};
      render();
    },
  };
  renderDialogSheet();
}

function onLock() {
  for (const s of sessions.values()) s.free();
  sessions.clear();
  backends.clear();
  appSecret = null;
  overview.balances.clear();
  overview.usd = {};
  detail = { status: null, balances: null, history: [], feerate: {}, usd: null, _shownBal: undefined };
  ui.prfAutoTried = false;
  ui.nav = 'home';
  render();
}

/* ------------------------------------------------------------------ shell (home / wallet / settings) */

function renderNav() {
  return el('div', { class: 'tabs card' },
    [['home', 'nav_home'], ['wallet', 'nav_wallet'], ['settings', 'nav_settings']].map(([id, key]) =>
      el('button', {
        class: ui.nav === id ? 'active' : '',
        onclick: () => { ui.nav = id; if (id === 'wallet') ui.tab = 'receive'; render(); },
      }, t(key))));
}

/** `fresh` (only on genuine navigation — see the sole call site in render())
 *  mounts a brand-new `.screen` element, playing its entrance animation.
 *  Background poll refreshes call this with no args instead, updating the
 *  existing shell's children in place — otherwise every 20-45s refresh would
 *  replace the `.screen` element and replay the animation, a visible
 *  "twitch" the user never asked for. */
function renderShell(fresh = false) {
  let body;
  if (ui.nav === 'home') body = renderHomeTab();
  else if (ui.nav === 'settings') body = renderSettingsTab();
  else body = renderWalletTab();
  const nav = renderNav();
  const existing = fresh ? null : document.querySelector('#app > .screen[data-shell]');
  if (existing) existing.replaceChildren(nav, body);
  else mount(el('div', { class: 'screen', style: 'gap:.9rem', 'data-shell': '1' }, nav, body));
  if (ui.nav === 'wallet' && detail.balances) {
    const num = document.querySelector('.bal .num');
    if (num) countUp(num, detail._shownBal ?? 0, detail.balances.confirmed_sat);
    detail._shownBal = detail.balances.confirmed_sat;
  }
  startPolling();
}

/** Re-render the shell in place (no phase/backend re-check, no polling churn)
 *  — what poll timers call once new data has arrived. */
function renderShellIfIdle() {
  // Mirrors render()'s own gate: the shell is valid to show once state
  // exists and either no lock exists at all (a watch-only-only device,
  // which never sets appSecret) or the lock is unlocked. This used to just
  // check `appSecret`, which was correct back when every unlocked app had
  // one — with watch-only wallets that's no longer true, and every
  // background poll's re-render was silently a no-op on such a device: the
  // data was fetched and detail.status updated correctly, the screen just
  // never got told to redraw, so it looked permanently stuck connecting.
  if (state && (!state.lock || appSecret) && !ui.addingWallet && !ui.pendingPlan && !ui.dialog) renderShell();
}

function renderHomeTab() {
  return el('div', { class: 'stack' },
    el('h2', {}, t('home_title')),
    state.wallets.length
      ? el('div', { class: 'card walletlist' },
          state.wallets.map((w) => {
            const sat = overview.balances.get(w.id);
            const usdPrice = overview.usd[w.chain];
            const usd = usdPrice != null && sat != null ? fmtUsd((usdPrice * sat) / SAT) : null;
            return el('div', {
              class: 'walletrow',
              onclick: () => { state.selected = w.id; resetDetail(); saveState(state); ui.nav = 'wallet'; ui.tab = 'receive'; render(); },
            },
              el('div', {},
                el('div', { class: 'name' }, w.name),
                el('div', { class: 'chain' }, `${UNIT[w.chain]}${w.network === 'regtest' ? ' · regtest' : ''}${w.watchOnly ? ` · ${t('watch_badge')}` : ''}  ·  ${t('home_tap_to_open')}`)),
              el('div', { style: 'text-align:right' },
                el('div', { class: 'amt' }, sat != null ? `${fmt(sat)} ${UNIT[w.chain]}` : '…'),
                usd ? el('div', { class: 'hint' }, usd) : null));
          }))
      : null,
    state.wallets.length < MAX_WALLETS
      ? el('button', { class: 'primary wide', onclick: onAddWalletStart }, t('action_add_wallet'))
      : el('div', { class: 'hint center' }, t('wallets_max', MAX_WALLETS)));
}

function onAddWalletStart() {
  ui.addingWallet = true;
  ui.screen = 'main';
  render();
}

/* ------------------------------------------------------------------ wallet tab */

function explorerTxUrl(w, txid) {
  if (w.network !== 'mainnet') return null;
  const base = EXPLORER[w.chain];
  return base ? `${base}/tx/${txid}` : null;
}

function renderWalletTab() {
  const w = currentWallet();
  if (!w) return el('div', { class: 'card center hint' }, t('wallet_choose'));
  const unit = UNIT[w.chain];
  const b = detail.balances;
  const st = detail.status;
  let dot = 'warn';
  let hint = t('wallet_connecting');
  if (st) {
    const p = st.node?.progress ?? 0;
    if (st.scanning) { dot = 'warn'; hint = t('wallet_rescanning', Math.round(st.scanning.progress * 100)); }
    else if (st.node?.ibd || p < 0.999) { dot = 'warn'; hint = t('wallet_syncing_pct', (p * 100).toFixed(2)); }
    else { dot = 'ok'; hint = t('wallet_block', st.node?.blocks); }
  }
  const via = w.backend?.kind === 'edge' ? 'fortis' : w.backend?.kind === 'esplora' ? new URL(w.backend.url).host : t('wallet_via_node');
  const usdLine = detail.usd != null && b ? t('fiat_approx', fmtUsd((detail.usd * b.confirmed_sat) / SAT)) : null;

  const top = el('div', { class: 'topbar' },
    el('div', {},
      el('div', { class: 'name' }, w.name + (w.watchOnly ? `  ·  ${t('watch_badge')}` : '')),
      el('div', { class: 'bal' },
        el('span', { class: 'num' }, b ? fmt(b.confirmed_sat) : '—'), ' ',
        el('span', { class: 'unit' }, unit)),
      usdLine ? el('div', { class: 'hint' }, usdLine) : null,
      el('div', { class: 'hint' },
        el('span', { class: `dot ${dot}` }), ' ', hint, `  ·  ${via}`,
        b && b.pending_sat ? `  ·  ${t('wallet_pending', fmt(b.pending_sat), unit)}` : '')));

  // Building (not signing) a transaction needs no key, so a watch-only
  // wallet keeps Send too — for air-gapped signing, see paneSend().
  const tabIds = ['receive', 'send', 'history'];
  if (!tabIds.includes(ui.tab)) ui.tab = 'receive'; // e.g. stale 'send' on a watch-only wallet
  const tabs = el('div', { class: 'tabs card' },
    tabIds.map((tid) =>
      el('button', { class: ui.tab === tid ? 'active' : '', onclick: () => { ui.tab = tid; render(); } }, t(`tab_${tid}`))));

  let body;
  if (ui.tab === 'receive') body = paneReceive(w);
  else if (ui.tab === 'send') body = paneSend(w);
  else body = paneHistory(w);

  return el('div', { style: 'display:flex;flex-direction:column;gap:.9rem' }, top, tabs, body);
}

function paneReceive(w) {
  const session = ensureSession(w);
  let addr = '…';
  try { addr = session.receiveAddress(w.next_receive).address; } catch (e) { addr = String(e.message || e); }
  return el('div', { class: 'card stack' },
    el('h2', {}, t('tab_receive')),
    el('div', { onclick: () => copy(addr), style: 'cursor:pointer' }, qrCode(addr)),
    el('div', { class: 'addr-box mono', style: 'cursor:pointer', ondblclick: () => copy(addr) }, addr),
    el('div', { class: 'hint' }, t('receive_address_index', w.next_receive)),
    el('div', { class: 'row' },
      el('button', { onclick: () => copy(addr) }, t('action_copy')),
      el('button', { class: 'ghost', onclick: async () => {
        w.next_receive += 1;
        await saveState(state);
        render();
      } }, t('action_new_address'))));
}

function paneSend(w) {
  const d = ui.draft || (ui.draft = { to: '', amount: '', sweep: false, target: 6 });
  const unit = UNIT[w.chain];
  const presets = detail.feerate;

  const feeSeg = el('div', { class: 'seg' },
    [['1', 'fee_fast'], ['6', 'fee_normal'], ['144', 'fee_slow']].map(([target, key]) =>
      el('button', {
        class: String(d.target) === target ? 'on' : '',
        onclick: () => { d.target = Number(target); d.customFee = null; render(); },
      }, `${t(key)}${presets[target] ? ` · ${presets[target]} s/vB` : ''}`)));

  let fiatLine = null;
  if (!d.sweep && d.amount && detail.usd != null) {
    const n = Number(d.amount);
    if (isFinite(n) && n > 0) fiatLine = t('value_with_fiat', d.amount, unit, fmtUsd(detail.usd * n));
  }

  return el('div', { class: 'card stack' },
    el('h2', {}, t('tab_send')),
    el('label', {}, t('field_to_address')),
    el('input', { id: 'to', value: d.to, autocapitalize: 'none', spellcheck: 'false',
      oninput: (e) => (d.to = e.target.value.trim()) }),
    el('label', { class: 'row', style: 'align-items:center;gap:.5rem' },
      el('input', { type: 'checkbox', style: 'width:auto;flex:0', checked: d.sweep,
        onchange: (e) => { d.sweep = e.target.checked; render(); } }),
      el('span', {}, t('send_sweep'))),
    d.sweep ? null : el('label', {}, `${t('field_amount')} (${unit})`),
    d.sweep ? null : el('input', { id: 'amount', value: d.amount, inputmode: 'decimal',
      oninput: (e) => (d.amount = e.target.value) }),
    fiatLine ? el('div', { class: 'hint' }, fiatLine) : null,
    el('label', {}, t('confirm_network_fee')),
    feeSeg,
    el('input', { id: 'customfee', placeholder: t('field_custom_feerate'), inputmode: 'numeric',
      value: d.customFee || '', oninput: (e) => (d.customFee = e.target.value) }),
    w.chain === 'btc' && !d.sweep
      ? el('label', { class: 'row', style: 'align-items:center;gap:.5rem' },
          el('input', { type: 'checkbox', style: 'width:auto;flex:0', checked: d.replayProtect,
            onchange: (e) => (d.replayProtect = e.target.checked) }),
          el('span', {}, t('send_replay_protect')))
      : null,
    w.chain === 'btc' && !d.sweep && d.replayProtect
      ? el('div', { class: 'hint' }, t('send_replay_note'))
      : null,
    detail.status?.pricing
      ? el('div', { class: 'hint' }, t('send_service_fee', (detail.status.pricing.bps / 100).toFixed(2), detail.status.pricing.floor_sat))
      : null,
    el('div', { id: 'err', class: 'err' }),
    el('button', { class: 'primary wide', onclick: () => onReview(w) }, t('action_review')),
    // No key here to sign with — "Review" above still builds an unsigned
    // plan/PSBT to export. This is the other half of that round trip:
    // bringing back whatever the offline device produced. Independent of
    // having just exported in this same session — covers closing the tab
    // and returning later with the signed result.
    w.watchOnly
      ? el('button', { class: 'ghost wide', onclick: () => {
          ui.dialog = { kind: 'import-signed', walletId: w.id };
          renderDialogSheet();
        } }, t('action_import_signed'))
      : null);
}

async function onReview(w) {
  const err = document.getElementById('err');
  const d = ui.draft;
  err.textContent = '…';
  try {
    if (!d.to) throw new Error(t('error_enter_destination'));
    const session = ensureSession(w);
    session.checkAddress(d.to); // clear "… is not a valid address" before any network I/O
    const backend = ensureBackend(w);
    const minConf = 1;
    const [feerateResp, utxos] = await Promise.all([
      d.customFee ? Promise.resolve({ sat_vb: Number(d.customFee) }) : backend.feerate(d.target),
      backend.utxos(minConf),
    ]);
    const feerate = Math.max(1, Math.round(feerateResp.sat_vb));
    if (!utxos.length) throw new Error(t('error_no_coins'));

    let opReturnHex;
    if (w.chain === 'btc' && !d.sweep && d.replayProtect) {
      opReturnHex = [...crypto.getRandomValues(new Uint8Array(100))]
        .map((b) => b.toString(16).padStart(2, '0')).join('');
    }
    const pricing = detail.status?.pricing;
    const serviceFee = pricing
      ? { address: pricing.address, bps: pricing.bps, floor_sat: pricing.floor_sat, cap_sat: pricing.cap_sat }
      : undefined;
    const plan = d.sweep
      ? session.planSweep(utxos, d.to, feerate, minConf, serviceFee)
      : session.planPayment(
          utxos, [{ address: d.to, amount_sat: parseAmount(d.amount) }], feerate, minConf, opReturnHex, serviceFee,
        );

    ui.pendingPlan = { plan, feerate, to: d.to, sweep: d.sweep, replayProtect: !!opReturnHex, walletId: w.id };
    renderConfirm();
  } catch (e) {
    err.textContent = String(e.message || e).replace(/^.*?: /, '');
  }
}

function renderConfirm() {
  const { plan, feerate, to, sweep, walletId } = ui.pendingPlan;
  const w = state.wallets.find((x) => x.id === walletId);
  const noFingerprint = w.watchOnly && !plan.psbt_base64;
  const unit = UNIT[w.chain];
  const inTotal = plan.selected.reduce((s, u) => s + Number(u.value_sat), 0);
  const svcFee = Number(plan.service_fee_sat || 0);
  const outAmount = inTotal - Number(plan.fee_sat) - svcFee - Number(plan.change_sat || 0);
  const totalOut = outAmount + Number(plan.fee_sat) + svcFee;
  const usdAmount = detail.usd != null ? fmtUsd((detail.usd * outAmount) / SAT) : null;
  const usdTotal = detail.usd != null ? fmtUsd((detail.usd * totalOut) / SAT) : null;
  const cancel = () => { ui.pendingPlan = null; render(); };

  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: cancel }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t(sweep ? 'confirm_sweep_title' : 'confirm_payment_title')),
      el('div', { class: 'amount-lead mono' }, `${fmt(outAmount)} `, el('span', { class: 'unit' }, unit)),
      usdAmount ? el('div', { class: 'hint' }, t('fiat_approx', usdAmount)) : null,
      el('div', { class: 'kvs' },
        row(t('confirm_to'), el('span', { class: 'mono', style: 'word-break:break-all;text-align:right' }, to)),
        row(t('confirm_network_fee'), t('confirm_network_fee_value', fmt(plan.fee_sat), unit, feerate)),
        svcFee > 0 ? row(t('confirm_service_fee'), `${fmt(svcFee)} ${unit}`) : null,
        plan.change_sat != null ? row(t('confirm_change'), `${fmt(plan.change_sat)} ${unit}`) : null,
        row(t('confirm_inputs'), tPlural('confirm_inputs_value', plan.selected.length)),
        ui.pendingPlan.replayProtect ? row(t('confirm_replay'), t('confirm_replay_value')) : null,
        row(t('confirm_total'), el('b', {}, usdTotal ? t('value_with_fiat', fmt(totalOut), unit, usdTotal) : `${fmt(totalOut)} ${unit}`))),
      noFingerprint ? el('p', { class: 'hint' }, t('error_no_fingerprint_on_file')) : null,
      el('div', { id: 'err', class: 'err' }),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: cancel }, t('action_cancel')),
        w.watchOnly
          ? el('button', { class: 'primary', id: 'send', disabled: noFingerprint, onclick: onExportOffline },
              t('action_export_offline'))
          : el('button', { class: 'primary', id: 'send', onclick: onSend }, t('action_sign_send'))))));
}
const row = (k, v) => el('div', { class: 'kv' }, el('span', {}, k), v?.nodeType ? v : el('span', {}, v));

async function onSend() {
  const err = document.getElementById('err');
  const btn = document.getElementById('send');
  btn.disabled = true;
  err.textContent = '…';
  try {
    const { plan, walletId } = ui.pendingPlan;
    const w = state.wallets.find((x) => x.id === walletId);
    const session = ensureSession(w);
    const backend = ensureBackend(w);
    const signed = session.sign(plan.tx_hex, plan.selected);
    const { txid } = await backend.broadcast(signed);
    const { next_change } = session.indices();
    w.next_change = Math.max(w.next_change, next_change ?? 0);
    await saveState(state);
    ui.pendingPlan = null;
    ui.draft = null;
    ui.tab = 'history';
    toast(`${t('sent_title')} · ${shortTxid(txid)}`);
    await refresh();
    render();
    pollForTxid(w.id, txid);
  } catch (e) {
    btn.disabled = false;
    err.textContent = String(e.message || e).replace(/^.*?: /, '');
  }
}

/** A watch-only wallet's equivalent of onSend() — there's no key here to sign
 *  with, so this exports the plan's PSBT (QR + always-present copy box)
 *  instead of broadcasting. The change index plan_payment()/plan_sweep()
 *  already consumed is persisted right away, same as onSend()'s tail,
 *  because the gap until a signed transaction comes back from the offline
 *  device is unbounded — waiting to persist it "until it's really sent"
 *  would let the very next plan on this wallet reuse the same change
 *  address. */
function onExportOffline() {
  const { plan, walletId } = ui.pendingPlan;
  const w = state.wallets.find((x) => x.id === walletId);
  const session = ensureSession(w);
  const { next_change } = session.indices();
  w.next_change = Math.max(w.next_change, next_change ?? 0);
  saveState(state);
  ui.pendingPlan = null;
  ui.draft = null;
  ui.dialog = {
    kind: 'export-blob',
    title: t('export_psbt_title'),
    body: t('export_psbt_body'),
    text: plan.psbt_base64,
  };
  renderDialogSheet();
}

/** A backend's own mempool view can lag a broadcast by up to one of its own
 *  refresh cycles — fortis-index, for instance, rebuilds its mempool overlay
 *  on an independent ~10s timer, not in response to any individual
 *  broadcast. The single refresh() right after onSend() routinely lands
 *  before that cycle runs, so the just-sent transaction is briefly invisible
 *  in history — and the app's normal poll is 25-45s, long enough that
 *  "briefly" can read as "never showed up." Poll faster for a short window
 *  right after a send instead of waiting for the next regular tick. */
async function pollForTxid(walletId, txid) {
  for (let i = 0; i < 7; i++) {
    await new Promise((r) => setTimeout(r, 3000));
    if (state?.selected !== walletId) return; // navigated to a different wallet
    await refresh();
    renderShellIfIdle();
    if (detail.history.some((h) => h.txid === txid)) return;
  }
}

function paneHistory(w) {
  const h = detail.history;
  const unit = UNIT[w.chain];
  if (!h.length) return el('div', { class: 'card center hint' }, t('history_empty'));
  const openTx = (txid) => {
    const u = explorerTxUrl(w, txid);
    if (u) window.open(u, '_blank', 'noopener'); else copy(txid);
  };
  return el('div', { class: 'card hist' }, [
    ...h.map((tx) => {
      const pos = tx.amount_sat > 0;
      const conf = tx.confirmations;
      const send = tx.direction === 'send';
      const label = send && tx.amount_sat === 0 ? t('history_internal') : t(send ? 'history_send' : 'history_receive');
      const fee = send && tx.fee_sat ? ` · ${Math.abs(tx.fee_sat)} sat` : '';
      const usd = detail.usd != null ? fmtUsd((detail.usd * Math.abs(tx.amount_sat)) / SAT) : null;
      return el('div', { class: 'item', onclick: () => openTx(tx.txid) },
        el('div', {},
          el('div', { class: `amt ${pos ? 'pos' : 'neg'}` }, `${pos ? '+' : ''}${fmt(tx.amount_sat)} ${unit}`),
          usd ? el('div', { class: 'hint' }, usd) : null,
          el('div', { class: 'meta' }, `${label} · ${timeAgo(tx.time)} · ${shortTxid(tx.txid)}${fee}`)),
        el('span', { class: `badge ${conf < 1 ? 'pending' : ''}` },
          conf < 1 ? t('history_pending') : conf < 6 ? t('history_conf_n', conf) : t('history_confirmed')));
    }),
    explorerTxUrl(w, '') ? el('div', { class: 'hint', style: 'padding:8px 2px 0' }, t('history_hint_web')) : null,
  ]);
}

/* ------------------------------------------------------------------ settings */

function renderSettingsTab() {
  return el('div', { class: 'stack' },
    el('div', { class: 'card stack' },
      el('h2', {}, t('settings_wallets')),
      ...state.wallets.map((w) => renderWalletCard(w)),
      state.wallets.length < MAX_WALLETS
        ? el('button', { class: 'ghost wide', onclick: onAddWalletStart }, t('action_add_wallet'))
        : el('div', { class: 'hint' }, t('wallets_max', MAX_WALLETS))),
    // A device holding only watch-only wallets has no app-lock and nothing
    // secret to protect — there's no password to change, no appSecret for
    // quick-unlock to wrap, and nothing for "Lock app" to lock.
    state.lock ? el('div', { class: 'card stack' },
      el('h2', {}, t('settings_security')),
      el('div', { class: 'hint' }, t('settings_one_unlock')),
      state.lock.prf
        ? el('div', { class: 'stack' },
            el('div', { class: 'hint' }, t('prf_status_on')),
            el('button', { class: 'ghost wide', onclick: actionDisableQuickUnlock }, t('action_turn_off_quick_unlock')))
        : prfPossible()
          ? el('button', { class: 'ghost wide', onclick: actionEnableQuickUnlock }, t('action_enable_quick_unlock'))
          : el('div', { class: 'hint' }, t('prf_not_supported')),
      el('button', { class: 'ghost wide', onclick: () => { ui.dialog = { kind: 'change-password' }; renderDialogSheet(); } }, t('action_change_password'))) : null,
    renderVerifyCard(),
    el('div', { class: 'card stack' },
      el('h2', {}, t('settings_language')),
      el('button', { class: 'ghost wide', onclick: () => { ui.dialog = { kind: 'locale' }; renderDialogSheet(); } },
        overrideTag() ? localeLabel(overrideTag()) : t('language_system_default'))),
    el('div', { class: 'card stack' },
      el('h2', {}, t('settings_theme')),
      el('div', { class: 'seg' },
        [['system', t('language_system_default')], ['light', t('theme_light')], ['dark', t('theme_dark')]].map(([id, label]) =>
          el('button', {
            class: storedTheme() === id ? 'on' : '',
            onclick: () => { setTheme(id); render(); },
          }, label)))),
    state.lock ? el('div', { class: 'card stack' },
      el('button', { class: 'ghost wide danger', onclick: onLock }, t('action_lock_app'))) : null,
    // Same key + format Android's Settings footer uses ("fortis 0.4.0"). The
    // human-maintained web/VERSION (bumped by hand, see RELEASING.md-style
    // discipline) is the friendly number; the commit alongside it is what's
    // actually authoritative, since VERSION can go stale if forgotten to bump
    // and the commit never can. Falls back to the commit alone against an
    // older build-info.json (from before VERSION existed) that has no
    // `version` field. Omitted entirely until the fetch resolves.
    ui.buildInfo?.commit
      ? el('div', { class: 'hint', style: 'text-align:center' },
          t('settings_version', ui.buildInfo.version ? `${ui.buildInfo.version} (${ui.buildInfo.commit})` : ui.buildInfo.commit))
      : null);
}

/** Fetches `build-info.json` once (git commit + a SHA-256 hash per served
 *  file, written by deploy/build-web-stage.mjs at publish time — see
 *  renderVerifyCard()) and re-renders when it resolves. Absent on a local
 *  checkout that never ran the publish script, or a build from before this
 *  existed — treated as "not available", not an error: `ui.buildInfo` ends
 *  up `false` either way, distinct from `null` (still loading) and
 *  `undefined` (never asked). */
function loadBuildInfo() {
  if (ui.buildInfo !== undefined) return;
  ui.buildInfo = null;
  fetch('./build-info.json')
    .then((r) => (r.ok ? r.json() : null))
    .catch(() => null)
    .then((info) => { ui.buildInfo = info || false; render(); });
}

/** "Verify this build": answers the fair criticism that a website, unlike
 *  the signed Android APK, has no code-signing a user can check — this is
 *  as close as a static site can get, plus the one mitigation nothing here
 *  can provide (a hostile browser extension), stated plainly instead of
 *  pretended away. */
function renderVerifyCard() {
  loadBuildInfo();
  const info = ui.buildInfo;
  return el('div', { class: 'card stack' },
    el('h2', {}, t('settings_verify_title')),
    el('div', { class: 'hint' }, t('settings_verify_body')),
    info
      ? el('div', { class: 'stack' },
          el('div', { class: 'hint' },
            t('settings_verify_commit', info.commit) + (info.dirty ? ` ${t('settings_verify_dirty')}` : '')),
          el('a', { href: './build-info.json', target: '_blank', rel: 'noopener' }, t('settings_verify_link')))
      : el('div', { class: 'hint' }, t('settings_verify_unavailable')),
    el('div', { class: 'hint' }, t('settings_dedicated_profile')));
}

function renderWalletCard(w) {
  const otherChain = w.chain === 'xbt' ? 'btc' : 'xbt';
  const canClone = state.wallets.length < MAX_WALLETS
    && !state.wallets.some((x) => x.chain === otherChain && x.name === w.name);
  const connLabel = !w.backend ? '—'
    : w.backend.kind === 'edge' ? 'fortis'
    : w.backend.kind === 'esplora' ? new URL(w.backend.url).host
    : t('wallet_via_node');
  return el('div', { class: 'card stack wallet-card' },
    el('div', { class: 'row', style: 'align-items:center' },
      el('b', {}, w.name), el('span', { class: 'badge' }, UNIT[w.chain]),
      w.watchOnly ? el('span', { class: 'badge' }, t('watch_badge')) : null),
    el('div', { class: 'hint' }, `${t('settings_connection')}: ${connLabel}`),
    el('div', { class: 'actions-wrap' },
      el('button', { onclick: () => actionRename(w.id) }, t('action_rename')),
      el('button', { onclick: () => actionCopyXpub(w.id) }, t('action_copy_xpub')),
      w.watchOnly ? null : el('button', { onclick: () => actionCopyFingerprint(w.id) }, t('action_copy_fingerprint')),
      w.watchOnly ? null : el('button', { onclick: () => actionReveal(w.id) }, t('action_recovery_phrase')),
      w.watchOnly ? null : el('button', { onclick: () => actionImportSignOffline(w.id) }, t('action_import_sign_offline')),
      el('button', { onclick: () => actionChangeBackend(w.id) }, t('action_reconnect')),
      canClone ? el('button', { onclick: () => actionClone(w.id) }, t('also_add_on', UNIT[otherChain])) : null,
      el('button', { class: 'danger', onclick: () => actionRemove(w.id) }, t('action_remove'))));
}

function actionRename(id) {
  const w = state.wallets.find((x) => x.id === id);
  ui.dialog = {
    kind: 'prompt', title: t('dialog_rename_title'), value: w.name, confirmLabel: t('action_save'),
    maxlength: MAX_WALLET_NAME_LEN,
    onConfirm: (name) => {
      name = (name || '').trim();
      if (!name) return;
      w.name = name;
      saveState(state);
      render();
    },
  };
  renderDialogSheet();
}

function actionRemove(id) {
  const w = state.wallets.find((x) => x.id === id);
  ui.dialog = {
    kind: 'confirm', title: t('dialog_remove_title', w.name), body: t('dialog_remove_body'),
    confirmLabel: t('action_remove'), danger: true,
    onConfirm: () => {
      sessions.get(id)?.free();
      sessions.delete(id);
      backends.delete(id);
      overview.balances.delete(id);
      state.wallets = state.wallets.filter((x) => x.id !== id);
      if (state.selected === id) { state.selected = state.wallets[0]?.id || null; resetDetail(); }
      saveState(state);
      render();
    },
  };
  renderDialogSheet();
}

function actionClone(id) {
  const w = state.wallets.find((x) => x.id === id);
  const otherChain = w.chain === 'xbt' ? 'btc' : 'xbt';
  if (state.wallets.length >= MAX_WALLETS) return toast(t('error_wallet_max', MAX_WALLETS));
  if (state.wallets.some((x) => x.chain === otherChain && x.name === w.name)) {
    return toast(t('error_clone_exists', w.name, UNIT[otherChain]));
  }
  ui.dialog = {
    kind: 'confirm', title: `${t('also_add_on', UNIT[otherChain])}?`, confirmLabel: t('action_continue'),
    onConfirm: () => {
      const id2 = crypto.randomUUID();
      state.wallets.push(w.watchOnly ? {
        id: id2, name: w.name, chain: otherChain, network: w.network, watchOnly: true, xpub: w.xpub,
        fingerprint: w.fingerprint || null,
        next_receive: 0, next_change: 0,
        backend: w.backend?.kind === 'edge' ? { ...w.backend } : null,
      } : {
        id: id2, name: w.name, chain: otherChain, network: w.network, sealed: w.sealed, salt: w.salt,
        next_receive: 0, next_change: 0,
        backend: w.backend?.kind === 'edge' ? { ...w.backend } : null,
      });
      state.selected = id2;
      resetDetail();
      saveState(state);
      render();
    },
  };
  renderDialogSheet();
}

async function actionChangeBackend(id) {
  const w = state.wallets.find((x) => x.id === id);
  backends.delete(id);
  await autoConnectBackend(w);
  await saveState(state);
  render();
  toast(t('toast_reconnected'));
}

function actionCopyXpub(id) {
  const w = state.wallets.find((x) => x.id === id);
  try {
    copy(ensureSession(w).xpub);
  } catch {
    toast(t('xpub_loading'));
  }
}

/** Master fingerprint (8 hex chars) — what a watch-only import needs, in
 *  addition to the xpub, to later export a transaction for this wallet to
 *  sign offline. */
function actionCopyFingerprint(id) {
  const w = state.wallets.find((x) => x.id === id);
  try {
    copy(ensureSession(w).fingerprint);
  } catch {
    toast(t('xpub_loading'));
  }
}

function actionImportSignOffline(id) {
  ui.dialog = { kind: 'psbt-paste', walletId: id };
  renderDialogSheet();
}

function actionReveal(id) {
  const w = state.wallets.find((x) => x.id === id);
  ui.dialog = {
    kind: 'confirm', title: t('dialog_reveal_title', w.name), body: t('dialog_reveal_body_web'),
    confirmLabel: t('action_view'),
    onConfirm: () => {
      try {
        const { mnemonic, passphrase } = unsealWithAppSecret(w.sealed, w.salt, appSecret);
        ui.dialog = { kind: 'reveal', words: mnemonic.split(/\s+/), passphrase };
        renderDialogSheet();
      } catch {
        toast(t('reveal_failed'));
      }
    },
  };
  renderDialogSheet();
}

async function actionEnableQuickUnlock() {
  const reg = await registerPrf();
  if (!reg) return toast(t('prf_not_supported'));
  const { wrapped } = wrapAppSecretWithKey(appSecret, reg.secret);
  state.lock.prf = { credentialId: reg.credentialId, prfSalt: reg.prfSalt, wrapped };
  await saveState(state);
  render();
}

function actionDisableQuickUnlock() {
  state.lock.prf = null;
  saveState(state);
  render();
}

function closeDialog() {
  ui.dialog = null;
  render();
}

function renderDialogSheet() {
  if (!ui.dialog) return;
  if (ui.dialog.kind === 'reveal') return renderRevealSheet();
  if (ui.dialog.kind === 'change-password') return renderChangePasswordSheet();
  if (ui.dialog.kind === 'locale') return renderLocaleSheet();
  if (ui.dialog.kind === 'confirm') return renderConfirmSheet();
  if (ui.dialog.kind === 'prompt') return renderPromptSheet();
  if (ui.dialog.kind === 'export-blob') return renderExportBlobSheet();
  if (ui.dialog.kind === 'import-signed') return renderImportSignedSheet();
  if (ui.dialog.kind === 'psbt-paste') return renderPsbtPasteSheet();
  if (ui.dialog.kind === 'psbt-review') return renderPsbtReviewSheet();
}

/** Generic yes/no sheet — replaces native confirm() so it matches the rest of
 *  the app instead of popping an unstyled browser dialog. */
function renderConfirmSheet() {
  const { title, body, confirmLabel, danger } = ui.dialog;
  const onYes = () => {
    const { onConfirm } = ui.dialog;
    closeDialog();
    onConfirm();
  };
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, title),
      body ? el('p', {}, body) : null,
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: closeDialog }, t('action_cancel')),
        el('button', { class: danger ? 'danger' : 'primary', onclick: onYes }, confirmLabel || t('action_continue'))))));
}

/** Generic single-text-field sheet — replaces native prompt(). */
function renderPromptSheet() {
  const { title, label, value, confirmLabel, maxlength } = ui.dialog;
  const onYes = () => {
    const { onConfirm } = ui.dialog;
    const v = val('prompt-input');
    closeDialog();
    onConfirm(v);
  };
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, title),
      label ? el('label', {}, label) : null,
      el('input', { id: 'prompt-input', value: value || '', autocomplete: 'off', maxlength,
        onkeydown: (e) => e.key === 'Enter' && onYes() }),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: closeDialog }, t('action_cancel')),
        el('button', { class: 'primary', onclick: onYes }, confirmLabel || t('action_save'))))));
}

function renderRevealSheet() {
  const { words, passphrase } = ui.dialog;
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t('reveal_title')),
      el('p', { class: 'hint' }, t('reveal_warning_web')),
      el('ol', { class: 'words card' }, words.map((wd, i) => el('li', {}, el('b', {}, i + 1), wd))),
      el('label', {}, t('reveal_passphrase_label')),
      el('div', { class: 'addr-box mono' }, passphrase || t('reveal_no_passphrase')),
      el('div', { class: 'row' },
        el('button', { class: 'primary wide', onclick: closeDialog }, t('action_done'))))));
}

/** Shared by both legs of an air-gapped signing round trip — an unsigned
 *  PSBT (watch-only side, exporting) and a finalized signed transaction
 *  (signing side, exporting back) are both just "here's a blob, get it to
 *  the other device": QR when it fits one frame, and — always, regardless of
 *  size — the same `.addr-box.mono` copy-box pattern the recovery-phrase
 *  reveal screen already uses, so this never depends on the QR working. */
function renderExportBlobSheet() {
  const { title, body, text } = ui.dialog;
  const qr = qrCodeOrNull(text);
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, title),
      body ? el('p', { class: 'hint' }, body) : null,
      qr || el('p', { class: 'hint' }, t('export_psbt_too_large_for_qr')),
      el('div', { class: 'addr-box mono', style: 'word-break:break-all;max-height:9rem;overflow:auto' }, text),
      el('div', { class: 'row' },
        el('button', { onclick: () => copy(text) }, t('action_copy')),
        el('button', { class: 'primary', onclick: closeDialog }, t('action_done'))))));
}

/** The watch-only side's other half of the round trip: paste back whatever
 *  the offline device produced and broadcast it through the normal pipeline
 *  — the offline side already finalized it, so this is exactly the same
 *  hex onSend() broadcasts, just arriving by hand instead of from sign(). */
function renderImportSignedSheet() {
  const { walletId } = ui.dialog;
  const submit = async () => {
    const err = document.getElementById('err');
    const btn = document.getElementById('broadcastBtn');
    const hex = val('signed-hex').trim();
    if (!hex) { err.textContent = t('error_paste_signed_tx'); return; }
    btn.disabled = true;
    err.textContent = '…';
    try {
      const w = state.wallets.find((x) => x.id === walletId);
      const backend = ensureBackend(w);
      const { txid } = await backend.broadcast(hex);
      closeDialog();
      ui.tab = 'history';
      toast(`${t('sent_title')} · ${shortTxid(txid)}`);
      await refresh();
      render();
      pollForTxid(w.id, txid);
    } catch (e) {
      btn.disabled = false;
      err.textContent = String(e.message || e).replace(/^.*?: /, '');
    }
  };
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t('import_signed_title')),
      el('p', { class: 'hint' }, t('import_signed_body')),
      el('textarea', { id: 'signed-hex', rows: 4, autocapitalize: 'none', autocomplete: 'off', spellcheck: 'false' }),
      el('div', { id: 'err', class: 'err' }),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: closeDialog }, t('action_cancel')),
        el('button', { class: 'primary', id: 'broadcastBtn', onclick: submit }, t('action_broadcast'))))));
}

/** The signing side's entry point: paste an unsigned PSBT built by the
 *  watch-only side and review it — 100% local (see
 *  wallet-core::psbt::review_unsigned_psbt's doc), safe while genuinely
 *  offline. `navigator.onLine` is deliberately *not* checked anywhere in
 *  this flow — it's spoofable/unreliable, so the advisory below just says
 *  so plainly instead of pretending to enforce it. */
function renderPsbtPasteSheet() {
  const { walletId } = ui.dialog;
  const submit = () => {
    const err = document.getElementById('err');
    const text = val('psbt-input').trim();
    if (!text) { err.textContent = t('error_paste_psbt'); return; }
    err.textContent = '…';
    try {
      const w = state.wallets.find((x) => x.id === walletId);
      const session = ensureSession(w);
      const review = session.reviewImportedPsbt(text);
      ui.dialog = { kind: 'psbt-review', review, psbtText: text, walletId };
      renderDialogSheet();
    } catch (e) {
      err.textContent = String(e.message || e).replace(/^.*?: /, '');
    }
  };
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t('psbt_paste_title')),
      el('p', { class: 'hint' }, t('psbt_paste_body')),
      el('p', { class: 'hint' }, t('offline_advisory_note')),
      el('textarea', { id: 'psbt-input', rows: 4, autocapitalize: 'none', autocomplete: 'off', spellcheck: 'false' }),
      el('div', { id: 'err', class: 'err' }),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: closeDialog }, t('action_cancel')),
        el('button', { class: 'primary', onclick: submit }, t('action_review'))))));
}

/** Everything shown here comes from `PsbtReview` — built entirely from what's
 *  embedded in the PSBT itself (wallet-core's `attribute()`), never from
 *  anything the online side merely claims. Same `.kvs`/`.kv` row shape as
 *  the live confirm screen. */
function renderPsbtReviewSheet() {
  const { review, psbtText, walletId } = ui.dialog;
  const w = state.wallets.find((x) => x.id === walletId);
  const unit = UNIT[w.chain];
  const totalOut = review.destinations.reduce((s, d) => s + Number(d.amount_sat), 0);
  const doSign = () => {
    const err = document.getElementById('err');
    try {
      const session = ensureSession(w);
      const signedHex = session.signImportedPsbt(psbtText);
      ui.dialog = {
        kind: 'export-blob',
        title: t('signed_export_title'),
        body: t('signed_export_body'),
        text: signedHex,
      };
      renderDialogSheet();
    } catch (e) {
      err.textContent = String(e.message || e).replace(/^.*?: /, '');
    }
  };
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t('psbt_review_signing_for', `${unit} · ${w.name}`)),
      el('div', { class: 'kvs' },
        ...review.destinations.map((d) =>
          row(el('span', { class: 'mono', style: 'word-break:break-all' }, d.address || d.script_pubkey_hex),
            `${fmt(d.amount_sat)} ${unit}`)),
        row(t('confirm_network_fee'), `${fmt(review.fee_sat)} ${unit}`),
        review.change_sat != null ? row(t('confirm_change'), `${fmt(review.change_sat)} ${unit}`) : null,
        row(t('confirm_inputs'), tPlural('confirm_inputs_value', review.selected.length)),
        review.op_return_hex ? row(t('confirm_replay'), t('confirm_replay_value')) : null,
        row(t('confirm_total'), el('b', {}, `${fmt(totalOut)} ${unit}`))),
      el('div', { id: 'err', class: 'err' }),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: closeDialog }, t('action_cancel')),
        el('button', { class: 'primary', onclick: doSign }, t('action_sign'))))));
}

function renderChangePasswordSheet() {
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t('dialog_change_password_title')),
      el('label', {}, t('field_current_password')),
      el('input', { id: 'cur', type: 'password', autocomplete: 'current-password' }),
      el('label', {}, t('field_new_password')),
      el('input', { id: 'new1', type: 'password', autocomplete: 'new-password' }),
      el('label', {}, t('field_confirm_password')),
      el('input', { id: 'new2', type: 'password', autocomplete: 'new-password' }),
      el('div', { id: 'err', class: 'err' }),
      el('div', { class: 'row' },
        el('button', { class: 'ghost', onclick: closeDialog }, t('action_cancel')),
        el('button', { class: 'primary', onclick: onChangePassword }, t('action_save'))))));
}

async function onChangePassword() {
  const err = document.getElementById('err');
  const cur = val('cur');
  const n1 = val('new1');
  const n2 = val('new2');
  if (n1.length < 8) return (err.textContent = t('lock_password_hint', 8));
  if (n1 !== n2) return (err.textContent = t('lock_password_mismatch'));
  try {
    const secret = unwrapAppSecretWithPassword(state.lock.password.wrapped, state.lock.password.salt, cur);
    state.lock.password = wrapAppSecretWithPassword(secret, n1);
    await saveState(state);
    toast(t('toast_saved'));
    closeDialog();
  } catch {
    err.textContent = t('error_wrong_password');
  }
}

function renderLocaleSheet() {
  const cur = overrideTag();
  const tags = [...SUPPORTED_LOCALES].sort((a, b) => localeLabel(a).localeCompare(localeLabel(b)));
  const pick = async (tag) => { await setLocale(tag); closeDialog(); };
  mount(el('div', { class: 'sheet-wrap' },
    el('div', { class: 'scrim', onclick: closeDialog }),
    el('div', { class: 'sheet' },
      el('div', { class: 'grabber' }),
      el('h2', {}, t('settings_language')),
      el('div', { class: 'locale-list' }, [
        el('button', { class: !cur ? 'on' : '', onclick: () => pick(null) }, t('language_system_default')),
        ...tags.map((tag) => el('button', { class: cur === tag ? 'on' : '', onclick: () => pick(tag) }, localeLabel(tag))),
      ]))));
}

/* ------------------------------------------------------------------ polling */

async function refresh() {
  // The interval in startPolling() has no memory of whether the last tick's
  // refresh() ever finished — without this guard, one slow/stuck backend call
  // (see the AbortSignal.timeout note in esplora.js's _fetch) means every
  // subsequent 15-25s tick piles another overlapping call on top instead of
  // waiting, accumulating in-flight requests for as long as the tab stays open.
  if (detailScanning) return;
  detailScanning = true;
  let refreshingFor;
  try {
    const w = currentWallet();
    if (!w) return;
    refreshingFor = w.id;
    const backend = ensureBackend(w);
    if (!backend) return;
    await backend.prewarm?.();

    // status/history first, committed and painted immediately — history()
    // only needs the cache-seeded watch-set (fast on a warm reopen);
    // balances() additionally needs a live per-address UTXO fetch that's
    // never cached (see esplora.js). Splitting this from the balances
    // stage below means a warm reopen's history actually shows as soon
    // as it's ready instead of sitting fetched-but-unseen behind the
    // slow live balance scan — same reasoning, same fix shape, as the
    // Android port of this class.
    const [status, history] = await Promise.all([
      backend.status(),
      backend.history(50).catch(() => []),
    ]);
    if (state.selected !== refreshingFor) return;
    detail.status = status;
    detail.history = history || [];
    renderShellIfIdle();

    const [balances, usd] = await Promise.all([
      backend.balances().catch(() => null),
      backend.price ? backend.price().catch(() => null) : Promise.resolve(null),
    ]);
    // The user may have switched wallets while the awaits above were
    // in flight — `state.selected` would no longer be `refreshingFor`.
    // Committing this call's results to the shared `detail` object at
    // that point would show the *previous* wallet's data under the
    // *new* one: found live, a slow BTC scan finishing after switching
    // to an XBT wallet displayed the BTC balance/history there until
    // the next poll tick corrected it. Discard silently instead — the
    // new selection's own refresh() owns `detail` now.
    if (state.selected !== refreshingFor) return;
    if (balances) detail.balances = balances;
    if (usd != null) { detail.usd = usd; overview.usd[w.chain] = usd; }
    if (detail.balances) overview.balances.set(w.id, detail.balances.confirmed_sat);
    if (!Object.keys(detail.feerate).length) {
      for (const target of [1, 6, 144]) {
        backend.feerate(target).then((r) => {
          if (state.selected !== refreshingFor) return;
          detail.feerate[target] = r.sat_vb;
          renderShellIfIdle();
        }).catch(() => {});
      }
    }
  } catch {
    if (state.selected === refreshingFor) detail.status = null;
  } finally {
    detailScanning = false;
  }
}

async function refreshAllBalances() {
  if (overviewScanning) return;
  overviewScanning = true;
  const priced = new Set();
  try {
    for (const w of state.wallets) {
      try {
        const backend = ensureBackend(w);
        if (!backend) continue;
        await backend.prewarm?.();
        const bal = await backend.balances();
        overview.balances.set(w.id, bal.confirmed_sat);
        if (!priced.has(w.chain) && backend.price) {
          priced.add(w.chain);
          const p = await backend.price();
          if (p != null) overview.usd[w.chain] = p;
        }
      } catch {
        /* keep this wallet's last cached balance */
      }
    }
  } finally {
    overviewScanning = false;
  }
  renderShellIfIdle();
}

function startPolling() {
  if (ui.nav === 'wallet') {
    if (detailPoll) return;
    // Self-rescheduling rather than a fixed setInterval so the cadence can
    // react to the last result: still on the yellow "connecting…" dot
    // (detail.status not yet populated) -> retry every few seconds on this
    // async, non-blocking loop instead of waiting out a full normal-cadence
    // tick; once actually connected, back off to the normal interval.
    const tick = async () => {
      await refresh();
      renderShellIfIdle();
      const every = !detail.status ? 3_000
        : currentWallet()?.backend?.kind === 'gateway' ? 15_000 : 25_000;
      detailPoll = setTimeout(tick, every);
    };
    tick();
  } else {
    if (overviewPoll) return;
    refreshAllBalances();
    overviewPoll = setInterval(refreshAllBalances, 45_000);
  }
}
function stopPolling() {
  if (detailPoll) clearTimeout(detailPoll);
  detailPoll = null;
  if (overviewPoll) clearInterval(overviewPoll);
  overviewPoll = null;
}

/* ------------------------------------------------------------------ misc */

function go(screen) {
  ui.screen = screen;
  render();
}
function val(id) {
  return (document.getElementById(id)?.value ?? '').trim();
}

/* ------------------------------------------------------------------ PWA updates */

/** A new worker sitting in `reg.waiting` means a fresher build already
 *  downloaded in the background — it's just holding off activating so it
 *  doesn't swap the running code out from under this tab uninvited (see
 *  sw.js). Surface it instead of staying silent: this is a wallet, and
 *  "the code changed under you mid-session" is worth a heads-up, not
 *  something to spring silently. */
function offerUpdate(reg) {
  if (document.getElementById('update-bar')) return;
  const bar = el('div', { id: 'update-bar', class: 'update-bar' },
    el('span', {}, t('update_available')),
    el('div', { class: 'row', style: 'flex:0' },
      el('button', { class: 'ghost', onclick: () => bar.remove() }, t('action_dismiss')),
      el('button', { class: 'primary', onclick: () => reg.waiting.postMessage('SKIP_WAITING') }, t('action_refresh'))));
  document.body.append(bar);
}

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('./sw.js').then((reg) => {
    if (reg.waiting && navigator.serviceWorker.controller) offerUpdate(reg);
    reg.addEventListener('updatefound', () => {
      const worker = reg.installing;
      if (!worker) return;
      worker.addEventListener('statechange', () => {
        // "installed" while something already controls the page means this
        // is an update, not the first-ever install — a first install has no
        // controller yet, and never has anything to offer switching from.
        if (worker.state === 'installed' && navigator.serviceWorker.controller) offerUpdate(reg);
      });
    });
  }).catch(() => {});

  let reloadedForUpdate = false;
  navigator.serviceWorker.addEventListener('controllerchange', () => {
    if (reloadedForUpdate) return;
    reloadedForUpdate = true;
    location.reload();
  });
}
