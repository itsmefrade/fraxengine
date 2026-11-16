// SPDX-License-Identifier: LGPL-3.0-only
// Test/diagnostic helper imported first by every worker: remembers the linear memories of WASM instances created in
// this worker so the stats messages can report their size (the page cannot see a worker's WASM heap otherwise).
const mems = [];
function note(inst) { try { const m = inst && inst.exports && inst.exports.memory; if (m instanceof WebAssembly.Memory) mems.push(new WeakRef(m)); } catch {} }
if (!WebAssembly.__aetherMemPatched) {
  const OI = WebAssembly.Instance;
  WebAssembly.Instance = class extends OI { constructor(m, i) { super(m, i); note(this); } };
  const oi = WebAssembly.instantiate.bind(WebAssembly);
  WebAssembly.instantiate = async (...a) => { const r = await oi(...a); note(r.instance || r); return r; };
  WebAssembly.__aetherMemPatched = true;
}
export function wasmBytes() {
  let t = 0;
  for (let i = mems.length - 1; i >= 0; i--) { const m = mems[i].deref(); if (!m) mems.splice(i, 1); else t += m.buffer.byteLength; }
  return t;
}
export function jsHeapBytes() { return (typeof performance !== 'undefined' && performance.memory) ? performance.memory.usedJSHeapSize : null; }
