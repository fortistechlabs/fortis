// App-shell cache. Gateway (fortisd) traffic is cross-origin and never touched.
const CACHE = 'fortis-shell-v1';
const ASSETS = [
  './', './index.html', './style.css', './manifest.webmanifest', './icon.svg',
  './src/app.js', './src/ui.js', './src/store.js', './src/gateway.js', './src/wallet.js',
  './pkg/wallet_wasm.js', './pkg/wallet_wasm_bg.wasm',
];

self.addEventListener('install', (e) => {
  self.skipWaiting();
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(ASSETS).catch(() => {})));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys().then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))),
  );
});

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (url.origin !== location.origin || e.request.method !== 'GET') return;
  e.respondWith(
    caches.match(e.request).then((hit) => hit || fetch(e.request)),
  );
});
