// SPDX-License-Identifier: LGPL-3.0-only
// HDR10+ (ST 2094-40) through FelCompositor on real media: web/fel/media/hdr10plus.mkv (HDR10Plus_PB_EAC3JOC.mkv).
//   A  parse     demux with web/engine on the main thread, extractHdr10Plus on every video packet
//   B  patches   synthetic exact planes (luma ramp, three chroma cases) rendered with the metadata of 6 real pictures; GPU against the Rust reference curve
//   C  ramp      monotone luminance along a neutral ramp, no NaN
//   D  decode    hardware decode of the first frames (SPS relabelled), present with the packet's parameters, readback checks (no NaN / inf, <= peak)
// window.runHdr10PlusTest({frames})
import { FelCompositor } from './fel-compositor.mjs';
import { getDevice } from './fel-test.mjs';
import { openDemuxer, relabelPacketSps } from '../engine/demuxer.mjs';
import { initHdr10Plus, extractHdr10Plus, hdr10plusGpuParams, hdr10plusApply } from '../engine/hdr10plus.mjs';
import { bt2020LimitedMatrix, pqToNits, nitsToPq } from './fel-meta.mjs';

const log = s => { const el = document.getElementById('log'); if (el) el.textContent += s + '\n'; console.log(s); };
const half = b => { const s = b & 0x8000 ? -1 : 1, e = (b >> 10) & 31, m = b & 1023; return e === 0 ? s * 2 ** -14 * (m / 1024) : e === 31 ? NaN : s * 2 ** (e - 15) * (1 + m / 1024); };
const PEAK = 1000;

function annexB(lp) {
  const parts = []; let pos = 0, total = 0;
  while (pos + 4 <= lp.length) { const len = ((lp[pos] << 24) | (lp[pos + 1] << 16) | (lp[pos + 2] << 8) | lp[pos + 3]) >>> 0; parts.push(lp.subarray(pos + 4, pos + 4 + len)); total += 4 + len; pos += 4 + len; }
  const o = new Uint8Array(total); let p = 0; for (const x of parts) { o.set([0, 0, 0, 1], p); o.set(x, p + 4); p += 4 + x.length; } return o;
}

async function parseStep(opts) {
  const bytes = new Uint8Array(await (await fetch('./media/hdr10plus.mkv')).arrayBuffer());
  await initHdr10Plus();
  const d = await openDemuxer(bytes);
  const track = d.tracks().find(t => t.kind === 'video');
  const max = opts.frames ?? 240;
  const pkts = [];
  while (pkts.length < max) {
    const p = d.nextPacket(); if (!p) break;
    if (p.trackId !== track.id) continue;
    const params = extractHdr10Plus(p, track);
    pkts.push({ key: p.key, ptsUs: p.ptsUs, params, data: p.data });
  }
  const withParams = pkts.filter(p => p.params);
  const blocks = withParams.map(p => hdr10plusGpuParams(p.params, PEAK));
  const stat = f => { const v = blocks.filter(Boolean).map(f); return v.length ? { min: Math.min(...v), max: Math.max(...v) } : null; };
  const res = {
    track: { codec: track.codec, w: track.width, h: track.height }, packets: pkts.length, withParams: withParams.length,
    applicationVersion: [...new Set(withParams.map(p => p.params.applicationVersion))], windows: [...new Set(withParams.map(p => p.params.numWindows))],
    blocks: blocks.length, nullBlocks: blocks.filter(b => !b).length,
    withCurve: blocks.filter(b => b && b[1] > 0.5).length, sceneMaxNits: stat(b => b[2]), sceneAvgNits: stat(b => b[3]), targetNits: stat(b => b[4]), fade: stat(b => b[9]), anchors: stat(b => b[7]),
    nanInBlocks: blocks.some(b => b && Array.from(b).some(v => !Number.isFinite(v))),
  };
  log('parse ' + JSON.stringify(res));
  return { res, pkts, track };
}

function makeTarget(device, w, h) { return device.createTexture({ size: [w, h], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC }); }

function planesFor(w, h, fn) {         // fn(x) -> [Y, U, V] codes for column x
  const Y = new Uint16Array(w * h), U = new Uint16Array(w * h / 4), V = new Uint16Array(w * h / 4);
  for (let x = 0; x < w; x++) { const [y, u, v] = fn(x); for (let r = 0; r < h; r++) Y[r * w + x] = y; for (let r = 0; r < h / 2; r++) if (x % 2 === 0) { U[r * (w / 2) + x / 2] = u; V[r * (w / 2) + x / 2] = v; } }
  return { planes: { Y, U, V }, width: w, height: h };
}

async function patchStep(device, pkts) {
  const w = 64, h = 32, target = makeTarget(device, w, h);
  const comp = await FelCompositor.create(device, null, { target, peakNits: PEAK, h10pMap: 'curve' });
  comp.canvasConfig = { colorSpace: 'rec2100-pq' };          // output = PQ of absolute BT.2020 nits: comparable with the Rust reference
  const withParams = pkts.filter(p => p.params);
  const picks = [0, 1, 2, 3, 4, 5].map(i => withParams[Math.floor(i * (withParams.length - 1) / 5)]);
  const { matrix, offsets } = bt2020LimitedMatrix();
  const chromaCases = [[512, 512], [450, 700], [750, 420]];
  const rows = [];
  let worstAll = 0, nanCount = 0;
  for (const pk of picks) {
    const block = hdr10plusGpuParams(pk.params, PEAK);
    for (const [cu, cv] of chromaCases) {
      const frame = planesFor(w, h, x => [64 + Math.round(x / (w - 1) * 876), cu, cv]);
      comp.present(frame, null, null, null, { profile: 'hdr10', hdr10plus: pk.params, toneMapping: 'off' });
      const { halfWords, bytesPerRow } = await comp.readTarget(w, h);
      let worst = 0, mapped = 0;
      for (let x = 0; x < w; x++) {
        const sig = [(64 + Math.round(x / (w - 1) * 876)) / 1024, cu / 1024, cv / 1024].map((v, i) => v - offsets[i]);
        const rgbPq = matrix.map(r => r[0] * sig[0] + r[1] * sig[1] + r[2] * sig[2]).map(v => Math.min(Math.max(v, 0), 1));
        const want = hdr10plusApply(pk.params, PEAK, rgbPq);
        const got = [0, 1, 2].map(k => half(halfWords[x * 4 + k]));
        for (let k = 0; k < 3; k++) { if (!Number.isFinite(got[k])) nanCount++; const e = Math.abs(got[k] - want[k]); if (e > worst) worst = e; }
        if (Math.abs(want[0] - rgbPq[0]) > 1e-3) mapped++;
      }
      worstAll = Math.max(worstAll, worst);
      rows.push({ sceneMaxNits: +block[2].toFixed(0), hasCurve: block[1] > 0.5, chroma: [cu, cv], maxPqErr: +worst.toExponential(2), pixelsChangedByCurve: mapped });
    }
  }
  comp.dispose(); target.destroy();
  const res = { maxPqErr: worstAll, nan: nanCount, pass: worstAll < 3e-3 && nanCount === 0, rows };
  log(`patches maxPqErr ${worstAll.toExponential(2)} nan ${nanCount} ${res.pass ? 'PASS' : 'FAIL'}`);
  return res;
}

async function rampStep(device, pkts) {
  const w = 256, h = 2, target = makeTarget(device, w, h);
  const comp = await FelCompositor.create(device, null, { target, peakNits: PEAK, h10pMap: 'curve' });
  comp.canvasConfig = { colorSpace: 'rec2100-pq' };
  const withParams = pkts.filter(p => p.params);
  const rows = [];
  for (const pk of [withParams[0], withParams[Math.floor(withParams.length / 2)], withParams[withParams.length - 1], ...withParams.filter((_, i) => i % 20 === 0)]) {
    comp.present(planesFor(w, h, x => [64 + Math.round(x / (w - 1) * 876), 512, 512]), null, null, null, { profile: 'hdr10', hdr10plus: pk.params, toneMapping: 'off' });
    const { halfWords } = await comp.readTarget(w, h);
    let prev = -1, viol = 0, nan = 0, max = 0;
    for (let x = 0; x < w; x++) {
      const v = Math.max(...[0, 1, 2].map(k => half(halfWords[x * 4 + k])));
      if (!Number.isFinite(v)) { nan++; continue; }
      if (v < prev - 1e-3) viol++;
      prev = Math.max(prev, v); max = Math.max(max, v);
    }
    rows.push({ violations: viol, nan, maxPq: +max.toFixed(4) });
  }
  comp.dispose(); target.destroy();
  const res = { ramps: rows.length, violations: rows.reduce((a, r) => a + r.violations, 0), nan: rows.reduce((a, r) => a + r.nan, 0), maxPqOut: Math.max(...rows.map(r => r.maxPq)), peakPq: +nitsToPq(PEAK).toFixed(4) };
  res.pass = res.violations === 0 && res.nan === 0;
  log('ramp ' + JSON.stringify(res));
  return res;
}

async function decodeStep(device, parsed, opts) {
  const { pkts, track } = parsed;
  const W = track.width, H = track.height;
  const target = makeTarget(device, 1920, 1080);
  const comp = await FelCompositor.create(device, null, { target, peakNits: PEAK, h10pMap: 'curve' });
  comp.canvasConfig = { colorSpace: 'rec2100-pq' };
  const frames = [];
  let err = null;
  const dec = new VideoDecoder({ output: f => frames.push(f), error: e => { err = String(e); } });
  dec.configure({ codec: track.codec, codedWidth: W, codedHeight: H, hardwareAcceleration: 'prefer-hardware' });
  const t0 = performance.now();
  pkts.forEach((p, i) => dec.decode(new EncodedVideoChunk({ type: p.key ? 'key' : 'delta', timestamp: i * 41708, data: annexB(relabelPacketSps(p.data, 4).data) })));
  await dec.flush();
  const decodeMs = performance.now() - t0;
  const out = { decoded: frames.length, error: err, decodeMs: Math.round(decodeMs), checked: 0, nanFrames: 0, overPeak: 0, maxPq: 0, minPq: 1, noParamsFrames: 0, perFrame: [] };
  const wall0 = performance.now();
  // Output order is presentation order; the parameters belong to the picture, so map by timestamp.
  for (let k = 0; k < frames.length; k++) {
    const f = frames[k], i = Math.round(f.timestamp / 41708);
    const params = pkts[i].params;
    if (!params) out.noParamsFrames++;
    comp.present(f, null, null, null, { profile: 'hdr10', hdr10plus: params });   // null: previous parameters stay in effect
    f.close();
    if (k % 12 === 0) {
      await comp.whenIdle();
      const { halfWords, bytesPerRow } = await comp.readTarget(1920, 1080);
      let nan = 0, mx = 0, mn = 1, sum = 0, n = 0;
      for (let y = 0; y < 1080; y += 3) for (let x = 0; x < 1920; x += 3) {
        const o = y * (bytesPerRow / 2) + x * 4;
        for (let c = 0; c < 3; c++) { const v = half(halfWords[o + c]); if (!Number.isFinite(v)) nan++; else { if (v > mx) mx = v; if (v < mn) mn = v; } }
        sum += half(halfWords[o + 1]); n++;
      }
      out.checked++; if (nan) out.nanFrames++;
      out.maxPq = Math.max(out.maxPq, mx); out.minPq = Math.min(out.minPq, mn);
      if (mx > nitsToPq(PEAK) + 5e-3) out.overPeak++;
      out.perFrame.push({ k, i, maxPq: +mx.toFixed(4), meanGPq: +(sum / n).toFixed(4), nan });
    }
  }
  await comp.whenIdle();
  out.presentWallMs = Math.round(performance.now() - wall0);
  out.peakPq = +nitsToPq(PEAK).toFixed(4);
  out.pass = out.nanFrames === 0 && out.overPeak === 0 && out.decoded === pkts.length && !out.error;
  comp.dispose(); target.destroy(); dec.close();
  log('decode ' + JSON.stringify({ ...out, perFrame: out.perFrame.length }));
  return out;
}

export async function runHdr10PlusTest(opts = {}) {
  const { adapter, device } = await getDevice();
  const out = { adapter: adapter.info ? { vendor: adapter.info.vendor, architecture: adapter.info.architecture } : null, userAgent: navigator.userAgent };
  const parsed = await parseStep(opts);
  out.parse = parsed.res;
  for (const [name, fn] of [['patches', () => patchStep(device, parsed.pkts)], ['ramp', () => rampStep(device, parsed.pkts)], ['decode', () => decodeStep(device, parsed, opts)]]) {
    try { out[name] = await fn(); } catch (e) { out[name] = { error: String(e.stack ?? e) }; log(name + ' ERROR ' + (e.stack ?? e)); }
  }
  window.hdr10plusResult = out;
  return out;
}
