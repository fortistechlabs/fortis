// App-shell cache. Gateway (fortisd) traffic is cross-origin and never touched.
//
// Network-first, cache as a fallback for offline use — not cache-first. A
// cache-first strategy with a hand-maintained version string (the old
// `fortis-shell-v2`) means every future code fix silently fails to reach an
// already-visiting browser until someone remembers to bump the string *and*
// the user manually clears site data: the service-worker script itself is
// what browsers byte-compare to detect an update, so editing app.js alone
// (as most fixes do) never even triggers a reinstall. Network-first sidesteps
// needing to remember any of that — every load gets fresh code when online,
// and only falls back to the cache when the network is unavailable.
const CACHE = 'fortis-shell-v3';
const ASSETS = [
  './', './index.html', './style.css', './manifest.webmanifest',
  './icon.svg', './icon-192.png', './icon-512.png', './apple-touch-icon.png',
  './vendor/qrcode.js',
  './src/app.js', './src/ui.js', './src/store.js', './src/gateway.js', './src/wallet.js',
  './src/esplora.js', './src/entropy.js', './src/i18n.js', './src/webauthn.js',
  './src/theme.js', './src/preinit.js', './src/locales/en.json',
  './pkg/wallet_wasm.js', './pkg/wallet_wasm_bg.wasm',
];

self.addEventListener('install', (e) => {
  // No self.skipWaiting() here: a worker that replaces an already-active one
  // sits in "waiting" until the page tells it to take over (see the
  // 'message' handler below) — that's the hook app.js uses to show an
  // "update available" prompt instead of swapping code out from under an
  // open tab. A brand-new install (no prior controller) activates on its
  // own regardless, since there's nothing waiting behind it.
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(ASSETS).catch(() => {})));
});

self.addEventListener('message', (e) => {
  if (e.data === 'SKIP_WAITING') self.skipWaiting();
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      // Take over already-open tabs immediately instead of waiting for them
      // to be closed and reopened — the next reload then gets fresh code.
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (url.origin !== location.origin || e.request.method !== 'GET') return;
  e.respondWith(
    fetch(e.request)
      .then((res) => {
        if (res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(e.request, copy));
        }
        return res;
      })
      .catch(() => caches.match(e.request)),
  );
});
