// SPDX-License-Identifier: LGPL-3.0-only
// Colour maths and RPU display metadata handling for the FEL compositor (CPU side, f64).
// The shader in fel-shaders.mjs implements the same equations in f32; test code checks both.
import { buildDmParams, packDmUniforms, dmMapNits } from './fel-dm.mjs';

const M1 = 2610 / 16384, M2 = 2523 / 32, C1 = 3424 / 4096, C2 = 2413 / 128, C3 = 2392 / 128;
/** Normalised PQ signal (0..1) -> nits. */
export function pqToNits(e) {
  const p = Math.pow(Math.min(Math.max(e, 0), 1), 1 / M2);
  return Math.pow(Math.max(p - C1, 0) / (C2 - C3 * p), 1 / M1) * 10000;
}
/** Nits -> normalised PQ signal. */
export function nitsToPq(n) {
  const p = Math.pow(Math.max(n, 0) / 10000, M1);
  return Math.pow((C1 + C2 * p) / (1 + C3 * p), M2);
}
const pq12 = code => code / 4095;

// BT.2100 HLG (ITU-R BT.2100-2 table 5 and 6), f64 reference of the shader's HLG branch.
const HLG_A = 0.17883277, HLG_B = 1 - 4 * HLG_A, HLG_C = 0.5 - HLG_A * Math.log(4 * HLG_A);
/** Inverse OETF: non-linear E' (0..1) -> scene light E (0..1). */
export function hlgInvOetf(x) { const e = Math.min(Math.max(x, 0), 1); return e <= 0.5 ? e * e / 3 : (Math.exp((e - HLG_C) / HLG_A) + HLG_B) / 12; }
/** OOTF system gamma for a nominal display peak: 1.2 + 0.42 log10(Lw / 1000) (BT.2100 note 5b), 1.2 at 1000 nit. */
export function hlgSystemGamma(peakNits) { return 1.2 + 0.42 * Math.log10(peakNits / 1000); }
/**
 * Peak the HLG OOTF is evaluated at: the display peak when it is 400 to 2000 nit (the range the gamma law is specified for, 1000 nit gives the
 * reference gamma 1.2 and diffuse white 203 nit at signal 0.75). A dimmer display gets the 1000 nit reference rendition tone mapped by the same
 * BT.2390 stage as PQ content (the source peak for it is this value), so its diffuse white stays at the SDR white level instead of dropping with Lw.
 */
export function hlgOotfPeak(displayPeakNits) { return displayPeakNits < 400 ? 1000 : Math.min(displayPeakNits, 2000); }
/** HLG R'G'B' (0..1 each) -> display light in nits (BT.2020 primaries), black level 0. */
export function hlgToNits(rgb, peakNits) {
  const e = rgb.map(hlgInvOetf), ys = Math.max(0.2627 * e[0] + 0.6780 * e[1] + 0.0593 * e[2], 1e-9);
  const k = peakNits * Math.pow(ys, hlgSystemGamma(peakNits) - 1);
  return e.map(v => v * k);
}

// Primaries (exact rational XYZ matrices).
const REC2020_XYZ = [[63426534 / 99577255, 20160776 / 139408157, 47086771 / 278816314], [26158966 / 99577255, 472592308 / 697040785, 8267143 / 139408157], [0, 19567812 / 697040785, 295819943 / 278816314]];
const P3_XYZ = [[608311 / 1250200, 189793 / 714400, 198249 / 1000160], [35783 / 156275, 247089 / 357200, 198249 / 2500400], [0, 32229 / 714400, 5220557 / 5000800]];
const SRGB_XYZ = [[506752 / 1228815, 87881 / 245763, 12673 / 70218], [87098 / 409605, 175762 / 245763, 12673 / 175545], [7918 / 409605, 87881 / 737289, 1001167 / 1053270]];
// Hunt-Pointer-Estevez LMS -> BT.2020 RGB (libplacebo 63fdd7db).
const HPE_TO_2020 = [[3.06441879, -2.16597676, 0.10155818], [-0.65612108, 1.78554118, -0.12943749], [0.01736321, -0.04725154, 1.03004253]];
export const IDENTITY = [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
export const mul = (a, b) => a.map(r => b[0].map((_, c) => r.reduce((s, v, i) => s + v * b[i][c], 0)));
export function inv3(m) {
  const [a, b, c] = m[0], [d, e, f] = m[1], [g, h, i] = m[2];
  const det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
  if (Math.abs(det) < 1e-12) throw new Error('singular colour matrix');
  return [[e * i - f * h, c * h - b * i, b * f - c * e], [f * g - d * i, a * i - c * g, c * d - a * f], [d * h - e * g, b * g - a * h, a * e - b * d]].map(r => r.map(x => x / det));
}
export const GAMUTS = {
  'display-p3': mul(inv3(P3_XYZ), REC2020_XYZ),
  srgb: mul(inv3(SRGB_XYZ), REC2020_XYZ),
  bt2020: IDENTITY,
};

/** Standard BT.2020 NCL limited range 10-bit YCbCr (code / 1024 domain) -> PQ R'G'B'. */
export function bt2020LimitedMatrix() {
  const KR = 0.2627, KB = 0.0593, KG = 1 - KR - KB, sy = 1024 / 876, sc = 1024 / 896;
  const cr = 2 * (1 - KR), cb = 2 * (1 - KB);
  return { matrix: [[sy, 0, cr * sc], [sy, -(KB / KG) * cb * sc, -(KR / KG) * cr * sc], [sy, cb * sc, 0]], offsets: [64 / 1024, 512 / 1024, 512 / 1024] };
}

const levelOf = (blocks, name) => (blocks || []).map(b => b?.[name]).filter(Boolean);

/**
 * Normalises the display metadata of one RPU. Input is either an already normalised object (has `yccToRgb`) or
 * the `vdr_dm_data` object of libdovi's JSON (what `dovi_tool` prints; field names `ycc_to_rgb_coef0`, ...), or the display
 * metadata of web/engine/dovi.mjs (`parseRpu(nal).display`, has `norm`), which goes through `fromEngineDisplay`.
 */
export function normalizeRpuDisplayMeta(raw) {
  if (!raw) return null;
  if (raw.norm && raw.signal && raw.yccToRgb) return fromEngineDisplay(raw);
  if (raw.yccToRgb) return raw;
  const dm = raw.vdr_dm_data ?? raw;
  if (dm.ycc_to_rgb_coef0 === undefined) throw new Error('not RPU display metadata');
  const arr = (p, n) => Array.from({ length: n }, (_, i) => dm[p + i]);
  const blocks29 = dm.cmv29_metadata?.ext_metadata_blocks ?? [];
  const blocks40 = dm.cmv40_metadata?.ext_metadata_blocks ?? [];
  const all = [...blocks29, ...blocks40];
  const l1 = levelOf(all, 'Level1')[0];
  const l3 = levelOf(all, 'Level3')[0];
  const l6 = levelOf(all, 'Level6')[0];
  let level1 = l1 ? { minPq: l1.min_pq, avgPq: l1.avg_pq, maxPq: l1.max_pq } : null;
  if (level1 && l3) {
    // CMv4.0 Level 3 stores offsets from Level 1 (2048 = no offset).
    const off = (v, o) => Math.min(4095, Math.max(0, v + o - 2048));
    level1 = { minPq: off(level1.minPq, l3.min_pq_offset), avgPq: off(level1.avgPq, l3.avg_pq_offset), maxPq: off(level1.maxPq, l3.max_pq_offset) };
  }
  const trim = t => ({ targetMaxPq: t.target_max_pq, slope: t.trim_slope, offset: t.trim_offset, power: t.trim_power, chromaWeight: t.trim_chroma_weight, saturationGain: t.trim_saturation_gain, msWeight: t.ms_weight });
  return {
    yccToRgb: arr('ycc_to_rgb_coef', 9).map(v => v / 8192),
    yccOffset: arr('ycc_to_rgb_offset', 3).map(v => v / 2 ** 28),
    rgbToLms: arr('rgb_to_lms_coef', 9).map(v => v / 16384),
    signalBitDepth: dm.signal_bit_depth, signalFullRange: dm.signal_full_range_flag,
    sourceMinPq: dm.source_min_pq, sourceMaxPq: dm.source_max_pq,
    l1: level1,
    l2: levelOf(all, 'Level2').map(trim),
    // Level 8 targets are indexed (1 = 100 nit, 2 = 600?, ...), the indexes are kept, not interpreted.
    l8: levelOf(all, 'Level8').map(t => ({ targetIndex: t.target_display_index, ...trim(t) })),
    l6: l6 ? { maxMasteringNits: l6.max_display_mastering_luminance, minMasteringNits: l6.min_display_mastering_luminance, maxCll: l6.max_content_light_level, maxFall: l6.max_frame_average_light_level } : null,
  };
}

/**
 * Display metadata as returned by `web/engine/dovi.mjs` (`createDoviSession().parseRpu(nal).display`) in the normalised form of this
 * file. Compressed RPUs arrive with the matrices of the last full RPU already filled in (`inherited`). Returns null when the RPU
 * (and its history) carries no matrices, so the caller falls back to the BT.2020 limited range default.
 */
export function fromEngineDisplay(d) {
  const n = d.norm;
  if (!n.yccToRgb.some(v => v !== 0)) return null;
  let l1 = d.l1 ? { minPq: d.l1.minPq, avgPq: d.l1.avgPq, maxPq: d.l1.maxPq } : null;
  if (l1 && d.l3) {
    // CMv4.0 Level 3 stores offsets from Level 1 (2048 = no offset).
    const off = (v, o) => Math.min(4095, Math.max(0, v + o - 2048));
    l1 = { minPq: off(l1.minPq, d.l3.minPqOffset), avgPq: off(l1.avgPq, d.l3.avgPqOffset), maxPq: off(l1.maxPq, d.l3.maxPqOffset) };
  }
  const trim = t => ({ targetMaxPq: t.targetMaxPq, slope: t.slope, offset: t.offset, power: t.power, chromaWeight: t.chromaWeight, saturationGain: t.saturationGain, msWeight: t.msWeight });
  const l5 = d.l5 && (d.l5.left | d.l5.right | d.l5.top | d.l5.bottom) ? { left: d.l5.left, right: d.l5.right, top: d.l5.top, bottom: d.l5.bottom } : null;
  return {
    yccToRgb: n.yccToRgb, yccOffset: n.yccToRgbOffset, rgbToLms: n.rgbToLms,
    signalBitDepth: d.signal?.bitDepth, signalFullRange: d.signal?.fullRange,
    sourceMinPq: d.source?.minPq, sourceMaxPq: d.source?.maxPq,
    l1, l2: (d.l2 || []).map(trim),
    l8: (d.l8 || []).map(t => ({ targetIndex: t.targetDisplayIndex, ...trim({ ...t, targetMaxPq: undefined }) })),
    l6: d.l6 ? { maxMasteringNits: d.l6.maxMasteringLuminance, minMasteringNits: d.l6.minMasteringLuminance, maxCll: d.l6.maxCll, maxFall: d.l6.maxFall } : null,
    l5,
  };
}

/**
 * The single pass form of the RPU mapping for one hardware decoded layer: a 3 x 1024 float LUT (component * 1024 + 10 bit code)
 * from the GPU table packet of rust/aether-dovi/src/fel/gpu.rs (`aether_fel_gpu_tables`, 6832 words). Entry = the exact
 * Reference16 prediction of that code / 65536, which is what COMPOSE writes and DISPLAY reads for the same sample.
 * Returns null when one pass cannot carry the mapping: a residual (word 5 is 0, an enhancement layer is needed) or an MMR
 * piece (word 6, three inputs per sample). Those keep RECOVER + COMPOSE + DISPLAY.
 */
export const GPU_TABLE_WORDS = 6832, GPU_TABLE_MAGIC = 0x46474c31, GPU_PRED_OFFSET = 688, GPU_DESC_OFFSET = 64;
export function buildMappingLut(tables, out = new Float32Array(3072)) {
  if (!tables || tables.length < GPU_TABLE_WORDS || tables[0] !== GPU_TABLE_MAGIC) return null;
  if (tables[5] !== 1 || tables[6] !== 0) return null;
  for (let c = 0; c < 3; c++) {
    const desc = GPU_DESC_OFFSET + c * 32, pieces = tables[desc] - 1;
    if (pieces < 1 || pieces > 8) return null;
    for (let p = 0; p < pieces; p++) if (tables[desc + 11 + p] !== 0) return null;
    for (let code = 0; code < 1024; code++) out[c * 1024 + code] = tables[GPU_PRED_OFFSET + c * 1024 + code] / 65536;
  }
  return out;
}

/**
 * Builds a 3 x 1024 float LUT for Profile 5 base layer reshaping from a libdovi RPU JSON dump
 * (`header` + `rpu_data_mapping`). Polynomial pieces only; MMR curves throw (not implemented in f32 here).
 * Value = polynomial(code / 1024), normalised, the shader uses it directly in place of code / 1024.
 */
export function buildReshapeLut(rpuJson) {
  const denom = rpuJson.header.coefficient_log2_denom;
  const lut = new Float32Array(3072);
  rpuJson.rpu_data_mapping.curves.forEach((curve, c) => {
    if (curve.mapping_idc !== 'Polynomial') throw new Error('MMR reshaping is only available through the exact FEL tables');
    const pivots = []; let sum = 0;
    for (const p of curve.pivots) { sum += p; pivots.push(sum); }
    const pieces = pivots.length - 1;
    for (let code = 0; code < 1024; code++) {
      let k = 0;
      for (let i = 0; i < pieces; i++) if (code >= pivots[i]) k = i;
      const cl = Math.min(Math.max(code, pivots[0]), pivots[pivots.length - 1]);
      const ints = curve.poly_coef_int[k], fracs = curve.poly_coef[k];
      const coef = j => (ints[j] ?? 0) + (fracs[j] ?? 0) / 2 ** denom;
      const x = cl / 1024;
      const order = curve.poly_order_minus1[k] + 1;
      let v = coef(0) + coef(1) * x;
      if (order === 2) v += coef(2) * x * x;
      lut[c * 1024 + code] = Math.min(Math.max(v, 0), 1);
    }
  });
  return lut;
}

/** BT.2390 EETF on PQ code values, same as the shader. */
export function eetf(e, hi, lo, tgt) {
  if (hi <= tgt) return e;
  const span = Math.max(hi - lo, 1e-6), maxLum = (tgt - lo) / span, ks = 1.5 * maxLum - 0.5;
  const e1 = Math.min(Math.max((e - lo) / span, 0), 1);
  let e2 = e1;
  if (e1 >= ks) {
    const t = (e1 - ks) / (1 - ks), t2 = t * t, t3 = t2 * t;
    e2 = (2 * t3 - 3 * t2 + 1) * ks + (t3 - 2 * t2 + t) * (1 - ks) + (-2 * t3 + 3 * t2) * maxLum;
  }
  return e2 * span + lo;
}

/** Chooses the Level 2 / Level 8 trim whose target is closest to the display peak (within `tolerance` PQ codes). */
export function pickTrim(meta, peakPq12, tolerance = 250) {
  let best = null;
  for (const t of [...(meta?.l2 ?? []), ...(meta?.l8 ?? []).filter(t => t.targetMaxPq !== undefined)]) {
    const d = Math.abs(t.targetMaxPq - peakPq12);
    if (d <= tolerance && (!best || d < best.d)) best = { d, t };
  }
  return best?.t ?? null;
}

/** Floats in the DISPLAY uniform block (vec4 16 is the Level 5 active area: left, right, top, bottom in pixels, all zero = off; vec4 17 is the HLG block; vec4 18 to 36 the ST 2094-10 DM block, fel-dm.mjs). */
export const UNIFORM_FLOATS = 148;
export const OUT_MODE = { 'display-p3': 0, srgb: 0, 'rec2100-pq': 1, 'rec2100-linear': 2, 'display-p3-linear': 2, 'srgb-linear': 2 };

/**
 * Packs the DISPLAY uniform block (64 vec4 = 1024 bytes at most, 37 used).
 * @param {object} p {meta, display, profile, scale, kind, dims:[w,h,cw,ch], canvasColorSpace, reshape}
 */
export function buildDisplayUniforms({ meta, display, profile, scale, kind, dims, canvasColorSpace, reshape, supersample = 1, transfer = 'pq' }) {
  const f = new Float32Array(UNIFORM_FLOATS), u = new Uint32Array(f.buffer);
  const peak = display.peakNits, sdr = display.sdrWhiteNits;
  let matrix, offsets, linear;
  const rpuSignal = (profile === 'p7' || profile === 'p5') && meta;
  if (rpuSignal) {
    matrix = [0, 1, 2].map(r => meta.yccToRgb.slice(r * 3, r * 3 + 3));
    offsets = meta.yccOffset;
    const lms = [0, 1, 2].map(r => meta.rgbToLms.slice(r * 3, r * 3 + 3));
    // P7 signals are BT.2020 R'G'B' after the YCC matrix, HPE * rgb2lms is (nearly) identity there.
    // P5 (IPTPQc2) signals are L'M'S', the linear stage is the inverse of rgb2lms.
    linear = profile === 'p5' ? inv3(lms) : mul(HPE_TO_2020, lms);
  } else {
    ({ matrix, offsets } = bt2020LimitedMatrix());
    linear = IDENTITY;
  }
  const row = (m, i, at) => { f.set(m[i], at); };
  for (let i = 0; i < 3; i++) { row(matrix, i, i * 4); row(linear, i, 16 + i * 4); row(GAMUTS[canvasColorSpace === 'rec2100-pq' || canvasColorSpace === 'rec2100-linear' ? 'bt2020' : canvasColorSpace === 'srgb-linear' ? 'srgb' : canvasColorSpace] ?? GAMUTS['display-p3'], i, 28 + i * 4); }
  f.set(offsets, 12);
  // dims at f[40..43] (u32), cfg f[44..47]
  u.set(dims, 40);
  const phase = { 'left-center': [0, 0.5], left: [0, 0.5], center: [0.5, 0.5], 'top-left': [0, 0] }[display.chromaSiting ?? 'left-center'];
  f.set([scale, phase[0], phase[1], kind], 44);
  // tone mapping
  const peakPq = nitsToPq(peak);
  const hlg = transfer === 'hlg', hlgPeak = hlgOotfPeak(peak);
  let srcMax, srcMin = 0;
  if (hlg) srcMax = nitsToPq(hlgPeak);      // HLG has no mastering metadata: the OOTF output peak is the source peak
  else if (meta?.l1) { srcMax = pq12(meta.l1.maxPq); srcMin = pq12(meta.l1.minPq); }
  else if (meta?.sourceMaxPq) srcMax = pq12(meta.sourceMaxPq);
  else srcMax = nitsToPq(display.sourcePeakNits);
  if (display.toneMapping === 'static' && meta?.l6 && !hlg) srcMax = nitsToPq(meta.l6.maxMasteringNits);
  if (display.toneMapping === 'off') srcMax = 0;
  f.set([srcMax, srcMin, peakPq, display.toneMapping === 'off' ? 0 : 1], 48);
  f.set([sdr, peak, display.elementCurve ? 3 : OUT_MODE[canvasColorSpace] ?? 0, reshape ? 1 : 0], 52);
  const trim = display.trims === false || hlg ? null : pickTrim(meta, peakPq * 4095);
  if (trim) {
    const n = v => (v - 2048) / 4096;
    f.set([1 + n(trim.slope), n(trim.offset), 1 + n(trim.power), 1], 56);
  } else f.set([1, 0, 1, 0], 56);
  f.set([supersample, 0, 0, 0], 60);
  const a = meta?.l5;
  if (a && display.activeArea !== false) f.set([a.left, a.right, a.top, a.bottom], 64);
  if (hlg) f.set([1, hlgSystemGamma(hlgPeak), hlgPeak, 0], 68);
  // display.dm 'st2094-10': Dolby style display management (fel-dm.mjs) replaces the BT.2390 stage and its trims; HLG and 'off' keep the BT.2390 path
  let dm = null;
  if (display.dm === 'st2094-10' && !hlg && display.toneMapping !== 'off') {
    dm = buildDmParams({ meta, display, profile, opts: display.dmOptions });
    packDmUniforms(f, dm);
    f.set([srcMax, srcMin, peakPq, 0], 48);
    f.set([1, 0, 1, 0], 56);
  }
  return { data: f, info: { srcMaxPq: srcMax, srcMinPq: srcMin, peakPq, trim, linear, matrix, offsets, dm } };
}

/** f64 model of the whole DISPLAY stage for one pixel, used by the numeric test. */
export function modelDisplayPixel(sig, uniforms, canvasColorSpace, dm = null) {
  const f = uniforms, g = i => f[i];
  const M = [0, 1, 2].map(r => [g(r * 4), g(r * 4 + 1), g(r * 4 + 2)]);
  const off = [g(12), g(13), g(14)];
  const L = [0, 1, 2].map(r => [g(16 + r * 4), g(17 + r * 4), g(18 + r * 4)]);
  const D = [0, 1, 2].map(r => [g(28 + r * 4), g(29 + r * 4), g(30 + r * 4)]);
  const mv = (m, v) => m.map(r => r[0] * v[0] + r[1] * v[1] + r[2] * v[2]);
  const nl = mv(M, sig.map((v, i) => v - off[i]));
  const nits = g(68) > 0.5 ? hlgToNits(nl, g(70)) : nl.map(v => pqToNits(v));
  let lin = mv(L, nits).map(v => Math.max(v, 0));
  const hi = g(48), lo = g(49), tgt = g(50), on = g(51) > 0.5;
  const m = Math.max(...lin);
  if (dm && !dm.bypass) lin = dmMapNits(lin, dm);
  else if (on && m > 1e-3) {
    let e = nitsToPq(m);
    e = eetf(e, hi, lo, tgt);
    if (g(59) > 0.5) e = Math.pow(Math.min(Math.max(g(56) * e + g(57), 0), 1), g(58));
    lin = lin.map(v => v * pqToNits(e) / m);
  }
  const disp = mv(D, lin).map(v => Math.max(v, 0));
  const sdr = g(52), mode = g(54);
  const enc = x => { const a = Math.abs(x); return a <= 0.0031308 ? 12.92 * x : Math.sign(x) * (1.055 * Math.pow(a, 1 / 2.4) - 0.055); };
  if (mode < 0.5) return disp.map(v => enc(v / sdr));
  if (mode < 1.5) return disp.map(v => nitsToPq(v));
  return disp.map(v => v / sdr);
}
