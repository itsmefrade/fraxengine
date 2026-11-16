// Speed measurement of the wasm32 build in Node.
// Usage: node tools/wasm_bench.mjs target/wasm32-unknown-unknown/release/aether_dtsuhd.wasm [packets.pkts]
import fs from 'node:fs';
const wasmPath = process.argv[2];
const pkts = process.argv[3];
const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
const x = instance.exports;
const frames = 2000;
let t = performance.now();
const sum = x.dtsuhd_bench_synthetic(frames, 1);
let dt = (performance.now() - t) / 1000;
const audio = frames * 1024 / 48000;
console.log(`wasm32 synthetic 5.1.4 full load: ${frames} frames (${audio.toFixed(1)} s audio) in ${dt.toFixed(3)} s -> ${(audio / dt).toFixed(0)}x real time (checksum ${sum})`);
if (pkts) {
  const data = fs.readFileSync(pkts);
  const idx = JSON.parse(fs.readFileSync(pkts + '.idx', 'utf8'));
  const dec = x.dtsuhd_decoder_new();
  const out = x.dtsuhd_alloc(16 * 1024 * 4);
  let off = 0, ok = 0;
  t = performance.now();
  for (const n of idx) {
    const p = x.dtsuhd_alloc(n);
    new Uint8Array(x.memory.buffer, p, n).set(data.subarray(off, off + n));
    const r = x.dtsuhd_decode(dec, p, n, out);
    if (r > 0) ok++;
    x.dtsuhd_free(p, n);
    off += n;
  }
  dt = (performance.now() - t) / 1000;
  const a2 = idx.length * 1024 / 48000;
  console.log(`wasm32 lab sample: ${ok}/${idx.length} frames, ${dt.toFixed(3)} s for ${a2.toFixed(1)} s of audio -> ${(a2 / dt).toFixed(0)}x real time`);
  const lfe = new Float32Array(x.memory.buffer, out + 5 * 1024 * 4, 1024);
  console.log('last frame LFE sample[0..4]:', Array.from(lfe.slice(0, 4)));
}
