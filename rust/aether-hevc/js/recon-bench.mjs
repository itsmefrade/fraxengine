// Dev tool: CPU cost of the reconstruction command stream on WebAssembly (single thread, Node).
// Usage: node js/recon-bench.mjs <wasm built with --features recon-stream> <file.hevc> [--frames N] [--repeat R]
//   Runs mode 0 (full decode, reference output), 1 (full + recording + packing), 2 (entropy only + packing),
//   3 (entropy only, records dropped unpacked); --modes 0,1,2,3 picks.
import fs from 'node:fs';
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i < 0 ? d : args[i + 1]; };
const [wasmPath, file] = args;
const frames = +opt('--frames', 1e9), repeat = +opt('--repeat', 1);
const modes = (opt('--modes', '0,1,2')).split(',').map(Number);
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
const nals = splitAnnexB(new Uint8Array(fs.readFileSync(file)))
  .filter(n => ((n[0] >> 1) & 63) < 62 && (((n[0] & 1) << 5) | (n[1] >> 3)) === 0);
const name = ['full', 'record', 'entropy', 'entropy-nopack'];
for (const mode of modes) {
  let best = Infinity, n = 0, bytes = 0;
  for (let r = 0; r < repeat; r++) {
    // A fresh instance per run: equal memory state for every mode.
    const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
    const x = instance.exports;
    x.aether_recon_bench_new(mode);
    const c0 = process.cpuUsage(), t0 = performance.now();
    n = 0;
    for (const nal of nals) {
      const p = x.aether_video_alloc(nal.length);
      new Uint8Array(x.memory.buffer, p, nal.length).set(nal);
      n = x.aether_recon_bench_push(p, nal.length);
      x.aether_video_free(p, nal.length);
      if (n === 0xffffffff) throw new Error('decode error');
      if (n >= frames) break;
    }
    if (n < frames) n = x.aether_recon_bench_finish();
    const ms = performance.now() - t0, cpu = process.cpuUsage(c0);
    bytes = x.aether_recon_bench_stream_bytes();
    best = Math.min(best, ms);
    console.log(`${name[mode]} run ${r}: ${n} frames ${ms.toFixed(0)} ms = ${(n * 1000 / ms).toFixed(2)} fps, cpu ${((cpu.user + cpu.system) / 1000).toFixed(0)} ms, stream ${(bytes / n / 1e6).toFixed(2)} MB/frame`);
  }
  console.log(`BEST ${name[mode]} ${(n * 1000 / best).toFixed(2)} fps (${(best / n).toFixed(2)} ms/frame)`);
}
