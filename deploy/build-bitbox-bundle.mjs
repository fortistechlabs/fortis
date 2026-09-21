// Bundles @bitboxswiss/bitbox-api (BIP-174 PSBT-based BitBox02 hardware-wallet
// support) into a single vendored file the web wallet loads same-origin —
// never from a CDN at runtime, matching the "nothing loaded from a third
// party at runtime" property web/pkg/ (the wasm-pack output) already has.
//
// The package itself is plain ESM using bare npm specifiers throughout
// (bitcoinjs-lib, @noble/curves, ...) that a browser can't resolve directly,
// so it has to be bundled like web/pkg/ is built — compiled output, not
// usable source as-is, hence git-ignored and built fresh rather than
// committed the way web/vendor/qrcode.js (hand-vendored upstream source) is.
//
// Usage: node build-bitbox-bundle.mjs   (run from deploy/, after `npm ci`)

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import esbuild from 'esbuild';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.dirname(here);
const outFile = path.join(repo, 'web', 'vendor', 'bitbox-api.bundle.js');

// Only what web/src/bitbox.js actually needs: the two transports, the
// auto-picker that tries WebHID then falls back to BitBoxBridge, and the
// user-declined/timed-out error check. Everything else (BitBox/PairingBitBox/
// PairedBitBox, ethIdentifyCase, ...) either comes back as a return value
// from these or isn't Bitcoin-relevant.
const entryContents = `export {
  bitbox02ConnectAuto,
  bitbox02ConnectWebHID,
  bitbox02ConnectBridge,
  isUserAbort,
} from '@bitboxswiss/bitbox-api';
`;

const entryFile = path.join(here, '.bitbox-entry.mjs');
fs.writeFileSync(entryFile, entryContents);

try {
  const result = await esbuild.build({
    entryPoints: [entryFile],
    bundle: true,
    format: 'esm',
    platform: 'browser',
    minify: true,
    outfile: outFile,
    metafile: true,
  });

  const bytes = fs.statSync(outFile).size;
  console.log(`build-bitbox-bundle: wrote ${path.relative(repo, outFile)} (${(bytes / 1024).toFixed(0)} KiB)`);

  // node:net is a real dependency of the package's connect-simulator.js /
  // transport-simulator.js (Node-only test-simulator code) but is never
  // reached from the real entry points above — confirm that stays true on
  // every rebuild rather than trusting it silently, since a future version
  // bump could change that.
  const inputs = Object.keys(result.metafile.inputs);
  const simulatorFiles = inputs.filter((f) => /simulator/i.test(f));
  if (simulatorFiles.length) {
    throw new Error(
      `build-bitbox-bundle: simulator code leaked into the bundle (${simulatorFiles.join(', ')}) — ` +
        'this is Node-only test code that must not ship to the browser.',
    );
  }
} finally {
  fs.unlinkSync(entryFile);
}
