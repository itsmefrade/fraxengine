// Raw-ABI harness: decodes a raw Annex G VC-1 stream with the wasm build of aether-vc1 under Node,
// optionally verifies per-frame MD5 against an ffmpeg framemd5 list, and reports decode speed.
//
//   node tests/wasm/run.mjs <aether_vc1.wasm> <stream.vc1> [--frames N] [--ref hashes.txt] [--fps 23.976]
//
// Speed is measured with MD5 checking disabled when --ref is absent.
import fs from 'node:fs';
import crypto from 'node:crypto';

const args = process.argv.slice(2);
const wasmPath = args[0];
const streamPath = args[1];
let maxFrames = Infinity;
let refPath = null;
let fps = 24000 / 1001;
for (let i = 2; i < args.length; i++) {
  if (args[i] === '--frames') maxFrames = parseInt(args[++i]);
  else if (args[i] === '--ref') refPath = args[++i];
  else if (args[i] === '--fps') fps = parseFloat(args[++i]);
}
const refs = refPath ? fs.readFileSync(refPath, 'utf8').split(/\s+/).filter(Boolean) : null;

const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
const x = instance.exports;
const mem = () => new Uint8Array(x.memory.buffer);
const h = x.vc1_create();

const CHUNK = 4 << 20;
const fd = fs.openSync(streamPath, 'r');
const total = fs.fstatSync(fd).size;
const inPtr = x.vc1_alloc(CHUNK * 2);
const outPtr = x.vc1_alloc(4 * 2 * 4096);
let carry = Buffer.alloc(0);
let pos = 0;
let nframes = 0;
let mismatches = 0;
let decodeMs = 0;
let md5Ms = 0;
let ptsBase = 0;

function drain() {
  while (x.vc1_next_frame(h)) {
    nframes++;
    if (refs) {
      const t = performance.now();
      const w = x.vc1_frame_info(h, 0), hh = x.vc1_frame_info(h, 1);
      const md5 = crypto.createHash('md5');
      for (let p = 0; p < 3; p++) {
        const ptr = x.vc1_frame_plane(h, p);
        const stride = x.vc1_frame_info(h, 8 + p);
        const pw = p ? (w + 1) >> 1 : w, ph = p ? (hh + 1) >> 1 : hh;
        const m = mem();
        for (let y = 0; y < ph; y++) md5.update(m.subarray(ptr + y * stride, ptr + y * stride + pw));
      }
      const hex = md5.digest('hex');
      if (nframes - 1 < refs.length && refs[nframes - 1] !== hex) {
        mismatches++;
        if (mismatches < 5) console.log(`MISMATCH frame ${nframes - 1}`);
      }
      md5Ms += performance.now() - t;
    }
  }
}

function feed(buf, last) {
  // split into access units; hold back the final one unless this is the end of the file
  mem().set(buf, inPtr);
  const n = x.vc1_split(inPtr, buf.length, outPtr, 4096);
  // copy: the wasm memory may grow (detaching the old ArrayBuffer) while decoding
  const pairs = Array.from(new Uint32Array(x.memory.buffer, outPtr, 2 * n));
  const complete = last ? n : n - 1;
  let consumed = 0;
  for (let i = 0; i < complete && nframes < maxFrames; i++) {
    const a = pairs[2 * i], b = pairs[2 * i + 1];
    const t = performance.now();
    x.vc1_decode(h, inPtr + a, b - a, ptsBase++, 0);
    decodeMs += performance.now() - t;
    drain();
    consumed = b;
  }
  return n > 0 && complete > 0 ? consumed : 0;
}

while (pos < total && nframes < maxFrames) {
  const len = Math.min(CHUNK, total - pos);
  const chunk = Buffer.alloc(len);
  fs.readSync(fd, chunk, 0, len, pos);
  pos += len;
  const buf = Buffer.concat([carry, chunk]);
  const consumed = feed(buf, pos >= total);
  carry = buf.subarray(consumed);
}
if (nframes < maxFrames) {
  const t = performance.now();
  x.vc1_flush(h);
  decodeMs += performance.now() - t;
  drain();
}
const secs = decodeMs / 1000;
const rate = nframes / secs;
console.log(JSON.stringify({
  frames: nframes, decode_seconds: +secs.toFixed(3), fps: +rate.toFixed(1), x_realtime: +(rate / fps).toFixed(2),
  checked: !!refs, mismatches, errors: x.vc1_error_count(h), md5_ms: Math.round(md5Ms),
}));
if (mismatches) process.exit(1);
