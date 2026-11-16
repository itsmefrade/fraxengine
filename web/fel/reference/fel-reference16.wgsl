// SPDX-License-Identifier: LGPL-3.0-only
// Reference16 arithmetic under EngineBilinearLumaAnnexBChromaClip10.
// Integer-only. Pair values store low/high words of a two's-complement i64.
@group(0) @binding(0) var<storage, read> params: array<u32>;
@group(0) @binding(1) var<storage, read> base: array<u32>;
@group(0) @binding(2) var<storage, read> enhancement: array<u32>;
@group(0) @binding(3) var<storage, read> tables: array<u32>;
@group(0) @binding(4) var<storage, read_write> output: array<u32>;
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
fn packed_bl(index: u32) -> u32 { return (base[index >> 1u] >> ((index & 1u) * 16u)) & 65535u; }
fn packed_el(index: u32) -> u32 { return (enhancement[index >> 1u] >> ((index & 1u) * 16u)) & 65535u; }
fn bl_at(component: u32, x: i32, y: i32) -> u32 {
  let w = select(params[0], params[4], component != 0u);
  let h = select(params[1], params[5], component != 0u);
  var offset = 0u;
  if (component > 0u) { offset = params[0] * params[1] + (component - 1u) * params[4] * params[5]; }
  return packed_bl(offset + u32(clamp(y, 0, i32(h) - 1)) * w + u32(clamp(x, 0, i32(w) - 1)));
}
fn el_at(component: u32, x: i32, y: i32) -> i32 {
  let w = select(params[2], params[6], component != 0u);
  let h = select(params[3], params[7], component != 0u);
  var offset = 0u;
  if (component > 0u) { offset = params[2] * params[3] + (component - 1u) * params[6] * params[7]; }
  return i32(packed_el(offset + u32(clamp(y, 0, i32(h) - 1)) * w + u32(clamp(x, 0, i32(w) - 1))));
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
  if (params[0] == params[2]) { return u32(el_at(component, i32(x), i32(y))); }
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
  let curve = 16u + component * 208u;
  var selected = 0u;
  for (var i = 0u; i < params[curve]; i++) { if (code >= params[curve + 1u + i]) { selected = i; } }
  let piece = curve + 16u + selected * 24u;
  if (params[piece] == 0u) { return tables[component * 1024u + code]; }
  var values: vec3<u32>;
  for (var i = 0u; i < 3u; i++) {
    let c = 16u + i * 208u;
    values[i] = clamp(inputs[i], params[c + 1u], params[c + 1u + params[c]]);
  }
  let y = values.x; let u = values.y; let v = values.z;
  let first = array<u32, 7>(y << 10u, u << 10u, v << 10u, y * u, y * v, u * v, q20_product(y * u, v << 10u));
  let second = array<u32, 7>(y * y, u * u, v * v, q20_product(first[3], first[3]), q20_product(first[4], first[4]), q20_product(first[5], first[5]), q20_product(first[6], first[6]));
  var sum = signed_product(params[piece + 2u], 1048576u);
  for (var row = 0u; row < params[piece + 1u]; row++) {
    for (var i = 0u; i < 7u; i++) {
      var term = first[i];
      if (row == 1u) { term = second[i]; }
      if (row == 2u) { term = q20_product(first[i], second[i]); }
      sum = add64(sum, signed_product(params[piece + 3u + row * 7u + i], term));
    }
  }
  if ((sum.y & 0x80000000u) != 0u) { return 0u; }
  let shift = params[8] + 4u;
  if ((sum.y >> shift) != 0u) { return 65535u; }
  return min((sum.x >> shift) | (sum.y << (32u - shift)), 65535u);
}
@compute @workgroup_size(128)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  let index = id.x + id.y * params[11];
  if (index >= params[10]) { return; }
  let luma_count = params[0] * params[1]; let chroma_count = params[4] * params[5];
  var component = 0u; var local = index; var width = params[0];
  if (index >= luma_count) { component = 1u + (index - luma_count) / chroma_count; local = (index - luma_count) % chroma_count; width = params[4]; }
  let x = local % width; let y = local / width;
  var inputs = vec3<u32>(bl_at(0u, i32(x), i32(y)), bl_at(1u, i32(x / 2u), i32(y / 2u)), bl_at(2u, i32(x / 2u), i32(y / 2u)));
  if (component > 0u) { inputs = vec3<u32>(chroma_luma(x, y), bl_at(1u, i32(x), i32(y)), bl_at(2u, i32(x), i32(y))); }
  let pred = prediction(component, inputs[component], inputs);
  let residual = bitcast<i32>(tables[3072u + component * 1024u + enhancement_code(component, x, y)]);
  let raw = u32(clamp(i32(pred) + residual, 0, 65535));
  let shift = 16u - params[9]; var quantized = raw;
  if (shift > 0u) { quantized = min((raw + (1u << (shift - 1u))) >> shift, (1u << params[9]) - 1u); }
  output[index] = raw | (quantized << 16u);
}
