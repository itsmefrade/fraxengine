// SPDX-License-Identifier: LGPL-3.0-only
// Dolby Vision Profile 5 (IPTPQc2, single layer) through FelCompositor on "Test Jellyfin 4K DV P5.mp4" (media/p5-*, tools/prep-media.mjs).
//   codes   hardware decoded BL, RECOVER, codes against FFmpeg's exact planes at 8 checkpoints (SPS relabelled with matrix 9)
//   model   exact planes through the P5 DISPLAY path (reshape LUT, RPU ycc matrix, inverse rgb_to_lms) against the f64 model, 4000 pixels per frame
//   hw      same frame, hardware input, against the exact input result (PQ domain, tone mapping off) and with tone mapping on (finite check)
// window.runP5Test()
import { FelCompositor } from './fel-compositor.mjs';
import { getDevice, planeStats, fetchBytes } from './fel-test.mjs';
import { buildDisplayUniforms, buildReshapeLut, modelDisplayPixel, normalizeRpuDisplayMeta, pqToNits } from './fel-meta.mjs';

const log = s => { const el = document.getElementById('log'); if (el) el.textContent += s + '\n'; console.log(s); };
const half = b => { const s = b & 0x8000 ? -1 : 1, e = (b >> 10) & 31, m = b & 1023; return e === 0 ? s * 2 ** -14 * (m / 1024) : e === 31 ? NaN : s * 2 ** (e - 15) * (1 + m / 1024); };
const W = 3840, H = 2160, FRAME_BYTES = W * H * 3;

async function hwFrames(index, data, wanted) {
  // decode from the first keyframe through the last wanted display index, keep the wanted VideoFrames (cloned)
  const out = new Map(); let n = 0, err = null;
  const last = Math.max(...wanted);
  const dec = new VideoDecoder({ output: f => { if (wanted.includes(n)) out.set(n, f); else f.close(); n++; }, error: e => { err = String(e); } });
  dec.configure({ codec: 'hvc1.2.4.L153.90', codedWidth: W, codedHeight: H, hardwareAcceleration: 'prefer-hardware' });
  const dur = 1001000 / 60000 * 1000;
  for (let i = 0; i <= index.display.indexOf(last) + 8 && i < index.aus.length; i++) {
    const a = index.aus[i];
    while (dec.decodeQueueSize > 8) await new Promise(r => setTimeout(r, 5));
    dec.decode(new EncodedVideoChunk({ type: a.key ? 'key' : 'delta', timestamp: i * dur, data: data.subarray(a.bl[0], a.bl[0] + a.bl[1]) }));
  }
  await dec.flush(); dec.close();
  return { frames: out, decoded: n, err };
}

export async function runP5Test(opts = {}) {
  const { device } = await getDevice();
  const [index, data, cps, metas, goldBl] = await Promise.all([
    fetch('./media/p5-retag-index.json').then(r => r.json()), fetchBytes('./media/p5-retag-bl.hevc'), fetch('./media/p5-checkpoints.json').then(r => r.json()),
    fetch('./media/p5-meta.json').then(r => r.json()), fetchBytes('./media/p5-gold-bl.yuv')]);
  const frames = cps.frames;
  const target = device.createTexture({ size: [W, H], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
  const comp = await FelCompositor.create(device, null, { target, peakNits: 1000, canvasMaxWidth: 3840 });
  comp.canvasConfig = { colorSpace: 'rec2100-pq' };          // PQ of absolute BT.2020 nits out: no gamut or white scaling in the comparison
  const hw = await hwFrames(index, data, frames);
  const out = { decoded: hw.decoded, decodeError: hw.err, rows: [] };
  const rd = async () => { await comp.whenIdle(); return comp.readTarget(W, H); };
  for (let c = 0; c < frames.length; c++) {
    const k = frames[c], rj = metas[String(k)], meta = normalizeRpuDisplayMeta(rj.vdr_dm_data);
    const gold = new Uint16Array(goldBl.buffer, goldBl.byteOffset + c * FRAME_BYTES, FRAME_BYTES / 2);
    const plane = { Y: gold.subarray(0, W * H), U: gold.subarray(W * H, W * H * 5 / 4), V: gold.subarray(W * H * 5 / 4) };
    const exactFrame = { planes: plane, width: W, height: H };
    const info = { profile: 'p5', rpuJson: rj, toneMapping: 'off', trims: false };
    const row = { frame: k };
    // exact planes -> DISPLAY, against the f64 model
    comp.present(exactFrame, null, null, meta, info);
    const exact = await rd();
    const u = buildDisplayUniforms({ meta, display: { ...comp.display, ...info }, profile: 'p5', scale: 1024, kind: 1, canvasColorSpace: 'rec2100-pq', dims: [W, H, W / 2, H / 2], reshape: true, supersample: 1 });
    const lut = buildReshapeLut(rj);
    const el = (p, x, y) => {
      const w = p ? W / 2 : W, h = p ? H / 2 : H, arr = p === 0 ? plane.Y : p === 1 ? plane.U : plane.V;
      return lut[p * 1024 + Math.min(arr[Math.min(Math.max(y, 0), h - 1) * w + Math.min(Math.max(x, 0), w - 1)], 1023)];
    };
    const up = (p, x, y) => { const ux = (x - 0) * 0.5, uy = (y - 0.5) * 0.5, lx = Math.floor(ux), ly = Math.floor(uy), fx = ux - lx, fy = uy - ly;
      const m = (a, b, t) => a + (b - a) * t; return m(m(el(p, lx, ly), el(p, lx + 1, ly), fx), m(el(p, lx, ly + 1), el(p, lx + 1, ly + 1), fx), fy); };
    let worst = 0, sumErr = 0, nan = 0, n = 0;
    const bpr = exact.bytesPerRow / 2;
    for (let s = 0; s < 4000; s++) {
      const x = (Math.floor(((s * 2654435761) >>> 0) % (W / 2))) * 2, y = Math.floor(((s * 40503 + 7) >>> 0) % H);
      const sig = [el(0, x, y), up(1, x, y), up(2, x, y)];
      const want = modelDisplayPixel(sig, u.data, 'rec2100-pq');
      for (let ch = 0; ch < 3; ch++) { const g = half(exact.halfWords[y * bpr + x * 4 + ch]); if (!Number.isFinite(g)) { nan++; continue; } const e = Math.abs(g - want[ch]); worst = Math.max(worst, e); sumErr += e; n++; }
    }
    row.model = { samples: 4000, maxPqErr: +worst.toExponential(2), meanPqErr: +(sumErr / n).toExponential(2), nan, pass: worst < 3e-3 && nan === 0 };
    // hardware frame
    const vf = hw.frames.get(k);
    if (!vf) { row.hw = { error: 'frame not decoded' }; out.rows.push(row); continue; }
    comp.present(vf, null, null, meta, info);
    const hwOut = await rd();
    const codes = await comp.readBaseCodes();
    row.codesVsExact = planeStats(codes, gold, 1023, [['Y', 0, W * H], ['U', W * H, W * H / 4], ['V', W * H * 5 / 4, W * H / 4]]);
    // PQ domain difference between hardware and exact input (max channel, over a strided grid)
    let dmax = 0, dsum = 0, dn = 0, hnan = 0;
    for (let y = 0; y < H; y += 5) for (let x = 0; x < W; x += 5) for (let ch = 0; ch < 3; ch++) {
      const a = half(hwOut.halfWords[y * bpr + x * 4 + ch]), b = half(exact.halfWords[y * bpr + x * 4 + ch]);
      if (!Number.isFinite(a)) { hnan++; continue; }
      const d = Math.abs(a - b); dmax = Math.max(dmax, d); dsum += d; dn++;
    }
    row.hwVsExactPq = { max: +dmax.toFixed(4), mean: +(dsum / dn).toExponential(2), nan: hnan };
    // tone mapped, finite check
    comp.present(vf, null, null, meta, { profile: 'p5', rpuJson: rj });
    const tm = await rd();
    let tnan = 0, tmax = 0; for (let y = 0; y < H; y += 7) for (let x = 0; x < W; x += 7) for (let ch = 0; ch < 3; ch++) { const v = half(tm.halfWords[y * bpr + x * 4 + ch]); if (!Number.isFinite(v)) tnan++; else tmax = Math.max(tmax, v); }
    row.toneMapped = { nan: tnan, maxPq: +tmax.toFixed(4), srcMaxNitsFromRpu: +pqToNits(meta.l1?.maxPq ? meta.l1.maxPq / 4095 : meta.sourceMaxPq / 4095).toFixed(0) };
    vf.close();
    out.rows.push(row);
    log(`p5 frame ${k}: model max ${row.model.maxPqErr} | codes Y ${row.codesVsExact.Y.psnrDb} dB max ${row.codesVsExact.Y.maxAbs} U ${row.codesVsExact.U.psnrDb} V ${row.codesVsExact.V.psnrDb} | hw vs exact PQ max ${row.hwVsExactPq.max} mean ${row.hwVsExactPq.mean}`);
  }
  const pick = f => out.rows.map(f).filter(v => v !== undefined);
  out.summary = {
    modelPass: out.rows.every(r => r.model?.pass),
    codesPsnrY: pick(r => r.codesVsExact?.Y.psnrDb), codesPsnrU: pick(r => r.codesVsExact?.U.psnrDb), codesPsnrV: pick(r => r.codesVsExact?.V.psnrDb),
    codesMaxAbs: pick(r => r.codesVsExact?.all.maxAbs),
    hwVsExactPqMean: pick(r => r.hwVsExactPq?.mean), toneMappedNan: pick(r => r.toneMapped?.nan),
  };
  comp.dispose(); target.destroy();
  window.p5Result = out;
  log('p5 summary ' + JSON.stringify(out.summary));
  return out;
}
