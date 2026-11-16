// SPDX-License-Identifier: LGPL-3.0-only
// Entropy worker of the GPU reconstruction path (one per layer, web/gpu-recon/INTEGRATION.md): an entropy-only aether-hevc
// session (feature recon-stream, web/exact/wasm/aether_hevc_simd_recon.wasm) decodes the layer sequentially, with no GOP pool and
// no open-GOP prefix, and posts the reconstruction command stream (web/gpu-recon/FORMAT-STREAM.md) of every access unit to the
// video worker, which reconstructs the pictures on the GPU.
//
//   in:  {type:'init', wasmUrl, mtUrl?, threads?}  {type:'reset', epoch}  {type:'au', epoch, seq, bytes (Annex B, transferred)}  {type:'finish', epoch}  {type:'quit'}
//   out: {type:'ready'}  {type:'events', epoch, seq, events: [{type, payload (Uint32Array, transferred)}], ms}  {type:'error', epoch, message}  {type:'bye'}
//
// A Picture payload is copied out of the WASM memory into its own buffer and transferred; Output / Discard / Generated payloads too
// (a few words). A reset is a fresh decoder (seek to a random access point); messages of an older epoch are ignored.
//
// Threads (`threads` > 1 with `mtUrl`, cross-origin isolated only): the `build-wasm.sh --recon-threads` module on a shared memory,
// threads - 1 helper workers (web/exact/thread-mt.js) decoding the CTB rows of WPP slices; pictures stay sequential and the
// command stream is identical for every thread count. Any failure to set it up falls back to the single-thread module.
import { splitAnnexB } from '../../exact/annexb.js';
import { futexImports } from '../../exact/session-mt.mjs';

let x = null, mem = null, shared = false, threads = 1, helpers = [], epoch = -1, poisoned = false;

async function initThreads(mtUrl, t) {
  const module = await WebAssembly.compileStreaming(fetch(mtUrl));
  const memory = new WebAssembly.Memory({ initial: 1024, maximum: 16384, shared: true });
  const ex = new WebAssembly.Instance(module, { env: { memory, ...futexImports(memory) } }).exports;
  if (!ex.aether_recon_session_reset_threads) throw new Error('threaded recon module lacks aether_recon_session_reset_threads');
  const ws = [];
  try {
    await Promise.all(Array.from({ length: t - 1 }, (_, index) => new Promise((resolve, reject) => {
      const w = new Worker(new URL('../../exact/thread-mt.js', import.meta.url), { type: 'module' });
      w.onmessage = e => { if (e.data.type === 'up') resolve(); };
      w.onerror = e => reject(new Error('entropy thread: ' + (e.message || 'error')));
      ws.push(w);
      // aether_thread_stack registers the stack for the per-thread level sink: a stack from anywhere else would race.
      w.postMessage({ module, memory, index, stackTop: ex.aether_thread_stack(1 << 20) });
    })));
    const t0 = performance.now();
    while (ex.aether_thread_idle() < t - 1) {
      if (performance.now() - t0 > 5000) throw new Error('entropy threads did not start');
      await new Promise(r => setTimeout(r, 1));
    }
  } catch (e) { for (const w of ws) w.terminate(); throw e; }
  x = ex; mem = memory; shared = true; threads = t; helpers = ws;
}

function reset() { if (threads > 1) x.aether_recon_session_reset_threads(threads); else x.aether_recon_session_reset(); }

function takeEvents() {
  const words = x.aether_recon_session_out_words();
  const events = [], transfer = [];
  if (words) {
    const all = new Uint32Array(mem.buffer, x.aether_recon_session_out_ptr(), words);
    for (let p = 0; p + 2 <= words;) {
      const type = all[p], n = all[p + 1];
      // From a shared memory a plain loop: slice() goes through V8's relaxed memcpy, far slower in a worker (session-mt.mjs).
      let payload;
      if (shared) { payload = new Uint32Array(n); for (let i = 0; i < n; i++) payload[i] = all[p + 2 + i]; } else payload = all.slice(p + 2, p + 2 + n);
      events.push({ type, payload });
      transfer.push(payload.buffer);
      p += 2 + n;
    }
    x.aether_recon_session_out_clear();
  }
  return { events, transfer };
}

function push(nal) {
  const ptr = x.aether_video_alloc(nal.length);
  if (!ptr) throw new Error('NAL allocation rejected');
  new Uint8Array(mem.buffer, ptr, nal.length).set(nal);
  const r = x.aether_recon_session_push(ptr, nal.length);
  x.aether_video_free(ptr, nal.length);
  return r;
}

self.onmessage = async e => {
  const m = e.data;
  try {
    if (m.type === 'init') {
      if ((m.threads || 1) > 1 && m.mtUrl && globalThis.crossOriginIsolated) {
        try { await initThreads(m.mtUrl, m.threads); }
        catch (err) { postMessage({ type: 'warn', message: 'entropy threads unavailable, one thread: ' + (err.message || err) }); }
      }
      if (x) { postMessage({ type: 'ready', threads }); return; }
      const r = await fetch(m.wasmUrl);
      if (!r.ok) throw new Error('recon WASM HTTP ' + r.status);
      const { instance } = await WebAssembly.instantiate(await r.arrayBuffer(), {});
      x = instance.exports; mem = x.memory;
      if (!x.aether_recon_session_reset) throw new Error('WASM module lacks the recon session API');
      postMessage({ type: 'ready', threads: 1 });
    } else if (m.type === 'reset') {
      epoch = m.epoch; poisoned = false;
      reset();
    } else if (m.type === 'au') {
      if (m.epoch !== epoch) return;
      const t = performance.now();
      let pushMs = 0;
      if (!poisoned) {
        for (const nal of splitAnnexB(new Uint8Array(m.bytes))) {
          const type = (nal[0] >> 1) & 63, layerId = ((nal[0] & 1) << 5) | (nal[1] >> 3);
          if (type >= 62 || layerId !== 0) continue;   // RPU / EL carriage: not part of this layer's stream
          const a = performance.now();
          const r = push(nal);
          pushMs += performance.now() - a;
          if (r) { poisoned = true; postMessage({ type: 'error', epoch, message: `entropy decode failed (code ${r}, NAL type ${type})` }); break; }
        }
      }
      const b = performance.now();
      const { events, transfer } = takeEvents();
      let outBytes = 0;
      for (const x of transfer) outBytes += x.byteLength;
      const now = performance.now();
      // ms: the whole access unit; pushMs: inside the WASM decoder; takeMs: the command stream copied out of the WASM memory
      postMessage({ type: 'events', epoch, seq: m.seq, events, ms: now - t, pushMs, takeMs: now - b, inBytes: m.bytes.byteLength, outBytes }, transfer);
    } else if (m.type === 'finish') {
      if (m.epoch !== epoch) return;
      if (!poisoned) x.aether_recon_session_finish();
      const { events, transfer } = takeEvents();
      postMessage({ type: 'events', epoch, seq: -1, events, finished: true, ms: 0 }, transfer);
    } else if (m.type === 'quit') {
      for (const w of helpers) w.terminate();   // they block in Atomics.wait inside the module and never return
      postMessage({ type: 'bye' }); self.close();
    }
  } catch (err) {
    poisoned = true;
    postMessage({ type: 'error', epoch, message: String(err && err.stack || err) });
  }
};
