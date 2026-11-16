// Dev tool: single-thread WASM decode time of every GOP task as the pool worker runs it (prefix GOP + GOP, pruned by the plan when --plan).
// Usage: node js/gop-times.mjs <wasm> <session.mjs> <prefix e.g. dev-media/web/p5> [--plan] [--gops N] [--first G]
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL, fileURLToPath} from 'node:url';
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i < 0 ? d : args[i + 1]; };
const usePlan = args.includes('--plan'), maxG = +opt('--gops', 99), firstG = +opt('--first', 0);
globalThis.fetch = async u => { const b = fs.readFileSync(fileURLToPath(String(u))); return {ok: true, arrayBuffer: async () => b.buffer.slice(b.byteOffset, b.byteOffset + b.length)}; };
const {loadAetherVideoSessions} = await import(pathToFileURL(path.resolve(args[1])));
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
const api = await loadAetherVideoSessions(pathToFileURL(path.resolve(args[0])));
const pre = args[2];
const aus = JSON.parse(fs.readFileSync(pre + '-index.json')).aus, bl = fs.readFileSync(pre + '-bl.hevc');
const gops = [];
for (let i = 0; i < aus.length; i++) { if (aus[i].key || !gops.length) gops.push({first: i, count: 0, rasl: 0}); const g = gops[gops.length - 1]; g.count++; if (aus[i].rasl) g.rasl++; }
for (const g of gops) { const a = aus[g.first], b = aus[g.first + g.count - 1]; g.range = [a.bl[0], b.bl[0] + b.bl[1] - a.bl[0]]; }
console.log('gop frames bytesMB mbps tasks_nals decoded_pics skip ms fps_delivered');
const session = api.createSession(0); let epoch = 0, first = true;
for (let j = firstG; j < Math.min(gops.length, firstG + maxG); j++) {
  const g = gops[j], p = j > 0 && g.rasl > 0 ? gops[j - 1] : null;
  const off = p ? p.range[0] : g.range[0], len = p ? g.range[0] + g.range[1] - off : g.range[1];
  const bytes = new Uint8Array(bl.buffer, bl.byteOffset + off, len);
  const nals = splitAnnexB(bytes);
  let skip = p ? p.count - p.rasl : 0;
  const t0 = performance.now();
  const plan = usePlan && skip > 0 ? api.plan(nals) : null;
  if (plan) skip = plan.skip;
  if (!first) session.reset(++epoch); first = false;
  let n = 0, out = 0;
  const drain = () => { let f; while ((f = session.pull())) { out++; if (skip > 0) skip--; else n++; } };
  let pushed = 0;
  for (let i = 0; i < nals.length; i++) {
    if (plan && !plan.keep[i]) continue;
    for (;;) { try { session.pushNal(nals[i]); pushed++; break; } catch (e) { if (e.code === 'backpressure') { drain(); continue; } throw e; } }
    drain();
  }
  session.finish(); drain();
  const ms = performance.now() - t0;
  console.log(j, n, (g.range[1] / 1e6).toFixed(2), (g.range[1] * 8 / g.count * 60 / 1e6).toFixed(0), nals.length, out, plan ? plan.skip : (p ? p.count - p.rasl : 0), ms.toFixed(0), (n * 1000 / ms).toFixed(1));
}
