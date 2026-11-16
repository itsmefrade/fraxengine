// SPDX-License-Identifier: LGPL-3.0-only
// Profile 5 colour check. For display frames of "Test Jellyfin 4K DV P5.mp4":
//   input      FFmpeg's decode of the base layer (the raw IPTPQc2 codes) and the RPU the engine parsed (web/engine/dovi.mjs), both made by tools/p5-prep.mjs
//   compositor FelCompositor.present(planes, tables, RPU display) -> rgba16float target, PQ encoded BT.2020 R'G'B', before display management
//              (rec2100-pq output, tone mapping off, no trims), read back
//   reference  float64 JS, written from the Dolby IPTPQc2 definition and sharing no code with the compositor:
//                I'P'T' = polynomial reshaping of the BL codes (the 1024 entry curve table the engine built from the RPU, flat 16 bit scale)
//                L'M'S' = ycc_to_rgb * (I'P'T' - offset)        (RPU matrix /2^13, offset /2^28, signal normalised to 1.0 = full scale)
//                LMS    = ST 2084 EOTF(L'M'S') * 10000 nit
//                RGB    = inverse(rgb_to_lms /2^14) * LMS        (linear BT.2020)
//                R'G'B' = ST 2084 inverse EOTF(RGB / 10000)
//              chroma planes are bilinearly upsampled with left-centre siting before the matrix, as the compositor documents (the sampling filter is
//              not part of the Dolby definition).
// Also run with the metadata dropped (the picture decoded as BT.2020 limited range YCbCr) to show what a missing RPU looks like.
import { FelCompositor } from '../../fel/fel-compositor.mjs';

const q = new URLSearchParams(location.search);
const out = document.getElementById('out');
const log = s => { out.textContent += s + '\n'; console.log(s); };
const W = 3840, H = 2160;
const K = (q.get('frames') || '0,100,200,300,400').split(',').map(Number);
const M1 = 2610 / 16384, M2 = 2523 / 32, C1 = 3424 / 4096, C2 = 2413 / 128, C3 = 2392 / 128;
const eotf = e => { const p = Math.pow(Math.min(Math.max(e, 0), 1), 1 / M2); return Math.pow(Math.max(p - C1, 0) / (C2 - C3 * p), 1 / M1); };       // fraction of 10000 nit
const oetf = y => { const p = Math.pow(Math.max(y, 0), M1); return Math.pow((C1 + C2 * p) / (1 + C3 * p), M2); };
const half = b => { const s = b & 0x8000 ? -1 : 1, e = (b >> 10) & 31, m = b & 1023; return e === 0 ? s * 2 ** -14 * (m / 1024) : e === 31 ? NaN : s * 2 ** (e - 15) * (1 + m / 1024); };
const toHalf = (() => { const f = new Float32Array(1), u = new Uint32Array(f.buffer); return v => { f[0] = v; const x = u[0], s = (x >>> 16) & 0x8000; let e = ((x >>> 23) & 255) - 112, m = x & 0x7fffff; if (e <= 0) return s; if (e >= 31) return s | 0x7c00; let h = (e << 10) | (m >>> 13); if ((m & 0x1fff) > 0x1000 || ((m & 0x1fff) === 0x1000 && (h & 1))) h++; return s | h; }; })();
const inv3 = m => { const [a, b, c, d, e, f, g, h, i] = m; const det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g); return [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det, (f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det, (d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det]; };
const fetchBytes = async url => { const r = await fetch(url); if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return new Uint8Array(await r.arrayBuffer()); };

function referenceFrame(planes, display, tables) {
  const PRED = 688, lut = [0, 1, 2].map(c => Int32Array.from(tables.subarray(PRED + c * 1024, PRED + c * 1024 + 1024)));
  // every piece of every curve must be the table driven polynomial kind for this to be the whole mapping (P5 has no MMR here)
  for (let c = 0; c < 3; c++) { const curve = 64 + c * 32, pieces = tables[curve] - 1; for (let i = 0; i < pieces; i++) if (tables[curve + 11 + i] !== 0) throw new Error('curve piece with direct evaluation, not covered by this reference'); }
  const M = display.yccToRgb.map(v => v / 8192), off = display.yccToRgbOffset.map(v => v / 268435456);
  const Linv = inv3(display.rgbToLms.map(v => v / 16384));
  const cw = W / 2, ch = H / 2;
  const Y = new Float64Array(W * H), U = new Float64Array(cw * ch), V = new Float64Array(cw * ch);
  for (let i = 0; i < W * H; i++) Y[i] = lut[0][planes.Y[i]] / 65536;
  for (let i = 0; i < cw * ch; i++) { U[i] = lut[1][planes.U[i]] / 65536; V[i] = lut[2][planes.V[i]] / 65536; }
  const at = (P, x, y) => P[Math.min(Math.max(y, 0), ch - 1) * cw + Math.min(Math.max(x, 0), cw - 1)];
  const up = (P, x, y) => { const ux = x * 0.5, uy = (y - 0.5) * 0.5, lx = Math.floor(ux), ly = Math.floor(uy), fx = ux - lx, fy = uy - ly; return (at(P, lx, ly) * (1 - fx) + at(P, lx + 1, ly) * fx) * (1 - fy) + (at(P, lx, ly + 1) * (1 - fx) + at(P, lx + 1, ly + 1) * fx) * fy; };
  const rgb = new Float64Array(W * H * 3);
  const a = display.l5, active = a && (a.left | a.right | a.top | a.bottom);
  for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
    const o = (y * W + x) * 3;
    if (active && (x + 0.5 < a.left || x + 0.5 >= W - a.right || y + 0.5 < a.top || y + 0.5 >= H - a.bottom)) continue;
    const s0 = Y[y * W + x] - off[0], s1 = up(U, x, y) - off[1], s2 = up(V, x, y) - off[2];
    const L = eotf(M[0] * s0 + M[1] * s1 + M[2] * s2), Mm = eotf(M[3] * s0 + M[4] * s1 + M[5] * s2), S = eotf(M[6] * s0 + M[7] * s1 + M[8] * s2);
    rgb[o] = oetf(Linv[0] * L + Linv[1] * Mm + Linv[2] * S);
    rgb[o + 1] = oetf(Linv[3] * L + Linv[4] * Mm + Linv[5] * S);
    rgb[o + 2] = oetf(Linv[6] * L + Linv[7] * Mm + Linv[8] * S);
  }
  return rgb;
}

function compare(refRgb, readback, active) {
  const bpr = readback.bytesPerRow / 2, hw = readback.halfWords;
  const stat = (useHalfRef, area) => {
    let sq = 0, n = 0, max = 0, sum = 0, nan = 0;
    for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
      if (area && !area(x, y)) continue;
      for (let c = 0; c < 3; c++) {
        const g = half(hw[y * bpr + x * 4 + c]);
        if (!Number.isFinite(g)) { nan++; continue; }
        let r = refRgb[(y * W + x) * 3 + c];
        if (useHalfRef) r = half(toHalf(r));
        const e = Math.abs(g - r); sq += e * e; sum += e; n++; if (e > max) max = e;
      }
    }
    const mse = sq / n;
    return { psnrDb: mse ? +(10 * Math.log10(1 / mse)).toFixed(2) : 'inf', maxErr: +max.toExponential(3), maxErr10bitCodes: +(max * 1023).toFixed(3), meanErr: +(sum / n).toExponential(3), nan, samples: n };
  };
  return { vsFloat64: stat(false), vsFloat64RoundedToHalf: stat(true), ...(active ? { insideActiveArea: stat(false, active) } : {}) };
}

export async function runP5Verify() {
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  const device = await adapter.requestDevice({ requiredLimits: { maxStorageBufferBindingSize: Math.min(adapter.limits.maxStorageBufferBindingSize, 1 << 30), maxBufferSize: Math.min(adapter.limits.maxBufferSize, 1 << 30) } });
  const target = device.createTexture({ size: [W, H], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
  const comp = await FelCompositor.create(device, null, { target, peakNits: 1000, canvasMaxWidth: 3840 });
  comp.canvasConfig = { colorSpace: 'rec2100-pq' };     // absolute BT.2020 PQ out: no gamut or white level scaling in the comparison
  const res = { test: 'p5-colour', headless: /Headless/i.test(navigator.userAgent) ? 'headless' : 'visible', userAgent: navigator.userAgent, adapter: [adapter.info?.vendor, adapter.info?.architecture].filter(Boolean).join(' '), frames: [], pass: false };
  for (const k of K) {
    const t0 = performance.now();
    const [yuv, rpu] = await Promise.all([fetchBytes(`fixtures/p5ref/bl-${k}.yuv`), fetch(`fixtures/p5ref/rpu-${k}.json`).then(r => r.json())]);
    const codes = new Uint16Array(yuv.buffer, yuv.byteOffset, yuv.byteLength / 2);
    const planes = { Y: codes.subarray(0, W * H), U: codes.subarray(W * H, W * H * 5 / 4), V: codes.subarray(W * H * 5 / 4) };
    const tb = Uint8Array.from(atob(rpu.tablesB64), c => c.charCodeAt(0));
    const tables = new Int32Array(tb.buffer);
    const display = rpu.display;
    const frame = { planes, width: W, height: H };
    const info = { profile: 'p5', toneMapping: 'off', trims: false };
    const rd = async () => { await comp.whenIdle(); return comp.readTarget(W, H); };
    comp.present(frame, null, tables, display, info);
    const withMeta = await rd();
    const t1 = performance.now();
    const ref = referenceFrame(planes, display, tables);
    const t2 = performance.now();
    const a = display.l5, areaFn = a && (a.left | a.right | a.top | a.bottom) ? (x, y) => !(x + 0.5 < a.left || x + 0.5 >= W - a.right || y + 0.5 < a.top || y + 0.5 >= H - a.bottom) : null;
    const row = { frame: k, rpu: { compressed: display.compressed, inherited: display.inherited, l5: display.l5, fullRange: display.signal.fullRange, yccOffset: display.yccToRgbOffset, curvePieces: [64, 96, 128].map(o => tables[o]) }, ...compare(ref, withMeta, areaFn) };
    // no RPU metadata: BT.2020 limited range YCbCr instead of IPTPQc2
    comp.present(frame, null, tables, null, info);
    row.withoutMetadata = compare(ref, await rd()).vsFloat64;
    row.ms = { compositor: Math.round(t1 - t0), reference: Math.round(t2 - t1) };
    res.frames.push(row);
    log(`frame ${k}: PSNR ${row.vsFloat64.psnrDb} dB, max ${row.vsFloat64.maxErr10bitCodes} 10-bit PQ codes (vs half rounded reference ${row.vsFloat64RoundedToHalf.psnrDb} dB, max ${row.vsFloat64RoundedToHalf.maxErr10bitCodes}); without metadata ${row.withoutMetadata.psnrDb} dB`);
  }
  res.summary = { minPsnrDb: Math.min(...res.frames.map(f => f.vsFloat64.psnrDb)), maxErr10bitCodes: Math.max(...res.frames.map(f => f.vsFloat64.maxErr10bitCodes)), minPsnrWithoutMetadataDb: Math.min(...res.frames.map(f => f.withoutMetadata.psnrDb)) };
  res.pass = res.summary.minPsnrDb > 60 && res.frames.every(f => f.vsFloat64.nan === 0);
  comp.dispose(); target.destroy();
  window.p5VerifyResult = res;
  log(JSON.stringify(res.summary));
  return res;
}
window.runP5Verify = runP5Verify;
if (q.has('auto')) runP5Verify().then(r => fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ test: 'p5-colour', name: q.get('name') || '', result: r }) })).catch(e => { log('ERROR ' + (e.stack || e)); window.p5VerifyResult = { error: String(e.stack || e) }; fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ test: 'p5-colour', error: String(e.stack || e) }) }); });
