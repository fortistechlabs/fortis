// Prepares a STAGED COPY of web/ for publishing: hashes every served file,
// adds Subresource Integrity to index.html's directly-loaded <script>/<link>
// tags, and writes build-info.json — a manifest anyone can check the live
// site against before trusting it with real funds (see the "Verify this
// build" card in the app's Settings tab, and the criticism it answers: a
// static site has no code-signing the way the Android APK does, so without
// this there's no way for a user to confirm the JS they were served matches
// the public source).
//
// Run against a COPY, never the tracked web/ source — this script rewrites
// index.html in place and adds a new file alongside it.
//
// Usage: node build-web-stage.mjs <staged-dir> <commit> <commitFull> <dirty:true|false>

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const [, , dir, commit, commitFull, dirtyArg] = process.argv;
if (!dir || !commit) {
  console.error('usage: build-web-stage.mjs <staged-dir> <commit> <commitFull> <dirty:true|false>');
  process.exit(1);
}
if (!fs.existsSync(path.join(dir, 'index.html'))) {
  throw new Error(`${dir} doesn't look like a staged web/ copy (no index.html)`);
}

function walk(base, rel = '') {
  const out = [];
  for (const name of fs.readdirSync(path.join(base, rel))) {
    const r = rel ? `${rel}/${name}` : name;
    const stat = fs.statSync(path.join(base, r));
    if (stat.isDirectory()) out.push(...walk(base, r));
    else out.push(r.replace(/\\/g, '/'));
  }
  return out;
}

const sha256 = (buf) => crypto.createHash('sha256').update(buf).digest();
const hex = (buf) => sha256(buf).toString('hex');
const sri = (buf) => `sha256-${sha256(buf).toString('base64')}`;
const reEscape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

// Give sw.js's own cache name the current commit, so its bytes reliably
// change on every deploy. Browsers detect a new service worker by
// byte-diffing sw.js itself — app.js's "PWA updates" section (offerUpdate())
// depends on that to tell an already-open tab a new deploy exists, but a
// hand-set name (the old 'fortis-shell-v3') only changes when someone
// remembers to bump it, so an ordinary content-only deploy never triggered
// it. Deriving it from the commit removes that human step; sw.js's own
// 'activate' handler already deletes any Cache Storage bucket whose name
// isn't the current one, so a new bucket per deploy is exactly what it
// already expects — this changes nothing about how requests are served
// (still network-first), only the name of the bucket it opportunistically
// caches into.
const swPath = path.join(dir, 'sw.js');
if (fs.existsSync(swPath)) {
  const sw = fs.readFileSync(swPath, 'utf8');
  const next = sw.replace(/const CACHE = '[^']*';/, `const CACHE = 'fortis-shell-${commit}';`);
  if (next === sw) throw new Error("sw.js has no `const CACHE = '...';` line to version");
  fs.writeFileSync(swPath, next);
}

// Only these are *directly* loaded via a <script src>/<link href> in
// index.html, which is all the platform's integrity attribute can check —
// everything app.js further imports as an ES module (wallet.js, esplora.js,
// store.js, wallet_wasm.js, ...) has no browser-enforced integrity hook of
// its own.
//
// DISABLED 2026-09-20, first production deploy: broke the site outright.
// Cloudflare caches these four files at `max-age=14400` (4h) per edge node,
// ignoring `_headers`' `Cache-Control: no-cache` for them — confirmed live,
// different edge nodes were still serving genuinely different cached bytes
// of src/app.js hours apart. index.html revalidates fast and consistently,
// so its embedded SRI hash pins one exact version of app.js; any edge still
// serving an older cached copy then hard-fails the integrity check instead
// of just running slightly-stale JS (which was harmless before this
// existed). Re-enable only after confirming (via response headers post-
// deploy, not assumption) that these four files actually get a Cache-
// Control that revalidates as fast as index.html does — otherwise every
// future deploy that changes one of them risks the same outage for
// whichever edge nodes haven't caught up yet. build-info.json's per-file
// hashes below still cover every file and carry none of this risk, since
// nothing enforces them.
const SRI_TARGETS = [];

const relFiles = walk(dir).filter((f) => f !== 'index.html' && f !== 'build-info.json');
const hashes = {};
const sriHashes = {};
for (const f of relFiles) {
  const buf = fs.readFileSync(path.join(dir, f));
  hashes[f] = hex(buf);
  if (SRI_TARGETS.includes(f)) sriHashes[f] = sri(buf);
}
for (const f of SRI_TARGETS) {
  if (!sriHashes[f]) throw new Error(`SRI target missing from the staged copy: ${f}`);
}

let html = fs.readFileSync(path.join(dir, 'index.html'), 'utf8');
for (const f of SRI_TARGETS) {
  const attr = ` integrity="${sriHashes[f]}" crossorigin="anonymous"`;
  const re = new RegExp(`(<(?:script|link)\\b[^>]*?(?:src|href)="${reEscape(f)}"[^>]*?)(\\s*/?>)`);
  const next = html.replace(re, (_m, head, tail) => `${head}${attr}${tail}`);
  if (next === html) throw new Error(`index.html has no tag referencing ${f} to add integrity to`);
  html = next;
}
fs.writeFileSync(path.join(dir, 'index.html'), html);
// Hashed last, over its final (integrity-tagged) bytes — this is what's actually served.
hashes['index.html'] = hex(Buffer.from(html, 'utf8'));

const manifest = {
  commit,
  commitFull: commitFull || commit,
  dirty: dirtyArg === 'true',
  builtAt: new Date().toISOString(),
  hashAlgorithm: 'sha256',
  files: Object.fromEntries(Object.entries(hashes).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))),
};
fs.writeFileSync(path.join(dir, 'build-info.json'), JSON.stringify(manifest, null, 2) + '\n');

console.log(
  `build-web-stage: ${relFiles.length + 1} files hashed, SRI added to ${SRI_TARGETS.length} tags, ` +
    `commit ${commit}${manifest.dirty ? ' [+ uncommitted changes]' : ''}`,
);
