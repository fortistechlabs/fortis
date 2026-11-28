// A stand-in for the edge's /ln/account + /register and the web/ files, for driving the Lightning tab in
// headless Chromium (see README.md in this folder). It does NOT verify the node's signature (the Rust golden
// test `fortis-lightning/tests/web_account_identity.rs` does); it checks the shape, expiry window and replay.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
const HERE = path.dirname(new URL(import.meta.url).pathname);
const WEB = path.resolve(HERE, '../..');
let offset = 0;
const nowSecs = () => Math.floor(Date.now() / 1000) + offset;
const st = { held: 0, paid: new Set(), payFail: false, balance: 500_000_000, name: null, pays: 0, withdrawals: 0, nonces: new Set(), log: [], failClock: false };
const hist = [{ at_secs: 1800000000, kind: 'received', amount_msat: 500_000_000, fee_msat: 0, payment_hash: null }];
const send = (res, code, body, type = 'application/json') => { res.writeHead(code, { 'content-type': type }); res.end(Buffer.isBuffer(body) || typeof body === 'string' ? body : JSON.stringify(body)); };
const types = { '.js': 'text/javascript', '.json': 'application/json', '.wasm': 'application/wasm', '.html': 'text/html', '.css': 'text/css' };
http.createServer((req, res) => {
  const url = new URL(req.url, 'http://x');
  if (req.method === 'GET' && url.pathname === '/__state') return send(res, 200, { ...st, nonces: undefined, paid: undefined, hist });
  if (req.method === 'POST' && url.pathname === '/__offset') { offset = Number(url.searchParams.get('o')); return send(res, 200, {}); }
  if (req.method === 'POST' && url.pathname === '/__unfull') { st.full = false; return send(res, 200, {}); }
  if (req.method === 'POST' && url.pathname === '/__pay_fail') { st.payFail = url.searchParams.get('on') === '1'; return send(res, 200, {}); }
  if (req.method === 'POST' && url.pathname === '/__full') { st.full = true; return send(res, 200, {}); }
  if (req.method === 'POST' && url.pathname === '/__fail_clock') { st.failClock = true; return send(res, 200, {}); }
  if (req.method === 'POST' && url.pathname === '/register') return send(res, 200, { token: 't1' });
  if (url.pathname.startsWith('/btc/') || url.pathname.startsWith('/xbt/') || url.pathname === '/pricing') return send(res, 404, { error: 'mock' });
  if (req.method === 'POST' && url.pathname === '/ln/account') {
    let b = '';
    req.on('data', (c) => (b += c));
    req.on('end', () => {
      const sig = req.headers['x-fortis-sig'];
      const o = JSON.parse(b);
      const now = nowSecs();
      st.log.push({ op: o.op, sigLen: sig?.length, auth: req.headers.authorization, params: o.params });
      if (req.headers.authorization !== 'Bearer t1') return send(res, 401, '');
      if (!sig || sig.length !== 104 || !/^[ybndrfg8ejkmcpqxot1uwisza345h769]+$/.test(sig)) return send(res, 401, { error: 'auth', reason: 'bad_signature' });
      if (!/^[0-9a-f]{66}$/.test(o.node_id) || o.expires_at < now || o.expires_at > now + 300) return send(res, 401, { error: 'auth', reason: 'expired' });
      if (st.nonces.has(o.nonce)) return send(res, 401, { error: 'auth', reason: 'replayed' });
      st.nonces.add(o.nonce);
      if (st.full && o.op === 'invoice') return send(res, 409, { error: 'custody_full', min_channel_sat: 11000 });
      if (st.failClock && o.op === 'invoice') return send(res, 401, { error: 'auth', reason: 'expired' });
      switch (o.op) {
        case 'balance': return send(res, 200, { balance_msat: st.balance, held_msat: st.held, max_msat: 50_000_000 });
        case 'history': return send(res, 200, hist);
        case 'my_name': return send(res, 200, { name: st.name });
        case 'claim_name': if (o.params.name === 'taken') return send(res, 409, { error: 'name_taken' }); st.name = o.params.name; return send(res, 200, { name: st.name });
        case 'release_name': { const r = st.name; st.name = null; return send(res, 200, { released: r }); }
        case 'invoice': return send(res, 200, { invoice: 'lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu' });
        case 'offer': return send(res, 200, { offer: 'lno1pqps7sjqpgtyzm3qv4uxzmtsd3jjqer9wd3hy6tsw35k7msjzfpy7nz5yqcnygrfdej82um5wf5k2uckyypwa3eyt44h6txtxquqh7lz5djge4afgfjn7k4rgrkuag0jsd5xvxg' });
        case 'pay': {
          const key = o.params.invoice + ':' + (o.params.amount_msat || 0);
          if (st.paid.has(key)) return send(res, 409, { error: 'duplicate_payment' });
          st.paid.add(key);
          const amt = o.params.amount_msat || 0; const hold = amt + 1000; st.held += hold; st.pays++;
          const hash = String(st.pays).padStart(2, '0').repeat(32);
          // The LSP answers as soon as the payment is started; the ledger settles it (or releases the hold on a
          // failed route) in one step. (Settled before answering here: the harness runs on virtual time, where a
          // "later" settle would race the page's polling.)
          st.held -= hold;
          if (!st.payFail) {
            st.balance -= hold;
            hist.unshift({ at_secs: nowSecs(), kind: 'sent', amount_msat: -hold, fee_msat: 1000, payment_hash: hash });
          }
          return send(res, 200, { internal: false, payment_hash: hash });
        }
        case 'withdraw': { st.withdrawals++; st.balance -= o.params.amount_msat; hist.unshift({ at_secs: now, kind: 'withdrawn', amount_msat: -o.params.amount_msat, fee_msat: 900_000, payment_hash: 'cd'.repeat(32) }); return send(res, 200, { txid: 'ef'.repeat(32) }); }
        default: return send(res, 400, { error: 'unknown_op' });
      }
    });
    return;
  }
  let p = url.pathname === '/' ? '/index.html' : url.pathname;
  const file = p.startsWith('/__') ? path.join(HERE, p.slice(3)) : path.join(WEB, p);
  fs.readFile(file, (e, data) => (e ? send(res, 404, 'nope', 'text/plain') : send(res, 200, data, types[path.extname(file)] || 'application/octet-stream')));
}).listen(8199, () => console.log('mock on 8199'));
