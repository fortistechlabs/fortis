// Drives the real web app (index.html) in headless Chromium over CDP against the mock edge.
// Usage: node mock.mjs &  then  node app-e2e.mjs <empty output dir>
import { spawn } from 'node:child_process';
import fs from 'node:fs';
const OUT = process.argv[2];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const chrome = spawn('/usr/bin/chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--remote-debugging-port=9333', `--user-data-dir=${OUT}/profile`, '--window-size=480,900', 'about:blank'], { stdio: 'ignore' });
let ver; for (let i = 0; i < 50 && !ver; i++) { try { ver = await (await fetch('http://localhost:9333/json')).json(); } catch { await sleep(200); } }
const page = ver.find((t) => t.type === 'page');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0; const waiting = new Map(); const errors = [];
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  if (d.id && waiting.has(d.id)) { waiting.get(d.id)(d); waiting.delete(d.id); }
  if (d.method === 'Runtime.exceptionThrown') errors.push(d.params.exceptionDetails.exception?.description || d.params.exceptionDetails.text);
  if (d.method === 'Runtime.consoleAPICalled' && d.params.type === 'error') errors.push('console.error ' + d.params.args.map((a) => a.value ?? a.description).join(' '));
};
const send = (method, params = {}) => new Promise((r) => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.result.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails.exception?.description)); return r.result.result.value; };
const results = []; const ok = (n, c, x = '') => { results.push(`${c ? 'PASS' : 'FAIL'} ${n}${c ? '' : ' ' + x}`); };
const waitFor = async (expr, ms = 8000) => { const t0 = Date.now(); while (Date.now() - t0 < ms) { try { if (await ev(expr)) return true; } catch { /* page not ready */ } await sleep(100); } return false; };
const text = () => ev('document.body.innerText');
const click = (label, last = false) => ev(`(()=>{const b=[...document.querySelectorAll('button')].filter(b=>b.textContent.trim()===${JSON.stringify(label)});const x=${last}?b.pop():b[0];if(!x)return false;x.click();return true})()`);
const setv = (sel, v) => ev(`(()=>{const i=document.querySelector(${JSON.stringify(sel)});i.value=${JSON.stringify(v)};i.dispatchEvent(new Event('input'));return true})()`);
try {
  await send('Runtime.enable'); await send('Page.enable');
  const T = 1496314668;
  await fetch('http://localhost:8199/__offset?o=' + (T - Math.floor(Date.now() / 1000)), { method: 'POST' });
  // The hosted edge URL is served by the mock; the clock sits inside the test invoices' lifetime.
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `
    const f = window.fetch.bind(window);
    window.fetch = (u, o) => f(String(u).replace('https://api.fortistechlabs.com', 'http://localhost:8199'), o);
    const base = ${T * 1000}; const p0 = performance.now(); Date.now = () => base + (performance.now() - p0);
  ` });
  await send('Page.navigate', { url: 'http://localhost:8199/index.html' });
  ok('onboarding shows', await waitFor(`/restore from a recovery phrase/i.test(document.body.innerText)`), await text());
  await click('Restore from a recovery phrase');
  await waitFor(`!!document.querySelector('#phrase')`);
  await setv('#phrase', 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about');
  await ev(`document.querySelector('#chain').value='btc'`);
  await click('Restore');
  ok('lock setup shows', await waitFor(`!!document.querySelector('#pw')`));
  await setv('#pw', 'password123'); await setv('#pw2', 'password123');
  await click('Continue');
  ok('wallet created and shell shown', await waitFor(`!![...document.querySelectorAll('.tabs button')].length`), await text());
  // Open the wallet from Home.
  await ev(`(()=>{const t=document.querySelector('.tile');if(t)t.click();return true})()`);
  ok('wallet screen has a Lightning tab', await waitFor(`[...document.querySelectorAll('.tabs button')].some(b=>b.textContent.trim()==='Lightning')`), await text());
  await click('Lightning');
  ok('the account loads in the real app', await waitFor(`document.body.innerText.includes('500,000')`), await text());
  await send('Page.captureScreenshot', { format: 'png' }).then((r) => fs.writeFileSync(`${OUT}/ln-account.png`, Buffer.from(r.result.data, 'base64')));
  // Pay with the password re-ask.
  await ev(`[...document.querySelectorAll('[data-ln] .seg button')].find(b=>b.textContent.trim()==='Send').click()`);
  await setv('#wln-send-to', 'lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap2r09nt4ndd0unm3z9u5t48y6ucv4r5sg7lk98c77ctvjczkspk5qprc90gx');
  await setv('#wln-send-amount', '1000');
  await click('Review');
  ok('review shown', await waitFor(`document.body.innerText.includes('Confirm payment')`), await text());
  await click('Pay');
  ok('paying asks for the password again', await waitFor(`!!document.querySelector('#reauth-pw')`), await text());
  const pays0 = (await (await fetch('http://localhost:8199/__state')).json()).pays;
  await setv('#reauth-pw', 'wrong-password'); await click('Unlock');
  await sleep(500);
  ok('a wrong password sends nothing', (await (await fetch('http://localhost:8199/__state')).json()).pays === pays0 && (await text()).includes('Incorrect password'), await text());
  await setv('#reauth-pw', 'password123'); await click('Unlock');
  ok('the right password pays once', await waitFor(`document.body.innerText.includes('Paid.')`, 10000), await text());
  ok('exactly one payment reached the server', (await (await fetch('http://localhost:8199/__state')).json()).pays === pays0 + 1);
  await send('Page.captureScreenshot', { format: 'png' }).then((r) => fs.writeFileSync(`${OUT}/ln-paid.png`, Buffer.from(r.result.data, 'base64')));
  // Locking forgets the Lightning state; unlocking and coming back reloads it.
  ok('no script errors in the page', errors.length === 0, errors.join('\n'));
} catch (e) { results.push('ERROR ' + e.stack); }
console.log(results.join('\n'));
chrome.kill();
process.exit(0);
