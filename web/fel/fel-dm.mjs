// SPDX-License-Identifier: LGPL-3.0-only
// ST 2094-10 style display management ("DM mode st2094-10") for the FEL compositor: parameter building and the float64 reference.
//
// Everything here is plain f64 JavaScript with no imports (Node and browser). The WGSL in fel-shaders.mjs (`dm_map`) implements the same
// equations in f32 and consumes the parameter block that `packDmUniforms` writes; `dm-node-ref.mjs` and `dm-verify.mjs` compare the two.
// What is public and what is a design choice is spelled out in DM-THEORY.md ("What this implementation does"); the short version:
//   * tone curve on the I channel of ICtCp (BT.2100) or IPT-PQ (Profile 5), in the PQ domain, three anchors from RPU L1 min/avg/max:
//       'spline'   two cubic Hermite segments (Dolby patent US 10,600,166), default
//       'sigmoid'  c1 + c2 x / (1 + c3 x) in nits, SMPTE ST 2094-10 Annex B.2 (informative), y2 = sqrt(x2 sqrt(y1 y3))
//   * trims (slope, offset, power) applied to the curve output in PQ, interpolated in PQ of the target peak between the RPU's L2 / L8 targets
//   * P and T (Ct, Cp) follow the intensity change (US 10,540,920), saturation gain and chroma weight are approximations (see theory doc)

const M1 = 2610 / 16384, M2 = 2523 / 32, C1 = 3424 / 4096, C2 = 2413 / 128, C3 = 2392 / 128;
export const pqEotfNits = e => { const p = Math.pow(Math.min(Math.max(e, 0), 1), 1 / M2); return Math.pow(Math.max(p - C1, 0) / (C2 - C3 * p), 1 / M1) * 10000; };
export const pqInvNits = n => { const p = Math.pow(Math.max(n, 0) / 10000, M1); return Math.pow((C1 + C2 * p) / (1 + C3 * p), M2); };
const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);

const mulV = (m, v) => m.map(r => r[0] * v[0] + r[1] * v[1] + r[2] * v[2]);
function inv3(m) {
  const [a, b, c] = m[0], [d, e, f] = m[1], [g, h, i] = m[2];
  const det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
  return [[e * i - f * h, c * h - b * i, b * f - c * e], [f * g - d * i, a * i - c * g, c * d - a * f], [d * h - e * g, b * g - a * h, a * e - b * d]].map(r => r.map(x => x / det));
}

// BT.2100-2 table 7 (RGB BT.2020 linear -> LMS) and the L'M'S' -> ICtCp matrix, all exact /4096 integers.
export const ICTCP_RGB_TO_LMS = [[1688, 2146, 262], [683, 2951, 462], [99, 309, 3688]].map(r => r.map(v => v / 4096));
export const ICTCP_LMS_TO_ITP = [[2048, 2048, 0], [6610, -13613, 7003], [17933, -17390, -543]].map(r => r.map(v => v / 4096));
// Ebner and Fairchild IPT with PQ in place of the power law (the matrix of Dolby's IPT-PQ, Profile 5): I = 0.4 L' + 0.4 M' + 0.2 S'.
export const IPT_LMS_TO_IPT = [[0.4, 0.4, 0.2], [4.455, -4.851, 0.396], [0.8056, 0.3572, -1.1628]];
// Default BT.2020 -> HPE LMS (inverse of the HPE_TO_2020 constants of fel-meta.mjs, libplacebo 63fdd7db); Profile 5 RPUs carry their own rgb_to_lms.
const HPE_TO_2020 = [[3.06441879, -2.16597676, 0.10155818], [-0.65612108, 1.78554118, -0.12943749], [0.01736321, -0.04725154, 1.03004253]];
export const IPT_RGB_TO_LMS_DEFAULT = inv3(HPE_TO_2020);

/** Dolby L8 target_display_index to the target peak in nits (dovi_tool `trim_target_nits`, MIT); every other index is the 100 nit BT.709 family. */
export function l8TargetNits(index) {
  if ([16, 18, 21].includes(index)) return 48;
  if (index === 42) return 108;
  if ([24, 25].includes(index)) return 300;
  if ([27, 28].includes(index)) return 600;
  if ([48, 49].includes(index)) return 1000;
  if ([37, 38].includes(index)) return 2000;
  return 100;
}
const pq12 = nits => Math.round(pqInvNits(nits) * 4095);

export const DM_DEFAULTS = Object.freeze({
  curve: 'spline',          // 'spline' | 'sigmoid'
  space: 'auto',            // 'auto' (ipt for Profile 5, ictcp otherwise) | 'ictcp' | 'ipt'
  preservationHead: 0.5,    // US 10,600,166 Table 1 step 3
  preservationTail: 0.5,
  cutoff: 0.5,              // "a" of step 4
  contrastFactor: 1,        // slopeMid scale, suggested range 0.5 .. 2.0
  targetMinNits: 0,
  midFraction: 0.4,         // L1 avg missing: SMid = SMin + midFraction (SMax - SMin), a design choice
  sigmoidMidBound: 0.8,     // ST 2094-10 B.2: bound y2 at 0.8 y3
  chromaK1: 0.5, chromaK2: 1.0,   // US 10,540,920: CEDR = k1 Io + k2
  trims: true,
  peakGuard: true,          // scale RGB down when max(R,G,B) exceeds the target peak after the chroma stage
});

/**
 * Trim set for a display peak. `meta.l2` entries carry `targetMaxPq` (12 bit), `meta.l8` entries a `targetIndex`. Trims of the targets around the display
 * peak are interpolated linearly in the 12 bit PQ value of the target peak; below the lowest target the lowest is used; a neutral anchor (all 2048) sits at the
 * source (mastering) peak, so trims fade out towards the mastering display and are not applied at or above it. Returns null when there is nothing to apply.
 * Values are the raw 12 bit codes (2048 = neutral) plus their normalised form {slope, offset, power} as used in y = clamp(slope y + offset, 0, 1)^power.
 */
export function selectTrims(meta, displayPeakPq12, sourcePeakPq12) {
  const rows = [];
  for (const t of meta?.l2 ?? []) if (t.targetMaxPq > 0) rows.push({ pq: t.targetMaxPq, s: t.slope, o: t.offset, p: t.power, cw: t.chromaWeight, sg: t.saturationGain, from: 'L2' });
  for (const t of meta?.l8 ?? []) {
    const pq = pq12(l8TargetNits(t.targetIndex ?? t.targetDisplayIndex ?? 1));
    if (rows.some(r => Math.abs(r.pq - pq) <= 20)) continue;                   // CM v4.0 L2 is derived from L8: keep the explicit L2
    rows.push({ pq, s: t.slope, o: t.offset, p: t.power, cw: t.chromaWeight, sg: t.saturationGain, from: 'L8' });
  }
  if (!rows.length) return null;
  rows.sort((a, b) => a.pq - b.pq);
  if (displayPeakPq12 >= sourcePeakPq12) return null;                          // display at or above the mastering peak: no mapping, no trim
  if (sourcePeakPq12 > rows[rows.length - 1].pq) rows.push({ pq: sourcePeakPq12, s: 2048, o: 2048, p: 2048, cw: 2048, sg: 2048, from: 'neutral' });
  const t = displayPeakPq12;
  let a = rows[0], b = rows[0];
  if (t <= rows[0].pq) { a = b = rows[0]; }
  else if (t >= rows[rows.length - 1].pq) { a = b = rows[rows.length - 1]; }
  else for (let i = 0; i + 1 < rows.length; i++) if (t >= rows[i].pq && t <= rows[i + 1].pq) { a = rows[i]; b = rows[i + 1]; break; }
  const w = a === b || b.pq === a.pq ? 0 : (t - a.pq) / (b.pq - a.pq);
  const mix = k => a[k] + (b[k] - a[k]) * w;
  const n = v => (v - 2048) / 4096;                                            // 12 bit code to [-0.5, 0.5), 2048 neutral (patent: code = (value + 0.5) 4096)
  const raw = { slope: mix('s'), offset: mix('o'), power: mix('p'), chromaWeight: mix('cw'), saturationGain: mix('sg') };
  return {
    raw, from: [a.from + '@' + a.pq, b.from + '@' + b.pq], weight: w,
    slope: 1 + n(raw.slope), offset: n(raw.offset), power: 1 + n(raw.power), chromaWeight: n(raw.chromaWeight), saturationGain: n(raw.saturationGain),
  };
}

/** Source anchors (PQ, 0..1) from the RPU metadata, the same fallbacks as the BT.2390 path (L1, source_max_pq, sourcePeakNits, L6 for `static`). */
export function sourceAnchors(meta, display, opts) {
  let sMax, sMin = 0, sMid = null, from = 'sourcePeakNits';
  if (meta?.l1) { sMax = meta.l1.maxPq / 4095; sMin = meta.l1.minPq / 4095; sMid = meta.l1.avgPq / 4095; from = 'L1'; }
  else if (meta?.sourceMaxPq) { sMax = meta.sourceMaxPq / 4095; if (meta.sourceMinPq) sMin = meta.sourceMinPq / 4095; from = 'source_max_pq'; }
  else sMax = pqInvNits(display.sourcePeakNits ?? 1000);
  if (display.toneMapping === 'static' && meta?.l6) {            // mastering display range, per frame L1 statistics (including the average) are not used
    sMax = pqInvNits(meta.l6.maxMasteringNits); sMin = pqInvNits((meta.l6.minMasteringNits ?? 0) / 10000); sMid = null; from = 'L6';      // L6 minimum is in 1/10000 nit units
  }
  const eps = 4 / 4095;
  sMax = Math.max(sMax, sMin + 3 * eps);
  sMid = sMid === null ? sMin + opts.midFraction * (sMax - sMin) : sMid;
  sMid = clamp(sMid, sMin + eps, sMax - eps);                                  // ST 2094-10 6.1.9: min < avg < max
  return { sMin, sMid, sMax, from };
}

/** US 10,600,166 Table 1 steps 1 to 8, all values PQ coded 0..1. */
export function splineParams({ sMin, sMid, sMax }, tMinPq, tMaxPq, o) {
  const tdr = Math.max(tMaxPq - tMinPq, 1e-6);
  const midLoc = (sMid - tMinPq) / tdr, headroom = (sMax - sMid) / tdr, tailroom = (sMid - sMin) / tdr;
  const offsetHead = Math.min(Math.max(0, midLoc - o.cutoff) * tdr, Math.max(0, headroom * o.preservationHead + midLoc - 1) * tdr);
  const offsetTail = Math.min(Math.max(0, o.cutoff - midLoc) * tdr, Math.max(0, tailroom * o.preservationTail - midLoc) * tdr);
  const tMid = sMid - offsetHead + offsetTail;
  const tMax = Math.min(tMid + sMax - sMid, tMaxPq);
  const tMin = Math.max(tMid - sMid + sMin, tMinPq);
  const dMin = tMid - tMin, dMax = tMax - tMid, wMin = sMid - sMin, wMax = sMax - sMid;
  const maxMinSlope = 3 * dMin / wMin, maxMaxSlope = 3 * dMax / wMax;
  const slopeMin = Math.min(maxMinSlope, (dMin / wMin) ** 2);
  const slopeMax = Math.min(maxMaxSlope, 1, (dMax / wMax) ** 4);
  const slopeMid = Math.min(maxMinSlope, maxMaxSlope, o.contrastFactor * (1 - sMid + tMid));
  return { sMin, sMid, sMax, tMin, tMid, tMax, slopeMin, slopeMid, slopeMax };
}
const hermite = (t, y0, m0, y1, m1, w) => { const t2 = t * t, t3 = t2 * t; return (2 * t3 - 3 * t2 + 1) * y0 + (t3 - 2 * t2 + t) * w * m0 + (-2 * t3 + 3 * t2) * y1 + (t3 - t2) * w * m1; };
export function splineEval(x, p) {
  if (x <= p.sMin) return p.tMin + (x - p.sMin) * p.slopeMin;
  if (x >= p.sMax) return p.tMax + (x - p.sMax) * p.slopeMax;
  if (x < p.sMid) { const w = p.sMid - p.sMin; return hermite((x - p.sMin) / w, p.tMin, p.slopeMin, p.tMid, p.slopeMid, w); }
  const w = p.sMax - p.sMid;
  return hermite((x - p.sMid) / w, p.tMid, p.slopeMid, p.tMax, p.slopeMax, w);
}

/** ST 2094-10 B.2 equations (4) to (6), nits. Source inside the target range (x3 <= y3) is the identity (c = 1, 0, 0), see the theory doc. */
export function sigmoidParams({ sMin, sMid, sMax }, tMinNits, tMaxNits, o) {
  const x1 = Math.max(pqEotfNits(sMin), 1e-4), x2 = pqEotfNits(sMid), x3 = pqEotfNits(sMax);
  const y3 = Math.min(tMaxNits, x3), y1 = Math.max(tMinNits, x1, 1e-4);
  if (x3 <= tMaxNits * (1 + 1e-6) || !(x1 < x2 && x2 < x3)) return { c1: 0, c2: 1, c3: 0, identity: true, x1, x2, x3, y1, y2: x2, y3: x3 };
  const y2 = clamp(Math.min(Math.sqrt(x2 * Math.sqrt(y3 * y1)), o.sigmoidMidBound * y3), y1 * 1.0001, y3 * 0.9999);
  const alpha = x3 * y3 * (x1 - x2) + x2 * y2 * (x3 - x1) + x1 * y1 * (x2 - x3);
  let c1 = (x2 * x3 * (y2 - y3) * y1 + x1 * x3 * (y3 - y1) * y2 + x1 * x2 * (y1 - y2) * y3) / alpha;
  let c2 = ((x3 * y3 - x2 * y2) * y1 + (x1 * y1 - x3 * y3) * y2 + (x2 * y2 - x1 * y1) * y3) / alpha;
  let c3 = ((x3 - x2) * y1 + (x1 - x3) * y2 + (x2 - x1) * y3) / alpha;
  if (c3 < 0) c3 = 0;                                                         // monotonicity condition of B.2
  return { c1, c2, c3, identity: false, x1, x2, x3, y1, y2, y3 };
}
export const sigmoidEvalPq = (x, s) => { const l = pqEotfNits(x); return pqInvNits((s.c1 + s.c2 * l) / (1 + s.c3 * l)); };

/** Colour space matrices {m1: RGB(BT.2020 nits/10000 domain) -> LMS, m2: L'M'S' -> I/Ct/Cp (or I/P/T), and inverses}. */
export function dmMatrices(space, rgbToLms) {
  const m1 = space === 'ipt' ? (rgbToLms ?? IPT_RGB_TO_LMS_DEFAULT) : ICTCP_RGB_TO_LMS;
  const m2 = space === 'ipt' ? IPT_LMS_TO_IPT : ICTCP_LMS_TO_ITP;
  return { m1, m2, m1i: inv3(m1), m2i: inv3(m2) };
}

/**
 * Full parameter set. `display` = {peakNits, toneMapping, sourcePeakNits}; `meta` = normalised RPU display metadata or null; `profile` p5 selects IPT for space 'auto'.
 * Returns `bypass: true` when the transform would be the identity (source inside the target range, neutral trims): the shader then skips it.
 */
export function buildDmParams({ meta, display, profile = 'p7', opts = {} }) {
  const o = { ...DM_DEFAULTS, ...opts };
  const peak = display.peakNits;
  const src = sourceAnchors(meta, display, o);
  const tMinPq = o.targetMinNits > 0 ? pqInvNits(o.targetMinNits) : 0, tMaxPq = pqInvNits(peak);
  const space = o.space === 'auto' ? (profile === 'p5' ? 'ipt' : 'ictcp') : o.space;
  const sourcePeak12 = meta?.sourceMaxPq ?? Math.round(src.sMax * 4095);
  const trim = o.trims && display.trims !== false ? selectTrims(meta, Math.round(tMaxPq * 4095), sourcePeak12) : null;
  const curve = o.curve === 'sigmoid'
    ? { kind: 'sigmoid', ...sigmoidParams(src, o.targetMinNits, peak, o) }
    : { kind: 'spline', ...splineParams(src, tMinPq, tMaxPq, o) };
  const neutralTrim = !trim || (trim.slope === 1 && trim.offset === 0 && trim.power === 1 && trim.chromaWeight === 0 && trim.saturationGain === 0);
  const curveIdentity = curve.kind === 'sigmoid' ? curve.identity : (src.sMax <= tMaxPq + 1e-9 && src.sMin >= tMinPq - 1e-9);
  return {
    opts: o, space, src, tMinPq, tMaxPq, peak, curve, trim,
    sop: trim ? { slope: trim.slope, offset: trim.offset, power: trim.power } : null,
    satGain: trim?.saturationGain ?? 0, chromaWeight: trim?.chromaWeight ?? 0,
    matrices: dmMatrices(space, meta?.rgbToLms?.some?.(v => v !== 0) ? [0, 1, 2].map(r => meta.rgbToLms.slice(r * 3, r * 3 + 3)) : null),
    bypass: curveIdentity && neutralTrim,
  };
}

/** The tone curve on the I channel (PQ in, PQ out), clamped to the target range, trims included. */
export function dmTone(i, P) {
  let y = P.curve.kind === 'sigmoid' ? sigmoidEvalPq(i, P.curve) : splineEval(i, P.curve);
  y = clamp(y, P.tMinPq, P.tMaxPq);
  if (P.sop) y = Math.pow(clamp(P.sop.slope * y + P.sop.offset, 0, 1), P.sop.power);
  return clamp(y, P.tMinPq, P.tMaxPq);
}

/** Linear BT.2020 RGB in nits (one pixel) to the mapped pixel, same space. f64 reference of the WGSL `dm_map`. */
export function dmMapNits(rgb, P) {
  if (P.bypass) return rgb.map(v => Math.max(v, 0));
  const { m1, m2, m1i, m2i } = P.matrices, o = P.opts;
  const lmsp = mulV(m1, rgb.map(v => Math.max(v, 0))).map(v => pqInvNits(Math.max(v, 0)));
  const [i0, c0, p0] = mulV(m2, lmsp);
  const i1 = dmTone(i0, P);
  const ratio = i0 > 1e-5 ? i1 / i0 : 1;
  let k = ratio * (o.chromaK1 * i0 + o.chromaK2) / (o.chromaK1 * i1 + o.chromaK2);
  if (!(i0 > 1e-5)) k = 1;
  k *= 1 + P.satGain;
  const ct = c0 * k, cp = p0 * k;
  const i2 = Math.max(i1 - P.chromaWeight * Math.hypot(ct, cp), 0);
  const lmsOut = mulV(m2i, [i2, ct, cp]).map(v => pqEotfNits(clamp(v, 0, 1)));
  let out = mulV(m1i, lmsOut);
  if (o.peakGuard) { const m = Math.max(...out); if (m > P.peak) out = out.map(v => v * P.peak / m); }
  return out;
}

/** ITU-R BT.2124 Delta E ITP between two linear BT.2020 nits pixels (ICtCp with T = Ct / 2, scale 720). */
export function deltaEItp(a, b) {
  const f = rgb => { const [i, ct, cp] = mulV(ICTCP_LMS_TO_ITP, mulV(ICTCP_RGB_TO_LMS, rgb.map(v => Math.max(v, 0))).map(v => pqInvNits(v))); return [i, ct / 2, cp]; };
  const [i1, t1, p1] = f(a), [i2, t2, p2] = f(b);
  return 720 * Math.sqrt((i1 - i2) ** 2 + (t1 - t2) ** 2 + (p1 - p2) ** 2);
}

export const DM_UNIFORM_VEC4 = 19;     // vec4 18 .. 36 of the DISPLAY block; fel-meta.mjs UNIFORM_FLOATS covers them

/** Writes the DM block into `f` (Float32Array of the DISPLAY uniforms) starting at vec4 `at` (18). Layout documented in fel-shaders.mjs (`dmc` ... `dmd2`). */
export function packDmUniforms(f, P, at = 18) {
  const w = (v, a) => f.set(a, (at + v) * 4);
  const c = P.curve;
  w(0, [P.bypass ? 0 : 1, c.kind === 'sigmoid' ? 1 : 0, P.opts.peakGuard ? 1 : 0, P.peak]);
  if (c.kind === 'spline') {
    w(1, [c.sMin, c.sMid, c.sMax, P.tMinPq]);
    w(2, [c.tMin, c.tMid, c.tMax, P.tMaxPq]);
    w(3, [c.slopeMin, c.slopeMid, c.slopeMax, 0]);
    w(4, [0, 0, 0, 0]);
  } else {
    w(1, [0, 0, 0, P.tMinPq]); w(2, [0, 0, 0, P.tMaxPq]); w(3, [0, 0, 0, 0]);
    w(4, [c.c1, c.c2, c.c3, 0]);
  }
  w(5, P.sop ? [P.sop.slope, P.sop.offset, P.sop.power, 1] : [1, 0, 1, 0]);
  w(6, [P.satGain, P.chromaWeight, P.opts.chromaK1, P.opts.chromaK2]);
  const { m1, m2, m2i, m1i } = P.matrices;
  [m1, m2, m2i, m1i].forEach((m, k) => { for (let r = 0; r < 3; r++) w(7 + k * 3 + r, [m[r][0], m[r][1], m[r][2], 0]); });
}
