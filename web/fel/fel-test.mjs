// SPDX-License-Identifier: LGPL-3.0-only
// Test driver for web/fel. window.runFelTest({steps, frames, peakNits, canvasMaxWidth}) from fel-reference.test.html.
//
//   probe    canvas colour spaces and formats Chrome accepts (scratch canvases, never the visible one)
//   display  DISPLAY stage (RPU colour decode + BT.2390 + encoding) against an f64 model, synthetic ramps
//   exact    COMPOSE stage on exact FFmpeg planes: sha256 of raw16 and vdr12 against the independent gold manifest
//   hw       WebCodecs hardware decode of the relabelled streams, compose, compare 12 checkpoint frames with the exact result
//   fps      sustained decode + recover + compose + present of all frames (no readback)
import { FelCompositor, probeCanvasColorSpaces } from './fel-compositor.mjs';
import { FelTables } from './fel-tables.mjs';
import { normalizeRpuDisplayMeta, modelDisplayPixel } from './fel-meta.mjs';

const log = s => { const el = document.getElementById('log'); if (el) el.textContent += s + '\n'; console.log(s); };
const FX = './fixtures/';                       // git-ignored local test media (docs/testing.md)
const W = 3840, H = 2160, EW = 1920, EH = 1080;
const hex = buf => [...new Uint8Array(buf)].map(v => v.toString(16).padStart(2, '0')).join('');
const sha256 = async arr => hex(await crypto.subtle.digest('SHA-256', arr));
const pad4 = n => String(n).padStart(4, '0');
const fetchBytes = async url => { const r = await fetch(url); if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return new Uint8Array(await r.arrayBuffer()); };
const half = b => { const s = b & 0x8000 ? -1 : 1, e = (b >> 10) & 31, m = b & 1023; return e === 0 ? s * 2 ** -14 * (m / 1024) : e === 31 ? NaN : s * 2 ** (e - 15) * (1 + m / 1024); };

async function getDevice() {
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  const need = ['maxStorageBufferBindingSize', 'maxBufferSize'];
  const device = await adapter.requestDevice({ requiredLimits: Object.fromEntries(need.map(k => [k, Math.min(adapter.limits[k], 1 << 30)])) });
  device.lost.then(i => log('DEVICE LOST ' + i.reason + ' ' + i.message));
  return { adapter, device };
}

const PLANES = [['Y', 0, W * H], ['U', W * H, W * H / 4], ['V', W * H + W * H / 4, W * H / 4]];
/** Error statistics of `a` against `b` per plane. `peak` is the full scale for PSNR. */
function planeStats(a, b, peak, planes = PLANES) {
  const out = {};
  let totalSq = 0, totalN = 0, totalMax = 0;
  for (const [name, off, n] of planes) {
    let max = 0, sum = 0, sq = 0, exact = 0, signed = 0;
    for (let i = off; i < off + n; i++) {
      const e = a[i] - b[i], ae = e < 0 ? -e : e;
      if (ae > max) max = ae;
      sum += ae; sq += e * e; signed += e;
      if (e === 0) exact++;
    }
    out[name] = { maxAbs: max, meanAbs: +(sum / n).toFixed(4), bias: +(signed / n).toFixed(4), psnrDb: sq ? +(10 * Math.log10(peak * peak / (sq / n))).toFixed(2) : Infinity, exactPct: +(100 * exact / n).toFixed(2) };
    totalSq += sq; totalN += n; totalMax = Math.max(totalMax, max);
  }
  out.all = { maxAbs: totalMax, psnrDb: totalSq ? +(10 * Math.log10(peak * peak / (totalSq / totalN))).toFixed(2) : Infinity };
  return out;
}

let manifestCache = null;
async function manifest() {
  if (manifestCache) return manifestCache;
  const gold = await (await fetch(FX + 'full-fel-manifest-composition-gold.json')).json();
  manifestCache = gold.checkpoints;
  return manifestCache;
}
async function checkpointRpu(index) {
  const [nal, json] = await Promise.all([fetchBytes(`${FX}full-fel-manifest-checkpoint-${pad4(index)}-rpu.nal`), fetch(`${FX}full-fel-manifest-frame-${pad4(index)}-rpu.json`).then(r => r.json())]);
  return { nal, json, meta: normalizeRpuDisplayMeta(json.vdr_dm_data) };
}

// ---------------------------------------------------------------- probe
async function stepProbe(ctx) {
  const t = {};
  t.rgba16float = probeCanvasColorSpaces(ctx.device, { format: 'rgba16float' });
  t.bgra8unormExtended = probeCanvasColorSpaces(ctx.device, { format: 'bgra8unorm' });
  t.rgba16floatStandardToneMapping = probeCanvasColorSpaces(ctx.device, { format: 'rgba16float', toneMapping: { mode: 'standard' } });
  log('canvas colour spaces (accepted by configure) per format / tone mapping:');
  for (const [k, v] of Object.entries(t)) log('  ' + k + ': ' + Object.entries(v).map(([n, r]) => `${n}=${r.accepted ? 'yes' + (r.retained === false ? '(not retained)' : '') : 'NO'}`).join(' '));
  t.dynamicRangeMedia = ['(dynamic-range: high)', '(color-gamut: p3)', '(color-gamut: rec2020)'].map(q => [q, matchMedia(q).matches]);
  t.screen = { w: screen.width, h: screen.height, dpr: devicePixelRatio, colorDepth: screen.colorDepth };
  log('  media: ' + JSON.stringify(t.dynamicRangeMedia) + ' screen ' + JSON.stringify(t.screen));
  return t;
}

// ---------------------------------------------------------------- display model
async function stepDisplay(ctx) {
  const { device } = ctx;
  const w = 64, h = 32;
  const target = device.createTexture({ size: [w, h], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
  const comp = await FelCompositor.create(device, null, { target, peakNits: 1000 });
  comp.ensureGeometry(w, h, 0, 0);
  const results = [];
  const cases = [
    { name: 'hdr10 neutral, no tone mapping needed', display: { peakNits: 1000, sourcePeakNits: 1000 }, chroma: [512, 512] },
    { name: 'hdr10 neutral, 4000 nit source on 1000 nit panel (BT.2390)', display: { peakNits: 1000, sourcePeakNits: 4000 }, chroma: [512, 512] },
    { name: 'hdr10 saturated red-ish, 4000 -> 600', display: { peakNits: 600, sourcePeakNits: 4000 }, chroma: [450, 700] },
    { name: 'hdr10 blue-ish, 1000 -> 1000, SDR white 100', display: { peakNits: 1000, sourcePeakNits: 1000, sdrWhiteNits: 100 }, chroma: [750, 420] },
  ];
  for (const c of cases) {
    const Y = new Uint16Array(w * h), U = new Uint16Array(w * h / 4).fill(c.chroma[0]), V = new Uint16Array(w * h / 4).fill(c.chroma[1]);
    for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) Y[y * w + x] = 64 + Math.round(x / (w - 1) * 876);
    const bytes = new Uint8Array(w * h * 3);
    bytes.set(new Uint8Array(Y.buffer), 0); bytes.set(new Uint8Array(U.buffer), w * h * 2); bytes.set(new Uint8Array(V.buffer), w * h * 2 + w * h / 2);
    device.queue.writeBuffer(comp.blBuf, 0, bytes);
    const u = comp.displayOnly({ exact: false, meta: null, displayInfo: c.display, profile: 'hdr10' });
    const { halfWords, bytesPerRow } = await comp.readTarget(w, h);
    let maxErr = 0, worst = null;
    for (let x = 0; x < w; x++) {
      const expect = modelDisplayPixel([Y[x] / 1024, c.chroma[0] / 1024, c.chroma[1] / 1024], u.data, 'display-p3');
      for (let k = 0; k < 3; k++) {
        const got = half(halfWords[x * 4 + k]);
        const err = Math.abs(got - expect[k]) / (1 + Math.abs(expect[k]));
        if (err > maxErr) { maxErr = err; worst = { x, k, got, expect: expect[k] }; }
      }
    }
    const last = [0, 1, 2].map(k => half(halfWords[(w - 1) * 4 + k]));
    results.push({ name: c.name, maxRelErr: +maxErr.toFixed(5), worst, lastPixelEncoded: last.map(v => +v.toFixed(3)), tone: { srcMaxPq: +u.info.srcMaxPq.toFixed(4), peakPq: +u.info.peakPq.toFixed(4) }, pass: maxErr < 0.01 });
    log(`display ${c.name}: maxRelErr ${maxErr.toFixed(5)} ${maxErr < 0.01 ? 'PASS' : 'FAIL'} last px ${last.map(v => v.toFixed(3))}`);
  }
  comp.dispose(); target.destroy();
  return results;
}

// ---------------------------------------------------------------- exact planes
const refs = new Map();       // frameIndex -> raw16 Uint16Array
async function stepExact(ctx, opts) {
  const { device } = ctx;
  const comp = await FelCompositor.create(device, null, { target: device.createTexture({ size: [64, 64], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT }) });
  const tables = await FelTables.create(new URL('../wasm/aether_wasm.wasm', import.meta.url));
  const points = await manifest();
  const rows = [];
  for (const cp of points) {
    const i = cp.frameIndex;
    const [bl, el, rpu] = await Promise.all([fetchBytes(`${FX}full-fel-manifest-frame-${pad4(i)}-bl.yuv`), fetchBytes(`${FX}full-fel-manifest-frame-${pad4(i)}-el.yuv`), checkpointRpu(i)]);
    tables.reset();
    const words = tables.tables(rpu.nal);
    const t0 = performance.now();
    comp.composeExact(bl, el, words);
    const { raw16, vdr } = await comp.readComposed();
    const ms = performance.now() - t0;
    const rawHash = await sha256(raw16), vdrHash = await sha256(vdr);
    refs.set(i, raw16);
    const row = { frame: i, raw16Match: rawHash === cp.raw16Sha256, vdr12Match: vdrHash === cp.vdr12Sha256, ms: +ms.toFixed(0) };
    rows.push(row);
    log(`exact frame ${i}: raw16 ${row.raw16Match ? 'MATCH' : 'DIFF'} vdr12 ${row.vdr12Match ? 'MATCH' : 'DIFF'} (${row.ms} ms incl. upload+readback)`);
  }
  comp.dispose();
  return { allMatch: rows.every(r => r.raw16Match && r.vdr12Match), rows };
}

// ---------------------------------------------------------------- decoding
async function loadStreams() {
  const base = '../fixtures/fel-p7-retag';
  const [bl, el, index] = await Promise.all([fetchBytes(base + '-bl.hevc'), fetchBytes(base + '-el.hevc'), fetch(base + '-index.json').then(r => r.json())]);
  return { bl, el, aus: index.aus };
}

/**
 * Decodes BL and EL with hardware WebCodecs and calls `onPair(k, blFrame, elFrame)` for every pair in output order.
 * The callback is awaited, frames are closed afterwards.
 */
async function decodePairs({ bl, el, aus }, count, onPair, { maxQueue = 6 } = {}) {
  const q = { bl: [], el: [] };
  let wake = () => {}; const kick = () => wake();
  const wait = () => new Promise(r => { wake = r; setTimeout(r, 20); });
  const errors = [];
  const mk = name => new VideoDecoder({ output: f => { q[name].push(f); kick(); }, error: e => { errors.push(name + ': ' + e); kick(); } });
  const dec = { bl: mk('bl'), el: mk('el') };
  dec.bl.configure({ codec: 'hvc1.2.4.L153.90', codedWidth: W, codedHeight: H, hardwareAcceleration: 'prefer-hardware' });
  dec.el.configure({ codec: 'hvc1.2.4.L123.90', codedWidth: EW, codedHeight: EH, hardwareAcceleration: 'prefer-hardware' });
  dec.bl.ondequeue = dec.el.ondequeue = kick;
  const dur = 1001000 / 24000 * 1000;
  let fedDone = false;
  const producer = (async () => {
    for (let i = 0; i < count; i++) {
      while (dec.bl.decodeQueueSize > 6 || dec.el.decodeQueueSize > 6 || q.bl.length > maxQueue || q.el.length > maxQueue) { if (errors.length) return; await wait(); }
      const a = aus[i], type = a.key ? 'key' : 'delta';
      dec.bl.decode(new EncodedVideoChunk({ type, timestamp: i * dur, data: bl.subarray(a.bl[0], a.bl[0] + a.bl[1]) }));
      dec.el.decode(new EncodedVideoChunk({ type, timestamp: i * dur, data: el.subarray(a.el[0], a.el[0] + a.el[1]) }));
    }
    await Promise.all([dec.bl.flush(), dec.el.flush()]);
    fedDone = true; kick();
  })();
  let k = 0;
  while (k < count) {
    if (errors.length) throw new Error(errors.join('; '));
    if (q.bl.length && q.el.length) {
      const b = q.bl.shift(), e = q.el.shift();
      try { await onPair(k, b, e); } finally { b.close(); e.close(); }
      k++; kick();
    } else if (fedDone) break;
    else await wait();
  }
  await producer.catch(() => {});
  dec.bl.close(); dec.el.close();
  for (const f of [...q.bl, ...q.el]) f.close();
  return { pairs: k, errors };
}

// ---------------------------------------------------------------- hardware compare
async function stepHw(ctx, opts) {
  const { device } = ctx;
  if (!refs.size) await stepExact(ctx, opts);
  const streams = await loadStreams();
  const points = await manifest();
  const rpus = new Map(), tables = await FelTables.create(new URL('../wasm/aether_wasm.wasm', import.meta.url));
  for (const cp of points) { const r = await checkpointRpu(cp.frameIndex); tables.reset(); rpus.set(cp.frameIndex, { words: tables.tables(r.nal), meta: r.meta }); }
  const canvas = document.getElementById('out');
  const comp = await FelCompositor.create(device, canvas.getContext('webgpu'), { canvasColorSpace: opts.canvasColorSpace ?? 'display-p3', canvasMaxWidth: opts.canvasMaxWidth ?? 1920, peakNits: opts.peakNits ?? 1000 });
  const cpSet = new Set(points.map(p => p.frameIndex));
  const order = points.map(p => p.frameIndex);
  const results = [];
  let current = rpus.get(0);
  const count = Math.min(opts.frames ?? streams.aus.length, streams.aus.length);
  const info = await decodePairs(streams, count, async (k, b, e) => {
    if (rpus.has(k)) current = rpus.get(k);
    comp.present(b, e, current.words, current.meta, { profile: 'p7' });
    if (!cpSet.has(k)) return;
    await comp.whenIdle();
    const [{ raw16, vdr }, codes, blExact] = await Promise.all([comp.readComposed(), comp.readBaseCodes(), fetchBytes(`${FX}full-fel-manifest-frame-${pad4(k)}-bl.yuv`)]);
    const ref = refs.get(k);
    const refVdr = new Uint16Array(ref.length);
    for (let i = 0; i < ref.length; i++) refVdr[i] = Math.min((ref[i] + 8) >> 4, 4095);
    const blRef = new Uint16Array(blExact.buffer, blExact.byteOffset, blExact.byteLength / 2);
    const row = { frame: k, vdr12: planeStats(vdr, refVdr, 4095), raw16: planeStats(raw16, ref, 65535), blCodes10: planeStats(codes, blRef, 1023) };
    results.push(row);
    log(`hw frame ${k}: vdr12 PSNR ${row.vdr12.all.psnrDb} dB max ${row.vdr12.all.maxAbs} | Y ${row.vdr12.Y.psnrDb}/${row.vdr12.Y.maxAbs} U ${row.vdr12.U.psnrDb}/${row.vdr12.U.maxAbs} V ${row.vdr12.V.psnrDb}/${row.vdr12.V.maxAbs} | BL codes PSNR ${row.blCodes10.all.psnrDb} max ${row.blCodes10.all.maxAbs}`);
  });
  const agg = fn => { const v = results.map(fn); return { min: Math.min(...v), max: Math.max(...v), mean: +(v.reduce((a, b) => a + b, 0) / v.length).toFixed(2) }; };
  const summary = {
    frames: results.length, decoded: info.pairs, errors: info.errors,
    vdr12: { psnrDb: agg(r => r.vdr12.all.psnrDb), maxAbs: agg(r => r.vdr12.all.maxAbs) },
    raw16: { psnrDb: agg(r => r.raw16.all.psnrDb), maxAbs: agg(r => r.raw16.all.maxAbs) },
    blCodes10: { psnrDb: agg(r => r.blCodes10.all.psnrDb), maxAbs: agg(r => r.blCodes10.all.maxAbs) },
    vdr12Planes: { Y: agg(r => r.vdr12.Y.psnrDb), U: agg(r => r.vdr12.U.psnrDb), V: agg(r => r.vdr12.V.psnrDb) },
  };
  log('hw summary ' + JSON.stringify(summary));
  comp.dispose();
  return { summary, results };
}

// ---------------------------------------------------------------- fps
async function stepFps(ctx, opts) {
  const { device } = ctx;
  const streams = await loadStreams();
  const tables = await FelTables.create(new URL('../wasm/aether_wasm.wasm', import.meta.url));
  const points = await manifest();
  const rpu = await checkpointRpu(points[0].frameIndex);
  tables.reset();
  const words = tables.tables(rpu.nal);
  const canvas = document.getElementById('out');
  const comp = await FelCompositor.create(device, canvas.getContext('webgpu'), { canvasColorSpace: opts.canvasColorSpace ?? 'display-p3', canvasMaxWidth: opts.canvasMaxWidth ?? 1920, peakNits: opts.peakNits ?? 1000 });
  const count = Math.min(opts.frames ?? streams.aus.length, streams.aus.length);
  const presentMs = [], stamps = [];
  const inflight = [];
  const t0 = performance.now();
  const info = await decodePairs(streams, count, async (k, b, e) => {
    const t = performance.now();
    comp.present(b, e, words, rpu.meta, { profile: 'p7' });
    presentMs.push(performance.now() - t);
    // keep at most 3 frames in flight on the GPU so decoder surfaces are released in step with the queue
    inflight.push(device.queue.onSubmittedWorkDone());
    if (inflight.length > 3) await inflight.shift();
    stamps.push(performance.now());
  }, { maxQueue: 6 });
  await comp.whenIdle();
  const wall = performance.now() - t0;
  const gaps = stamps.slice(1).map((s, i) => s - stamps[i]).sort((a, b) => a - b);
  const pct = p => +gaps[Math.min(gaps.length - 1, Math.floor(p * gaps.length))].toFixed(2);
  presentMs.sort((a, b) => a - b);
  const fps = info.pairs / wall * 1000;
  const result = {
    pairs: info.pairs, errors: info.errors, wallMs: +wall.toFixed(0), fps: +fps.toFixed(1), realtimeX: +(fps / (24000 / 1001)).toFixed(2),
    pairIntervalMs: { p50: pct(0.5), p95: pct(0.95), p99: pct(0.99), max: pct(1) },
    over41_7ms: gaps.filter(g => g > 41.7).length,
    presentCpuMs: { p50: +presentMs[presentMs.length >> 1].toFixed(2), max: +presentMs[presentMs.length - 1].toFixed(2) },
    canvas: comp.canvasSize, canvasConfig: comp.canvasConfig, supersample: comp.supersample,
  };
  log('fps ' + JSON.stringify(result));
  comp.dispose();
  return result;
}

export async function runFelTest(opts = {}) {
  const steps = opts.steps ?? ['probe', 'display', 'exact', 'hw', 'fps'];
  const ctx = await getDevice();
  log(`UA ${navigator.userAgent}\nadapter ${JSON.stringify(ctx.adapter.info ? { vendor: ctx.adapter.info.vendor, architecture: ctx.adapter.info.architecture, description: ctx.adapter.info.description } : {})}`);
  const out = { userAgent: navigator.userAgent };
  const table = { probe: stepProbe, display: stepDisplay, exact: stepExact, hw: stepHw, fps: stepFps };
  for (const s of steps) {
    log(`--- ${s}`);
    try { out[s] = await table[s](ctx, opts); } catch (e) { out[s] = { error: String(e.stack ?? e) }; log(`${s} ERROR ${e.stack ?? e}`); }
  }
  window.felTestResult = out;
  return out;
}

// Shared with hybrid-test.mjs.
export { planeStats, manifest, checkpointRpu, fetchBytes, getDevice, pad4, FX };
