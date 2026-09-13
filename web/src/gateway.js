// HTTP client for a fortisd chain gateway. The gateway holds no keys; it serves
// chain data and broadcasts finished transactions.

export class Gateway {
  constructor(url, token) {
    this.url = String(url || '').replace(/\/+$/, '');
    this.token = token || '';
  }

  async req(method, path, body) {
    let res;
    try {
      res = await fetch(this.url + path, {
        method,
        headers: {
          authorization: 'Bearer ' + this.token,
          ...(body ? { 'content-type': 'application/json' } : {}),
        },
        body: body ? JSON.stringify(body) : undefined,
        // Without this, a fortisd that's running but unreachable (wrong
        // port, firewalled, stuck) hangs the caller forever instead of
        // surfacing "cannot reach gateway" — e.g. the connect screen would
        // just sit on "connecting…" with no way out.
        signal: AbortSignal.timeout(10_000),
      });
    } catch (e) {
      throw new Error(`cannot reach gateway at ${this.url} — is fortisd running?`);
    }
    const text = await res.text();
    const data = text ? JSON.parse(text) : null;
    if (!res.ok) throw new Error(data?.error || `gateway HTTP ${res.status}`);
    return data;
  }

  ping() { return this.req('GET', '/'); }
  status() { return this.req('GET', '/v1/status'); }
  connect(body) { return this.req('POST', '/v1/connect', body); }
  balances() { return this.req('GET', '/v1/balances'); }
  utxos(minConf = 1) { return this.req('GET', `/v1/utxos?min_conf=${minConf}`); }
  feerate(confTarget = 6) { return this.req('GET', `/v1/feerate?conf_target=${confTarget}`); }
  history(count = 50) { return this.req('GET', `/v1/history?count=${count}`); }
  broadcast(hex) { return this.req('POST', '/v1/broadcast', { hex }); }
}
