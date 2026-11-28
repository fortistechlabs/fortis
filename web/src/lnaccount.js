// The Fortis account (the LSP holds a small Lightning balance keyed by this wallet's node id).
// Every request is a JSON body signed by the node key — the signing happens in wasm
// (`Wallet.lnSignAccount`), JS never holds the key — and posted to the edge's /ln/account.
// Mirrors android/.../lightning/FortisAccount.kt op for op.

const NONCE_CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

export class AccountError extends Error {
  /** kind: custody_full | insufficient | auth | rejected | network | name_invalid | name_taken | has_name */
  constructor(kind, message, extra = {}) {
    super(message || kind);
    this.kind = kind;
    Object.assign(this, extra);
  }
}

export function newNonce() {
  // 64 characters, so a byte masked to 6 bits is uniform.
  const b = crypto.getRandomValues(new Uint8Array(32));
  return Array.from(b, (x) => NONCE_CHARS[x & 63]).join('');
}

/** The body the node signs, and the server checks byte for byte. */
export function buildAccountRequest(nodeId, op, params, nonce, expiresAt) {
  return JSON.stringify({ node_id: nodeId, op, params, nonce, expires_at: expiresAt });
}

export class FortisAccount {
  /** `transport(body, sig) -> Promise<{status, text}>`; `nowSecs()` is injectable for tests. */
  constructor(transport, nowSecs = () => Math.floor(Date.now() / 1000)) {
    this.transport = transport;
    this.nowSecs = nowSecs;
  }

  async call(signer, op, params = {}) {
    const body = buildAccountRequest(signer.nodeId(), op, params, newNonce(), this.nowSecs() + 120);
    let status, text;
    try {
      ({ status, text } = await this.transport(body, signer.sign(body)));
    } catch (e) {
      throw new AccountError('network', e?.message || 'network');
    }
    if (status >= 200 && status < 300) {
      try { return JSON.parse(text); } catch { throw new AccountError('network', 'bad response'); }
    }
    let o = null;
    try { o = JSON.parse(text); } catch { /* not JSON */ }
    const err = o?.error;
    if (status === 409 && err === 'custody_full') throw new AccountError('custody_full', err, { minChannelSat: o.min_channel_sat });
    if (status === 409 && err === 'name_taken') throw new AccountError('name_taken');
    if (status === 409 && err === 'has_name') throw new AccountError('has_name');
    // The LSP already holds or paid this invoice: the first attempt may have gone through.
    if (status === 409 && err === 'duplicate_payment') throw new AccountError('duplicate');
    if (status === 400 && err === 'name_invalid') throw new AccountError('name_invalid', err, { reason: o.reason });
    if (status === 402) throw new AccountError('insufficient');
    if (status === 401) throw new AccountError('auth', `auth: ${o?.reason || ''}`, { reason: o?.reason || '' });
    if (status >= 400 && status < 500) throw new AccountError('rejected', err || `http ${status}`);
    throw new AccountError('network', `http ${status}`);
  }

  async balance(s) {
    const o = await this.call(s, 'balance');
    const balanceMsat = o.balance_msat || 0;
    const heldMsat = o.held_msat || 0;
    return { balanceMsat, heldMsat, maxMsat: o.max_msat || 0, freeMsat: Math.max(0, balanceMsat - heldMsat) };
  }

  /** A BOLT11 invoice paying into this account. */
  async invoice(s, amountMsat, description) {
    return (await this.call(s, 'invoice', { amount_msat: amountMsat, description })).invoice;
  }

  /** Pays a BOLT11 invoice from the account (`amountMsat` for an amountless one). */
  async pay(s, invoice, amountMsat) {
    const p = { invoice };
    if (amountMsat != null) p.amount_msat = amountMsat;
    const o = await this.call(s, 'pay', p);
    return { internal: !!o.internal, paymentHash: o.payment_hash || '' };
  }

  /** Pays a BOLT12 offer; returns the payment id to follow with `payStatus`. */
  async payOffer(s, offer, amountMsat) {
    const p = { offer };
    if (amountMsat != null) p.amount_msat = amountMsat;
    return (await this.call(s, 'pay_offer', p)).payment_id;
  }

  /** `pending`, `succeeded`, `failed` or `unknown`. */
  async payStatus(s, paymentId) {
    return (await this.call(s, 'pay_status', { payment_id: paymentId })).status;
  }

  /** The account's reusable BOLT12 offer (`lno1…`). */
  async offer(s) {
    return (await this.call(s, 'offer')).offer;
  }

  /** This wallet's Lightning address name, or null. `chain` is `btc` or `xbt` (signed with the request). */
  async myName(s, chain) {
    const o = await this.call(s, 'my_name', { chain });
    return o.name ? o.name : null;
  }

  async claimName(s, chain, name) {
    return (await this.call(s, 'claim_name', { chain, name })).name;
  }

  /** The name released, or null if there was none. */
  async releaseName(s, chain) {
    const o = await this.call(s, 'release_name', { chain });
    return o.released ? o.released : null;
  }

  /** Moves `amountMsat` (fee included) out to an on-chain address; the txid. */
  async withdraw(s, amountMsat, address, feerateSatVb) {
    return (await this.call(s, 'withdraw', { amount_msat: amountMsat, address, feerate_sat_vb: feerateSatVb })).txid;
  }

  async history(s, limit = 50) {
    const a = await this.call(s, 'history', { limit });
    return a.map((e) => ({
      atSecs: e.at_secs || 0,
      kind: e.kind || '',
      amountMsat: e.amount_msat || 0,
      feeMsat: e.fee_msat || 0,
      paymentHash: e.payment_hash ? e.payment_hash : null,
    }));
  }
}

/** The edge transport: `POST {base}/ln/account` (BTC) or `?chain=xbt` (XBT) with the node's signature.
 *  A 401 with no reason is a stale install token: refresh it and retry once. */
export function edgeTransport({ base, chain, getToken, refreshToken, fetchImpl = (...a) => fetch(...a) }) {
  const url = String(base).replace(/\/+$/, '') + (chain === 'xbt' ? '/ln/account?chain=xbt' : '/ln/account');
  const post = async (body, sig, token) => {
    const res = await fetchImpl(url, {
      method: 'POST',
      headers: { authorization: 'Bearer ' + token, 'x-fortis-sig': sig, 'content-type': 'application/json' },
      body,
      // Longer than the edge's own 20 s wait for the LSP, so a slow LSP answers instead of being given up on
      // (a given-up withdrawal or payment may still complete).
      signal: AbortSignal.timeout(30_000),
    });
    return { status: res.status, text: await res.text() };
  };
  return async (body, sig) => {
    let r = await post(body, sig, await getToken());
    // Only a stale install token is refreshed: not a named refusal (`reason`), not a missing signature.
    if (r.status === 401 && !r.text.includes('"reason"') && !r.text.includes('X-Fortis-Sig') && refreshToken) {
      r = await post(body, sig, await refreshToken());
    }
    return r;
  };
}
