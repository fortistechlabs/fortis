// IndexedDB store for the wallet config: an app-lock block (password wrap,
// optional WebAuthn-PRF wrap) plus a list of wallets (sealed seed + backend +
// address counters each). Nothing here is a secret except the `wrapped`/
// `sealed` blobs, which are encrypted.
//
// v1 (superseded) kept one record under key 'wallet': {chain, network,
// sealed, salt, next_receive, next_change, backend}. v2 introduces
// multi-wallet + an app-secret lock layer, under key 'wallets'. A v1 record
// is migrated by the caller (app.js drives the crypto — this module only
// knows storage), then deleted — never before the v2 write is confirmed.

const DB = 'fortis';
const STORE = 'kv';
const LEGACY_KEY = 'wallet';
const KEY = 'wallets';
// v2: adds ADDR_STORE, a permanent cache of confirmed transaction history per
// (chain, address) — see loadAddrTxs/saveAddrTxs. Confirmed transactions
// never change, so EsploraBackend seeds its watch-set from this instead of
// always starting cold on a page reload / wallet switch.
const ADDR_STORE = 'addrtxs';

export const MAX_WALLETS = 10;

function open() {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open(DB, 2);
    r.onupgradeneeded = () => {
      const db = r.result;
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
      if (!db.objectStoreNames.contains(ADDR_STORE)) db.createObjectStore(ADDR_STORE);
    };
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

async function tx(mode, fn) {
  const db = await open();
  return new Promise((resolve, reject) => {
    const store = db.transaction(STORE, mode).objectStore(STORE);
    const req = fn(store);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

/** Persisted confirmed-tx history for every address of `chain` this device
 *  has ever successfully scanned — `summarizeTx()`-shaped entries, keyed by
 *  address. Missing an address just means it's never been scanned before,
 *  same as any cache miss — callers still need a live check for it. */
export async function loadAddrTxs(chain) {
  const db = await open();
  return new Promise((resolve, reject) => {
    const store = db.transaction(ADDR_STORE, 'readonly').objectStore(ADDR_STORE);
    const range = IDBKeyRange.bound(`${chain}:`, `${chain}:￿`);
    const req = store.openCursor(range);
    const out = {};
    req.onsuccess = () => {
      const cursor = req.result;
      if (cursor) {
        out[cursor.key.slice(chain.length + 1)] = cursor.value;
        cursor.continue();
      } else {
        resolve(out);
      }
    };
    req.onerror = () => reject(req.error);
  });
}

/** Replace `address`'s whole confirmed-tx set — safe to call repeatedly, a
 *  gap-limit walk always hands over that address's complete current
 *  confirmed list, never a partial delta. Callers only ever pass confirmed
 *  entries; pending ones can still change, so they're never persisted. */
export async function saveAddrTxs(chain, address, txs) {
  const db = await open();
  return new Promise((resolve, reject) => {
    const req = db.transaction(ADDR_STORE, 'readwrite').objectStore(ADDR_STORE).put(txs, `${chain}:${address}`);
    req.onsuccess = () => resolve();
    req.onerror = () => reject(req.error);
  });
}

/** `{kind:'v2', state}` | `{kind:'legacy', record}` | `{kind:'empty'}`. */
export async function loadRaw() {
  const v2 = await tx('readonly', (s) => s.get(KEY));
  if (v2) return { kind: 'v2', state: v2 };
  const legacy = await tx('readonly', (s) => s.get(LEGACY_KEY));
  if (legacy) return { kind: 'legacy', record: legacy };
  return { kind: 'empty' };
}

export async function saveState(state) {
  return tx('readwrite', (s) => s.put(state, KEY));
}

/** Write the migrated v2 state, verify it landed, only then drop the legacy
 *  record — a crash between these steps just means migration retries next
 *  load, never data loss. */
export async function finishMigration(state) {
  await saveState(state);
  const check = await tx('readonly', (s) => s.get(KEY));
  if (!check) throw new Error('migration write failed');
  await tx('readwrite', (s) => s.delete(LEGACY_KEY));
}

export async function wipeState() {
  await tx('readwrite', (s) => s.delete(KEY));
  await tx('readwrite', (s) => s.delete(LEGACY_KEY));
}
