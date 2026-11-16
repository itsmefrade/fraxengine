// SPDX-License-Identifier: LGPL-3.0-only
// HLG / Dolby Vision Profile 8.4 colour check. For display frames of a P8.4 file (tools/hlg-prep.mjs):
//   input      FFmpeg's decode of the base layer (the raw HLG codes) and the packets the player would decode (SPS relabelled, hardware decoder)
//   baseCodes  the codes RECOVER reads back from the hardware VideoFrame, against FFmpeg's (checks range / matrix handling of Chrome's import)
//   hlg        FelCompositor.present(frame, ..., {transfer: 'hlg'}) -> rgba16float, rec2100-pq out, tone mapping off, peak 1000 nit
//   rpu        the same frame through the RPU mapping tables (the alternative to the HLG path: polynomial / MMR to PQ)
//   reference  float64 JS from BT.2100 (tables 5 and 6) sharing no code with the compositor: limited range BT.2020 NCL Y'CbCr (chroma bilinear,
//              left siting) -> R'G'B' -> inverse OETF -> OOTF (gamma 1.2, alpha 1000 nit, Y_S with BT.2020 weights) -> ST 2084 inverse EOTF
// PSNR is reported in the PQ domain (signal peak 1.0) and in linear light (peak 1000 nit). With ?snap=1 it also renders both paths for an SDR
// style display (peak 203 nit, display-p3 target) and posts 8 bit PNGs.
import { FelCompositor } from './fel-compositor.mjs';
import { hvccFullRange } from './sps-range.mjs';

const q = new URLSearchParams(location.search);
const out = document.getElementById('out');
const log = s => { out.textContent += s + '\n'; console.log(s); };
const name = q.get('name') || 'iphone';
const base = `media/hlg/${name}`;
const meta = await (await fetch(`${base}.json`)).json();
const W = meta.width, H = meta.height;
const K = (q.get('frames') || '5,100,200,280').split(',').map(Number);
const M1 = 2610 / 16384, M2 = 2523 / 32, C1 = 3424 / 4096, C2 = 2413 / 128, C3 = 2392 / 128;
const pqInv = y => { const p = Math.pow(Math.max(y, 0), M1); return Math.pow((C1 + C2 * p) / (1 + C3 * p), M2); };
const pqEotf = e => { const p = Math.pow(Math.min(Math.max(e, 0), 1), 1 / M2); return Math.pow(Math.max(p - C1, 0) / (C2 - C3 * p), 1 / M1); };
const half = b => { const s = b & 0x8000 ? -1 : 1, e = (b >> 10) & 31, m = b & 1023; return e === 0 ? s * 2 ** -14 * (m / 1024) : e === 31 ? NaN : s * 2 ** (e - 15) * (1 + m / 1024); };
const fetchBytes = async url => { const r = await fetch(url); if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return new Uint8Array(await r.arrayBuffer()); };

// ---- independent float64 reference (BT.2100 HLG) ----
const A = 0.17883277, B = 1 - 4 * A, C = 0.5 - A * Math.log(4 * A);
const hlgInv = ep => { const e = Math.min(Math.max(ep, 0), 1); return e <= 0.5 ? e * e / 3 : (Math.exp((e - C) / A) + B) / 12; };
function referenceFrame(planes, peak = 1000) {
  const cw = W / 2, ch = H / 2, gamma = 1.2 + 0.42 * Math.log10(peak / 1000);
  const at = (P, x, y) => P[Math.min(Math.max(y, 0), ch - 1) * cw + Math.min(Math.max(x, 0), cw - 1)];
  const up = (P, x, y) => { const ux = x * 0.5, uy = (y - 0.5) * 0.5, lx = Math.floor(ux), ly = Math.floor(uy), fx = ux - lx, fy = uy - ly; return (at(P, lx, ly) * (1 - fx) + at(P, lx + 1, ly) * fx) * (1 - fy) + (at(P, lx, ly + 1) * (1 - fx) + at(P, lx + 1, ly + 1) * fx) * fy; };
  const KR = 0.2627, KB = 0.0593, KG = 1 - KR - KB;
  const nits = new Float64Array(W * H * 3);
  for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
    const yy = (planes.Y[y * W + x] - 64) / 876, cb = (up(planes.U, x, y) - 512) / 896, cr = (up(planes.V, x, y) - 512) / 896;
    const r = yy + 2 * (1 - KR) * cr, b = yy + 2 * (1 - KB) * cb, g = (yy - KR * r - KB * b) / KG;
    const e = [hlgInv(r), hlgInv(g), hlgInv(b)];
    const ys = Math.max(KR * e[0] + KG * e[1] + KB * e[2], 1e-9), k = peak * Math.pow(ys, gamma - 1), o = (y * W + x) * 3;
    nits[o] = e[0] * k; nits[o + 1] = e[1] * k; nits[o + 2] = e[2] * k;
  }
  return nits;
}

// gpu: rgba16float halfwords of PQ encoded BT.2020 (rec2100-pq out). refNits: Float64Array of display light in nits.
function comparePq(readback, refNits, stride = 1) {
  const bpr = readback.bytesPerRow / 2, hw = readback.halfWords;
  let sqPq = 0, sqLin = 0, n = 0, maxPq = 0, nan = 0, sumPq = 0;
  for (let y = 0; y < H; y += stride) for (let x = 0; x < W; x += stride) for (let c = 0; c < 3; c++) {
    const g = half(hw[y * bpr + x * 4 + c]);
    if (!Number.isFinite(g)) { nan++; continue; }
    const r = refNits[(y * W + x) * 3 + c], rp = pqInv(r / 10000);
    const e = g - rp; sqPq += e * e; sumPq += Math.abs(e); if (Math.abs(e) > maxPq) maxPq = Math.abs(e);
    const gl = pqEotf(g) * 10000, d = gl - r; sqLin += d * d; n++;
  }
  const db = (peak, sq) => (sq ? +(10 * Math.log10(peak * peak / (sq / n))).toFixed(2) : 'inf');
  return { psnrPqDb: db(1, sqPq), psnrLinearDb: db(1000, sqLin), maxErrPq10bitCodes: +(maxPq * 1023).toFixed(2), meanErrPq10bitCodes: +(sumPq / n * 1023).toFixed(3), nan, samples: n, stride };
}
function fit(rec, ref) {   // least squares rec = a * ref + b on every 7th sample: a range mismatch shows as a = 876 / 1023 (0.856) or 1023 / 876 (1.168)
  let n = 0, sx = 0, sy = 0, sxx = 0, sxy = 0;
  for (let i = 0; i < ref.length; i += 7) { n++; sx += ref[i]; sy += rec[i]; sxx += ref[i] * ref[i]; sxy += ref[i] * rec[i]; }
  const a = (n * sxy - sx * sy) / (n * sxx - sx * sx);
  return { slope: +a.toFixed(4), intercept: +((sy - a * sx) / n).toFixed(2) };
}
function codeStats(rec, ref) {
  let max = 0, sum = 0, exact = 0, n = rec.length, sq = 0;
  for (let i = 0; i < n; i++) { const e = Math.abs(rec[i] - ref[i]); if (e > max) max = e; sum += e; sq += e * e; if (!e) exact++; }
  return { fitYRec_on_ref: fit(rec, ref), maxErr: max, meanErr: +(sum / n).toFixed(3), exactPct: +(100 * exact / n).toFixed(1), psnrDb: sq ? +(10 * Math.log10(1023 * 1023 / (sq / n))).toFixed(2) : 'inf' };
}
async function decodeFrames(wanted) {
  const bin = await fetchBytes(`${base}.bin`);
  const desc = Uint8Array.from(atob(meta.description), c => c.charCodeAt(0));
  const cfg = { codec: meta.fallbackCodecString || meta.codec, codedWidth: W, codedHeight: H, hardwareAcceleration: 'prefer-hardware', description: desc };
  const got = new Map(); let err = null;
  const dec = new VideoDecoder({ output: f => { if (wanted.has(f.timestamp)) got.set(f.timestamp, f); else f.close(); }, error: e => { err = String(e); } });
  dec.configure(cfg);
  const maxIdx = Math.max(...meta.frames.map(f => f.decodeIndex)) + 40;
  for (const [i, p] of meta.packets.entries()) {
    if (i > maxIdx) break;
    while (dec.decodeQueueSize > 4) await new Promise(r => setTimeout(r, 0));
    dec.decode(new EncodedVideoChunk({ type: p.key ? 'key' : 'delta', timestamp: p.pts, duration: p.dur, data: bin.subarray(p.off, p.off + p.len) }));
  }
  await dec.flush(); dec.close();
  if (err) throw new Error('decoder: ' + err);
  return got;
}
function toPng(readback, scale = 2) {
  const bpr = readback.bytesPerRow / 2, hw = readback.halfWords, w = W / scale, h = H / scale;
  const cv = new OffscreenCanvas(w, h), ctx = cv.getContext('2d'), img = ctx.createImageData(w, h);
  for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) for (let c = 0; c < 3; c++) {
    let v = 0; for (let j = 0; j < scale; j++) for (let i = 0; i < scale; i++) v += half(hw[(y * scale + j) * bpr + (x * scale + i) * 4 + c]);
    img.data[(y * w + x) * 4 + c] = Math.round(Math.min(Math.max(v / (scale * scale), 0), 1) * 255);
    img.data[(y * w + x) * 4 + 3] = 255;
  }
  ctx.putImageData(img, 0, 0);
  return cv.convertToBlob({ type: 'image/png' }).then(b => new Promise(r => { const f = new FileReader(); f.onload = () => r(f.result); f.readAsDataURL(b); }));
}
const post = body => fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

export async function runHlgVerify() {
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  const device = await adapter.requestDevice({ requiredLimits: { maxStorageBufferBindingSize: Math.min(adapter.limits.maxStorageBufferBindingSize, 1 << 30), maxBufferSize: Math.min(adapter.limits.maxBufferSize, 1 << 30) } });
  const target = device.createTexture({ size: [W, H], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
  const comp = await FelCompositor.create(device, null, { target, peakNits: 1000, canvasMaxWidth: W });
  const res = { test: 'hlg-colour', name, label: q.get('label') || '', userAgent: navigator.userAgent, adapter: [adapter.info?.vendor, adapter.info?.architecture].filter(Boolean).join('/'), size: [W, H], frames: [] };
  const spsFull = hvccFullRange(Uint8Array.from(atob(meta.description), c => c.charCodeAt(0)), { primaries: 1, transfer: 13 });
  const wanted = new Set(meta.frames.filter(f => K.includes(f.k)).map(f => f.pts));
  const frames = await decodeFrames(wanted);
  for (const k of K) {
    const fi = meta.frames.find(f => f.k === k); if (!fi) continue;
    const frame = frames.get(fi.pts); if (!frame) { res.frames.push({ frame: k, error: 'frame not decoded' }); continue; }
    const rpu = await (await fetch(`${base}-rpu-${k}.json`)).json();
    const tables = new Int32Array(Uint8Array.from(atob(rpu.tablesB64), c => c.charCodeAt(0)).buffer);
    const yuv = await fetchBytes(`${base}-bl-${k}.yuv`), codes = new Uint16Array(yuv.buffer, yuv.byteOffset, yuv.byteLength / 2);
    const planes = { Y: codes.subarray(0, W * H), U: codes.subarray(W * H, W * H * 5 / 4), V: codes.subarray(W * H * 5 / 4) };
    const rd = async () => { await comp.whenIdle(); return comp.readTarget(W, H); };
    const row = { frame: k, ptsUs: fi.pts, frameColorSpace: { primaries: frame.colorSpace.primaries, transfer: frame.colorSpace.transfer, matrix: frame.colorSpace.matrix, fullRange: frame.colorSpace.fullRange } };
    comp.canvasConfig = { colorSpace: 'rec2100-pq' };
    const common = { toneMapping: 'off', trims: false, peakNits: 1000, sourceFullRange: spsFull === true };
    row.spsFullRangeFlag = spsFull;
    // HLG path
    comp.present(frame, null, null, rpu.display, { profile: 'p8', transfer: 'hlg', ...common });
    const hlgOut = await rd();
    const rec = await comp.readBaseCodes();
    row.baseCodes = { Y: codeStats(rec.subarray(0, W * H), planes.Y), U: codeStats(rec.subarray(W * H, W * H * 5 / 4), planes.U), V: codeStats(rec.subarray(W * H * 5 / 4, W * H * 3 / 2), planes.V) };
    const t0 = performance.now();
    const ref = referenceFrame(planes);
    row.refMs = Math.round(performance.now() - t0);
    row.hlgVsReference = comparePq(hlgOut, ref);
    // reference computed from the compositor's own recovered codes: isolates the display maths from the hardware decode difference
    const recPlanes = { Y: rec.subarray(0, W * H), U: rec.subarray(W * H, W * H * 5 / 4), V: rec.subarray(W * H * 5 / 4) };
    row.hlgVsReferenceFromRecoveredCodes = comparePq(hlgOut, referenceFrame(recPlanes));
    // display adaptation: a 600 nit display uses gamma 1.2 + 0.42 log10(0.6) = 1.107 at alpha 600
    comp.present(frame, null, null, rpu.display, { profile: 'p8', transfer: 'hlg', ...common, peakNits: 600 });
    row.hlgPeak600VsReference = comparePq(await rd(), referenceFrame(planes, 600), 2);
    // RPU mapping path (profile 8 with the tables)
    comp.present(frame, null, tables, rpu.display, { profile: 'p8', ...common });
    const rpuOut = await rd();
    row.rpuMappingVsReference = comparePq(rpuOut, ref, 2);
    row.rpu = { vdrBitDepth: tables[4], residual: tables[5] === 0 ? 'FEL' : 'none', l1: rpu.display.l1, signal: rpu.display.signal, sourcePeakNits: rpu.display.norm?.sourceMaxNits };
    // an SDR display (peak 203): both paths after display management, as PNG snapshots
    if (q.has('snap')) {
      comp.canvasConfig = { colorSpace: 'display-p3' };
      const sdr = { toneMapping: 'l1', trims: true, peakNits: 203, sdrWhiteNits: 203, sourceFullRange: spsFull === true };
      comp.present(frame, null, null, rpu.display, { profile: 'p8', transfer: 'hlg', ...sdr });
      row.snapHlg = await toPng(await rd());
      comp.present(frame, null, tables, rpu.display, { profile: 'p8', ...sdr });
      row.snapRpu = await toPng(await rd());
    }
    frame.close();
    res.frames.push(row);
    log(`frame ${k}: HLG vs f64 ${row.hlgVsReference.psnrPqDb} dB PQ / ${row.hlgVsReference.psnrLinearDb} dB linear (max ${row.hlgVsReference.maxErrPq10bitCodes} codes); from recovered codes ${row.hlgVsReferenceFromRecoveredCodes.psnrPqDb} dB; base codes Y max ${row.baseCodes.Y.maxErr}; RPU path vs HLG ref ${row.rpuMappingVsReference.psnrPqDb} dB PQ / ${row.rpuMappingVsReference.psnrLinearDb} dB linear`);
  }
  comp.dispose(); target.destroy();
  const ok = res.frames.filter(f => !f.error);
  res.summary = { minHlgPsnrPqDb: Math.min(...ok.map(f => f.hlgVsReference.psnrPqDb)), minHlgPsnrLinearDb: Math.min(...ok.map(f => f.hlgVsReference.psnrLinearDb)), minHlgPeak600PsnrPqDb: Math.min(...ok.map(f => f.hlgPeak600VsReference.psnrPqDb)), minHlgFromRecoveredDb: Math.min(...ok.map(f => f.hlgVsReferenceFromRecoveredCodes.psnrPqDb)), minRpuPsnrPqDb: Math.min(...ok.map(f => f.rpuMappingVsReference.psnrPqDb)), minRpuPsnrLinearDb: Math.min(...ok.map(f => f.rpuMappingVsReference.psnrLinearDb)) };
  window.hlgVerifyResult = res;
  log(JSON.stringify(res.summary));
  return res;
}
window.runHlgVerify = runHlgVerify;
if (q.has('auto')) runHlgVerify().then(async r => {
  const snaps = [];
  for (const f of r.frames) for (const key of ['snapHlg', 'snapRpu']) if (f[key]) { snaps.push({ test: 'hlg-snap', name, path: key === 'snapHlg' ? 'hlg' : 'rpu', frame: f.frame, png: f[key] }); delete f[key]; }
  await post({ test: 'hlg-colour', name: q.get('label') || '', result: r });
  for (const s of snaps) await post(s);
}).catch(e => post({ test: 'hlg-colour', error: String(e && e.stack || e) }));
