// Speed (and a native-vs-wasm cross check) of the wasm32 build under Node.
//
//   RUSTFLAGS="-C target-feature=+simd128" cargo build --release --target wasm32-unknown-unknown
//   node tests/wasm_bench.mjs target/wasm32-unknown-unknown/release/aether_joc.wasm stream.eac3 [iterations]
import fs from 'node:fs';

const [wasmPath, streamPath, itersArg] = process.argv.slice(2);
const iters = Number(itersArg ?? 5);
const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
const x = instance.exports;
const stream = fs.readFileSync(streamPath);
const base = x.joc_alloc(stream.length);
new Uint8Array(x.memory.buffer, base, stream.length).set(stream);
const count = x.joc_split(base, stream.length);
const units = Array.from(new Uint32Array(x.memory.buffer, x.joc_units_ptr(), count * 2));

function run(withObjects) {
  x.joc_init();
  let samples = 0, sum = 0, nobj = 0;
  for (let i = 0; i < count; i++) {
    const r = x.joc_decode(base + units[2 * i], units[2 * i + 1], withObjects);
    if (r < 0) throw new Error(`decode failed ${r} at ${i}`);
    samples += x.joc_bed_frames();
    if (r > 0) {
      nobj = r;
      const f = x.joc_frames();
      const p = new Float32Array(x.memory.buffer, x.joc_objects_ptr(), r * f);
      sum += p[3] + p[f + 5] + p[(r - 1) * f + 7]; // cheap checksum so nothing is optimised away
    }
  }
  return { samples, sum, nobj };
}
function time(withObjects) {
  let best = Infinity, r;
  for (let k = 0; k < iters; k++) { const t = process.hrtime.bigint(); r = run(withObjects); best = Math.min(best, Number(process.hrtime.bigint() - t) / 1e9); }
  return { best, ...r };
}
const bed = time(0);
const both = time(1);
const secs = both.samples / 48000;
console.log(`audio ${secs.toFixed(1)} s, ${count} access units, ${both.nobj} objects`);
console.log(`E-AC-3 decode only : ${(secs / bed.best).toFixed(1)}x realtime (${(bed.best * 1e3).toFixed(1)} ms)`);
console.log(`E-AC-3 + JOC       : ${(secs / both.best).toFixed(1)}x realtime (${(both.best * 1e3).toFixed(1)} ms)`);
console.log(`object-only share  : ${(secs / (both.best - bed.best)).toFixed(1)}x realtime`);
console.log(`checksum ${both.sum.toFixed(6)}`);
