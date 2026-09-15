// Esplora REST backend (blockstream.info / mempool.guide) — no node, no gateway.
//
// Esplora is address-based, so the wallet derives its own addresses (via the wasm
// `Session`) and this scans them with a gap limit. Same method surface as
// `Gateway`, so `app.js` treats the two interchangeably.

import { loadAddrTxs, saveAddrTxs } from './store.js';

const GAP = 20; // stop scanning a branch after this many consecutive unused
const CHUNK = 8; // parallel requests per batch
// How deep _guessWatchAddresses()'s prewarm guess goes per branch. 500×2
// branches = 1000, matching the edge's own /btc/prewarm address cap exactly.
const PREWARM_DEPTH = 500;
// Absolute backstop on how far a single gap-limit walk will go per branch —
// not a limit any real wallet should hit (the walk already stops itself after
// GAP consecutive never-used addresses); just a bound on worst-case work
// against a pathological xpub.
const WATCH_SET_HARD_CAP = 2_000;
// The gap-limit scan can run long on a heavily-used wallet against a public
// explorer (not the edge, which has its own prewarm path) — it now genuinely
// walks as far as real activity goes (see `_watchSet` below), which for a
// freshly imported deep-history xpub can be a lot of requests. 60s reuses one
// scan across several poll ticks instead of re-running it every 15-25s;
// utxos() still forces a fresh scan (see `_refresh(true)` below) since
// spend-planning can't work from a stale UTXO set.
const SNAP_TTL = 60_000; // ms — reuse the UTXO scan within a poll burst
const HIST_TTL = 60_000;

/** Just what `history()` needs from one `/txs` entry — not the raw object.
 *  A real, heavily-automated wallet found live, 2026-09-15, kept several
 *  hundred used addresses (still climbing past 300 when this was caught)
 *  with many-input/many-output transactions; the Android port of this same
 *  class cached the *raw* parsed tx (full scriptpubkey/witness hex for every
 *  vin and vout) per address and crashed with a genuine OutOfMemoryError
 *  mid-scan once enough addresses piled up. Browsers have a much larger heap
 *  than a phone, but the waste is the same — keep only the txid, fee,
 *  confirmation status, and each vin/vout's (address, value) pair. */
function summarizeTx(tx) {
  return {
    txid: tx.txid,
    fee: tx.fee || 0,
    confirmed: !!tx.status?.confirmed,
    block_height: tx.status?.block_height || 0,
    block_time: tx.status?.block_time || 0,
    vin: (tx.vin || []).map((v) => [v.prevout?.scriptpubkey_address ?? null, v.prevout?.value ?? 0]),
    vout: (tx.vout || []).map((o) => [o.scriptpubkey_address ?? null, o.value ?? 0]),
  };
}

async function chunked(items, n, fn) {
  const out = [];
  for (let i = 0; i < items.length; i += n) {
    out.push(...(await Promise.all(items.slice(i, i + n).map(fn))));
  }
  return out;
}

/** Mint a per-install token at a fortis-edge base URL (`POST {base}/register`). */
export async function edgeRegister(base) {
  // A bounded timeout matters here specifically because auto-connect (see
  // app.js) awaits this during onboarding with no user-visible cancel — a
  // hung request (not even a failure, just slow) would otherwise strand the
  // user on the lock-setup screen forever instead of falling back to the
  // manual backend picker.
  const r = await fetch(String(base).replace(/\/+$/, '') + '/register', { method: 'POST', signal: AbortSignal.timeout(8000) });
  const t = (await r.json())?.token;
  if (!r.ok || !t) throw new Error(`register failed at ${base} (${r.status})`);
  return t;
}

export class EsploraBackend {
  // `auth` (optional): `{ token, refresh: async () => newToken }` for a
  // fortis-edge that requires `Authorization: Bearer`. A 401 triggers one
  // `refresh()` + retry.
  constructor(url, session, network, auth, pricingUrl) {
    this.kind = auth ? 'edge' : 'esplora';
    this.base = String(url || '').replace(/\/+$/, '');
    this.session = session;
    this.network = network || 'mainnet';
    this.auth = auth || null;
    // `{edge}/pricing`; when it advertises a service fee, status() surfaces it so
    // a send attaches the fee output. Absent on the public-explorer fallback.
    this.pricingUrl = pricingUrl || null;
    this._pricing = null; // null = not fetched, false = none, object = fee
    this._snap = null;
    this._snapAt = 0;
    this._refreshing = null; // in-flight _refresh() promise, so concurrent callers share it
    this._hist = null;
    this._histAt = 0;
    this._fees = null;
    this._feesLoading = null;
    this._price = null; // last-known-good USD price, or null before any success
    this._priceAt = 0;
    this._lastPrewarm = 0;
    this._watchSetCache = null;
    this._watchSetCacheAt = 0;
  }

  /** One POST of the whole watch-set to `{base}/prewarm` — the hosted edge's
   *  batch BTC path (Haskoin, two calls) instead of the paced per-address
   *  proxy fanned out to the configured `--btc-upstream` (`--btc-upstream-rate`, there to keep
   *  a ~40-address gap-limit scan from getting the edge 429'd). Without this
   *  a fresh BTC wallet's first scan is genuinely slow — tens of seconds, not
   *  the ~2s an XBT wallet gets from fortis-index directly. Only the hosted
   *  BTC backend has this route (XBT/esplora-fallback both 404 harmlessly via
   *  the outer `kind`/chain check, never even try); best-effort and silently
   *  ignored on failure — the scan just falls back to the slow path exactly
   *  as it did before this existed. Throttled to sit just inside the edge's
   *  own 60s cache TTL, mirroring the Android client's `prewarm()`. */
  async prewarm() {
    if (this.kind !== 'edge' || this.session.chain !== 'btc') return;
    const now = Date.now();
    if (now - this._lastPrewarm < 45_000) return;
    try {
      const addrs = this._guessWatchAddresses();
      const r = await this._fetch('/prewarm', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(addrs),
      });
      if (r.ok) this._lastPrewarm = now;
    } catch {
      /* best-effort */
    }
  }

  /** Approximate USD spot price for this chain, or `null` (no feed — either a
   *  non-`edge` backend, which has no `/v1/prices` route, or a transient
   *  failure, in which case the last-known-good value is kept). Client-cached
   *  ~120s on top of the edge's own 60s cache. */
  async price() {
    if (this.kind !== 'edge') return null;
    if (this._price != null && Date.now() - this._priceAt < 120_000) return this._price;
    try {
      const r = await this.get('/v1/prices');
      const v = Number(r?.USD);
      if (isFinite(v) && v > 0) {
        this._price = v;
        this._priceAt = Date.now();
      }
    } catch {
      /* keep the last-known-good value, if any */
    }
    return this._price;
  }

  async _loadPricing() {
    if (this._pricing !== null || !this.pricingUrl) return;
    try {
      const r = await fetch(this.pricingUrl, { signal: AbortSignal.timeout(10_000) });
      this._pricing = r.ok ? await r.json() : false;
    } catch {
      this._pricing = false;
    }
  }

  async _fetch(path, init = {}) {
    const withAuth = () => ({
      ...init,
      // Every call here — balances, history, fees, broadcast, the address-gap
      // scan — went out with no bound on how long it could hang. A stalled
      // upstream (this class's whole reason to exist is talking to
      // third-party explorers, which do go slow or wedge) doesn't just fail
      // that one call: refreshAllBalances()'s reentrancy guard never clears
      // because the await it's blocked on never settles, silently freezing
      // balance polling for every wallet, and the per-wallet detail poll has
      // no such guard at all, so it piles up a fresh hung request every
      // interval instead. This is the same failure shape that took down
      // fortis-edge's own price fetch in production — same fix.
      signal: AbortSignal.timeout(10_000),
      headers: { ...(init.headers || {}), ...(this.auth?.token ? { authorization: 'Bearer ' + this.auth.token } : {}) },
    });
    let r;
    try {
      r = await fetch(this.base + path, withAuth());
    } catch {
      throw new Error(`cannot reach the explorer at ${this.base}`);
    }
    if (r.status === 401 && this.auth?.refresh) {
      this.auth.token = await this.auth.refresh();
      r = await fetch(this.base + path, withAuth());
    }
    return r;
  }

  /** A wallet scan is dozens of these, CHUNK at a time — public explorers (the
   *  no-auth fallback especially, straight to mempool.space/mempool.guide with
   *  no server-side pacing) throw an occasional `429`/`5xx` under that
   *  fan-out. Retry those a couple of times with backoff before giving up,
   *  same shape as fortis-edge's own retry against its upstream, so a
   *  transient blip resolves here instead of failing `_watchSet()`/`_refresh()`
   *  outright — confirmed live: a plain sequential walk against mempool.space
   *  started 429ing by request ~28 with zero retry. */
  async get(path) {
    let lastErr;
    for (let attempt = 0; attempt <= 2; attempt++) {
      if (attempt > 0) await new Promise((res) => setTimeout(res, 200 * attempt));
      const r = await this._fetch(path);
      const text = await r.text();
      if (r.ok) return text && text.trimStart()[0] !== '<' ? JSON.parse(text) : text;
      lastErr = new Error(`explorer ${r.status} on ${path}: ${text.slice(0, 120)}`);
      if (attempt === 2 || (r.status !== 429 && (r.status < 500 || r.status > 599))) throw lastErr;
    }
    throw lastErr;
  }

  // Interface parity with Gateway
  ping() {
    return this.get('/blocks/tip/height').then((h) => ({ tip: Number(h) }));
  }
  connect() {
    return Promise.resolve({ imported: true });
  }

  /** Cheap, no-network guess at the watch set: every address from 0 up to
   *  PREWARM_DEPTH on each branch, handed to the edge's batch Haskoin path
   *  so the real per-address walk that follows is served from cache instead
   *  of the slow, paced, one-at-a-time route this exists to avoid.
   *
   *  Used to be just `[from, from+GAP)` near the stored next-index — cheap,
   *  but only ever covered a wallet with a handful of used addresses. Found
   *  live, 2026-09-15: a real watch-only import with several hundred used
   *  addresses barely benefited from prewarm at all, since the guessed
   *  window covered a small fraction of what the walk actually needed.
   *  Deriving addresses is a local, no-network computation regardless of how
   *  many, so guessing wide costs nothing extra on this side; the edge's own
   *  `/btc/prewarm` cap and chunked batch calls bound the real cost. */
  _guessWatchAddresses() {
    const list = [];
    for (const branch of [0, 1]) {
      for (let i = 0; i < PREWARM_DEPTH; i++) list.push(this.session.addressAt(branch, i).address);
    }
    return list;
  }

  /** Every address worth checking right now, each carrying its `/txs`
   *  response (so `history()` never re-fetches what this walk already has).
   *
   *  For each branch, walks forward from index 0 in GAP-sized batches
   *  (fetched CHUNK at a time, in parallel), extending the window whenever
   *  the batch just checked had *any* address with transaction history — the
   *  standard BIP-44 gap-limit walk — instead of one fixed-size `[0, GAP)`
   *  pass. That fixed pass is wrong for a wallet with more than ~20 used
   *  receive or change addresses: everything past index ~20 was silently
   *  invisible — wrong balance, missing history, forever (nothing about the
   *  bug is self-correcting, since the window never had a reason to grow).
   *
   *  "Used" here means *ever* appeared in a transaction (`/txs` non-empty),
   *  not "currently has an unspent output" (`/utxo` non-empty) — an address
   *  that received funds and was later fully spent shows an *empty* `/utxo`
   *  response but is still a real, used address. Deciding gap continuation
   *  on `/utxo` alone stops the walk right after a run of spent-through
   *  addresses, before it ever reaches a live balance sitting past them.
   *
   *  Always starts at 0, never at `session.indices()`' stored next-index —
   *  that counter exists purely to pick which address the UI offers next for
   *  "Receive"; treating it as the scan floor drops every address below it
   *  from balance *and* history the moment it advances. Found live on
   *  Android's port of this same class: its gap-limit auto-advance persisted
   *  next-receive past dozens of addresses still holding real, unspent
   *  balance, and the next scan never looked at them again. Nothing stops a
   *  later deposit landing on an address below that counter either, so there
   *  is no index this can safely stop rechecking. */
  async _watchSet() {
    const now = Date.now();
    if (this._watchSetCache && now - this._watchSetCacheAt < SNAP_TTL) return this._watchSetCache;

    // Before ever doing a live walk (only reached once per instance, until
    // the first resolution — cache-seeded or live — sets `_watchSetCache`),
    // try painting from the permanent local cache instead: lets a cold page
    // load / wallet switch show last-known balance/history with zero network
    // calls. `_watchSetCacheAt` is deliberately left at 0 (not `now`) so the
    // very next call is treated as stale and does a real walk, which both
    // confirms the seed and persists anything new. Confirmed-tx history never
    // changes, but "is there anything new" still needs a live check
    // eventually — this only defers that, never skips it.
    if (!this._watchSetCache) {
      const persisted = await loadAddrTxs(this.session.chain).catch(() => ({}));
      const seed = [];
      for (const branch of [0, 1]) {
        for (let i = 0; i < PREWARM_DEPTH; i++) {
          const a = this.session.addressAt(branch, i);
          const txs = persisted[a.address];
          if (txs) seed.push({ address: a.address, spk: a.script_pubkey_hex, branch, index: i, txs, ok: true });
        }
      }
      if (seed.length) {
        this._watchSetCache = seed;
        return seed;
      }
    }

    const out = [];
    let anySuccess = false;
    for (const branch of [0, 1]) {
      let next = 0;
      let end = GAP;
      while (next < end && next < WATCH_SET_HARD_CAP) {
        const batchEnd = Math.min(end, next + CHUNK, WATCH_SET_HARD_CAP);
        const batch = await Promise.all(
          Array.from({ length: batchEnd - next }, (_, k) => next + k).map(async (i) => {
            const a = this.session.addressAt(branch, i);
            let ok = true;
            const txs = await this.get(`/address/${a.address}/txs`)
              .then((t) => { anySuccess = true; return t; })
              .catch(() => { ok = false; return []; });
            const summarized = (Array.isArray(txs) ? txs : []).map(summarizeTx);
            return { address: a.address, spk: a.script_pubkey_hex, branch, index: i, txs: summarized, ok };
          }),
        );
        for (const entry of batch) {
          out.push(entry);
          // A persistent failure (after get()'s own retries) must never be
          // treated as "confirmed unused" — extend the window defensively
          // instead of letting it shrink the "consecutive unused" runway.
          // Found live, 2026-09-15: this exact `.catch(() => [])` pattern, with
          // no `ok` distinction, silently capped a real ~300-address wallet's
          // walk partway through a spell of upstream 403s, undercounting its
          // balance with no error shown anywhere — same bug as Android's
          // `break`, just via "confirmed empty" instead of "stop".
          if (entry.txs.length > 0 || !entry.ok) end = Math.max(end, entry.index + 1 + GAP);
          // Write-through: persist this address's confirmed-only results
          // (never pending — those can still change or vanish) so the *next*
          // cold page load / wallet switch can paint from them instantly.
          // Fire-and-forget — an IndexedDB write has no business slowing the
          // scan down, and a failure here just means this address isn't
          // cached yet, not that the walk itself failed.
          if (entry.ok) {
            saveAddrTxs(this.session.chain, entry.address, entry.txs.filter((t) => t.confirmed)).catch(() => {});
          }
        }
        next = batchEnd;
      }
    }
    // Every probe failed — the upstream is unreachable right now, not "this
    // xpub genuinely has no history". Without this, balances()/history() both
    // resolve to a confident, wrong zero (every address's txs quietly became
    // `[]` via the .catch() above) instead of surfacing as the failure it is.
    // Same bug, same fix, as the Android port of this class.
    if (!anySuccess) throw new Error('no address probe succeeded — upstream unreachable');
    this._watchSetCache = out;
    this._watchSetCacheAt = now;
    return out;
  }

  async _refresh(force = false) {
    if (!force && this._snap && Date.now() - this._snapAt < SNAP_TTL) return this._snap;
    // status()/balances()/history() all call this via the same Promise.all in
    // app.js's refresh() — without this, each one independently sees no cache
    // yet and kicks off its own full tip+address-gap scan at the same instant,
    // three times the address-fanout traffic for one poll tick (this is what
    // was actually behind BTC wallets occasionally never leaving "connecting…"
    // — three concurrent identical /blocks/tip/height calls, some of which the
    // browser aborts outright). Any concurrent caller — forced or not — just
    // waits on whichever scan is already in flight instead of starting a new
    // one; `force` only skips the "cache is still fresh" shortcut above.
    if (this._refreshing) return this._refreshing;
    const run = (async () => {
      const addrs = await this._watchSet();
      const tip = Number(await this.get('/blocks/tip/height'));
      const perAddr = await chunked(addrs, CHUNK, async (a) => {
        const utxo = await this.get(`/address/${a.address}/utxo`).catch(() => []);
        return { a, utxo: Array.isArray(utxo) ? utxo : [] };
      });
      const utxos = perAddr.flatMap(({ a, utxo }) =>
        utxo.map((u) => ({
          txid: u.txid,
          vout: u.vout,
          value_sat: u.value,
          script_pubkey_hex: a.spk,
          confirmations: u.status?.confirmed ? Math.max(1, tip - u.status.block_height + 1) : 0,
          is_change: a.branch === 1,
          derivation_index: a.index,
        })),
      );
      this._snap = { tip, utxos, addrs };
      this._snapAt = Date.now();
      return this._snap;
    })();
    this._refreshing = run;
    try {
      return await run;
    } finally {
      if (this._refreshing === run) this._refreshing = null;
    }
  }

  async status() {
    const snap = await this._refresh();
    await this._loadPricing();
    return {
      node: {
        chain: this.network === 'regtest' ? 'regtest' : 'main',
        blocks: snap.tip,
        headers: snap.tip,
        progress: 1,
        ibd: false,
        pruned: false,
        blake2b_active: true,
        blake2b_height: null,
        subversion: 'esplora',
      },
      connected: true,
      scanning: null,
      pricing: this._pricing || null,
    };
  }

  async balances() {
    const { utxos } = await this._refresh();
    const sum = (f) => utxos.filter(f).reduce((s, u) => s + u.value_sat, 0);
    return {
      confirmed_sat: sum((u) => u.confirmations >= 1),
      pending_sat: sum((u) => u.confirmations < 1),
      immature_sat: 0,
    };
  }

  async utxos(minConf = 1) {
    const { utxos } = await this._refresh(true);
    return utxos.filter((u) => u.confirmations >= minConf);
  }

  /** Two incompatible conventions among Esplora-family APIs, and no way to
   *  know which one a given `--btc-upstream`/esplora-fallback host speaks
   *  without asking: mempool.space's family (including mempool.guide) uses
   *  `/v1/fees/recommended` → named tiers (`fastestFee`/`halfHourFee`/…);
   *  blockstream.info uses `/fee-estimates` → `{"<confTargetBlocks>":
   *  satPerVb, ...}`. Whichever answers first (not 404) is cached and used
   *  for the rest of this session. */
  async _loadFees() {
    const named = await this.get('/v1/fees/recommended').catch(() => null);
    if (named) return { kind: 'named', data: named };
    const byBlocks = await this.get('/fee-estimates').catch(() => null);
    if (byBlocks) return { kind: 'byBlocks', data: byBlocks };
    return null;
  }

  async feerate(confTarget = 6) {
    if (!this._fees) {
      // renderConfirm() calls this for 3 targets (1/6/144) at once — without
      // coalescing, each independently probes /v1/fees/recommended, all miss
      // the still-empty cache, and all three fall back to /fee-estimates too.
      if (!this._feesLoading) {
        this._feesLoading = this._loadFees().finally(() => { this._feesLoading = null; });
      }
      this._fees = await this._feesLoading;
    }
    if (this._fees?.kind === 'byBlocks') {
      const entries = Object.entries(this._fees.data)
        .map(([k, v]) => [Number(k), Number(v)])
        .filter(([k, v]) => Number.isFinite(k) && k > 0 && Number.isFinite(v))
        .sort((a, b) => a[0] - b[0]);
      // The largest listed block-target that's still <= what was asked for —
      // a tighter/faster estimate than requested is fine to reuse, a looser
      // one risks confirming later than the caller wanted. Falls back to the
      // fastest available estimate if even that isn't loose enough.
      let best = entries[0];
      for (const e of entries) {
        if (e[0] <= confTarget) best = e; else break;
      }
      return { sat_vb: Math.max(1, Math.round(best?.[1] || 1)) };
    }
    const f = this._fees?.data || {};
    const pick =
      confTarget <= 1 ? f.fastestFee : confTarget <= 6 ? f.halfHourFee : (f.economyFee ?? f.hourFee);
    return { sat_vb: Math.max(1, Math.round(pick || f.minimumFee || 1)) };
  }

  async history(count = 50) {
    const snap = await this._refresh();
    if (!this._hist || Date.now() - this._histAt > HIST_TTL) {
      const mine = new Set(snap.addrs.map((a) => a.address));
      const byTxid = new Map();
      for (const a of snap.addrs) {
        for (const tx of a.txs) {
          if (byTxid.has(tx.txid)) continue;
          const inOurs = tx.vin.reduce((s, [addr, value]) => s + (addr && mine.has(addr) ? value : 0), 0);
          const outOurs = tx.vout.reduce((s, [addr, value]) => s + (addr && mine.has(addr) ? value : 0), 0);
          const delta = outOurs - inOurs;
          const send = delta < 0;
          const counterparty = send
            ? tx.vout.find(([addr]) => addr && !mine.has(addr))?.[0]
            : tx.vout.find(([addr]) => addr && mine.has(addr))?.[0];
          byTxid.set(tx.txid, {
            txid: tx.txid,
            direction: send ? 'send' : 'receive',
            amount_sat: send ? delta + tx.fee : delta,
            fee_sat: send ? -tx.fee : 0,
            confirmations: tx.confirmed ? Math.max(1, snap.tip - tx.block_height + 1) : 0,
            time: tx.block_time || Math.floor(Date.now() / 1000),
            address: counterparty || null,
          });
        }
      }
      this._hist = [...byTxid.values()].sort((a, b) => b.time - a.time);
      this._histAt = Date.now();
    }
    return this._hist.slice(0, count);
  }

  async broadcast(hex) {
    const r = await this._fetch('/tx', {
      method: 'POST',
      headers: { 'content-type': 'text/plain' },
      body: hex,
    });
    const text = (await r.text()).trim();
    if (!r.ok) throw new Error(text || `explorer rejected the transaction (${r.status})`);
    if (!/^[0-9a-fA-F]{64}$/.test(text)) throw new Error(text || 'unexpected broadcast response');
    this._snap = null; // reflect the spend on the next poll
    this._hist = null;
    this._watchSetCache = null; // its cached /txs per address predates this broadcast
    return { txid: text.toLowerCase() };
  }
}
