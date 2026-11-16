// SPDX-License-Identifier: LGPL-3.0-only
// VC-1 decode worker: one Rust/WASM decoder (web/engine/vc1.mjs) per worker. The video worker (vc1-path.js) feeds it packets and gets
// display order 8 bit 4:2:0 frames back as transferable buffers. Everything is tagged with a generation so a seek can drop what is in flight.
//   in : {type:'init', track}  {type:'pkt', gen, ptsUs, data}  {type:'reset', gen}  {type:'flush', gen}
//   out: {type:'ready', info}  {type:'frames', gen, frames:[{buffer,width,height,ptsUs,flags...}], decodeMs}  {type:'flushed', gen}  {type:'error', message}
import { createVc1Decoder } from '../../engine/vc1.mjs';

let dec = null, gen = 0;
const queue = [];
let busy = false;

async function run() {
  if (busy) return;
  busy = true;
  try {
    while (queue.length) {
      const m = queue.shift();
      if (m.type === 'reset') { gen = m.gen; dec.reset(); continue; }
      if (m.gen !== gen) continue;      // belongs to a position that was left
      let frames, t0 = performance.now();
      if (m.type === 'flush') frames = dec.flush();
      else {
        try { frames = dec.decode(new Uint8Array(m.data), m.ptsUs); }
        catch (e) { self.postMessage({ type: 'error', message: String(e && e.message || e) }); continue; }
      }
      const decodeMs = performance.now() - t0;
      if (frames.length || m.type === 'pkt') {
        self.postMessage({ type: 'frames', for: m.type, gen: m.gen, frames: frames.map(f => ({ buffer: f.buffer, width: f.width, height: f.height, ptsUs: f.ptsUs, interlaced: f.interlaced, topFieldFirst: f.topFieldFirst, repeatFirstField: f.repeatFirstField, repeatPict: f.repeatPict, durationUs: f.durationUs, key: f.key, pictType: f.pictType })), decodeMs, errors: dec.errors }, frames.map(f => f.buffer));
      }
      if (m.type === 'flush') self.postMessage({ type: 'flushed', gen: m.gen });
    }
  } finally { busy = false; }
}

self.onmessage = async e => {
  const m = e.data;
  try {
    if (m.type === 'init') {
      dec = await createVc1Decoder(m.track);
      self.postMessage({ type: 'ready', info: dec.info() });
    } else if (m.type === 'close') {
      queue.length = 0; dec?.close(); dec = null; self.postMessage({ type: 'closed' });
    } else {
      queue.push(m);
      run();
    }
  } catch (err) { self.postMessage({ type: 'error', message: String(err && err.stack || err) }); }
};
