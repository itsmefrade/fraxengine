// SPDX-License-Identifier: LGPL-3.0-only
// Dev bench for session-mt.mjs (mt-bench.html): decodes a raw HEVC fixture with T threads, reports wall time and per-frame hashes.
import {loadAetherVideoSessionsMt} from './session-mt.mjs';
import {loadAetherVideoSessions} from './session.mjs';
import {loadAetherVideoSessions as loadRef} from './session-ref.mjs';
import {splitAnnexB} from './annexb.js';
self.onmessage = async e => {
  const {src, threads, workers, frames, hash, single, nocopy} = e.data;
  try {
    const t0 = performance.now();
    const api = single && nocopy ? await loadRef(new URL('./wasm/aether_hevc_simd.wasm', import.meta.url)) : single ? await loadAetherVideoSessions(new URL('./wasm/aether_hevc_simd.wasm', import.meta.url)) : await loadAetherVideoSessionsMt(undefined, {threads, workers});
    const initMs = performance.now() - t0;
    const nals = splitAnnexB(new Uint8Array(await (await fetch(src)).arrayBuffer())).filter(n => ((n[0] >> 1) & 63) < 62 && (((n[0] & 1) << 5) | (n[1] >> 3)) === 0);
    const s = api.createSession(0);
    const hashes = []; let n = 0, hashMs = 0;
    const emit = async f => {
      n++;
      if (!hash) return;
      const t = performance.now();
      const d = await Promise.all([f.Y, f.U, f.V].map(p => crypto.subtle.digest('SHA-256', p)));
      hashes.push(d.map(b => Array.from(new Uint8Array(b), x => x.toString(16).padStart(2, '0')).join('')).join(' '));
      hashMs += performance.now() - t;
    };
    postMessage({started: true});
    const t1 = performance.now();
    const drain = async () => { let f; while (n < frames && (f = s.pull(undefined, !!nocopy))) await emit(f); };
    for (const nal of nals) {
      if (n >= frames) break;
      for (;;) { try { s.pushNal(nal); break; } catch (err) { if (err.code === 'backpressure') { await drain(); continue; } throw err; } }
      await drain();
    }
    if (n < frames) { s.finish(); await drain(); }
    const ms = performance.now() - t1 - hashMs;
    postMessage({ok: true, frames: n, ms: Math.round(ms), fps: +(n * 1000 / ms).toFixed(2), initMs: Math.round(initMs), memMB: Math.round((api.memoryBytes ?? 0) / 1048576), hashes});
    api.stop?.();
  } catch (err) { postMessage({ok: false, error: String(err && err.stack || err)}); }
};
