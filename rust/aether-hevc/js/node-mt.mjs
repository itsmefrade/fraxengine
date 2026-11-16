// Dev tool: one aether-hevc decoder on several threads in Node (worker_threads), for the `--threads` build
// (dist/aether_hevc_simd_mt.wasm). Same output format as node-bench.mjs (per-frame SHA-256 of Y U V).
// Usage: node js/node-mt.mjs <wasm> <file.hevc> [--threads T] [--workers W] [--frames N] [--hash out.txt]
// T: decoder threads (frame threading, T pictures in flight). W: host workers, at least T + 1 (default T + 1 + T).
import fs from 'node:fs';
import crypto from 'node:crypto';
import {Worker, isMainThread, workerData} from 'node:worker_threads';

// Wasm's futex: the decoder blocks only on these (Atomics.wait is allowed in Node's main thread and in workers).
function futexImports(memory) {
  let view = new Int32Array(memory.buffer);
  const v = addr => { if (addr >= view.byteLength) view = new Int32Array(memory.buffer); return view; };
  return {
    aether_futex_wait(addr, expected) { Atomics.wait(v(addr), addr >>> 2, expected); },
    aether_futex_wake(addr, count) { Atomics.notify(v(addr), addr >>> 2, count < 0 ? Infinity : count); },
  };
}

// Copy out of the shared memory with a plain JS loop: TypedArray.set / SharedArrayBuffer.slice from a shared buffer go through
// V8's relaxed (race-safe) memcpy, measured 8 ms per 1080p 16-bit picture in Node 25 (0.75 GB/s), which made the whole
// threaded decode 70% slower than the single-thread module at one thread. The loop copies 32 bits per element.
function copyShared(buffer, byteOffset, n16) {
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

if (!isMainThread) {
  const {module, memory, index, stackTop} = workerData;
  const {exports} = new WebAssembly.Instance(module, {env: {memory, ...futexImports(memory)}});
  exports.__stack_pointer.value = stackTop;   // own stack before any Rust code runs
  exports.aether_thread_main(index);          // never returns
} else {
  const args = process.argv.slice(2);
  const opt = (k, d) => { const i = args.indexOf(k); return i < 0 ? d : args[i + 1]; };
  const [wasmPath, file] = args;
  const threads = +opt('--threads', 1), workers = +opt('--workers', threads === 1 ? 0 : 2 * threads + 1);
  const frames = +opt('--frames', 1e9), hashOut = opt('--hash', null);
  const module = new WebAssembly.Module(fs.readFileSync(wasmPath));
  const memory = new WebAssembly.Memory({initial: 1024, maximum: 16384, shared: true});
  const w = new WebAssembly.Instance(module, {env: {memory, ...futexImports(memory)}}).exports;
  const pool = [];
  for (let i = 0; i < workers; i++) pool.push(new Worker(new URL(import.meta.url), {workerData: {module, memory, index: i, stackTop: w.aether_thread_stack(1 << 20)}}));
  while (w.aether_thread_idle() < workers) await new Promise(r => setTimeout(r, 5));

  function splitAnnexB(bytes) {
    const n = bytes.length, starts = [];
    for (let i = 0; i + 2 < n;) {
      if (bytes[i + 2] > 1) { i += 3; continue; }
      if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) { starts.push(i + 3); i += 3; } else i++;
    }
    const out = [];
    for (let s = 0; s < starts.length; s++) {
      let end = s + 1 < starts.length ? starts[s + 1] - 3 : n;
      while (end > starts[s] && bytes[end - 1] === 0) end--;
      if (end > starts[s]) out.push(bytes.subarray(starts[s], end));
    }
    return out;
  }
  const nals = splitAnnexB(new Uint8Array(fs.readFileSync(file))).filter(n => ((n[0] >> 1) & 63) < 62 && (((n[0] & 1) << 5) | (n[1] >> 3)) === 0);
  // Result block: [len, status, count] then per frame 8 u32 and the Y U V planes (16-bit).
  const result = ptr => {
    const dv = new DataView(memory.buffer), size = dv.getUint32(ptr, true), status = dv.getUint32(ptr + 4, true), count = dv.getUint32(ptr + 8, true);
    let out = null;
    if (status) { const e = new Error(new TextDecoder().decode(new Uint8Array(memory.buffer, ptr + 12, size - 12).slice())); e.code = status; w.aether_video_free(ptr, size); throw e; }
    if (count) {
      const h = Array.from({length: 8}, (_, j) => dv.getUint32(ptr + 12 + 4 * j, true));
      let c = ptr + 44;
      const take = n => { const p = copyShared(memory.buffer, c, n); c += 2 * n; return p; };
      out = {width: h[0], height: h[1], poc: h[4] | 0, Y: take(h[5]), U: take(h[6]), V: take(h[7])};
    }
    w.aether_video_free(ptr, size);
    return out;
  };
  const handle = threads > 1 ? w.aether_hevc_session_new_threads(0, threads) : w.aether_video_session_new(0);
  const lines = []; let n = 0, hashMs = 0;
  const emit = f => {
    n++;
    if (!hashOut) return;
    const t = performance.now();
    lines.push([f.Y, f.U, f.V].map(p => crypto.createHash('sha256').update(new Uint8Array(p.buffer, p.byteOffset, p.byteLength)).digest('hex')).join(' '));
    hashMs += performance.now() - t;
  };
  const drain = () => { let f; while (n < frames && (f = result(w.aether_video_session_next(handle, 0)))) emit(f); };
  const t0 = performance.now(), c0 = process.cpuUsage();
  for (const nal of nals) {
    if (n >= frames) break;
    for (;;) {
      const ptr = w.aether_video_alloc(nal.length);
      new Uint8Array(memory.buffer, ptr, nal.length).set(nal);
      try { result(w.aether_video_session_push(handle, ptr, nal.length, 0)); break; }
      catch (e) { if (e.code === 5) { drain(); continue; } throw e; }
      finally { w.aether_video_free(ptr, nal.length); }
    }
    drain();
  }
  if (n < frames) { result(w.aether_video_session_finish(handle, 0)); drain(); }
  const ms = performance.now() - t0 - hashMs, cpu = process.cpuUsage(c0), cpuMs = (cpu.user + cpu.system) / 1000 - hashMs;
  console.log(`threads ${threads} workers ${workers}: ${n} frames ${ms.toFixed(0)} ms = ${(n * 1000 / ms).toFixed(2)} fps, cpu ${cpuMs.toFixed(0)} ms (${(cpuMs / ms).toFixed(2)} cores), wasmMem ${(memory.buffer.byteLength / 1048576).toFixed(0)} MB`);
  if (hashOut) fs.writeFileSync(hashOut, lines.join('\n') + '\n');
  process.exit(0);
}
