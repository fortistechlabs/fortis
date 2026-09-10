// Supplementary entropy for seed generation.
//
// `crypto.getRandomValues` already gives 256 bits and is almost certainly sound.
// This is defence in depth: whatever is collected here is *mixed* with the
// CSPRNG bytes inside wasm (`wallet_core::entropy`) — it can only strengthen the
// seed, never weaken it, so every source is best-effort and skippable.

export class EntropyPool {
  constructor() {
    this._chunks = [];
    this._bits = 0;
  }

  /** A conservative lower-bound estimate of bits collected so far. */
  get bits() {
    return Math.floor(this._bits);
  }

  _add(bytes, bits) {
    this._chunks.push(bytes);
    this._bits += bits;
  }

  /**
   * Sample the low bits of a high-resolution timer inside a tight loop. The
   * spacing jitters with CPU frequency scaling, cache state, and OS scheduling.
   * Weak per sample (~0.5 bit) — a hedge that runs with no user interaction.
   */
  async collectJitter(ms = 300) {
    const out = [];
    const deadline = performance.now() + ms;
    let last = performance.now();
    while (performance.now() < deadline) {
      let acc = last;
      for (let i = 0; i < 700; i++) acc = Math.sqrt(acc * 1.0000001 + i);
      const now = performance.now();
      // fractional nanoseconds of the gap
      out.push(Math.floor((((now - last) % 1) + Number.EPSILON) * 1e9) & 0xff);
      out.push(Math.floor(acc) & 0xff);
      last = now;
      if (out.length % 64 === 0) await new Promise((r) => setTimeout(r, 0));
    }
    this._add(Uint8Array.from(out), out.length * 0.25);
  }

  /** One point of a human pointer/touch path: coordinates + event timestamp. */
  addPointer(x, y, t) {
    const b = new Uint8Array(10);
    const dv = new DataView(b.buffer);
    dv.setUint16(0, (x | 0) & 0xffff);
    dv.setUint16(2, (y | 0) & 0xffff);
    dv.setFloat32(4, t); // sub-ms precision where the browser allows it
    dv.setUint16(8, (performance.now() * 1000) & 0xffff);
    this._add(b, 3); // ~3 bits/sample, conservative for a chaotic drag
  }

  /** A device-motion reading (deg/s or m/s²), if the browser grants it. */
  addMotion(a, b, c) {
    const buf = new Uint8Array(12);
    new DataView(buf.buffer).setFloat32(0, a || 0);
    new DataView(buf.buffer).setFloat32(4, b || 0);
    new DataView(buf.buffer).setFloat32(8, c || 0);
    this._add(buf, 2);
  }

  /** Everything collected, as one flat Uint8Array for `generateMnemonic`. */
  bytes() {
    const n = this._chunks.reduce((s, c) => s + c.length, 0);
    const out = new Uint8Array(n);
    let o = 0;
    for (const c of this._chunks) {
      out.set(c, o);
      o += c.length;
    }
    return out;
  }
}
