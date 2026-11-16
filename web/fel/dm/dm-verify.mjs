// SPDX-License-Identifier: LGPL-3.0-only
// Browser side of the ST 2094-10 style DM verification (WebGPU, Chrome). See dm-verify.html for the query parameters.
//   gpu     WGSL DISPLAY stage against the f64 Node reference (dm-samples.json from `node dm-node-ref.mjs`): 8 cases x 3600 sampled pixels, rgba32float target
//   frames  behaviour on exact decoded frames of the three DV test files (web/fel/media, tools/prep-media.mjs): rangers (P7 FEL), awaken (P7 MEL), p5 (Profile 5)
//   chart   synthetic 4000 nit chart, BT.2390 and ST 2094-10 at several targets
//   snaps   side-by-side PNGs (BT.2390 left, ST 2094-10 right), posted to /results as test 'dm-snap' (dm/save-snaps.py writes them to web/results)
import { FelCompositor } from '../fel-compositor.mjs';
import { pqEotfNits, pqInvNits, deltaEItp, buildDmParams, dmTone, ICTCP_RGB_TO_LMS, ICTCP_LMS_TO_ITP } from '../fel-dm.mjs';
import { bt2020LimitedMatrix } from '../fel-meta.mjs';
import { createDoviSession } from '../../engine/dovi.mjs';

const q = new URLSearchParams(location.search);
const name = q.get('name') || 'dm';
const log = s => { const el = document.getElementById('out'); if (el) el.textContent += s + '\n'; console.log(s); };
const post = body => fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
const align = (n, a) => Math.ceil(n / a) * a;
const fetchBytes = async url => { const r = await fetch(url); if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return new Uint8Array(await r.arrayBuffer()); };
const mulV = (m, v) => [m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2], m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2], m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2]];
const itp = n => { const [i, ct, cp] = mulV(ICTCP_LMS_TO_ITP, mulV(ICTCP_RGB_TO_LMS, n).map(pqInvNits)); return [i, ct / 2, cp]; };   // T = Ct / 2 (BT.2124)

async function getDevice() {
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  const need = ['maxStorageBufferBindingSize', 'maxBufferSize'];
  const device = await adapter.requestDevice({ requiredLimits: Object.fromEntries(need.map(k => [k, Math.min(adapter.limits[k], 1 << 30)])) });
  device.lost.then(i => log('DEVICE LOST ' + i.reason + ' ' + i.message));
  return { adapter, device };
}
async function readF32(device, tex, w, h) {
  const bpr = align(w * 16, 256), rb = device.createBuffer({ size: bpr * h, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
  const enc = device.createCommandEncoder();
  enc.copyTextureToBuffer({ texture: tex }, { buffer: rb, bytesPerRow: bpr }, [w, h]);
  device.queue.submit([enc.finish()]);
  await rb.mapAsync(GPUMapMode.READ);
  const out = new Float32Array(rb.getMappedRange().slice(0));
  rb.unmap(); rb.destroy();
  return { f: out, stride: bpr / 4 };
}
const mkTarget = (device, w, h) => device.createTexture({ size: [w, h], format: 'rgba32float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
const normalizedMeta = m => m && { ...m, yccToRgb: [1, 0, 0, 0, 1, 0, 0, 0, 1], yccOffset: [0, 0, 0], rgbToLms: new Array(9).fill(0) };
const pctl = (arr, p) => { const a = Float64Array.from(arr).sort(); return a.length ? a[Math.min(a.length - 1, Math.floor(p * a.length))] : null; };
const r4 = v => (v === null || v === undefined || !Number.isFinite(v) ? v : +v.toPrecision(5));

// ---------------------------------------------------------------------------------------------------------------- gpu vs Node f64
async function gpuCompare(device) {
  const samples = await (await fetch('./dm-samples.json')).json();
  const { width: W, height: H, patch: PS } = samples;
  const target = mkTarget(device, W, H);
  const comp = await FelCompositor.create(device, null, { target, format: 'rgba32float', canvasMaxWidth: W });
  comp.canvasConfig = { colorSpace: 'rec2100-pq' };
  const rows = [];
  const per = W / PS;
  for (const c of samples.cases) {
    const Y = new Uint16Array(W * H).fill(512), U = new Uint16Array(W * H / 4).fill(512), V = new Uint16Array(W * H / 4).fill(512);
    c.codes.forEach(([y, cb, cr], s) => {
      const px = (s % per) * PS, py = Math.floor(s / per) * PS;
      for (let j = 0; j < PS; j++) for (let i = 0; i < PS; i++) Y[(py + j) * W + px + i] = y;
      for (let j = 0; j < PS / 2; j++) for (let i = 0; i < PS / 2; i++) { U[(py / 2 + j) * (W / 2) + px / 2 + i] = cb; V[(py / 2 + j) * (W / 2) + px / 2 + i] = cr; }
    });
    const info = { profile: 'hdr10', toneMapping: 'l1', trims: true, peakNits: c.peak, sdrWhiteNits: 203, sourcePeakNits: 4000, dm: 'st2094-10', dmOptions: c.opts };
    const meta = normalizedMeta(samples.metas[c.meta]);
    const ret = comp.present({ planes: { Y, U, V }, width: W, height: H }, null, null, meta, info);
    await comp.whenIdle();
    const { f, stride } = await readF32(device, target, W, H);
    let maxPq = 0, sumPq = 0, nPq = 0, maxDe = 0, sumDe = 0, nan = 0, maxNeutralChroma = 0, worstS = -1, maxPqLit = 0;
    const kinds = {};
    c.codes.forEach((_, s) => {
      const x = (s % per) * PS + PS / 2, y = Math.floor(s / per) * PS + PS / 2, o = y * stride + x * 4;
      const got = [f[o], f[o + 1], f[o + 2]], want = c.expectNits[s];
      if (!got.every(Number.isFinite)) { nan++; return; }
      const gotN = got.map(pqEotfNits);
      let e = 0; for (let k = 0; k < 3; k++) if (want[k] >= 0.1) e = Math.max(e, Math.abs(got[k] - pqInvNits(want[k])));     // channels below 0.1 nit: PQ is too steep there, DeltaE ITP covers them
      const de = deltaEItp(gotN, want);
      const kind = s % 6 === 0 ? 'neutral' : s % 6 === 1 ? 'saturated' : 'random';
      const kk = kinds[kind] ??= { maxPq: 0, maxDe: 0 };
      kk.maxPq = Math.max(kk.maxPq, e); kk.maxDe = Math.max(kk.maxDe, de);
      if (e > maxPq) { maxPq = e; worstS = s; }
      maxPqLit = Math.max(maxPqLit, e);
      maxDe = Math.max(maxDe, de); sumDe += de; sumPq += e; nPq++;
      if (kind === 'neutral') { const [, t, p] = itp(gotN); maxNeutralChroma = Math.max(maxNeutralChroma, 720 * Math.hypot(t, p)); }
    });
    const row = { id: c.id, label: c.label, bypass: c.bypass, curve: c.curve, space: c.space, trim: c.trim, samples: c.codes.length, nan,
      maxPqErr: r4(maxPq), maxPqErrLitPixels: r4(maxPqLit), meanPqErr: r4(sumPq / nPq), maxDeItp: r4(maxDe), meanDeItp: r4(sumDe / nPq), neutralOutputChromaDeItpMax: r4(maxNeutralChroma), byKind: Object.fromEntries(Object.entries(kinds).map(([k, v]) => [k, { maxPq: r4(v.maxPq), maxDeItp: r4(v.maxDe) }])),
      uniformDmOn: ret.tone.dm ? !ret.tone.dm.bypass : null,
      worst: worstS >= 0 ? { s: worstS, codes: c.codes[worstS], want: c.expectNits[worstS] } : null };
    rows.push(row);
    log(`gpu ${c.id} ${c.label}: max PQ err ${row.maxPqErr} (lit ${row.maxPqErrLitPixels}), max DeltaE ITP ${row.maxDeItp}, mean ${row.meanDeItp}, nan ${nan}`);
  }
  comp.dispose(); target.destroy();
  return { rows, maxPqErr: Math.max(...rows.map(r => r.maxPqErr)), maxPqErrLit: Math.max(...rows.map(r => r.maxPqErrLitPixels)), maxDeItp: Math.max(...rows.map(r => r.maxDeItp)) };
}

// ---------------------------------------------------------------------------------------------------------------- frame analysis
function analyse(ref, out, { stride, w, h, peak, P, step = 2 }) {
  const bins = 100, binSum = new Float64Array(bins), binN = new Float64Array(bins);
  const maxOut = [], hueDiffs = [], over = { n: 0, ratioSum: 0 }, mid = { n: 0, inSum: 0, outSum: 0, predSum: 0 };
  let neutralOutMax = 0, nNeutral = 0, nPix = 0, overPeak = 0, nearClip = 0, iInMax = 0, iOutMax = 0, sumMaxRgbIn = 0, nanCount = 0;
  const sMid = P?.curve?.sMid ?? null;
  for (let y = 0; y < h; y += step) for (let x = 0; x < w; x += step) {
    const o = y * stride + x * 4;
    const a = [pqEotfNits(ref.f[o]), pqEotfNits(ref.f[o + 1]), pqEotfNits(ref.f[o + 2])];
    const b = [pqEotfNits(out.f[o]), pqEotfNits(out.f[o + 1]), pqEotfNits(out.f[o + 2])];
    if (!b.every(Number.isFinite)) { nanCount++; continue; }
    nPix++;
    const [ia, ta, pa] = itp(a), [ib, tb, pb] = itp(b);
    const ca = 720 * Math.hypot(ta, pa), cb = 720 * Math.hypot(tb, pb);
    const mo = Math.max(...b); maxOut.push(mo);
    sumMaxRgbIn += pqInvNits(Math.max(...a));
    if (mo > peak * 1.002) overPeak++;
    if (mo > peak * 0.98) nearClip++;
    iInMax = Math.max(iInMax, ia); iOutMax = Math.max(iOutMax, ib);
    if (ca < 0.5 && ia > 0.02) {                                 // (almost exactly) neutral input pixels: monotonic tone curve, no chroma added (output chroma stays below 0.5)
      const k = Math.min(bins - 1, Math.floor(ia * bins)); binSum[k] += ib; binN[k]++;
      neutralOutMax = Math.max(neutralOutMax, cb); nNeutral++;
    }
    if (ca > 8 && ia > 0.02 && cb > 0.5) { let d = Math.abs(Math.atan2(pb, tb) - Math.atan2(pa, ta)); if (d > Math.PI) d = 2 * Math.PI - d; hueDiffs.push(d * 180 / Math.PI); }
    if (P && ia > P.tMaxPq) { over.n++; over.ratioSum += ib / P.tMaxPq; }
    if (sMid !== null && Math.abs(ia - sMid) < 0.005) { mid.n++; mid.inSum += ia; mid.outSum += ib; mid.predSum += dmTone(ia, P); }
  }
  let prev = null, worstDrop = 0, viol = 0, used = 0;
  for (let k = 0; k < bins; k++) if (binN[k] >= 30) { const m = binSum[k] / binN[k]; if (prev !== null) { const d = prev - m; if (d > 2e-3) viol++; worstDrop = Math.max(worstDrop, d); } prev = m; used++; }
  return {
    pixels: nPix, nan: nanCount,
    monotonic: { neutralPixels: nNeutral, binsUsed: used, worstDropPq: r4(worstDrop), violations: viol },
    peak: { targetNits: peak, outMaxRgbNitsMax: r4(maxOut.reduce((m, v) => (v > m ? v : m), 0)), p999: r4(pctl(maxOut, 0.999)), p99: r4(pctl(maxOut, 0.99)), fractionAbovePeak: r4(overPeak / nPix), fractionAbove98pct: r4(nearClip / nPix) },
    rolloff: P ? { inputAboveTargetPeakPixels: over.n, meanOutOverTargetPeakPq: over.n ? r4(over.ratioSum / over.n) : null } : null,
    anchor: P ? { sMid: r4(sMid), tMid: r4(P.curve.tMid ?? null), pixelsNearAvg: mid.n, measuredMeanI: mid.n ? r4(mid.outSum / mid.n) : null, curvePredictedMeanI: mid.n ? r4(mid.predSum / mid.n) : null, inputMeanI: mid.n ? r4(mid.inSum / mid.n) : null } : null,
    hue: { pixels: hueDiffs.length, p50Deg: r4(pctl(hueDiffs, 0.5)), p99Deg: r4(pctl(hueDiffs, 0.99)), maxDeg: r4(hueDiffs.reduce((m, v) => (v > m ? v : m), 0)) },
    neutralOutputChromaDeItpMax: r4(neutralOutMax),
    frameMeanMaxRgbPq: r4(sumMaxRgbIn / nPix), maxIInput: r4(iInMax), maxIOutput: r4(iOutMax),
  };
}

// ---------------------------------------------------------------------------------------------------------------- real frames
const CLIPS = {
  rangers: { file: 'DV FEL Power Rangers Credits Test.mkv', profile: 'p7', W: 3840, H: 2160, EW: 1920, EH: 1080 },
  awaken: { file: 'awaken-girl.4K.HDR.DV.mkv', profile: 'p7', W: 3840, H: 2160, EW: 1920, EH: 1080 },
  p5: { file: 'Test Jellyfin 4K DV P5.mp4', profile: 'p5', W: 3840, H: 2160, EW: 0, EH: 0 },
};
async function loadClip(clipName, wanted) {
  const clip = CLIPS[clipName], dir = `../media/${clipName}`;
  const [index, rpuBin, cps] = await Promise.all([fetch(`${dir}-index.json`).then(r => r.json()), fetchBytes(`${dir}-rpu.bin`), fetch(`${dir}-checkpoints.json`).then(r => r.json())]);
  const session = await createDoviSession();
  const ks = wanted.map(i => cps.frames[i]), need = new Map(ks.map(k => [index.display[k], k]));
  const per = new Map();
  for (let i = 0; i < index.aus.length; i++) {
    const a = index.aus[i], r = session.parseRpu(rpuBin.subarray(a.rpu[0], a.rpu[0] + a.rpu[1]));
    if (need.has(i)) per.set(need.get(i), { display: r.display, tables: r.tables, tablesError: r.tablesError ? String(r.tablesError) : null });
  }
  const { W, H, EW, EH } = clip, blBytes = W * H * 3, elBytes = EW * EH * 3;
  const gold = await fetchBytes(`${dir}-gold-bl.yuv`), goldEl = clip.EW ? await fetchBytes(`${dir}-gold-el.yuv`) : null;
  const frames = wanted.map(i => {
    const k = cps.frames[i], bl = new Uint16Array(gold.buffer, gold.byteOffset + i * blBytes, blBytes / 2);
    const planes = (a, w, h) => ({ Y: a.subarray(0, w * h), U: a.subarray(w * h, w * h * 5 / 4), V: a.subarray(w * h * 5 / 4) });
    const el = goldEl ? new Uint16Array(goldEl.buffer, goldEl.byteOffset + i * elBytes, elBytes / 2) : null;
    return { k, ...per.get(k), bl: { planes: planes(bl, W, H), width: W, height: H }, el: el ? { planes: planes(el, EW, EH), width: EW, height: EH } : null };
  });
  return { clip, frames, name: clipName };
}

const CONFIGS = [
  { id: 'sdr100', peak: 100, sdr: 100 },
  { id: 'sdr203', peak: 203, sdr: 203 },
  { id: 'hdr600', peak: 600, sdr: 600 },
  { id: 'hdr1000', peak: 1000, sdr: 1000 },
  { id: 'sdr100-static', peak: 100, sdr: 100, tm: 'static' },        // mastering peak (RPU L6) as the source range instead of per frame L1
  { id: 'hdr1000-static', peak: 1000, sdr: 1000, tm: 'static' },
];

function pngOf(f, stride, w, h, scale = 1) {                      // f32 sRGB-encoded rgba -> 8 bit ImageData (box downscale)
  const ow = Math.floor(w / scale), oh = Math.floor(h / scale), img = new ImageData(ow, oh);
  for (let y = 0; y < oh; y++) for (let x = 0; x < ow; x++) for (let c = 0; c < 3; c++) {
    let v = 0; for (let j = 0; j < scale; j++) for (let i = 0; i < scale; i++) v += f[(y * scale + j) * stride + (x * scale + i) * 4 + c];
    img.data[(y * ow + x) * 4 + c] = Math.round(Math.min(Math.max(v / (scale * scale), 0), 1) * 255);
    img.data[(y * ow + x) * 4 + 3] = 255;
  }
  return img;
}
async function sideBySide(left, right, labels) {
  const w = left.width, h = left.height, gap = 8, bar = 26, cv = new OffscreenCanvas(w * 2 + gap, h + bar), ctx = cv.getContext('2d');
  ctx.fillStyle = '#111'; ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.putImageData(left, 0, bar); ctx.putImageData(right, w + gap, bar);
  ctx.fillStyle = '#eee'; ctx.font = '15px sans-serif'; ctx.textBaseline = 'middle';
  ctx.fillText(labels[0], 8, bar / 2); ctx.fillText(labels[1], w + gap + 8, bar / 2);
  const blob = await cv.convertToBlob({ type: 'image/png' });
  return new Promise(r => { const fr = new FileReader(); fr.onload = () => r(fr.result); fr.readAsDataURL(blob); });
}

async function runFrames(device, clipNames, which, snaps) {
  const out = [];
  for (const cn of clipNames) {
    const lc = await loadClip(cn, which);
    for (const fr of lc.frames) {
      const { clip } = lc, rowBase = { clip: cn, file: clip.file, frame: fr.k, profile: clip.profile, tablesError: fr.tablesError };
      const target = mkTarget(device, 1920, 1080);
      const comp = await FelCompositor.create(device, null, { target, format: 'rgba32float', canvasMaxWidth: 1920 });
      const baseInfo = { profile: clip.profile, canvasColorSpaceUnused: 1 };
      const meta = fr.display;
      const l1 = meta.l1, l6 = meta.l6;
      const rowInfo = { ...rowBase, l1: l1 && { minPq: l1.minPq, avgPq: l1.avgPq, maxPq: l1.maxPq }, mastering: l6 && { maxNits: l6.maxMasteringLuminance, minNits: l6.minMasteringLuminance }, sourceMaxPq: meta.source?.maxPq, l2: meta.l2.map(t => ({ target: t.targetMaxPq, s: t.slope, o: t.offset, p: t.power, cw: t.chromaWeight, sg: t.saturationGain })), l8: meta.l8?.length ?? 0, results: [] };
      const run = async (info, colorSpace = 'rec2100-pq') => {
        comp.canvasConfig = { colorSpace };
        const ret = comp.present(fr.bl, fr.el, fr.tables, fr.display, { ...baseInfo, ...info });
        await comp.whenIdle();
        return { ret, img: await readF32(device, target, 1920, 1080) };
      };
      const ref = (await run({ toneMapping: 'off', trims: false, peakNits: 10000 })).img;
      for (const cfg of CONFIGS) {
        const common = { toneMapping: cfg.tm ?? 'l1', trims: true, peakNits: cfg.peak, sdrWhiteNits: cfg.sdr };
        const a = await run({ ...common, dm: 'bt2390' });
        const b = await run({ ...common, dm: 'st2094-10' });
        const P = b.ret.tone.dm;
        const o = { cfg: cfg.id, peak: cfg.peak, dmBypass: P.bypass, dmTrim: P.trim?.from ?? null, dmSpace: P.space, dmCurve: { kind: P.curve.kind, sMin: r4(P.curve.sMin), sMid: r4(P.curve.sMid), sMax: r4(P.curve.sMax), tMid: r4(P.curve.tMid), tMax: r4(P.curve.tMax), slopeMax: r4(P.curve.slopeMax) },
          bt2390: analyse(ref, a.img, { stride: ref.stride, w: 1920, h: 1080, peak: cfg.peak, P: null }),
          dm: analyse(ref, b.img, { stride: ref.stride, w: 1920, h: 1080, peak: cfg.peak, P }) };
        rowInfo.results.push(o);
        log(`${cn} f${fr.k} ${cfg.id}: DM bypass ${P.bypass} trim ${JSON.stringify(P.trim?.from)} | mono viol ${o.dm.monotonic.violations} (bt2390 ${o.bt2390.monotonic.violations}) | out peak ${o.dm.peak.outMaxRgbNitsMax} | neutral chroma ${o.dm.neutralOutputChromaDeItpMax} | hue p99 ${o.dm.hue.p99Deg} deg`);
        if (snaps && (cfg.id === 'sdr100' || cfg.id === 'sdr203' || cfg.id === 'sdr100-static') && fr === lc.frames[0]) {
          const sc = (c) => c;
          const [sa, sb] = [await run({ ...common, dm: 'bt2390' }, 'srgb'), await run({ ...common, dm: 'st2094-10' }, 'srgb')];
          const png = await sideBySide(pngOf(sa.img.f, sa.img.stride, 1920, 1080, 2), pngOf(sb.img.f, sb.img.stride, 1920, 1080, 2), [`BT.2390 EETF on max(RGB), ${cfg.peak} nit target`, `ST 2094-10 style DM (${P.curve.kind}, ${P.space}), ${cfg.peak} nit target`]);
          await post({ test: 'dm-snap', file: `dm-${cn}-f${fr.k}-${cfg.id}.png`, png });
        }
      }
      out.push(rowInfo);
      comp.dispose(); target.destroy();
    }
    lc.frames.length = 0;
  }
  return out;
}

// ---------------------------------------------------------------------------------------------------------------- synthetic chart
function chartPlanes(W, H) {
  const KR = 0.2627, KB = 0.0593, KG = 1 - KR - KB;
  const Y = new Uint16Array(W * H), U = new Uint16Array(W * H / 4), V = new Uint16Array(W * H / 4);
  const rows = [[1, 1, 1], [1, 0, 0], [0, 1, 0], [0, 0, 1], [0, 1, 1], [1, 0, 1], [1, 1, 0], [1, 0.55, 0.35]];      // grey, primaries, secondaries, a skin-like mix
  const rh = Math.floor(H / rows.length / 2) * 2;
  const codes = (r, g, b) => { const y = KR * r + KG * g + KB * b, cb = (b - y) / (2 * (1 - KB)), cr = (r - y) / (2 * (1 - KR)), c = v => Math.min(1023, Math.max(0, Math.round(v))); return [c(64 + 876 * y), c(512 + 896 * cb), c(512 + 896 * cr)]; };
  for (let ry = 0; ry < rows.length; ry++) for (let x = 0; x < W; x++) {
    const nits = 0.01 * Math.pow(4000 / 0.01, x / (W - 1));                                  // log ramp 0.01 .. 4000 nits
    const e = pqInvNits(nits), d = rows[ry], pq = d.map(v => v > 0 ? pqInvNits(nits * v) : pqInvNits(nits * 0.0));
    const [y, cb, cr] = codes(...pq.map(v => v));
    for (let j = 0; j < rh; j++) { const yy = ry * rh + j; if (yy >= H) break; Y[yy * W + x] = y; if (!(x & 1) && !(yy & 1)) { U[(yy >> 1) * (W / 2) + (x >> 1)] = cb; V[(yy >> 1) * (W / 2) + (x >> 1)] = cr; } }
  }
  for (let yy = rows.length * rh; yy < H; yy++) for (let x = 0; x < W; x++) { Y[yy * W + x] = 64; if (!(x & 1) && !(yy & 1)) { U[(yy >> 1) * (W / 2) + (x >> 1)] = 512; V[(yy >> 1) * (W / 2) + (x >> 1)] = 512; } }
  return { planes: { Y, U, V }, width: W, height: H };
}
async function runChart(device, snaps) {
  const { m4000 } = await (await fetch('./dm-samples.json')).json().then(j => j.metas);
  const meta = normalizedMeta(m4000), W = 1920, H = 1080, res = [];
  const target = mkTarget(device, W, H);
  const comp = await FelCompositor.create(device, null, { target, format: 'rgba32float', canvasMaxWidth: W });
  const frame = chartPlanes(W, H);
  for (const cfg of CONFIGS.filter(c => !c.tm)) {
    const common = { profile: 'hdr10', toneMapping: 'l1', trims: true, peakNits: cfg.peak, sdrWhiteNits: cfg.sdr, sourcePeakNits: 4000 };
    comp.canvasConfig = { colorSpace: 'rec2100-pq' };
    comp.present(frame, null, null, meta, { ...common, toneMapping: 'off', peakNits: 10000 }); await comp.whenIdle();
    const ref = await readF32(device, target, W, H);
    const imgs = {};
    for (const dm of ['bt2390', 'st2094-10']) {
      comp.present(frame, null, null, meta, { ...common, dm }); await comp.whenIdle();
      imgs[dm] = await readF32(device, target, W, H);
    }
    comp.present(frame, null, null, meta, { ...common, dm: 'st2094-10' });
    const P = buildDmParams({ meta, display: { ...common }, profile: 'hdr10' });
    const row = { cfg: cfg.id, peak: cfg.peak, bt2390: analyse(ref, imgs.bt2390, { stride: ref.stride, w: W, h: H, peak: cfg.peak, P: null }), dm: analyse(ref, imgs['st2094-10'], { stride: ref.stride, w: W, h: H, peak: cfg.peak, P }) };
    res.push(row);
    log(`chart ${cfg.id}: DM mono viol ${row.dm.monotonic.violations}, out peak ${row.dm.peak.outMaxRgbNitsMax}, neutral chroma ${row.dm.neutralOutputChromaDeItpMax}, hue p99 ${row.dm.hue.p99Deg} (BT.2390 hue p99 ${row.bt2390.hue.p99Deg})`);
    if (snaps) {
      comp.canvasConfig = { colorSpace: 'srgb' };
      comp.present(frame, null, null, meta, { ...common, dm: 'bt2390' }); await comp.whenIdle(); const a = await readF32(device, target, W, H);
      comp.present(frame, null, null, meta, { ...common, dm: 'st2094-10' }); await comp.whenIdle(); const b = await readF32(device, target, W, H);
      const png = await sideBySide(pngOf(a.f, a.stride, W, H, 2), pngOf(b.f, b.stride, W, H, 2), [`BT.2390, ${cfg.peak} nit target (white = ${cfg.sdr} nit)`, `ST 2094-10 style DM, ${cfg.peak} nit target`]);
      await post({ test: 'dm-snap', file: `dm-chart-${cfg.id}.png`, png });
    }
  }
  comp.dispose(); target.destroy();
  return res;
}

export async function runDmVerify(parts = ['gpu', 'frames', 'chart'], opts = {}) {
  const { adapter, device } = await getDevice();
  const res = { test: 'dm-verify', name, userAgent: navigator.userAgent, adapter: [adapter.info?.vendor, adapter.info?.architecture, adapter.info?.description].filter(Boolean).join('/'), parts };
  const snaps = !!opts.snaps;
  if (parts.includes('gpu')) res.gpu = await gpuCompare(device);
  if (parts.includes('frames')) res.frames = await runFrames(device, (opts.clips ?? 'rangers,awaken,p5').split(','), opts.which ?? [1, 4], snaps);
  if (parts.includes('chart')) res.chart = await runChart(device, snaps);
  window.dmVerifyResult = res;
  return res;
}
window.runDmVerify = runDmVerify;
if (q.has('auto')) {
  runDmVerify((q.get('parts') || 'gpu,frames,chart').split(','), { snaps: q.has('snaps'), clips: q.get('clips') || undefined, which: q.get('which') ? q.get('which').split(',').map(Number) : undefined })
    .then(r => post(r)).then(() => log('DONE')).catch(e => { log('ERROR ' + (e && e.stack || e)); return post({ test: 'dm-verify', name, error: String(e && e.stack || e) }); });
}
