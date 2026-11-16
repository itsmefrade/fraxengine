// Dev tool: single-thread WASM decode in Node through the session API (same flow as web/exact/worker.js).
// Usage: node js/node-bench.mjs <wasm> <file.hevc> [--frames N] [--hash out.txt] [--session path/to/session.mjs] [--repeat R]
import fs from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';
import {pathToFileURL, fileURLToPath} from 'node:url';
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i < 0 ? d : args[i + 1]; };
const wasmPath = path.resolve(args[0]), file = args[1];
const frames = +opt('--frames', 1e9), hashOut = opt('--hash', null), repeat = +opt('--repeat', 1);
const here = path.dirname(fileURLToPath(import.meta.url));
const sessionPath = path.resolve(opt('--session', path.join(here, '../../../web/exact/session.mjs')));
const realFetch = globalThis.fetch;
globalThis.fetch = async (u, o) => {
  const s = String(u);
  if (s.startsWith('file:')) { const b = fs.readFileSync(fileURLToPath(s)); return {ok: true, arrayBuffer: async () => b.buffer.slice(b.byteOffset, b.byteOffset + b.length)}; }
  return realFetch(u, o);
};
const {loadAetherVideoSessions} = await import(pathToFileURL(sessionPath));
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
const data = new Uint8Array(fs.readFileSync(file));
const nals = splitAnnexB(data).filter(n => ((n[0] >> 1) & 63) < 62 && (((n[0] & 1) << 5) | (n[1] >> 3)) === 0);
const api = await loadAetherVideoSessions(pathToFileURL(wasmPath));
let best = Infinity;
for (let r = 0; r < repeat; r++) {
  const session = api.createSession(r);
  const lines = []; let n = 0, hashMs = 0;
  const emit = f => {
    n++;
    if (hashOut && r === 0) {
      const t = performance.now();
      lines.push([f.Y, f.U, f.V].map(p => crypto.createHash('sha256').update(new Uint8Array(p.buffer, p.byteOffset, p.byteLength)).digest('hex')).join(' '));
      hashMs += performance.now() - t;
    }
  };
  const drain = () => { let f; while (n < frames && (f = session.pull())) emit(f); };
  const t0 = performance.now(); const c0 = process.cpuUsage();
  for (const nal of nals) {
    if (n >= frames) break;
    for (;;) {
      try { session.pushNal(nal); break; }
      catch (e) { if (e.code === 'backpressure') { drain(); continue; } throw e; }
    }
    drain();
  }
  if (n < frames) { session.finish(); drain(); }
  const cpu = process.cpuUsage(c0); const cpuMs = (cpu.user + cpu.system) / 1000;
  const ms = performance.now() - t0 - hashMs;
  best = Math.min(best, ms);
  console.log(`run ${r}: ${n} frames ${ms.toFixed(0)} ms = ${(n * 1000 / ms).toFixed(2)} fps (cpu ${(n * 1000 / (cpuMs - (hashOut && r===0 ? hashMs : 0))).toFixed(2)} fps)`);
  if (hashOut && r === 0) fs.writeFileSync(hashOut, lines.join('\n') + '\n');
  session.close();
  if (r === repeat - 1) console.log(`BEST ${(n * 1000 / best).toFixed(2)} fps wasmMem=${typeof api.memoryBytes === 'number' ? api.memoryBytes : 0}`);
}
