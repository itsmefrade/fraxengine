// Runs the wasm32 build of the decoder under Node, checks it against an
// FFmpeg reference dump and reports speed.
//
//   cd wasm-bench && cargo build --release --target wasm32-unknown-unknown
//   node tests/wasm_bench.mjs wasm-bench/target/wasm32-unknown-unknown/release/aether_ac3_wasm_bench.wasm \
//        tests/fixtures/real-71-channel-check.eac3 [tests/fixtures/real-71-channel-check.ref.f32le] [iterations]
import fs from 'node:fs';

const [wasmPath, streamPath, refPath, itersArg] = process.argv.slice(2);
const iters = Number(itersArg ?? 5);
const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
const x = instance.exports;
const stream = fs.readFileSync(streamPath);

// Copy the stream into wasm memory once and split it into access units.
const base = x.ac3_alloc(stream.length);
new Uint8Array(x.memory.buffer, base, stream.length).set(stream);
const units = [];
for (let pos = 0; pos < stream.length;) {
  const n = x.ac3_first_au_len(base + pos, stream.length - pos);
  if (n === 0) break;
  units.push([pos, n]);
  pos += n;
}

function decodeAll(collect) {
  x.ac3_init(refPath ? 1 : 0); // FFmpeg-compat mode when comparing with an FFmpeg dump
  let samples = 0, rate = 48000, ch = 0, out = [];
  for (const [pos, n] of units) {
    const frames = x.ac3_decode(base + pos, n);
    if (frames < 0) throw new Error(`decode failed: ${frames}`);
    if (frames === 0) continue;
    ch = x.ac3_channels();
    rate = x.ac3_sample_rate();
    samples += frames;
    if (collect) {
      const planes = new Float32Array(x.memory.buffer, x.ac3_output_ptr(), frames * ch);
      const inter = new Float32Array(frames * ch);
      for (let c = 0; c < ch; c++) for (let i = 0; i < frames; i++) inter[i * ch + c] = planes[c * frames + i];
      out.push(inter);
    }
  }
  return { samples, rate, ch, out };
}

if (refPath) {
  const ref = new Float32Array(fs.readFileSync(refPath).buffer.slice(0));
  const { out, ch } = decodeAll(true);
  let off = 0, max = 0;
  for (const f of out) { for (let i = 0; i < f.length; i++) max = Math.max(max, Math.abs(f[i] - ref[off + i])); off += f.length; }
  console.log(`wasm vs FFmpeg: ${off} samples (${ref.length} expected), max abs err ${max.toExponential(2)}`);
}

let best = Infinity, res;
for (let i = 0; i < iters; i++) {
  const t = performance.now();
  res = decodeAll(false);
  best = Math.min(best, performance.now() - t);
}
const secs = res.samples / res.rate;
console.log(`${streamPath}: ${res.ch} ch, ${secs.toFixed(1)} s of audio in ${best.toFixed(1)} ms = ${(secs / (best / 1000)).toFixed(0)}x realtime (wasm, Node ${process.version})`);
