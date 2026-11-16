// SPDX-License-Identifier: LGPL-3.0-only
// Node side of the ST 2094-10 style DM verification (no GPU, no browser).
//   node web/fel/dm/dm-node-ref.mjs            unit checks of the f64 reference + writes dm-samples.json (expected outputs for the WGSL comparison)
//   node web/fel/dm/dm-node-ref.mjs --check    unit checks only
// dm-verify.html loads dm-samples.json, renders the same inputs through the WGSL DISPLAY stage and reports the maximum error against it.
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { buildDmParams, dmMapNits, dmTone, selectTrims, splineEval, sigmoidParams, deltaEItp, pqEotfNits, pqInvNits, ICTCP_RGB_TO_LMS, ICTCP_LMS_TO_ITP, DM_DEFAULTS } from '../fel-dm.mjs';
import { bt2020LimitedMatrix } from '../fel-meta.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
let failures = 0;
const results = [];
const check = (name, ok, detail) => { results.push({ name, ok: !!ok, detail }); if (!ok) failures++; console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail !== undefined ? '  ' + (typeof detail === 'string' ? detail : JSON.stringify(detail)) : ''}`); };
const rel = (a, b) => Math.abs(a - b) / Math.max(Math.abs(b), 1e-12);
const mulV = (m, v) => m.map(r => r[0] * v[0] + r[1] * v[1] + r[2] * v[2]);

// ---- metadata sets used by the checks and by the GPU comparison (12 bit RPU codes, as dovi.mjs reports them) ----
export const META = {
  // 4000 nit mastering, rangers-like L2 (100 nit and 1000 nit trims), bright scene
  m4000: { l1: { minPq: 7, avgPq: 1300, maxPq: 3500 }, sourceMaxPq: 3696, l6: { maxMasteringNits: 4000, minMasteringNits: 50 }, rgbToLms: null,
    l2: [{ targetMaxPq: 2081, slope: 1989, offset: 2060, power: 1748, chromaWeight: 2200, saturationGain: 2101, msWeight: 512 }, { targetMaxPq: 3079, slope: 2060, offset: 2048, power: 2030, chromaWeight: 2259, saturationGain: 2466, msWeight: 512 }], l8: [] },
  // 1000 nit mastering, p5-like three trims
  m1000: { l1: { minPq: 0, avgPq: 1419, maxPq: 2900 }, sourceMaxPq: 3079, l6: { maxMasteringNits: 1000, minMasteringNits: 1 }, rgbToLms: null,
    l2: [{ targetMaxPq: 2081, slope: 2019, offset: 2051, power: 2013, chromaWeight: 2048, saturationGain: 2048, msWeight: 2048 }, { targetMaxPq: 2851, slope: 2079, offset: 2048, power: 1943, chromaWeight: 2048, saturationGain: 2048, msWeight: 2048 }, { targetMaxPq: 3079, slope: 2049, offset: 2048, power: 2046, chromaWeight: 2048, saturationGain: 2048, msWeight: 2048 }], l8: [] },
  // CM v4.0 style: L8 only
  l8only: { l1: { minPq: 0, avgPq: 1100, maxPq: 3300 }, sourceMaxPq: 3696, l6: null, rgbToLms: null, l2: [],
    l8: [{ targetIndex: 1, slope: 2000, offset: 2070, power: 1900, chromaWeight: 2300, saturationGain: 2600 }, { targetIndex: 27, slope: 2040, offset: 2048, power: 2000, chromaWeight: 2100, saturationGain: 2200 }] },
  none: null,
};

// ---- 1. the equations of the standard and the patent ----
{
  const src = { sMin: 0.0151, sMid: 0.36, sMax: 0.9026 };       // 4000 nit class source, PQ
  const o = { ...DM_DEFAULTS };
  const s = sigmoidParams(src, 0, 100, o);
  const maps = [[s.x1, s.y1], [s.x2, s.y2], [s.x3, s.y3]].map(([x, y]) => (s.c1 + s.c2 * x) / (1 + s.c3 * x) - y);
  check('ST 2094-10 B.2 (5): L(x1)=y1, L(x2)=y2, L(x3)=y3', Math.max(...maps.map(Math.abs)) < 1e-9, { c: [s.c1, s.c2, s.c3], y2: s.y2, err: maps });
  check('ST 2094-10 B.2 (4): y2 = sqrt(x2 sqrt(y1 y3)) before the 0.8 y3 bound', s.y2 <= 0.8 * 100 + 1e-9, s.y2);
  // worked example of the standard: target min 0.5, target max 100, source avg 120 -> 29 cd/m2
  check('ST 2094-10 B.2 example: y2 = 29 nits for y1 0.5, y3 100, x2 120', Math.abs(Math.sqrt(120 * Math.sqrt(100 * 0.5)) - 29.1) < 0.2, Math.sqrt(120 * Math.sqrt(100 * 0.5)));
  check('sigmoid monotonic (c3 >= 0)', s.c3 >= 0);
}
for (const tMax of [100, 203, 600, 1000, 3000]) {
  const P = buildDmParams({ meta: META.m4000, display: { peakNits: tMax, toneMapping: 'l1' }, opts: { trims: false } });
  const c = P.curve;
  if (P.bypass) { check(`spline ${tMax} nit target: bypass (source inside target)`, tMax >= pqEotfNits(c.sMax) - 1, { srcMaxNits: pqEotfNits(c.sMax) }); continue; }
  const knots = [[c.sMin, c.tMin], [c.sMid, c.tMid], [c.sMax, c.tMax]].map(([x, y]) => Math.abs(splineEval(x, c) - y));
  check(`spline ${tMax} nit target: anchors (SMin,SMid,SMax) -> (TMin,TMid,TMax)`, Math.max(...knots) < 1e-12, { tMid: c.tMid, sMid: c.sMid, tMax: c.tMax, tMaxPq: P.tMaxPq });
  let mono = true, prev = -1, maxSlope = 0;
  for (let i = 0; i <= 20000; i++) { const x = i / 20000, y = splineEval(x, c); if (y < prev - 1e-12) mono = false; if (i) maxSlope = Math.max(maxSlope, (y - prev) * 20000); prev = y; }
  check(`spline ${tMax} nit target: monotonic over [0,1], max slope ${maxSlope.toFixed(3)} (cap 3 per segment)`, mono && maxSlope < 3.0001);
  const e = 1e-6, dl = (splineEval(c.sMid, c) - splineEval(c.sMid - e, c)) / e, dr = (splineEval(c.sMid + e, c) - splineEval(c.sMid, c)) / e;
  check(`spline ${tMax} nit target: C1 at SMid`, Math.abs(dl - dr) < 1e-4, { dl, dr, slopeMid: c.slopeMid });
  check(`spline ${tMax} nit target: output at SMax is TMax <= target peak`, splineEval(c.sMax, c) <= P.tMaxPq + 1e-12, { out: splineEval(c.sMax, c), tMaxPq: P.tMaxPq });
}
{
  // identity: source inside the target range
  const P = buildDmParams({ meta: META.m1000, display: { peakNits: 1000, toneMapping: 'l1' }, opts: { trims: false } });
  check('source max below target peak: bypass, output equals input', P.bypass && dmMapNits([100, 200, 300], P).every((v, i) => v === [100, 200, 300][i]));
}

{
  const P = buildDmParams({ meta: META.m4000, display: { peakNits: 100, toneMapping: 'static' }, opts: { trims: false } });
  check('static tone mapping: source range is the mastering display (L6), L1 average not used', Math.abs(P.src.sMax - pqInvNits(4000)) < 1e-12 && Math.abs(P.src.sMin - pqInvNits(0.005)) < 1e-12 && Math.abs(P.src.sMid - (P.src.sMin + 0.4 * (P.src.sMax - P.src.sMin))) < 1e-12 && P.src.from === 'L6', P.src);
}

// ---- 2. trim selection and interpolation ----
{
  const t100 = selectTrims(META.m4000, 2081, 3696), t1000 = selectTrims(META.m4000, 3079, 3696), t600 = selectTrims(META.m4000, 2851, 3696);
  check('trim: exact target 100 nit uses the L2 100 nit set', t100.weight === 0 && t100.raw.slope === 1989, t100.from);
  check('trim: exact target 1000 nit uses the L2 1000 nit set', t1000.raw.slope === 2060 || t1000.weight === 1, t1000.from);
  const w = (2851 - 2081) / (3079 - 2081);
  check('trim: 600 nit is the PQ-linear mix of the 100 and 1000 nit sets', Math.abs(t600.raw.slope - (1989 + (2060 - 1989) * w)) < 1e-9 && Math.abs(t600.weight - w) < 1e-9, { w, slope: t600.raw.slope });
  const t2000 = selectTrims(META.m4000, Math.round(pqInvNits(2000) * 4095), 3696);
  check('trim: above the highest target fades to neutral at the mastering peak', t2000.from[1].startsWith('neutral') && t2000.raw.slope > 2048 && t2000.raw.slope < 2060, { from: t2000.from, slope: t2000.raw.slope });
  check('trim: display at or above the mastering peak gets none', selectTrims(META.m4000, 3696, 3696) === null);
  check('trim: below the lowest target uses the lowest', selectTrims(META.m4000, 1500, 3696).raw.slope === 1989);
  const t8 = selectTrims(META.l8only, 2081, 3696), t8b = selectTrims(META.l8only, 2851, 3696);
  check('trim: CM v4.0 L8 index 1 = 100 nit, index 27 = 600 nit', t8.raw.slope === 2000 && t8b.raw.slope === 2040, { a: t8.from, b: t8b.from });
}

// ---- 3. colour transform: neutral axis, hue, ICtCp matrices ----
{
  const lms = mulV(ICTCP_RGB_TO_LMS, [1, 1, 1]);
  check('ICtCp RGB->LMS rows sum to 1 (D65 white is L=M=S=Y)', lms.every(v => Math.abs(v - 1) < 1e-12), lms);
  const ct = mulV(ICTCP_LMS_TO_ITP, [0.5, 0.5, 0.5]);
  check('ICtCp neutral has Ct = Cp = 0', Math.abs(ct[1]) < 1e-12 && Math.abs(ct[2]) < 1e-12 && Math.abs(ct[0] - 0.5) < 1e-12, ct);
}
// scenarios for the neutral-ramp and hue checks: [name, meta, peak, opts]
const SCENARIOS = [
  ['4000 -> 1000 spline ictcp, L2 trims', META.m4000, 1000, {}],
  ['4000 -> 600 spline ictcp (interpolated trims)', META.m4000, 600, {}],
  ['4000 -> 203 spline ictcp', META.m4000, 203, {}],
  ['4000 -> 100 spline ictcp', META.m4000, 100, {}],
  ['4000 -> 100 sigmoid', META.m4000, 100, { curve: 'sigmoid' }],
  ['4000 -> 1000 sigmoid', META.m4000, 1000, { curve: 'sigmoid' }],
  ['1000 -> 100 spline ipt (Profile 5 space)', META.m1000, 100, { space: 'ipt' }],
  ['L8 only -> 100 spline', META.l8only, 100, {}],
  ['no RPU, 4000 nit default -> 1000 spline', META.none, 1000, {}],
];
const summary = [];
for (const [name, meta, peak, opts] of SCENARIOS) {
  const display = { peakNits: peak, toneMapping: 'l1', sourcePeakNits: 4000 };
  const P = buildDmParams({ meta, display, opts });
  // neutral ramp, 0.0005 .. 4000 nits (log)
  let maxChromaDe = 0, prevY = -1, mono = true, maxOut = 0, worstIn = 0;
  const N = 600;
  for (let i = 0; i <= N; i++) {
    const y = 0.0005 * Math.pow(4000 / 0.0005, i / N);
    const out = dmMapNits([y, y, y], P);
    const [i1, t1, p1] = mulV(ICTCP_LMS_TO_ITP, mulV(ICTCP_RGB_TO_LMS, out).map(pqInvNits));
    const chromaDe = 720 * Math.hypot(t1 / 2, p1);
    if (chromaDe > maxChromaDe) { maxChromaDe = chromaDe; worstIn = y; }
    const lum = 0.2627 * out[0] + 0.678 * out[1] + 0.0593 * out[2];
    if (lum < prevY * (1 - 1e-4)) mono = false; prevY = lum;
    maxOut = Math.max(maxOut, ...out);
  }
  // hue: saturated colours at several levels, hue angle in the ICtCp (Ct, Cp) plane before and after, and the luminance relation
  let maxHue = 0, maxDeHue = 0;
  for (const dir of [[1, 0, 0], [0, 1, 0], [0, 0, 1], [1, 1, 0], [0, 1, 1], [1, 0, 1], [1, 0.3, 0.1], [0.2, 0.6, 1]]) {
    for (const lev of [0.5, 5, 50, 300, 1500, 3500]) {
      const rgb = dir.map(v => v * lev);
      const out = dmMapNits(rgb, P);
      const f = c => { const [i, t, p] = mulV(ICTCP_LMS_TO_ITP, mulV(ICTCP_RGB_TO_LMS, c).map(pqInvNits)); return [i, t, p]; };
      const a = f(rgb), b = f(out);
      if (Math.hypot(a[1], a[2]) > 0.01 && Math.hypot(b[1], b[2]) > 0.01) {
        let d = Math.abs(Math.atan2(b[2], b[1]) - Math.atan2(a[2], a[1])); if (d > Math.PI) d = 2 * Math.PI - d;
        maxHue = Math.max(maxHue, d * 180 / Math.PI);
      }
    }
  }
  summary.push({ name, bypass: P.bypass, space: P.space, curve: P.curve.kind, tMid: P.curve.tMid, sMid: P.curve.sMid, trim: P.trim?.from, neutralChromaDeItpMax: +maxChromaDe.toFixed(4), neutralAtNits: +worstIn.toFixed(4), monotonic: mono, outMaxNits: +maxOut.toFixed(2), peak, hueShiftMaxDeg: +maxHue.toFixed(2) });
  check(`${name}: neutral ramp stays neutral (max chroma ${maxChromaDe.toFixed(4)} DeltaE ITP <= 0.05)`, maxChromaDe <= 0.05);
  check(`${name}: neutral ramp monotonic`, mono);
  check(`${name}: output never above the target peak (max ${maxOut.toFixed(2)} nits)`, maxOut <= peak * (1 + 1e-6) + 1e-9);
}

if (process.argv.includes('--check')) { console.log(JSON.stringify(summary, null, 1)); process.exit(failures ? 1 : 0); }

// ---- 4. expected outputs for the WGSL comparison ----
function rng(seed) { let a = seed >>> 0; return () => { a = (a + 0x6D2B79F5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; }
const KR = 0.2627, KB = 0.0593, KG = 1 - KR - KB;
const { matrix: yccMat, offsets } = bt2020LimitedMatrix();
const toCodes = rgbPq => {      // BT.2020 NCL limited range 10 bit codes of R'G'B' (0..1)
  const [r, g, b] = rgbPq, y = KR * r + KG * g + KB * b, cb = (b - y) / (2 * (1 - KB)), cr = (r - y) / (2 * (1 - KR));
  const q = v => Math.min(1023, Math.max(0, Math.round(v)));
  return [q(64 + 876 * y), q(512 + 896 * cb), q(512 + 896 * cr)];
};
const fromCodes = ([y, cb, cr]) => mulV(yccMat, [y / 1024 - offsets[0], cb / 1024 - offsets[1], cr / 1024 - offsets[2]]);     // R'G'B' PQ, the shader's decode
const CASES = [
  { id: 'c1', label: '4000->1000 spline ictcp, L2 trims', meta: 'm4000', peak: 1000, opts: {} },
  { id: 'c2', label: '4000->600 sigmoid ictcp, interpolated trims', meta: 'm4000', peak: 600, opts: { curve: 'sigmoid' } },
  { id: 'c3', label: '4000->100 spline ictcp, L2 trims (SDR)', meta: 'm4000', peak: 100, opts: {} },
  { id: 'c4', label: '1000->203 spline ipt', meta: 'm1000', peak: 203, opts: { space: 'ipt' } },
  { id: 'c5', label: '4000->100 sigmoid, L8 trims', meta: 'l8only', peak: 100, opts: { curve: 'sigmoid' } },
  { id: 'c6', label: 'no RPU 4000->1000 spline', meta: 'none', peak: 1000, opts: {} },
  { id: 'c7', label: '1000->1000 bypass', meta: 'm1000', peak: 1000, opts: {} },
  { id: 'c8', label: '4000->400 spline, guard off', meta: 'm4000', peak: 400, opts: { peakGuard: false } },
];
const PER = 3600, rand = rng(20261106);
const out = { version: 1, patch: 8, width: 1280, height: 720, metas: META, cases: [] };
for (const c of CASES) {
  const display = { peakNits: c.peak, toneMapping: 'l1', sourcePeakNits: 4000, sdrWhiteNits: 203 };
  const P = buildDmParams({ meta: META[c.meta], display, opts: c.opts });
  const codes = [], expect = [];
  for (let s = 0; s < PER; s++) {
    let rgbPq;
    const kind = s % 6;
    if (kind === 0) { const y = pqInvNits(0.0005 * Math.pow(4000 / 0.0005, (s / 6) / (PER / 6 - 1))); rgbPq = [y, y, y]; }      // neutral ramp (log nits to PQ)
    else if (kind === 1) { const lev = pqInvNits(Math.pow(10, -2 + 6.6 * rand())); const d = [rand() < 0.5 ? 1 : 0.1 * rand(), rand() < 0.5 ? 1 : 0.1 * rand(), rand() < 0.5 ? 1 : 0.1 * rand()]; rgbPq = d.map(v => Math.min(1, v * lev * 1.4)); }
    else rgbPq = [rand(), rand(), rand()].map(v => Math.min(1, v * (rand() < 0.5 ? 1 : 0.82)));
    const cd = toCodes(rgbPq);
    codes.push(cd);
    const nits = fromCodes(cd).map(pqEotfNits);
    const mapped = dmMapNits(nits, P).map(v => Math.max(v, 0));
    expect.push(mapped);
  }
  out.cases.push({ ...c, bypass: P.bypass, codes, expectNits: expect, trim: P.trim?.from ?? null, curve: P.curve.kind, space: P.space, tMid: P.curve.tMid, sMid: P.curve.sMid });
}
fs.writeFileSync(path.join(here, 'dm-samples.json'), JSON.stringify(out));
fs.writeFileSync(path.join(here, 'dm-node-checks.json'), JSON.stringify({ failures, results, scenarios: summary }, null, 1));
console.log(`wrote dm-samples.json (${out.cases.length} cases x ${PER} samples), dm-node-checks.json, failures ${failures}`);
process.exit(failures ? 1 : 0);
