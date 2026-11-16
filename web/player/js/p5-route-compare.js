// SPDX-License-Identifier: LGPL-3.0-only
// Profile 5 route comparison. For display frames K of fixtures/dv-p5.mp4, the same picture through three routes of FelCompositor, each
// to an rgba16float target (rec2100-pq output, tone mapping off, no trims), read back and compared in 10-bit PQ codes:
//   exact      the decoded planes (FFmpeg's decode from tools/p5-prep.mjs, what the WASM base layer delivers) -> COMPOSE -> DISPLAY
//   lean       the hardware VideoFrame (WebCodecs, imported as external texture) -> the one pass route (mapping LUT in DISPLAY_LEAN_WGSL)
//   hardware   the same VideoFrame -> RECOVER -> COMPOSE -> DISPLAY (options.lean false)
// plus the recovered base layer codes against the exact ones (what the import costs before any colour maths).
// The RPU is parsed here in decode order from the start of the stream, as the player does. Readback only: nothing here looks at a screen.
import { FelCompositor } from '../../fel/fel-compositor.mjs';
import { initEngine, openDemuxer, splitDolbyVision, relabelPacketSps, relabelForChrome, P5_CARRIER } from '../../engine/demuxer.mjs';
import { createDoviSession } from '../../engine/dovi.mjs';
import { hvccFullRange } from '../../fel/sps-range.mjs';

const q = new URLSearchParams(location.search);
const out = document.getElementById('out');
const log = s => { out.textContent += s + '\n'; console.log(s); };
const W = 3840, H = 2160;
const K = (q.get('frames') || '0,449,898,1347,1790').split(',').map(Number);
const DIR = q.get('dir') || 'p5spread';
const half = b => { const s = b & 0x8000 ? -1 : 1, e = (b >> 10) & 31, m = b & 1023; return e === 0 ? s * 2 ** -14 * (m / 1024) : e === 31 ? NaN : s * 2 ** (e - 15) * (1 + m / 1024); };
const HALF = Float32Array.from({ length: 65536 }, (_, i) => half(i));
const fetchBytes = async url => { const r = await fetch(url); if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return new Uint8Array(await r.arrayBuffer()); };

// Differences of two readbacks in 10-bit codes of the PQ encoded R'G'B'. `edge` separately counts pixels where the exact picture's chroma
// changes by more than 8 codes to a neighbour (the place where the chroma filters of the routes can differ).
function diff(a, b, planes) {
  const bpr = a.bytesPerRow / 2, x = a.halfWords, y = b.halfWords, hist = new Uint32Array(1024);
  let max = 0, sum = 0, n = 0, at = null, flatMax = 0, flatSum = 0, flatN = 0;
  const cw = W / 2, ch = H / 2, U = planes.U, V = planes.V;
  for (let r = 0; r < H; r++) for (let c = 0; c < W; c++) {
    const cx = c >> 1, cy = r >> 1, i = cy * cw + cx, l = cy * cw + Math.max(cx - 1, 0), rr = cy * cw + Math.min(cx + 1, cw - 1), u = Math.max(cy - 1, 0) * cw + cx, d = Math.min(cy + 1, ch - 1) * cw + cx;
    const flat = Math.max(Math.abs(U[i] - U[l]), Math.abs(U[i] - U[rr]), Math.abs(U[i] - U[u]), Math.abs(U[i] - U[d]), Math.abs(V[i] - V[l]), Math.abs(V[i] - V[rr]), Math.abs(V[i] - V[u]), Math.abs(V[i] - V[d])) <= 8;
    for (let k = 0; k < 3; k++) {
      const e = Math.abs(HALF[x[r * bpr + c * 4 + k]] - HALF[y[r * bpr + c * 4 + k]]) * 1023;
      sum += e; n++; hist[Math.min(1023, Math.round(e * 4))]++;
      if (e > max) { max = e; at = [c, r, k]; }
      if (flat) { flatSum += e; flatN++; if (e > flatMax) flatMax = e; }
    }
  }
  const pct = p => { let acc = 0; for (let i = 0; i < 1024; i++) { acc += hist[i]; if (acc >= n * p) return i / 4; } return 255; };
  let over2 = 0; for (let i = 9; i < 1024; i++) over2 += hist[i];
  return { max: +max.toFixed(3), mean: +(sum / n).toFixed(4), p99: pct(0.99), p999: pct(0.999), over2CodesPct: +(100 * over2 / n).toFixed(4), maxAt: at,
    flatChroma: { sharePct: +(100 * flatN / n).toFixed(2), max: +flatMax.toFixed(3), mean: +(flatSum / Math.max(flatN, 1)).toFixed(4) } };
}

// Hardware decode of the base layer from the start of the file; resolves with the VideoFrames of the wanted display indices.
async function hardwareFrames(dovi) {
  await initEngine();
  const bytes = await fetchBytes('fixtures/dv-p5.mp4');
  const d = await openDemuxer(bytes);
  let track = d.tracks().find(t => t.kind === 'video');
  const ls = track.description && track.description.length > 21 ? (track.description[21] & 3) + 1 : 4;
  const carrier = q.get('carrier') === 'sdr' ? undefined : P5_CARRIER;     // carrier=sdr: the SDR carrier (the stream's own matrix and range kept)
  track = relabelForChrome(track, carrier).track;
  const fullRange = hvccFullRange(track.description, { primaries: 1, transfer: 13 }) === true;
  const kmax = Math.max(...K), pk = [];
  for (let p, n = 0; n < kmax + 40 && (p = d.nextPacket());) {
    if (p.trackId !== track.id) continue;
    n++;
    const sp = splitDolbyVision(p, ls);
    const r = sp.rpu ? dovi.parseRpu(sp.rpu) : null;
    pk.push({ pts: p.ptsUs, key: p.key, data: relabelPacketSps(sp.bl.length ? sp.bl : p.data, ls, carrier).data.slice(), display: r && r.display, tables: r && r.tables && r.tables.slice() });
  }
  d.close();
  const order = pk.map((x, i) => i).sort((a, b) => pk[a].pts - pk[b].pts);
  const want = new Map(K.map(k => [pk[order[k]].pts, k]));
  const got = new Map();
  let codec = null;
  const list = [];
  for (const c of [track.fallbackCodecString, track.codec]) if (c && !list.includes(c)) { list.push(c); if (/^hvc1\..*\.H/.test(c)) list.push(c.replace('.H', '.L')); }
  const base = { codedWidth: track.width, codedHeight: track.height, hardwareAcceleration: 'prefer-hardware', ...(track.description ? { description: track.description } : {}) };
  for (const c of list) { if ((await VideoDecoder.isConfigSupported({ ...base, codec: c })).supported) { codec = c; break; } }
  if (!codec) throw new Error('no hardware decoder for ' + list.join(', '));
  let err = null, outN = 0;
  const dec = new VideoDecoder({ output: f => { outN++; const k = want.get(f.timestamp); if (k === undefined) f.close(); else got.set(k, f); }, error: e => { err = e; } });
  dec.configure({ ...base, codec, optimizeForLatency: true });
  for (const p of pk) {
    if (err) throw err;
    dec.decode(new EncodedVideoChunk({ type: p.key ? 'key' : 'delta', timestamp: p.pts, data: p.data }));
    while (dec.decodeQueueSize > 8) await new Promise(r => setTimeout(r, 2));
  }
  await dec.flush(); dec.close();
  if (err) throw err;
  return { got, meta: new Map(K.map(k => [k, pk[order[k]]])), codec, fullRange, decoded: outN, packets: pk.length };
}

export async function runCompare() {
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  const device = await adapter.requestDevice({ requiredLimits: { maxStorageBufferBindingSize: Math.min(adapter.limits.maxStorageBufferBindingSize, 1 << 30), maxBufferSize: Math.min(adapter.limits.maxBufferSize, 1 << 30) } });
  const mk = async lean => {
    const target = device.createTexture({ size: [W, H], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
    const comp = await FelCompositor.create(device, null, { target, peakNits: 1000, canvasMaxWidth: 3840, lean });
    comp.canvasConfig = { colorSpace: 'rec2100-pq' };
    return { comp, target };
  };
  const one = await mk(true), three = await mk(false);
  const dovi = await createDoviSession(); dovi.reset();
  const hw = await hardwareFrames(dovi);
  const res = { test: 'p5-route-compare', userAgent: navigator.userAgent, adapter: [adapter.info?.vendor, adapter.info?.architecture].filter(Boolean).join(' '), codec: hw.codec, sourceFullRange: hw.fullRange, packets: hw.packets, decoded: hw.decoded, frames: [] };
  log(`decoded ${hw.decoded} of ${hw.packets} pictures, ${hw.codec}, SPS full range ${hw.fullRange}`);
  const rd = async c => { await c.comp.whenIdle(); return c.comp.readTarget(W, H); };
  for (const k of K) {
    const vf = hw.got.get(k), m = hw.meta.get(k);
    if (!vf) throw new Error(`frame ${k}: no hardware picture`);
    const [yuv, ref] = await Promise.all([fetchBytes(`fixtures/${DIR}/bl-${k}.yuv`), fetch(`fixtures/${DIR}/rpu-${k}.json`).then(r => r.json())]);
    const codes = new Uint16Array(yuv.buffer, yuv.byteOffset, yuv.byteLength / 2);
    const planes = { Y: codes.subarray(0, W * H), U: codes.subarray(W * H, W * H * 5 / 4), V: codes.subarray(W * H * 5 / 4) };
    const refTables = new Int32Array(Uint8Array.from(atob(ref.tablesB64), c => c.charCodeAt(0)).buffer);
    let tablesEqual = refTables.length === m.tables.length && ref.ptsUs === m.pts;
    for (let i = 0; tablesEqual && i < refTables.length; i++) if (refTables[i] !== m.tables[i]) tablesEqual = false;
    const info = { profile: 'p5', toneMapping: 'off', trims: false };
    const hwInfo = { ...info, sourceFullRange: hw.fullRange };
    const mode = {};
    mode.exact = three.comp.present({ planes, width: W, height: H }, null, m.tables, m.display, info)?.mode ?? 'three stages';
    const a = await rd(three);
    mode.lean = one.comp.present(vf, null, m.tables, m.display, hwInfo)?.mode;
    const b = await rd(one);
    mode.hardware = three.comp.present(vf, null, m.tables, m.display, hwInfo)?.mode ?? 'three stages';
    const c = await rd(three);
    // the recovered codes of the hardware picture against FFmpeg's (three stage route, read after its present)
    const rec = await three.comp.readBaseCodes();
    // split by whether the exact triplet (nearest chroma sample) is a displayable RGB of the full range BT.2020 matrix Chrome converts with:
    // outside it the import clips, and the codes cannot come back
    let ydiff = 0, ymax = 0, inN = 0, inDiff = 0, inMax = 0, outN = 0, outDiff = 0, bias = 0; const inHist = new Array(8).fill(0);
    for (let r = 0; r < H; r++) for (let c = 0; c < W; c++) {
      const i = r * W + c, ci = (r >> 1) * (W / 2) + (c >> 1);
      const y = planes.Y[i] / 1023, cb = (planes.U[ci] - 512) / 1023, cr = (planes.V[ci] - 512) / 1023;
      const R = y + 1.4746 * cr, B = y + 1.8814 * cb, G = (y - 0.2627 * R - 0.0593 * B) / 0.678;
      const lo = Math.min(R, G, B), hi = Math.max(R, G, B), e = Math.abs(rec[i] - planes.Y[i]);
      if (e) { ydiff++; if (e > ymax) ymax = e; }
      if (lo >= 0.02 && hi <= 0.98) { inN++; inHist[Math.min(e, 7)]++; bias += rec[i] - planes.Y[i]; if (e) { inDiff++; if (e > inMax) inMax = e; } }
      else if (lo < -0.01 || hi > 1.01) { outN++; if (e) outDiff++; }
    }
    // mean signed error of the recovered codes per 64 code bucket of the exact code, per plane (a scale error of the import shows as a slope)
    const slope = [planes.Y, planes.U, planes.V].map((P, pl) => {
      const off = pl === 0 ? 0 : pl === 1 ? W * H : W * H * 5 / 4, sum = new Float64Array(16), n = new Uint32Array(16);
      for (let i = 0; i < P.length; i++) { const b = P[i] >> 6; sum[b] += rec[off + i] - P[i]; n[b]++; }
      return Array.from(sum, (v, b) => n[b] > 2000 ? +(v / n[b]).toFixed(2) : null);
    });
    // luma error where chroma is flat and the exact RGB lies at least 0.1 inside the cube, by code, chroma code, position
    const strict = { sum: new Float64Array(32), n: new Uint32Array(32), hist: new Array(9).fill(0), by: { y: new Map(), u: new Map(), v: new Map(), parity: new Map(), row: new Map() } };     // luma only, RGB at least 0.1 inside the cube; hist: error -4 .. +4
    {
      const cw = W / 2, ch = H / 2, U = planes.U, V = planes.V;
      for (let cy = 1; cy < ch - 1; cy++) for (let cx = 1; cx < cw - 1; cx++) {
        const ci = cy * cw + cx, u = U[ci], v = V[ci];
        if (Math.abs(u - U[ci - 1]) > 1 || Math.abs(u - U[ci + 1]) > 1 || Math.abs(u - U[ci - cw]) > 1 || Math.abs(u - U[ci + cw]) > 1 || Math.abs(v - V[ci - 1]) > 1 || Math.abs(v - V[ci + 1]) > 1 || Math.abs(v - V[ci - cw]) > 1 || Math.abs(v - V[ci + cw]) > 1) continue;
        const i = cy * 2 * W + cx * 2, yv = planes.Y[i];
        const y = yv / 1023, cb = (u - 512) / 1023, cr = (v - 512) / 1023, R = y + 1.4746 * cr, B = y + 1.8814 * cb, G = (y - 0.2627 * R - 0.0593 * B) / 0.678;
        if (Math.min(R, G, B) >= 0.1 && Math.max(R, G, B) <= 0.9) { const b = yv >> 5, e = rec[i] - yv; strict.sum[b] += e; strict.n[b]++; const add = (k, key) => { const m = strict.by[k]; const v = m.get(key) || [0, 0]; v[0] += e; v[1]++; m.set(key, v); }; add('y', yv); add('u', u); add('v', v); add('parity', (cx & 1) + 2 * (cy & 1)); add('row', cy >> 6); strict.hist[Math.max(0, Math.min(8, e + 4))]++; }
      }
    }
    vf.close();
    const curve = [0, 1, 2].map(p => ({ pieces: m.tables[64 + p * 32] - 1, identity: (() => { for (let i = 0; i < 1024; i++) if (m.tables[688 + p * 1024 + i] !== i * 64) return false; return true; })() }));
    const row = { frame: k, ptsUs: m.pts, mode, recoveredErrorPerBucket: slope, strictLuma: { by: Object.fromEntries(Object.entries(strict.by).map(([k, m]) => [k, [...m].filter(x => x[1][1] > 3000).sort((p, q) => p[0] - q[0]).map(([key, v]) => [key, +(v[0] / v[1]).toFixed(2), v[1]])])), hist: strict.hist, meanPer32: Array.from(strict.sum, (v, b) => strict.n[b] > 500 ? +(v / strict.n[b]).toFixed(3) : null) }, tablesEqualPrep: tablesEqual, curve, hwLumaCodesDiffer: { count: ydiff, max: ymax, inGamut: { pixels: inN, differ: inDiff, max: inMax, histogram: inHist, meanSigned: +(bias / Math.max(inN, 1)).toFixed(4) }, outOfGamut: { pixels: outN, differ: outDiff } }, leanVsExact: diff(b, a, planes), hardwareVsExact: diff(c, a, planes), leanVsHardware: diff(b, c, planes) };
    res.frames.push(row);
    log(`frame ${k}: lean vs exact max ${row.leanVsExact.max} mean ${row.leanVsExact.mean} p99 ${row.leanVsExact.p99}; lean vs hardware max ${row.leanVsHardware.max} mean ${row.leanVsHardware.mean}`);
    log(`frame ${k}: hardware vs exact max ${row.hardwareVsExact.max} mean ${row.hardwareVsExact.mean} p99 ${row.hardwareVsExact.p99} p99.9 ${row.hardwareVsExact.p999}; recovered luma differs on ${ydiff} pixels (max ${ymax})`);
  }
  const agg = key => ({ max: Math.max(...res.frames.map(f => f[key].max)), mean: +(res.frames.reduce((s, f) => s + f[key].mean, 0) / res.frames.length).toFixed(4), p99: Math.max(...res.frames.map(f => f[key].p99)), p999: Math.max(...res.frames.map(f => f[key].p999)), flatChromaMax: Math.max(...res.frames.map(f => f[key].flatChroma.max)) });
  res.summary = { leanVsExact: agg('leanVsExact'), hardwareVsExact: agg('hardwareVsExact'), leanVsHardware: agg('leanVsHardware') };
  log(JSON.stringify(res.summary));
  window.p5RouteCompare = res;
  return res;
}
window.runCompare = runCompare;
const post = result => fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ test: 'p5-route-compare', name: q.get('name') || '', result }) });
if (q.has('auto')) runCompare().then(post).catch(e => { log('ERROR ' + (e.stack || e)); post({ error: String(e.stack || e) }); });
