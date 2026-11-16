// SPDX-License-Identifier: LGPL-3.0-only
// WGSL for the realtime FEL compositor. Three stages, see README.md:
//   1. RECOVER  hardware-decoded VideoFrame (relabelled SPS) -> 10-bit YCbCr codes in a packed u16 buffer
//   2. COMPOSE  exact Reference16 arithmetic (polynomial / MMR reshaping, NLQ residual) from those codes
//   3. DISPLAY  RPU colour decode, tone mapping, canvas encoding
// Stage 2 is a port of reference/fel-reference16.wgsl (integer only, i64 emulated with u32 pairs)
// that reads the table packet of rust/aether-dovi/src/fel/gpu.rs instead of the experiment's parameter block.

import { HDR10PLUS_WGSL } from '../engine/hdr10plus.mjs';

/** Stage 1. One thread owns a 4 x 2 luma block (8 external texture samples). */
export const RECOVER_WGSL = /* wgsl */ `
struct RP { w: u32, h: u32, full: u32, pad1: u32 }     // full: 1 when Chrome converted with the full range YCbCr matrix (SPS video_full_range_flag), see sps-range.mjs
@group(0) @binding(0) var<uniform> rp: RP;
@group(0) @binding(1) var tex: texture_external;
@group(0) @binding(2) var smp: sampler;
@group(0) @binding(3) var<storage, read_write> dst: array<u32>;
// Inverse of the BT.2020 NCL matrix (limited range, or full range when the SPS says so). The SPS was relabelled (primaries 1, transfer 13), so
// importExternalTexture(colorSpace 'srgb') applies the YCbCr -> RGB matrix and nothing else.
const KR = 0.2627; const KB = 0.0593; const KG = 0.6780;
fn ycc(x: u32, y: u32) -> vec3<f32> {
  let uv = vec2<f32>(f32(x) + 0.5, f32(y) + 0.5) / vec2<f32>(f32(rp.w), f32(rp.h));
  let rgb = textureSampleBaseClampToEdge(tex, smp, uv).rgb;
  let l = KR * rgb.r + KG * rgb.g + KB * rgb.b;
  if (rp.full == 1u) {
    return vec3<f32>(1023.0 * l, 512.0 + 1023.0 * (rgb.b - l) / (2.0 * (1.0 - KB)), 512.0 + 1023.0 * (rgb.r - l) / (2.0 * (1.0 - KR)));
  }
  return vec3<f32>(64.0 + 876.0 * l,
                   512.0 + 896.0 * (rgb.b - l) / (2.0 * (1.0 - KB)),
                   512.0 + 896.0 * (rgb.r - l) / (2.0 * (1.0 - KR)));
}
fn q(v: f32) -> u32 { return u32(clamp(round(v), 0.0, 1023.0)); }
@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  let x0 = id.x * 4u; let y0 = id.y * 2u;
  if (x0 >= rp.w || y0 >= rp.h) { return; }
  var yl: array<u32, 8>;
  var cb = vec2<f32>(0.0); var cr = vec2<f32>(0.0);
  for (var r = 0u; r < 2u; r++) {
    for (var i = 0u; i < 4u; i++) {
      let c = ycc(x0 + i, y0 + r);
      yl[r * 4u + i] = q(c.x);
      // 4:2:0 chroma sits at the even luma column (left siting); both rows of the 2 x 2 block carry the same
      // (or bilinearly interpolated) chroma, their mean is the recovered sample.
      if ((i & 1u) == 0u) {
        if (i == 0u) { cb.x += c.y * 0.5; cr.x += c.z * 0.5; } else { cb.y += c.y * 0.5; cr.y += c.z * 0.5; }
      }
    }
  }
  let lumaRow = rp.w / 2u;
  for (var r = 0u; r < 2u; r++) {
    let base = (y0 + r) * lumaRow + x0 / 2u;
    dst[base] = yl[r * 4u] | (yl[r * 4u + 1u] << 16u);
    dst[base + 1u] = yl[r * 4u + 2u] | (yl[r * 4u + 3u] << 16u);
  }
  let lumaWords = rp.w * rp.h / 2u;
  let chromaWords = rp.w * rp.h / 8u;
  let at = lumaWords + (y0 / 2u) * (rp.w / 4u) + id.x;
  dst[at] = q(cb.x) | (q(cb.y) << 16u);
  dst[at + chromaWords] = q(cr.x) | (q(cr.y) << 16u);
}`;

/** Stage 2. Table layout constants mirror rust/aether-dovi/src/fel/gpu.rs. */
export const COMPOSE_WGSL = /* wgsl */ `
// (w, h, cw, ch) per layer (bl.z/w unused); then per layer the code buffer layout in samples, so a picture is read from a padded
// and cropped buffer (a GPU DPB slot) exactly as from its cropped planes: bls / els (luma stride, chroma stride, Cb offset, Cr
// offset), blo / elo (luma x, y, chroma x, y of the top left displayed sample, the conformance window). Tight planes: stride = width,
// offsets back to back, origin 0.
struct CP { bl: vec4<u32>, blc: vec4<u32>, el: vec4<u32>, elc: vec4<u32>, bls: vec4<u32>, blo: vec4<u32>, els: vec4<u32>, elo: vec4<u32> }
@group(0) @binding(0) var<uniform> cp: CP;
@group(0) @binding(1) var<storage, read> base: array<u32>;
@group(0) @binding(2) var<storage, read> enhancement: array<u32>;
@group(0) @binding(3) var<storage, read> tables: array<i32>;
@group(0) @binding(4) var<storage, read_write> output: array<u32>;
const DESC = 64u; const COEF = 160u; const PRED = 688u; const NLQ = 3760u;
fn wide_mul(a: u32, b: u32) -> vec2<u32> {
  let a0 = a & 65535u; let a1 = a >> 16u;
  let b0 = b & 65535u; let b1 = b >> 16u;
  let w0 = a0 * b0;
  let t = a1 * b0 + (w0 >> 16u);
  let w1 = (t & 65535u) + a0 * b1;
  return vec2<u32>((w1 << 16u) | (w0 & 65535u), a1 * b1 + (t >> 16u) + (w1 >> 16u));
}
fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
  let lo = a.x + b.x;
  return vec2<u32>(lo, a.y + b.y + select(0u, 1u, lo < a.x));
}
fn negate64(a: vec2<u32>) -> vec2<u32> { return add64(~a, vec2<u32>(1u, 0u)); }
fn signed_product(coefficient: u32, term: u32) -> vec2<u32> {
  let negative = (coefficient & 0x80000000u) != 0u;
  let magnitude = select(coefficient, ~coefficient + 1u, negative);
  let product = wide_mul(magnitude, term);
  if (negative) { return negate64(product); }
  return product;
}
fn q20_product(a: u32, b: u32) -> u32 {
  let product = wide_mul(a, b);
  return (product.x >> 20u) | (product.y << 12u);
}
fn bl_at(component: u32, x: i32, y: i32) -> u32 {
  let w = select(cp.bl.x, cp.blc.x, component != 0u);
  let h = select(cp.bl.y, cp.blc.y, component != 0u);
  var offset = 0u; var stride = cp.bls.x; var o = cp.blo.xy;
  if (component > 0u) { offset = select(cp.bls.z, cp.bls.w, component == 2u); stride = cp.bls.y; o = cp.blo.zw; }
  let e = offset + (u32(clamp(y, 0, i32(h) - 1)) + o.y) * stride + u32(clamp(x, 0, i32(w) - 1)) + o.x;
  return (base[e >> 1u] >> ((e & 1u) * 16u)) & 65535u;
}
fn el_at(component: u32, x: i32, y: i32) -> i32 {
  let w = select(cp.el.x, cp.elc.x, component != 0u);
  let h = select(cp.el.y, cp.elc.y, component != 0u);
  var offset = 0u; var stride = cp.els.x; var o = cp.elo.xy;
  if (component > 0u) { offset = select(cp.els.z, cp.els.w, component == 2u); stride = cp.els.y; o = cp.elo.zw; }
  let e = offset + (u32(clamp(y, 0, i32(h) - 1)) + o.y) * stride + u32(clamp(x, 0, i32(w) - 1)) + o.x;
  return i32((enhancement[e >> 1u] >> ((e & 1u) * 16u)) & 65535u);
}
fn chroma_luma(x: u32, y: u32) -> u32 {
  let xx = i32(2u * x); let yy = i32(2u * y);
  let a = (bl_at(0u, xx - 1, yy) + 2u * bl_at(0u, xx, yy) + bl_at(0u, xx + 1, yy) + 2u) >> 2u;
  let b = (bl_at(0u, xx - 1, yy + 1) + 2u * bl_at(0u, xx, yy + 1) + bl_at(0u, xx + 1, yy + 1) + 2u) >> 2u;
  return (a + b + 1u) >> 1u;
}
fn vertical(component: u32, x: i32, y: u32) -> i32 {
  let n = i32(y / 2u);
  if ((y & 1u) == 0u) { return (64 * el_at(component, x, n - 1) + 192 * el_at(component, x, n) + 128) >> 8u; }
  return (192 * el_at(component, x, n) + 64 * el_at(component, x, n + 1) + 128) >> 8u;
}
fn enhancement_code(component: u32, x: u32, y: u32) -> u32 {
  if (cp.bl.x == cp.el.x) { return u32(el_at(component, i32(x), i32(y))); }
  if (component == 0u) {
    // Centre-aligned exact 2x bilinear has quarter weights, replicated borders.
    let xx = i32(x); let yy = i32(y);
    let ax = (xx - 1) >> 1u; let ay = (yy - 1) >> 1u;
    let fx = select(3, 1, (x & 1u) != 0u); let fy = select(3, 1, (y & 1u) != 0u);
    let top = el_at(0u, ax, ay) * (4 - fx) + el_at(0u, ax + 1, ay) * fx;
    let bot = el_at(0u, ax, ay + 1) * (4 - fx) + el_at(0u, ax + 1, ay + 1) * fx;
    return u32((top * (4 - fy) + bot * fy + 8) >> 4u);
  }
  let m = i32(x / 2u);
  if ((x & 1u) == 0u) { return u32(clamp(vertical(component, m, y), 0, 1023)); }
  let taps = array<i32, 8>(22, 94, -524, 2456, 2456, -524, 94, 22);
  var sum = 2048;
  for (var i = 0u; i < 8u; i++) { sum += taps[i] * vertical(component, m + i32(i) - 3, y); }
  return u32(clamp(sum >> 12u, 0, 1023));
}
fn prediction(component: u32, code: u32, inputs: vec3<u32>) -> u32 {
  let curve = DESC + component * 32u;
  let pieces = u32(tables[curve]) - 1u;
  var selected = 0u;
  for (var i = 0u; i < pieces; i++) { if (code >= u32(tables[curve + 2u + i])) { selected = i; } }
  if (tables[curve + 11u + selected] == 0) { return u32(tables[PRED + component * 1024u + code]); }
  var values: vec3<u32>;
  for (var i = 0u; i < 3u; i++) {
    let c = DESC + i * 32u;
    values[i] = clamp(inputs[i], u32(tables[c + 27u]), u32(tables[c + 28u]));
  }
  let y = values.x; let u = values.y; let v = values.z;
  let first = array<u32, 7>(y << 10u, u << 10u, v << 10u, y * u, y * v, u * v, q20_product(y * u, v << 10u));
  let second = array<u32, 7>(y * y, u * u, v * v, q20_product(first[3], first[3]), q20_product(first[4], first[4]), q20_product(first[5], first[5]), q20_product(first[6], first[6]));
  let coef = COEF + (component * 8u + selected) * 22u;
  let order = u32(tables[curve + 19u + selected]);
  var sum = signed_product(bitcast<u32>(tables[coef]), 1048576u);
  for (var row = 0u; row < order; row++) {
    for (var i = 0u; i < 7u; i++) {
      var term = first[i];
      if (row == 1u) { term = second[i]; }
      if (row == 2u) { term = q20_product(first[i], second[i]); }
      sum = add64(sum, signed_product(bitcast<u32>(tables[coef + 1u + row * 7u + i]), term));
    }
  }
  if ((sum.y & 0x80000000u) != 0u) { return 0u; }
  let shift = u32(tables[3]) + 4u;
  if ((sum.y >> shift) != 0u) { return 65535u; }
  return min((sum.x >> shift) | (sum.y << (32u - shift)), 65535u);
}
// The composed signal is the raw 16-bit value per sample, two samples per word (x even in the low half), planar Y, Cb, Cr: every
// plane width is even (the compositor requires a luma width that is a multiple of 4), so a pair never straddles a plane or a row.
// DISPLAY reads it as packed u16; readComposed() derives the vdr_bit_depth quantised value from it.
fn compose_raw(component: u32, x: u32, y: u32) -> u32 {
  var inputs = vec3<u32>(bl_at(0u, i32(x), i32(y)), bl_at(1u, i32(x / 2u), i32(y / 2u)), bl_at(2u, i32(x / 2u), i32(y / 2u)));
  if (component > 0u) { inputs = vec3<u32>(chroma_luma(x, y), bl_at(1u, i32(x), i32(y)), bl_at(2u, i32(x), i32(y))); }
  let pred = prediction(component, inputs[component], inputs);
  var residual = 0;
  if (tables[5] == 0) { residual = tables[NLQ + component * 1024u + enhancement_code(component, x, y)]; }
  return u32(clamp(i32(pred) + residual, 0, 65535));
}
@compute @workgroup_size(16, 16)
fn luma(@builtin(global_invocation_id) id: vec3<u32>) {
  let x = 2u * id.x;
  if (x >= cp.bl.x || id.y >= cp.bl.y) { return; }
  output[(id.y * cp.bl.x + x) >> 1u] = compose_raw(0u, x, id.y) | (compose_raw(0u, x + 1u, id.y) << 16u);
}
@compute @workgroup_size(16, 16)
fn chroma(@builtin(global_invocation_id) id: vec3<u32>) {
  let x = 2u * id.x;
  if (x >= cp.blc.x || id.y >= cp.blc.y) { return; }
  let index = cp.bl.x * cp.bl.y + id.z * cp.blc.x * cp.blc.y + id.y * cp.blc.x + x;
  output[index >> 1u] = compose_raw(1u + id.z, x, id.y) | (compose_raw(1u + id.z, x + 1u, id.y) << 16u);
}`;

// Stage 3, the part shared by DISPLAY_WGSL and DISPLAY_LEAN_WGSL (uniforms, transfer functions, tone mapping, output); `decode` comes from the source part.
const DISPLAY_CORE_WGSL = /* wgsl */ `
${HDR10PLUS_WGSL}
struct DP {
  m0: vec4<f32>, m1: vec4<f32>, m2: vec4<f32>,      // signal -> nonlinear rows
  off: vec4<f32>,                                   // signal offsets (xyz)
  l0: vec4<f32>, l1: vec4<f32>, l2: vec4<f32>,      // linear light (nits, source primaries) -> BT.2020 nits rows
  d0: vec4<f32>, d1: vec4<f32>, d2: vec4<f32>,      // BT.2020 -> display primaries rows
  dims: vec4<u32>,                                  // luma w, h, chroma w, h
  cfg: vec4<f32>,                                   // code scale, chroma phase x, y, source kind (0 one sample per u32, 1 packed u16: the codes and the composed signal)
  tm: vec4<f32>,                                    // source max PQ, source min PQ, target peak PQ, tone map on
  fin: vec4<f32>,                                   // SDR white nits, peak nits, output mode (0 sRGB-encoded, 1 PQ, 2 linear), reshape on
  trim: vec4<f32>,                                  // slope, offset, power, on
  vp: vec4<f32>,                                    // x: integer supersample factor (source pixels per canvas pixel per axis)
  act: vec4<f32>,                                   // Level 5 active area offsets: left, right, top, bottom (pixels), all zero = whole picture
  hlg: vec4<f32>,                                   // x: 1 = the signal is BT.2100 HLG (not PQ), y: system gamma, z: OOTF peak in nits (alpha)
  // ST 2094-10 style display management (fel-dm.mjs packDmUniforms), vec4 18 .. 36
  dmc: vec4<f32>,                                   // x: 1 = DM on (replaces BT.2390), y: curve (0 spline, 1 sigmoid), z: peak guard, w: target peak nits
  dmx: vec4<f32>,                                   // spline source anchors SMin, SMid, SMax (PQ), w: target min PQ
  dmy: vec4<f32>,                                   // spline target anchors TMin, TMid, TMax (PQ), w: target max PQ
  dms: vec4<f32>,                                   // spline slopes min, mid, max
  dmg: vec4<f32>,                                   // sigmoid c1, c2, c3 (nits domain)
  dmo: vec4<f32>,                                   // trim slope, offset, power (PQ domain, applied to the curve output), w: on
  dmk: vec4<f32>,                                   // saturation gain, chroma weight, chroma k1, k2
  dma0: vec4<f32>, dma1: vec4<f32>, dma2: vec4<f32>,   // RGB (BT.2020, nits / 10000) -> LMS
  dmb0: vec4<f32>, dmb1: vec4<f32>, dmb2: vec4<f32>,   // L'M'S' -> I Ct Cp (or I P T)
  dmc0: vec4<f32>, dmc1: vec4<f32>, dmc2: vec4<f32>,   // inverse of dmb
  dmd0: vec4<f32>, dmd1: vec4<f32>, dmd2: vec4<f32>,   // inverse of dma
  // vec4 37, 38: written by the compositor after the fel-meta.mjs block (UNIFORM_FLOATS), the source buffer layout in samples as
  // COMPOSE's bls / blo: luma stride, chroma stride, Cb offset, Cr offset; luma x, y, chroma x, y of the top left displayed sample
  lay: vec4<u32>,
  org: vec4<u32>,
}
@group(0) @binding(0) var<uniform> dp: DP;
@group(0) @binding(3) var<uniform> h10p: Hdr10PlusParams;     // HDR10+ block (web/engine/hdr10plus.mjs), valid flag 0 = off
const M1 = 2610.0 / 16384.0; const M2 = 2523.0 / 32.0;
const C1 = 3424.0 / 4096.0; const C2 = 2413.0 / 128.0; const C3 = 2392.0 / 128.0;
fn pq_eotf(x: f32) -> f32 {          // normalised PQ -> fraction of 10000 nits
  let e = pow(clamp(x, 0.0, 1.0), 1.0 / M2);
  return pow(max(e - C1, 0.0) / (C2 - C3 * e), 1.0 / M1);
}
fn pq_inv(y: f32) -> f32 {           // fraction of 10000 nits -> normalised PQ
  let p = pow(max(y, 0.0), M1);
  return pow((C1 + C2 * p) / (1.0 + C3 * p), M2);
}
// BT.2100 HLG inverse OETF: non-linear E' (0..1) -> normalised scene light E (0..1).
const HLG_A = 0.17883277; const HLG_B = 0.28466892; const HLG_C = 0.55991073;
fn hlg_inv_oetf(x: f32) -> f32 {
  let e = clamp(x, 0.0, 1.0);
  if (e <= 0.5) { return e * e / 3.0; }
  return (exp((e - HLG_C) / HLG_A) + HLG_B) / 12.0;
}
// BT.2100 HLG reference OOTF with zero black level: F_D = alpha * Y_S^(gamma - 1) * E per colour (BT.2020 luminance weights).
fn hlg_to_nits(ep: vec3<f32>) -> vec3<f32> {
  let e = vec3<f32>(hlg_inv_oetf(ep.x), hlg_inv_oetf(ep.y), hlg_inv_oetf(ep.z));
  let ys = max(0.2627 * e.x + 0.6780 * e.y + 0.0593 * e.z, 1e-9);
  return e * (dp.hlg.z * pow(ys, dp.hlg.y - 1.0));
}
fn srgb_oetf(x: f32) -> f32 {
  let a = abs(x);
  if (a <= 0.0031308) { return 12.92 * x; }
  return sign(x) * (1.055 * pow(a, 1.0 / 2.4) - 0.055);
}
// BT.2390-3 EETF on PQ code values, source range [lo, hi] mapped onto [lo, tgt].
fn eetf(e: f32) -> f32 {
  let hi = dp.tm.x; let lo = dp.tm.y; let tgt = dp.tm.z;
  if (hi <= tgt) { return e; }
  let span = max(hi - lo, 1e-6);
  let max_lum = (tgt - lo) / span;
  let ks = 1.5 * max_lum - 0.5;
  let e1 = clamp((e - lo) / span, 0.0, 1.0);
  var e2 = e1;
  if (e1 >= ks) {
    let t = (e1 - ks) / (1.0 - ks);
    let t2 = t * t; let t3 = t2 * t;
    e2 = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks) + (-2.0 * t3 + 3.0 * t2) * max_lum;
  }
  return e2 * span + lo;
}

// ST 2094-10 style display management, f32 twin of fel-dm.mjs (dmMapNits). BT.2020 linear nits in and out.
fn dm_mul(a: vec4<f32>, b: vec4<f32>, c: vec4<f32>, v: vec3<f32>) -> vec3<f32> { return vec3<f32>(dot(a.xyz, v), dot(b.xyz, v), dot(c.xyz, v)); }
fn dm_hermite(t: f32, y0: f32, m0: f32, y1: f32, m1: f32, w: f32) -> f32 {
  let t2 = t * t; let t3 = t2 * t;
  return (2.0 * t3 - 3.0 * t2 + 1.0) * y0 + (t3 - 2.0 * t2 + t) * w * m0 + (-2.0 * t3 + 3.0 * t2) * y1 + (t3 - t2) * w * m1;
}
fn dm_tone(i: f32) -> f32 {
  var y: f32;
  if (dp.dmc.y > 0.5) {
    let l = pq_eotf(i) * 10000.0;
    y = pq_inv(max((dp.dmg.x + dp.dmg.y * l) / (1.0 + dp.dmg.z * l), 0.0) / 10000.0);
  } else {
    let s_min = dp.dmx.x; let s_mid = dp.dmx.y; let s_max = dp.dmx.z;
    if (i <= s_min) { y = dp.dmy.x + (i - s_min) * dp.dms.x; }
    else if (i >= s_max) { y = dp.dmy.z + (i - s_max) * dp.dms.z; }
    else if (i < s_mid) { let w = s_mid - s_min; y = dm_hermite((i - s_min) / w, dp.dmy.x, dp.dms.x, dp.dmy.y, dp.dms.y, w); }
    else { let w = s_max - s_mid; y = dm_hermite((i - s_mid) / w, dp.dmy.y, dp.dms.y, dp.dmy.z, dp.dms.z, w); }
  }
  y = clamp(y, dp.dmx.w, dp.dmy.w);
  if (dp.dmo.w > 0.5) { y = pow(clamp(dp.dmo.x * y + dp.dmo.y, 0.0, 1.0), dp.dmo.z); }
  return clamp(y, dp.dmx.w, dp.dmy.w);
}
fn dm_map(rgb: vec3<f32>) -> vec3<f32> {
  let lms = dm_mul(dp.dma0, dp.dma1, dp.dma2, max(rgb, vec3<f32>(0.0)));
  let lmsp = vec3<f32>(pq_inv(lms.x / 10000.0), pq_inv(lms.y / 10000.0), pq_inv(lms.z / 10000.0));
  let itp = dm_mul(dp.dmb0, dp.dmb1, dp.dmb2, lmsp);
  let i1 = dm_tone(itp.x);
  var k = 1.0;
  if (itp.x > 1e-5) { k = (i1 / itp.x) * (dp.dmk.z * itp.x + dp.dmk.w) / (dp.dmk.z * i1 + dp.dmk.w); }
  k = k * (1.0 + dp.dmk.x);
  let ct = itp.y * k; let cp = itp.z * k;
  let i2 = max(i1 - dp.dmk.y * length(vec2<f32>(ct, cp)), 0.0);
  let lp = clamp(dm_mul(dp.dmc0, dp.dmc1, dp.dmc2, vec3<f32>(i2, ct, cp)), vec3<f32>(0.0), vec3<f32>(1.0));
  let ln = vec3<f32>(pq_eotf(lp.x), pq_eotf(lp.y), pq_eotf(lp.z)) * 10000.0;
  var out = dm_mul(dp.dmd0, dp.dmd1, dp.dmd2, ln);
  if (dp.dmc.z > 0.5) { let m = max(max(out.x, out.y), out.z); if (m > dp.dmc.w) { out = out * (dp.dmc.w / m); } }
  return out;
}
@vertex fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
  let v = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
  return vec4<f32>(v[index], 0.0, 1.0);
}
// Source signal (Y, Cb, Cr over the code scale) -> BT.2020 linear light in nits
fn signal_nits(sig: vec3<f32>) -> vec3<f32> {
  let s = sig - dp.off.xyz;
  let nl = vec3<f32>(dot(dp.m0.xyz, s), dot(dp.m1.xyz, s), dot(dp.m2.xyz, s));
  var nits = vec3<f32>(pq_eotf(nl.x), pq_eotf(nl.y), pq_eotf(nl.z)) * 10000.0;
  if (dp.hlg.x > 0.5) { nits = hlg_to_nits(nl); }
  return max(vec3<f32>(dot(dp.l0.xyz, nits), dot(dp.l1.xyz, nits), dot(dp.l2.xyz, nits)), vec3<f32>(0.0));
}
@fragment fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
  let n = max(u32(dp.vp.x), 1u);
  let origin = vec2<u32>(floor(position.xy)) * n;
  if (dp.act.x + dp.act.y + dp.act.z + dp.act.w > 0.5) {
    // outside the Level 5 active area (letterbox, pillarbox): black, whatever the IPT / YCC codes of the bars decode to
    let p = vec2<f32>(origin) + vec2<f32>(0.5 * f32(n));
    if (p.x < dp.act.x || p.x >= f32(dp.dims.x) - dp.act.y || p.y < dp.act.z || p.y >= f32(dp.dims.y) - dp.act.w) { return vec4<f32>(0.0, 0.0, 0.0, 1.0); }
  }
  var lin = vec3<f32>(0.0);
  for (var j = 0u; j < n; j++) { for (var i = 0u; i < n; i++) { lin += decode(origin + vec2<u32>(i, j)); } }
  lin = lin / f32(n * n);
  let m = max(max(lin.r, lin.g), lin.b);
  if (h10p_f(h10p, 0u) > 0.5 && h10p_f(h10p, 1u) > 0.5) {
    if (h10p_f(h10p, 33u) > 0.5) {
      // measurement only (?h10pmap=macos): what macOS gives the same picture, on maxRGB so the hue holds
      if (m > 0.0) { lin = lin * (hdr10plus_system_edr(m) * hdr10plus_scene_gain(m, h10p_f(h10p, 34u)) * dp.fin.x / m); }
    } else if (h10p_f(h10p, 35u) > 0.0) {
      // The ST 2094-40 curve as absolute nits, on maxRGB as the standard defines it, placed on the canvas at the screen's PQ anchor (fin.x divides again below)
      if (m > 0.0) { lin = lin * (hdr10plus_fit_ceiling(hdr10plus_curve_nits(m, h10p), h10p_f(h10p, 34u)) * dp.fin.x / (h10p_f(h10p, 35u) * m)); }
    } else {
      // HDR10+ tone curve (ST 2094-40) on PQ encoded BT.2020 R'G'B', replaces the BT.2390 stage
      let pq = vec3<f32>(pq_inv(lin.x / 10000.0), pq_inv(lin.y / 10000.0), pq_inv(lin.z / 10000.0));
      let mapped = hdr10plus_map(pq, h10p);
      lin = vec3<f32>(pq_eotf(mapped.x), pq_eotf(mapped.y), pq_eotf(mapped.z)) * 10000.0;
    }
  } else if (dp.dmc.x > 0.5) {
    lin = dm_map(lin);
  } else if (dp.tm.w > 0.5 && m > 1e-3) {
    var e = pq_inv(m / 10000.0);
    e = eetf(e);
    if (dp.trim.w > 0.5) { e = pow(clamp(dp.trim.x * e + dp.trim.y, 0.0, 1.0), dp.trim.z); }
    lin = lin * (pq_eotf(e) * 10000.0 / m);
  }
  var disp = max(vec3<f32>(dot(dp.d0.xyz, lin), dot(dp.d1.xyz, lin), dot(dp.d2.xyz, lin)), vec3<f32>(0.0));
  var rgb: vec3<f32>;
  if (dp.fin.z < 0.5) {
    let v = disp / dp.fin.x;
    rgb = vec3<f32>(srgb_oetf(v.x), srgb_oetf(v.y), srgb_oetf(v.z));
  } else if (dp.fin.z < 1.5) {
    rgb = vec3<f32>(pq_inv(disp.x / 10000.0), pq_inv(disp.y / 10000.0), pq_inv(disp.z / 10000.0));
  } else if (dp.fin.z < 2.5) {
    rgb = disp / dp.fin.x;
  } else {
    let n = max(disp.x, max(disp.y, disp.z));
    rgb = disp * (webkit_element_edr(n) / max(n, 1e-6));
  }
  return vec4<f32>(rgb, 1.0);
}
// Safari's HDR video pipeline, maxRGB nits -> EDR value of the picture it shows for that pixel (same HDR10 frame on the element and on a
// linear EDR canvas, three frames, screen capture, Safari 27.2, XDR panel; about 20 nits per 1.0 at 1 nit, 100 at 100 nits, 150 to 170 above 250, the element then stops rising near 8.4).
// A linear canvas is exact (test page: 0.03 to 2 read back as written), so only this curve is missing on it. Chromaticity follows the scale
// of maxRGB (channel / max within 0.015 of the element). Output mode 3 applies it on WebKit only, nothing else reaches this branch.
fn webkit_element_edr(n: f32) -> f32 {
  if (n <= 0.64) { return n * 0.04813; }
  if (n < 1.02) { return 0.0308 * pow(1.37662, log(n / 0.64) / 0.46609); }
  if (n < 2.05) { return 0.0424 * pow(1.54245, log(n / 1.02) / 0.69804); }
  if (n < 3.3) { return 0.0654 * pow(1.48624, log(n / 2.05) / 0.47608); }
  if (n < 6.71) { return 0.0972 * pow(1.66152, log(n / 3.3) / 0.70968); }
  if (n < 10.71) { return 0.1615 * pow(1.41424, log(n / 6.71) / 0.46758); }
  if (n < 17.12) { return 0.2284 * pow(1.39799, log(n / 10.71) / 0.46907); }
  if (n < 27.36) { return 0.3193 * pow(1.39931, log(n / 17.12) / 0.46883); }
  if (n < 43.79) { return 0.4468 * pow(1.38474, log(n / 27.36) / 0.47032); }
  if (n < 70.08) { return 0.6187 * pow(1.3488, log(n / 43.79) / 0.47023); }
  if (n < 112.4) { return 0.8345 * pow(1.30126, log(n / 70.08) / 0.47243); }
  if (n < 179.8) { return 1.0859 * pow(1.26356, log(n / 112.4) / 0.46978); }
  if (n < 226.4) { return 1.3721 * pow(1.14627, log(n / 179.8) / 0.23046); }
  if (n < 287.5) { return 1.5728 * pow(1.17351, log(n / 226.4) / 0.23892); }
  if (n < 363.4) { return 1.8457 * pow(1.19142, log(n / 287.5) / 0.23428); }
  if (n < 459.5) { return 2.199 * pow(1.21874, log(n / 363.4) / 0.23463); }
  if (n < 581.6) { return 2.68 * pow(1.25075, log(n / 459.5) / 0.23564); }
  if (n < 735.5) { return 3.352 * pow(1.27953, log(n / 581.6) / 0.23477); }
  if (n < 931.7) { return 4.289 * pow(1.34064, log(n / 735.5) / 0.23646); }
  if (n < 1187.0) { return 5.75 * pow(1.31861, log(n / 931.7) / 0.24217); }
  if (n < 1442.0) { return 7.582 * pow(1.10459, log(n / 1187.0) / 0.1946); }
  return 8.375;
}`;

/** Stage 3. Fullscreen triangle. All colour maths in f32. Reads the code buffer, the composed signal or a GPU DPB slot. */
export const DISPLAY_WGSL = DISPLAY_CORE_WGSL + /* wgsl */ `
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read> lut: array<f32>;
fn element(plane: u32, x: i32, y: i32) -> f32 {
  var w = dp.dims.x; var h = dp.dims.y; var base = 0u; var stride = dp.lay.x; var o = dp.org.xy;
  if (plane > 0u) { w = dp.dims.z; h = dp.dims.w; base = select(dp.lay.z, dp.lay.w, plane == 2u); stride = dp.lay.y; o = dp.org.zw; }
  let i = base + (u32(clamp(y, 0, i32(h) - 1)) + o.y) * stride + u32(clamp(x, 0, i32(w) - 1)) + o.x;
  var c: u32;
  if (dp.cfg.w < 0.5) { c = src[i] & 65535u; } else { c = (src[i >> 1u] >> ((i & 1u) * 16u)) & 65535u; }
  if (dp.fin.w > 0.5) { return lut[plane * 1024u + min(c, 1023u)]; }
  return f32(c) / dp.cfg.x;
}
fn upsample(plane: u32, pixel: vec2<u32>) -> f32 {
  let uv = (vec2<f32>(pixel) - dp.cfg.yz) * 0.5;
  let lo = vec2<i32>(floor(uv)); let f = fract(uv);
  return mix(mix(element(plane, lo.x, lo.y), element(plane, lo.x + 1, lo.y), f.x),
             mix(element(plane, lo.x, lo.y + 1), element(plane, lo.x + 1, lo.y + 1), f.x), f.y);
}
fn decode(xy: vec2<u32>) -> vec3<f32> {      // BT.2020 linear light in nits
  return signal_nits(vec3<f32>(element(0u, i32(xy.x), i32(xy.y)), upsample(1u, xy), upsample(2u, xy)));
}
`;

/**
 * Stage 3 without stages 1 and 2 (`FelCompositor` option `lean`): a single layer hardware picture (plain HDR10, HDR10+, HLG, or profile 5
 * with its mapping as a LUT) decoded from the external texture inside the render pass. Same uniforms and output mapping as DISPLAY_WGSL.
 */
export const DISPLAY_LEAN_WGSL = DISPLAY_CORE_WGSL + /* wgsl */ `
// The codes come straight from the hardware decoded VideoFrame (SPS relabelled as in RECOVER, so the import applies the YCbCr -> RGB
// matrix and nothing else): no RECOVER pass and no code buffer. One sample per pixel; luma is rounded to its code like RECOVER.
// Chroma is the import's own full resolution chroma at that pixel (Chrome 157 macOS: a nearest and a linear sampler read identical
// values at pixel centres, and a chroma sample shifted to the left siting moved away from RECOVER + DISPLAY, p99 0.0136 -> 0.0179),
// where RECOVER + DISPLAY take it at even columns, average two rows and interpolate it again; that is the whole difference.
struct RP { w: u32, h: u32, full: u32, pad1: u32 }
@group(0) @binding(1) var tex: texture_external;
@group(0) @binding(2) var smp: sampler;
@group(0) @binding(4) var<uniform> rp: RP;
const KR = 0.2627; const KB = 0.0593; const KG = 0.6780;
fn ycc(rgb: vec3<f32>) -> vec3<f32> {
  let l = KR * rgb.r + KG * rgb.g + KB * rgb.b;
  if (rp.full == 1u) {
    return vec3<f32>(1023.0 * l, 512.0 + 1023.0 * (rgb.b - l) / (2.0 * (1.0 - KB)), 512.0 + 1023.0 * (rgb.r - l) / (2.0 * (1.0 - KR)));
  }
  return vec3<f32>(64.0 + 876.0 * l, 512.0 + 896.0 * (rgb.b - l) / (2.0 * (1.0 - KB)), 512.0 + 896.0 * (rgb.r - l) / (2.0 * (1.0 - KR)));
}
// dp.fin.w = 1: the RPU mapping as a LUT (fel-meta.mjs buildMappingLut, Dolby Vision without a residual and without MMR, profile 5).
// Luma is a code, so one entry; the import's chroma lies between two codes, so the two entries are interpolated.
@group(0) @binding(5) var<storage, read> lut: array<f32>;
fn mapped(plane: u32, v: f32) -> f32 {
  let c = clamp(v, 0.0, 1023.0); let lo = u32(floor(c)); let hi = min(lo + 1u, 1023u);
  return mix(lut[plane * 1024u + lo], lut[plane * 1024u + hi], c - f32(lo));
}
fn decode(xy: vec2<u32>) -> vec3<f32> {      // BT.2020 linear light in nits
  let c = ycc(textureSampleBaseClampToEdge(tex, smp, (vec2<f32>(xy) + 0.5) / vec2<f32>(f32(dp.dims.x), f32(dp.dims.y))).rgb);
  let y = clamp(round(c.x), 0.0, 1023.0);
  if (dp.fin.w > 0.5) { return signal_nits(vec3<f32>(lut[u32(y)], mapped(1u, c.y), mapped(2u, c.z))); }
  return signal_nits(vec3<f32>(y, c.y, c.z) / dp.cfg.x);
}
`;
