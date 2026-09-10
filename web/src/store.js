// One-record IndexedDB store for the wallet config (sealed seed + gateway creds +
// address counters). Nothing here is a secret except `sealed`, which is encrypted.

const DB = 'fortis';
const STORE = 'kv';
const KEY = 'wallet';

function open() {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore(STORE);
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

export const loadState = () => tx('readonly', (s) => s.get(KEY)).then((v) => v || null);
export const saveState = (state) => tx('readwrite', (s) => s.put(state, KEY));
export const wipeState = () => tx('readwrite', (s) => s.delete(KEY));
