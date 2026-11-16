// SPDX-License-Identifier: LGPL-3.0-only
// Runs the real mse-worker.js in Node against a fake MediaSource and records what it appends. Import after setting the user agent.
export const appended = [];
const realInterval = globalThis.setInterval, realTimeout = globalThis.setTimeout;
globalThis.setInterval = (f, t) => { const h = realInterval(f, t); h.unref(); return h; };
globalThis.setTimeout = (f, t) => { const h = realTimeout(f, t); h.unref?.(); return h; };

class FakeSourceBuffer {
  mode = 'segments'; updating = false; buffered = { length: 0 }; listeners = {};
  addEventListener(n, f) { (this.listeners[n] ??= []).push(f); }
  appendBuffer(d) {
    appended.push(new Uint8Array(d));
    this.updating = true;
    queueMicrotask(() => { this.updating = false; for (const f of this.listeners.updateend ?? []) f(); });
  }
  remove() { this.updating = true; queueMicrotask(() => { this.updating = false; for (const f of this.listeners.updateend ?? []) f(); }); }
  abort() {}
}
globalThis.MediaSource = class {
  static canConstructInDedicatedWorker = true;
  static isTypeSupported() { return true; }
  readyState = 'open'; handle = {}; duration = NaN;
  addEventListener(n, f) { if (n === 'sourceopen') queueMicrotask(f); }
  addSourceBuffer() { return new FakeSourceBuffer(); }
  endOfStream() {}
};
globalThis.self = globalThis;
globalThis.postMessage = () => {};

const flush = () => new Promise(r => realTimeout(r, 0));

export function boxTypes(b, start = 0, end = b.length, out = []) {
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  for (let p = start; p + 8 <= end;) {
    const size = dv.getUint32(p);
    const type = String.fromCharCode(...b.subarray(p + 4, p + 8));
    out.push(type);
    const skip = type === 'stsd' ? 8 : /^(hvc1|hev1)$/.test(type) ? 78 : 0;
    if (/^(moov|trak|mdia|minf|stbl|stsd|hvc1|hev1)$/.test(type)) boxTypes(b, p + 8 + skip, p + size, out);
    p += size;
  }
  return out;
}

export async function startWorker(track) {
  await import('../js/mse-worker.js');
  const port = { postMessage() {}, close() {} };
  await self.onmessage({ data: { type: 'init', epoch: 1, track, port, audio: null } });
  await flush();
  return {
    port,
    async send(m) { port.onmessage({ data: { epoch: 1, ...m } }); await flush(); },
  };
}

// Length prefixed access unit, 4 byte sizes.
export function accessUnit(...nals) {
  const n = nals.reduce((s, x) => s + 4 + x.length, 0);
  const out = new Uint8Array(n);
  let o = 0;
  for (const x of nals) { new DataView(out.buffer).setUint32(o, x.length); out.set(x, o + 4); o += 4 + x.length; }
  return out;
}

// An hvcC record without parameter sets, 4 byte NAL lengths.
export const emptyHvcc = () => { const h = new Uint8Array(23); h[0] = 1; h[21] = 3; return h; };

// HEVC SPS (type 33) with a VUI colour description: one layer, no conformance window, one intra short term set.
export function makeSps({ primaries = 9, transfer = 16, matrix = 9, fullRange = 0, rps = [] } = {}) {
  const bits = [];
  const u = (v, n) => { for (let i = n - 1; i >= 0; i--) bits.push((v / 2 ** i) & 1); };
  const ue = v => { const c = v + 1, n = Math.floor(Math.log2(c)); u(0, n); u(c, n + 1); };
  u(0, 4); u(0, 3); u(1, 1);
  u(0, 2); u(0, 1); u(2, 5); u(0x20000000, 32); u(0, 32); u(0, 16); u(153, 8);
  ue(0); ue(1); ue(3840); ue(2160); u(0, 1); ue(2); ue(2); ue(4);
  u(1, 1); ue(4); ue(2); ue(0);
  for (let i = 0; i < 6; i++) ue(i < 4 ? 1 : 0);
  u(0, 1); u(0, 2); u(0, 1);
  const sets = rps.length ? rps : [() => { ue(1); ue(0); ue(0); u(1, 1); }];
  ue(sets.length);
  for (const s of sets) s({ u, ue });
  u(0, 1); u(0, 2);
  u(1, 1); u(0, 1); u(0, 1);
  u(1, 1); u(5, 3); u(fullRange, 1); u(1, 1); u(primaries, 8); u(transfer, 8); u(matrix, 8);
  u(0, 1); u(0, 3); u(0, 1); u(0, 1);
  u(1, 1); while (bits.length % 8) bits.push(0);
  const out = [0x42, 0x01];
  let zeros = 0;
  for (let i = 0; i < bits.length; i += 8) {
    let b = 0; for (let k = 0; k < 8; k++) b = b * 2 + bits[i + k];
    if (zeros >= 2 && b <= 3) { out.push(3); zeros = 0; }
    out.push(b); zeros = b === 0 ? zeros + 1 : 0;
  }
  return Uint8Array.from(out);
}

export function makeSei(mastering, contentLight) {
  const body = [];
  if (mastering) {
    body.push(137, 24);
    for (const v of mastering.slice(0, 8)) body.push(v >> 8, v & 255);
    for (const v of mastering.slice(8)) body.push((v >>> 24) & 255, (v >>> 16) & 255, (v >>> 8) & 255, v & 255);
  }
  if (contentLight) { body.push(144, 4); for (const v of contentLight) body.push(v >> 8, v & 255); }
  body.push(0x80);
  return Uint8Array.from([0x4e, 0x01, ...body]);
}
