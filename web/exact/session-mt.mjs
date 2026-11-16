// SPDX-License-Identifier: LGPL-3.0-only
/** One aether-hevc decoder on several threads (prototype, plan v2 A1). Same session contract as session.mjs (pictures copied out of
 * the module), but the module is the `build-wasm.sh --threads` build: its memory is a shared WebAssembly.Memory, and `workers`
 * nested Web Workers (thread-mt.js) each run an instance of it as decoder threads. Dedicated Worker only (the decode thread blocks
 * in Atomics.wait); needs cross-origin isolation (COOP same-origin + COEP require-corp). Frame threading keeps `threads` pictures in
 * flight on threads + 1 workers; the rest serve the loop-filter bands and WPP rows (or those run inline when none is idle). */
const MAX_INPUT = 8 * 1024 * 1024;
const CODES = {1: 'unknown-session', 2: 'epoch', 3: 'finished', 4: 'poisoned', 5: 'backpressure', 6: 'invalid-nal', 7: 'decode-error', 8: 'epoch-must-advance'};

/** Wasm's futex: Atomics.wait / Atomics.notify on the shared memory (a view is refreshed after memory.grow). */
export function futexImports(memory) {
  let view = new Int32Array(memory.buffer);
  const v = addr => { if (addr >= view.byteLength) view = new Int32Array(memory.buffer); return view; };
  return {
    aether_futex_wait(addr, expected) { Atomics.wait(v(addr), addr >>> 2, expected); },
    aether_futex_wake(addr, count) { Atomics.notify(v(addr), addr >>> 2, count < 0 ? Infinity : count); },
  };
}

// Copy out of the shared memory with a plain 32-bit JS loop. TypedArray.set or slice from a shared view go through V8's relaxed
// (race-safe) memcpy, which in a worker is far slower: the whole 1454-picture 1080p decode at one thread took 23.7 s of CPU with
// set() and 18.1 s with this loop (headless Chrome; the single-thread module copying with slice() from non-shared memory: 14.7 s).
export function copyShared(buffer, byteOffset, n16) {
  const out = new Uint16Array(n16);
  if (byteOffset % 4 === 0 && n16 % 2 === 0) {
    const s = new Int32Array(buffer, byteOffset, n16 >> 1), d = new Int32Array(out.buffer);
    for (let i = 0; i < d.length; i++) d[i] = s[i];
  } else {
    const s = new Uint16Array(buffer, byteOffset, n16);
    for (let i = 0; i < n16; i++) out[i] = s[i];
  }
  return out;
}

export async function loadAetherVideoSessionsMt(url = new URL('./wasm/aether_hevc_simd_mt.wasm', import.meta.url), {threads = 3, workers = 2 * threads + 1, maxMB = 1024, stackKB = 1024} = {}) {
  if (typeof window !== 'undefined' && window.document) throw new Error('Threaded decoder must run in a DedicatedWorker');
  if (!globalThis.crossOriginIsolated) throw new Error('Threaded decoder needs cross-origin isolation (SharedArrayBuffer)');
  if (workers < threads + 1 || workers > 64) throw new Error('workers must be threads + 1 .. 64');
  const module = await WebAssembly.compileStreaming(fetch(url));
  const memory = new WebAssembly.Memory({initial: 1024, maximum: maxMB * 16, shared: true});
  const wasm = new WebAssembly.Instance(module, {env: {memory, ...futexImports(memory)}}).exports;
  const pool = [];
  await Promise.all(Array.from({length: workers}, (_, index) => new Promise((resolve, reject) => {
    const w = new Worker(new URL('./thread-mt.js', import.meta.url), {type: 'module'});
    w.onmessage = e => { if (e.data.type === 'up') resolve(); };
    w.onerror = e => reject(new Error('decoder thread: ' + (e.message || 'error')));
    pool.push(w);
    w.postMessage({module, memory, index, stackTop: wasm.aether_thread_stack(stackKB * 1024)});
  })));
  // 'up' is posted just before aether_thread_main: wait until every thread is parked on its job slot.
  while (wasm.aether_thread_idle() < workers) await new Promise(r => setTimeout(r, 1));
  let trapped = false;
  const call = fn => { if (trapped) throw new Error('WASM trapped; discard decoder'); try { return fn(); } catch (e) { if (e instanceof WebAssembly.RuntimeError) trapped = true; throw e; } };
  function result(pointer, discard = false) {
    const view = new DataView(memory.buffer), size = view.getUint32(pointer, true), status = view.getUint32(pointer + 4, true), count = view.getUint32(pointer + 8, true);
    try {
      if (size < 12 || pointer + size > memory.buffer.byteLength) { trapped = true; throw new Error('Invalid session result bounds'); }
      if (status) { const e = new Error(new TextDecoder().decode(new Uint8Array(memory.buffer, pointer + 12, size - 12).slice())); e.code = CODES[status] ?? 'unknown'; throw e; }
      if (!count) return null;
      if (discard) return {discarded: true};
      const h = Array.from({length: 8}, (_, j) => view.getUint32(pointer + 12 + 4 * j, true));
      let c = pointer + 44;
      const take = n => { const p = copyShared(memory.buffer, c, n); c += 2 * n; return p; };
      return {width: h[0], height: h[1], bitDepth: h[2], chromaFormat: h[3], poc: h[4] | 0, Y: take(h[5]), U: take(h[6]), V: take(h[7])};
    } finally { if (!trapped) wasm.aether_video_free(pointer, size); }
  }
  return {
    get memoryBytes() { return memory.buffer.byteLength; },
    threads, workers,
    createSession(epoch = 0) {
      const handle = call(() => threads > 1 ? wasm.aether_hevc_session_new_threads(epoch, threads) : wasm.aether_video_session_new(epoch));
      if (!handle) throw new Error('At most two active sessions per instance');
      let currentEpoch = epoch, closed = false;
      return {
        get epoch() { return currentEpoch; },
        pushNal(bytes, e = currentEpoch) {
          if (closed) throw new Error('Session closed');
          if (!(bytes instanceof Uint8Array) || bytes.length === 0 || bytes.length > MAX_INPUT) throw new RangeError('Complete 1..8MiB raw NAL required');
          call(() => {
            const ptr = wasm.aether_video_alloc(bytes.length);
            try { new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes); result(wasm.aether_video_session_push(handle, ptr, bytes.length, e)); }
            finally { if (!trapped) wasm.aether_video_free(ptr, bytes.length); }
          });
        },
        pull(e = currentEpoch, discard = false) { const f = call(() => result(wasm.aether_video_session_next(handle, e), discard)); return f ? {...f, epoch: e} : null; },
        finish(e = currentEpoch) { call(() => result(wasm.aether_video_session_finish(handle, e))); },
        reset(e) { call(() => result(wasm.aether_video_session_reset(handle, e))); currentEpoch = e; },
        close() { if (closed) return; closed = true; if (!trapped) call(() => wasm.aether_video_session_close(handle)); },
      };
    },
    /** The threads block in Atomics.wait inside wasm and cannot answer messages: terminate them. */
    stop() { for (const w of pool) w.terminate(); pool.length = 0; },
  };
}
