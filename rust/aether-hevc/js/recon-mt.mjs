// Dev tool: the entropy-only recon session (recon_abi.rs) of the `build-wasm.sh --recon-threads` module in Node, with the
// CTB rows of WPP slices on host workers (worker_threads). Prints the decode time and the FNV-1a hash of the packed stream
// in the form of `reconstream bench --mode entropy` (events up to the last NAL, before the final flush), which must equal
// the native hash for every thread count, then the hash including the flush.
// Usage: node js/recon-mt.mjs <wasm> <file.hevc> [--threads T] [--workers W] [--repeat R]
// T: wavefront row threads (pictures stay sequential). W: host workers (default T - 1: the calling thread decodes a row too).
import fs from 'node:fs';
import {Worker, isMainThread, workerData} from 'node:worker_threads';

function futexImports(memory) {
  let view = new Int32Array(memory.buffer);
  const v = addr => { if (addr >= view.byteLength) view = new Int32Array(memory.buffer); return view; };
  return {
    aether_futex_wait(addr, expected) { Atomics.wait(v(addr), addr >>> 2, expected); },
    aether_futex_wake(addr, count) { Atomics.notify(v(addr), addr >>> 2, count < 0 ? Infinity : count); },
  };
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
  const threads = +opt('--threads', 1), workers = +opt('--workers', threads - 1), repeat = +opt('--repeat', 1);
  const module = new WebAssembly.Module(fs.readFileSync(wasmPath));
  const memory = new WebAssembly.Memory({initial: 1024, maximum: 16384, shared: true});
  const w = new WebAssembly.Instance(module, {env: {memory, ...futexImports(memory)}}).exports;
  for (let i = 0; i < workers; i++) new Worker(new URL(import.meta.url), {workerData: {module, memory, index: i, stackTop: w.aether_thread_stack(1 << 20)}});
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

  // 64-bit FNV-1a over u32 words as in reconstream (hash ^= w; hash *= 0x100000001b3), on two 32-bit halves.
  let lo = 0x84222325, hi = 0xcbf29ce4;
  const fnv = words => {
    for (let i = 0; i < words.length; i++) {
      lo = (lo ^ words[i]) >>> 0;
      const l = lo * 0x1b3, carry = Math.floor(l / 4294967296);
      hi = (hi * 0x1b3 + carry + ((lo << 8) >>> 0)) >>> 0;
      lo = l >>> 0;
    }
  };
  const hex = () => hi.toString(16).padStart(8, '0') + lo.toString(16).padStart(8, '0');
  let hashMs = 0;
  const take = hash => {
    const n = w.aether_recon_session_out_words();
    if (n && hash) {
      const t = performance.now();
      fnv(new Uint32Array(memory.buffer, w.aether_recon_session_out_ptr(), n));
      hashMs += performance.now() - t;
    }
    w.aether_recon_session_out_clear();
  };
  for (let run = 0; run < repeat; run++) {
    const hash = run === 0;
    hashMs = 0; lo = 0x84222325; hi = 0xcbf29ce4;
    w.aether_recon_session_reset_threads(threads);
    const t0 = performance.now(), c0 = process.cpuUsage();
    for (const nal of nals) {
      const ptr = w.aether_video_alloc(nal.length);
      new Uint8Array(memory.buffer, ptr, nal.length).set(nal);
      const r = w.aether_recon_session_push(ptr, nal.length);
      w.aether_video_free(ptr, nal.length);
      if (r) throw new Error(`push failed (${r})`);
      take(hash);
    }
    const before = hex();
    if (w.aether_recon_session_finish()) throw new Error('finish failed');
    take(hash);
    const ms = performance.now() - t0 - hashMs, cpu = process.cpuUsage(c0), cpuMs = (cpu.user + cpu.system) / 1000 - hashMs;
    console.log(`threads ${threads} workers ${workers} run ${run}: ${ms.toFixed(0)} ms, cpu ${cpuMs.toFixed(0)} ms (${(cpuMs / ms).toFixed(2)} cores)` +
      (hash ? `, stream ${before} (with flush ${hex()})` : ''));
  }
  process.exit(0);
}
