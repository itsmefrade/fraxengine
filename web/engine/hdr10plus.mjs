// SPDX-License-Identifier: LGPL-3.0-only
// HDR10+ (SMPTE ST 2094-40) dynamic metadata for the Chrome player.
//
//   await initHdr10Plus();                                   // once (or after openDemuxer / loadModule)
//   const params = extractHdr10Plus(packet, track);          // object or null
//   const block  = hdr10plusGpuParams(params, displayPeakNits); // Float32Array(36)
//
// The parser and the tone curve live in Rust (`aether-dovi/src/hdr10plus.rs`); this file only moves bytes.
// Integer fields keep the raw bitstream values, so `maxscl[0] / 100000` is a fraction of 10000 nits.

import { AetherWasm, loadModule, moduleSync } from './aether-wasm.mjs';

export const HDR10PLUS_GPU_PARAM_LENGTH = 36;

let shared = null;
function wasm() {
  if (!shared) shared = new AetherWasm(moduleSync());
  return shared;
}

/** Await once before the sync calls when no demuxer has been opened yet. */
export async function initHdr10Plus(url) {
  await loadModule(url);
  wasm();
}

const HEX = /^[0-9a-f]*$/;
function fromHex(s) {
  if (typeof s !== 'string' || s.length % 2 || !HEX.test(s)) return new Uint8Array(0);
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.substr(i * 2, 2), 16);
  return out;
}

/** hvcC / HEVC length size: lengthSizeMinusOne is the low 2 bits of byte 21. */
function hevcFraming(track) {
  if (track?.annexB) return 0;
  const d = track?.description;
  if (d instanceof Uint8Array && d.length > 21) return (d[21] & 3) + 1;
  return 4;
}

/** Wire format code for the codec of `track`, or -1 when HDR10+ cannot be carried (H.264, VP9, ...). */
function formatFor(track) {
  const c = String(track?.codec ?? '').toLowerCase();
  if (/^(hvc1|hev1|dvh1|dvhe)/.test(c)) return hevcFraming(track);
  if (/^av01/.test(c)) return 16;
  return -1;
}

function shape(j) {
  const grid = g => g && { rows: g.rows, cols: g.cols, values: g.values };
  return {
    applicationVersion: j.v,
    numWindows: j.windows,
    /** nits */
    targetedSystemDisplayMaximumLuminance: j.target,
    /** 4 bit grids, denominator 15 */
    targetedSystemDisplayActualPeakLuminance: grid(j.targetPeak),
    masteringDisplayActualPeakLuminance: grid(j.masterPeak),
    /** Windows 1.. (window 0 is the full frame). */
    windowGeometry: j.geometry.map(g => ({
      upperLeftX: g[0], upperLeftY: g[1], lowerRightX: g[2], lowerRightY: g[3],
      centerOfEllipseX: g[4], centerOfEllipseY: g[5], rotationAngle: g[6],
      semimajorAxisInternalEllipse: g[7], semimajorAxisExternalEllipse: g[8], semiminorAxisExternalEllipse: g[9],
      overlapProcessOption: g[10],
    })),
    windows: j.params.map(p => ({
      /** denominator 100000 */
      maxscl: p.maxscl,
      /** denominator 100000 */
      averageMaxrgb: p.avg,
      /** [{percentage, percentile (denominator 100000)}] */
      distributionMaxrgb: p.pct.map(([percentage, percentile]) => ({ percentage, percentile })),
      /** denominator 1000 */
      fractionBrightPixels: p.fbp,
      /** denominators 4095 and 1023, or null without a tone mapping curve */
      kneePoint: p.knee && { x: p.knee[0], y: p.knee[1] },
      bezierCurveAnchors: p.anchors,
      /** denominator 8, or null */
      colorSaturationWeight: p.sat,
    })),
    /** The T.35 payload (from the 0xB5 country code), the input of `hdr10plusGpuParams`. */
    raw: fromHex(j.raw),
  };
}

/**
 * HDR10+ metadata of one video packet, or null. HEVC (length prefixed per the track's hvcC, or Annex B
 * when `track.annexB`) is read from prefix/suffix SEI; AV1 from metadata OBUs. Never throws on bad data.
 */
export function extractHdr10Plus(packet, track) {
  const fmt = formatFor(track);
  if (fmt < 0) return null;
  const data = packet?.data ?? packet;
  if (!(data instanceof Uint8Array) || data.length === 0) return null;
  const w = wasm();
  const status = w.withBytes(data, (p, l) => w.x.aether_hdr10plus_parse(p, l, fmt));
  if (status !== 1) return null;
  try { return shape(JSON.parse(w.resultText())); } catch { return null; }
}

/**
 * Float32Array(36) for the shader (see `HDR10PLUS_WGSL`): the layout is documented at
 * `ToneParams::to_gpu` in Rust and in the WGSL header. `displayPeakNits` is the canvas peak.
 * Returns null when `params` is null or invalid.
 */
export function hdr10plusGpuParams(params, displayPeakNits) {
  if (!params?.raw?.length) return null;
  const w = wasm();
  const n = w.withBytes(params.raw, (p, l) => w.x.aether_hdr10plus_gpu_params(p, l, Math.fround(displayPeakNits)));
  if (n !== HDR10PLUS_GPU_PARAM_LENGTH) return null;
  const r = w.result();
  return new Float32Array(r.buffer, r.byteOffset, HDR10PLUS_GPU_PARAM_LENGTH).slice();
}

/** The Rust reference curve for one PQ pixel (tests and CPU fallbacks). */
export function hdr10plusApply(params, displayPeakNits, rgb) {
  const w = wasm();
  const n = w.withBytes(params.raw, (p, l) => w.x.aether_hdr10plus_apply(p, l, Math.fround(displayPeakNits), rgb[0], rgb[1], rgb[2]));
  if (n !== 3) return null;
  const r = w.result();
  return Array.from(new Float32Array(r.buffer, r.byteOffset, 3));
}

// Canvas EDR scale for the curve's absolute nits. A display-p3 extended canvas has no absolute scale of its own (1.0 = SDR white), so the
// nits need the anchor the screen uses for PQ. Measured (linearised capture of Chrome <video> on six HDR10+ frames, H 6.11): nits / EDR is
// 155.7, 155.4, 157.1, 153.8, 153.3, 147.0, 147.3, 154.1 from 10 to 390 nits (below 10 the capture is too coarse, above 390 macOS compresses),
// which is 150 within 5 %. BT.2408's 203 would make the same nits 1.35x darker than any other HDR video on this screen.
export const HDR10PLUS_CANVAS_ANCHOR_NITS = 150;
// The only headroom we could measure (NSScreen maximumExtendedDynamicRangeColorComponentValue); a page cannot read it (no
// screen.highDynamicRangeHeadroom in Chrome 157, 'rec2100-pq' WebGPU canvas rejected). It only sets the roll-off ceiling.
export const HDR10PLUS_ASSUMED_HEADROOM = 6.11;

/**
 * The uniform block for the display, in one place so a corrected rule drops in here (and the curve itself in `hdr10plus_curve_nits`).
 *
 * Which display peak: the default adapts the curve to the panel peak D (headroom x anchor, 917 nits here), libplacebo's interpolation of
 * knee and anchors, chosen by the user after comparing it on screen with 'real'. The spec is silent on a display above the target (B.4.1).
 * The adaptation moves the curve towards identity and is identity for every scene whose maximum is under D (three of our six test frames,
 * 652 to 665 nits). `mode 'real'` asks for the display the metadata defines, `targeted_system_display_maximum_luminance` (T):
 * `hdr10plusGpuParams(params, T)`, fade 0, out_scale 1, a curve on every scene. Measured against AVPlayer (same frames, on screen): mean
 * luminance 0.75 to 0.93 with the default, 0.57 to 0.66 with 'real', chromaticity equal in both. T above the panel
 * (ceiling = headroom x anchor) goes through the roll-off of `hdr10plus_fit_ceiling`, which is inert otherwise.
 *
 * SDR displays (`peakNits` <= 203) keep the plain path: curve scaled to that peak, nits over SDR white.
 *
 * `mode`: unset or 'panel' adaptation to the panel; 'real' the target display as above; 'curve' plain `hdr10plus_map` at `peakNits` (the Rust reference
 * comparison, hdr10plus-test.mjs); 'macos' and 'macos-flat' the measured macOS table with and without the scene lift (measurement only).
 */
export function hdr10plusDisplayBlock(params, peakNits, mode) {
  if (mode === 'macos' || mode === 'macos-flat') return macosTableBlock(params, peakNits, mode === 'macos-flat');
  if (mode === 'curve' || !(peakNits > 203)) return hdr10plusGpuParams(params, peakNits);
  const ceiling = HDR10PLUS_ASSUMED_HEADROOM * HDR10PLUS_CANVAS_ANCHOR_NITS;
  const target = params.targetedSystemDisplayMaximumLuminance;
  const panel = mode !== 'real';
  const block = hdr10plusGpuParams(params, panel ? ceiling : target);
  if (!block) return block;
  if (panel) adaptToBrighterDisplay(block, params, ceiling, target);
  block[34] = target > ceiling && !panel ? ceiling : 0;
  block[35] = HDR10PLUS_CANVAS_ANCHOR_NITS;
  return block;
}

// libplacebo's st2094_40 for a display D above the target T (the spec is silent there, B.4.1): the knee and anchors move towards the
// linear curve with weight w, and the output is y * D. A scene already under D stays as hdr10plusGpuParams left it, the pass-through.
function adaptToBrighterDisplay(block, params, D, T) {
  const S = block[2], w0 = params.windows[0];
  if (!(block[1] > 0.5) || !(D > T) || !(S > D) || !w0?.kneePoint) return;
  const w = Math.pow(1 - (D - T) / (S - T), 1.4);
  const Kx = w0.kneePoint.x / 4095;
  const N = w0.bezierCurveAnchors.length + 1;
  const P = [0, ...w0.bezierCurveAnchors.map(a => a / 1023), 1];
  const mix = (a, b, t) => a + (b - a) * t;
  const Ky = mix(Kx * D / S, (w0.kneePoint.y / 4095) * (T / D), w);
  const intercept = (Kx <= 0 || Ky >= 1) ? 1 / N : Math.min((Ky / Kx) * (1 - Kx) / (1 - Ky) / N, 1);
  for (let p = 2; p < N; p++) P[p] = mix(p / N, P[p], w);
  P[1] = mix(intercept, P[1], w);
  block[4] = D; block[5] = Kx; block[6] = Ky; block[9] = 0; block[10] = 1;
  P.forEach((v, i) => { block[16 + i] = Ky + (1 - Ky) * v; });
}

function macosTableBlock(params, peakNits, flat) {
  const block = hdr10plusGpuParams(params, peakNits);
  if (!block) return block;
  block[33] = 1;
  // Size of hdr10plus_scene_gain from the HDR10+ average maxRGB in nits (block[3]): 0.17 at or below 15 nits, 0.03 at or above 90 nits.
  // AP / AP-without-HDR10+ peak gain on six frames: 0.137 to 0.218 at 2.4 to 12.5 nits, 0.03 at 89 and 168.
  const t = Math.min(1, Math.max(0, (Math.log10(Math.max(block[3], 1)) - Math.log10(15)) / (Math.log10(90) - Math.log10(15))));
  block[34] = flat ? 0 : 0.17 + (0.03 - 0.17) * t;
  return block;
}

/**
 * WGSL for FelCompositor. Add it to the shader source and declare the uniform:
 *
 *   @group(G) @binding(B) var<uniform> h10p: Hdr10PlusParams;   // 144 bytes, upload hdr10plusGpuParams()
 *
 * then, on PQ encoded BT.2020 `rgb`, after reconstruction and before (instead of) the BT.2390 stage:
 *
 *   if (h10p_f(h10p, 0u) > 0.5 && h10p_f(h10p, 1u) > 0.5) { rgb = hdr10plus_map(rgb, h10p); }
 *
 * Index 0 is the valid flag and 1 the has-curve flag; when only 0 is set the frame has HDR10+ statistics but no
 * curve, and index 2 (scene max nits) can drive the existing BT.2390 source peak. Uniform layout (f32 index):
 *   0 valid, 1 has_curve, 2 scene_max_nits, 3 scene_avg_nits, 4 target_nits, 5 knee_x, 6 knee_y,
 *   7 num_anchors (0..15), 8 display_peak_nits, 9 fade (0 curve .. 1 pass-through), 10 out_scale,
 *   11..13 maxscl R,G,B nits, 16..32 control values [knee_y, anchors.., 1], 33 to 35 written by `hdr10plusDisplayBlock`:
 *   33 1 = measured macOS curve hdr10plus_system_edr (measurement only), 34 scene lift amount with 33, otherwise the output ceiling in
 *   nits, 35 canvas anchor in nits per 1.0 EDR (0 = the plain `hdr10plus_map` path with the compositor's own SDR white).
 * Curve: x = min(max(r,g,b) / scene_max, 1) in linear nits; x < knee_x: y = x knee_y / knee_x; else y = Bezier(t)
 * over the control values with t = (x - knee_x) / (1 - knee_x). Output nits = y * target * out_scale, blended to
 * min(maxrgb, display_peak) by `fade`; all channels are scaled by output / maxrgb (hue preserving).
 */
export const HDR10PLUS_WGSL = /* wgsl */ `
struct Hdr10PlusParams { v: array<vec4<f32>, 9>, };

fn h10p_f(p: Hdr10PlusParams, i: u32) -> f32 { return p.v[i >> 2u][i & 3u]; }

fn h10p_pq_to_nits(v: vec3<f32>) -> vec3<f32> {
  let p = pow(clamp(v, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 78.84375));
  let num = max(p - vec3<f32>(0.8359375), vec3<f32>(0.0));
  let den = vec3<f32>(18.8515625) - 18.6875 * p;
  return 10000.0 * pow(num / den, vec3<f32>(1.0 / 0.1593017578125));
}

fn h10p_nits_to_pq(n: vec3<f32>) -> vec3<f32> {
  let y = pow(clamp(n / 10000.0, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(0.1593017578125));
  return pow((vec3<f32>(0.8359375) + 18.8515625 * y) / (vec3<f32>(1.0) + 18.6875 * y), vec3<f32>(78.84375));
}

// Knee segment plus Bezier (de Casteljau) over the control values [knee_y, knee_y + (1 - knee_y) anchors.., 1]; x and result are 0..1.
fn h10p_curve(p: Hdr10PlusParams, x_in: f32) -> f32 {
  let x = clamp(x_in, 0.0, 1.0);
  let kx = h10p_f(p, 5u);
  let ky = h10p_f(p, 6u);
  if (x < kx || kx >= 1.0) { return x * ky / kx; }
  let t = clamp((x - kx) / (1.0 - kx), 0.0, 1.0);
  var c: array<f32, 17>;
  for (var i = 0u; i < 17u; i = i + 1u) { c[i] = h10p_f(p, 16u + i); }
  let m = u32(h10p_f(p, 7u)) + 2u;
  for (var level = 1u; level < m; level = level + 1u) {
    for (var i = 0u; i + level < m; i = i + 1u) { c[i] = c[i] + (c[i + 1u] - c[i]) * t; }
  }
  return clamp(c[0], 0.0, 1.0);
}

fn hdr10plus_map(pq: vec3<f32>, p: Hdr10PlusParams) -> vec3<f32> {
  let lin = h10p_pq_to_nits(pq);
  let m = max(lin.r, max(lin.g, lin.b));
  if (!(m > 0.0)) { return pq; }
  let x = min(m / h10p_f(p, 2u), 1.0);
  let mapped = h10p_curve(p, x) * h10p_f(p, 4u) * h10p_f(p, 10u);
  let pass_through = min(m, h10p_f(p, 8u));
  let out_nits = mix(mapped, pass_through, h10p_f(p, 9u));
  return h10p_nits_to_pq(lin * (out_nits / m));
}

// The ST 2094-40 output of a pixel in nits: the only function to change for a corrected curve. maxRGB in nits in, nits out.
fn hdr10plus_curve_nits(m: f32, p: Hdr10PlusParams) -> f32 {
  let mapped = h10p_curve(p, min(m / h10p_f(p, 2u), 1.0)) * h10p_f(p, 4u) * h10p_f(p, 10u);
  return mix(mapped, min(m, h10p_f(p, 8u)), h10p_f(p, 9u));
}

// Only for a target above the panel (c = headroom x anchor, 0 = off): identity up to 0.75 c, then exponential to c. Matches value and slope at the knee.
fn hdr10plus_fit_ceiling(n: f32, c: f32) -> f32 {
  let k = 0.75 * c;
  if (c <= 0.0 || n <= k) { return n; }
  return k + (c - k) * (1.0 - exp(-(n - k) / (c - k)));
}

// Measurement only (?h10pmap=macos): macOS maps HDR video through one tone curve whatever the page does; this is that curve, read off the screen (ScreenCaptureKit, HDR
// capture linearised against a calibrated WebGPU canvas, H 6.11 from NSScreen maximumExtendedDynamicRangeColorComponentValue) on six
// frozen HDR10+ frames in Chrome <video> and in AVPlayer with the HDR10+ SEI stripped, which agree to 1 to 4 %. Knots: PQ code / 20,
// values are canvas EDR (1.0 = SDR white). A page cannot read the headroom (no screen.highDynamicRangeHeadroom in Chrome 157, and
// 'rec2100-pq' is not a valid WebGPU canvas color space: TypeError), so this table is the mapping at H 6.11, the one headroom we could
// measure; where the real headroom is lower the compositor clips above it.
const H10P_SYS_EDR = array<f32, 21>(0.0, 0.0031, 0.0058, 0.012, 0.0215, 0.0385, 0.068, 0.117, 0.204, 0.357, 0.598, 1.018, 1.639, 2.521, 3.6, 5.12, 5.6, 5.9, 6.1, 6.11, 6.11);

fn hdr10plus_system_edr(nits: f32) -> f32 {
  let p = pow(max(nits, 0.0) / 10000.0, 0.1593017578125);
  let x = pow((0.8359375 + 18.8515625 * p) / (1.0 + 18.6875 * p), 78.84375) * 20.0;
  let i = u32(clamp(floor(x), 0.0, 19.0));
  var g = H10P_SYS_EDR;
  if (i == 0u) { return g[1] * x; }
  let t = clamp(x - f32(i), 0.0, 1.0);
  return pow(2.0, mix(log2(g[i]), log2(g[i + 1u]), t));
}

// On top of that curve AVPlayer lifts dim scenes when HDR10+ is present (with the SEI stripped it does not): at a given source level
// AP / AP-without-SEI on six frames was 1.03 (avg maxRGB 89 and 168 nits) up to 1.14 to 1.22 (2.4 to 12.5 nits), largest near 48 nits, about
// 0.55 of that at 1.5 nits and 0.4 at 500 nits. The HDR10+ average picks the size (FelCompositor), this the shape over luminance.
fn hdr10plus_scene_gain(nits: f32, amount: f32) -> f32 {
  let oct = log2(max(nits, 0.01) / 48.0);
  return 1.0 + amount * clamp(1.0 - (select(0.09 * -oct, 0.2 * oct, oct > 0.0)), 0.0, 1.0);
}
`;
