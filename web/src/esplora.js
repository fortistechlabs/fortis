// Esplora REST backend (blockstream.info / mempool.guide) — no node, no gateway.
//
// Esplora is address-based, so the wallet derives its own addresses (via the wasm
// `Session`) and this scans them with a gap limit. Same method surface as
// `Gateway`, so `app.js` treats the two interchangeably.

const GAP = 20; // stop scanning a branch after this many consecutive unused
const CHUNK = 8; // parallel requests per batch
// The full gap-limit scan is unbounded — it re-fans-out to next_receive/change
// + GAP addresses on every call with no cap on top of that, so a heavily-used
// wallet on a public explorer (not the edge, which has its own prewarm path)
// can generate a lot of parallel requests. 60s reuses one scan across several
// poll ticks instead of re-running it every 15-25s; utxos() still forces a
// fresh scan (see `_refresh(true)` below) since spend-planning can't work
// from a stale UTXO set.
const SNAP_TTL = 60_000; // ms — reuse the UTXO scan within a poll burst
const HIST_TTL = 60_000;

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
      const addrs = this._watchSet().map((a) => a.address);
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

  async get(path) {
    const r = await this._fetch(path);
    const text = await r.text();
    if (!r.ok) throw new Error(`explorer ${r.status} on ${path}: ${text.slice(0, 120)}`);
    return text && text.trimStart()[0] !== '<' ? JSON.parse(text) : text;
  }

  // Interface parity with Gateway
  ping() {
    return this.get('/blocks/tip/height').then((h) => ({ tip: Number(h) }));
  }
  connect() {
    return Promise.resolve({ imported: true });
  }

  /** Addresses to watch: receive + change branches, each 0..counter+GAP. */
  _watchSet() {
    const { next_receive, next_change } = this.session.indices();
    const list = [];
    for (const [branch, upto] of [[0, next_receive + GAP], [1, next_change + GAP]]) {
      for (let i = 0; i < upto; i++) {
        const a = this.session.addressAt(branch, i);
        list.push({ address: a.address, spk: a.script_pubkey_hex, branch, index: i });
      }
    }
    return list;
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
      const addrs = this._watchSet();
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
      const lists = await chunked(snap.addrs, CHUNK, (a) =>
        this.get(`/address/${a.address}/txs`).catch(() => []),
      );
      const byTxid = new Map();
      for (const list of lists) {
        for (const tx of list) {
          if (byTxid.has(tx.txid)) continue;
          const inOurs = (tx.vin || []).reduce(
            (s, v) => s + (mine.has(v.prevout?.scriptpubkey_address) ? v.prevout.value : 0),
            0,
          );
          const outs = tx.vout || [];
          const outOurs = outs.reduce(
            (s, o) => s + (mine.has(o.scriptpubkey_address) ? o.value : 0),
            0,
          );
          const delta = outOurs - inOurs;
          const send = delta < 0;
          const counterparty = send
            ? outs.find((o) => !mine.has(o.scriptpubkey_address))?.scriptpubkey_address
            : outs.find((o) => mine.has(o.scriptpubkey_address))?.scriptpubkey_address;
          byTxid.set(tx.txid, {
            txid: tx.txid,
            direction: send ? 'send' : 'receive',
            amount_sat: send ? delta + (tx.fee || 0) : delta,
            fee_sat: send ? -(tx.fee || 0) : 0,
            confirmations: tx.status?.confirmed
              ? Math.max(1, snap.tip - tx.status.block_height + 1)
              : 0,
            time: tx.status?.block_time || Math.floor(Date.now() / 1000),
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
    return { txid: text.toLowerCase() };
  }
}
