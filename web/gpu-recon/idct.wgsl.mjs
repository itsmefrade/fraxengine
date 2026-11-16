// Dequantisation + inverse transform of HEVC TUs, bit exact with rust_h265. Format: FORMAT-idct.md.
// One workgroup (64 threads) per TU. Mirrors:
//   dequant      residual_coding.rs:1269-1286, 1452-1472 (decode_residual_simple), 1026-1055 (generic path)
//   ts shift     residual_coding.rs:1478-1487
//   bypass       residual_coding.rs:1450 (`level as i16`)
//   transforms   inverse_transform.rs: idct_*_skip (:266, even/odd matrix products), transform_4x4_luma (:359), idct_dc (:333)
// Extended precision, RDPCM and transform skip rotation are not covered (flag bits reserved, must be 0).

export const IDCT_MATRIX = [
  64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
  90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4, -4, -13, -22, -31, -38, -46, -54, -61, -67, -73, -78, -82, -85, -88, -90, -90,
  90, 87, 80, 70, 57, 43, 25, 9, -9, -25, -43, -57, -70, -80, -87, -90, -90, -87, -80, -70, -57, -43, -25, -9, 9, 25, 43, 57, 70, 80, 87, 90,
  90, 82, 67, 46, 22, -4, -31, -54, -73, -85, -90, -88, -78, -61, -38, -13, 13, 38, 61, 78, 88, 90, 85, 73, 54, 31, 4, -22, -46, -67, -82, -90,
  89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89, 89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89,
  88, 67, 31, -13, -54, -82, -90, -78, -46, -4, 38, 73, 90, 85, 61, 22, -22, -61, -85, -90, -73, -38, 4, 46, 78, 90, 82, 54, 13, -31, -67, -88,
  87, 57, 9, -43, -80, -90, -70, -25, 25, 70, 90, 80, 43, -9, -57, -87, -87, -57, -9, 43, 80, 90, 70, 25, -25, -70, -90, -80, -43, 9, 57, 87,
  85, 46, -13, -67, -90, -73, -22, 38, 82, 88, 54, -4, -61, -90, -78, -31, 31, 78, 90, 61, 4, -54, -88, -82, -38, 22, 73, 90, 67, 13, -46, -85,
  83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83,
  82, 22, -54, -90, -61, 13, 78, 85, 31, -46, -90, -67, 4, 73, 88, 38, -38, -88, -73, -4, 67, 90, 46, -31, -85, -78, -13, 61, 90, 54, -22, -82,
  80, 9, -70, -87, -25, 57, 90, 43, -43, -90, -57, 25, 87, 70, -9, -80, -80, -9, 70, 87, 25, -57, -90, -43, 43, 90, 57, -25, -87, -70, 9, 80,
  78, -4, -82, -73, 13, 85, 67, -22, -88, -61, 31, 90, 54, -38, -90, -46, 46, 90, 38, -54, -90, -31, 61, 88, 22, -67, -85, -13, 73, 82, 4, -78,
  75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75, 75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75,
  73, -31, -90, -22, 78, 67, -38, -90, -13, 82, 61, -46, -88, -4, 85, 54, -54, -85, 4, 88, 46, -61, -82, 13, 90, 38, -67, -78, 22, 90, 31, -73,
  70, -43, -87, 9, 90, 25, -80, -57, 57, 80, -25, -90, -9, 87, 43, -70, -70, 43, 87, -9, -90, -25, 80, 57, -57, -80, 25, 90, 9, -87, -43, 70,
  67, -54, -78, 38, 85, -22, -90, 4, 90, 13, -88, -31, 82, 46, -73, -61, 61, 73, -46, -82, 31, 88, -13, -90, -4, 90, 22, -85, -38, 78, 54, -67,
  64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64,
  61, -73, -46, 82, 31, -88, -13, 90, -4, -90, 22, 85, -38, -78, 54, 67, -67, -54, 78, 38, -85, -22, 90, 4, -90, 13, 88, -31, -82, 46, 73, -61,
  57, -80, -25, 90, -9, -87, 43, 70, -70, -43, 87, 9, -90, 25, 80, -57, -57, 80, 25, -90, 9, 87, -43, -70, 70, 43, -87, -9, 90, -25, -80, 57,
  54, -85, -4, 88, -46, -61, 82, 13, -90, 38, 67, -78, -22, 90, -31, -73, 73, 31, -90, 22, 78, -67, -38, 90, -13, -82, 61, 46, -88, 4, 85, -54,
  50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50, 50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50,
  46, -90, 38, 54, -90, 31, 61, -88, 22, 67, -85, 13, 73, -82, 4, 78, -78, -4, 82, -73, -13, 85, -67, -22, 88, -61, -31, 90, -54, -38, 90, -46,
  43, -90, 57, 25, -87, 70, 9, -80, 80, -9, -70, 87, -25, -57, 90, -43, -43, 90, -57, -25, 87, -70, -9, 80, -80, 9, 70, -87, 25, 57, -90, 43,
  38, -88, 73, -4, -67, 90, -46, -31, 85, -78, 13, 61, -90, 54, 22, -82, 82, -22, -54, 90, -61, -13, 78, -85, 31, 46, -90, 67, 4, -73, 88, -38,
  36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36,
  31, -78, 90, -61, 4, 54, -88, 82, -38, -22, 73, -90, 67, -13, -46, 85, -85, 46, 13, -67, 90, -73, 22, 38, -82, 88, -54, -4, 61, -90, 78, -31,
  25, -70, 90, -80, 43, 9, -57, 87, -87, 57, -9, -43, 80, -90, 70, -25, -25, 70, -90, 80, -43, -9, 57, -87, 87, -57, 9, 43, -80, 90, -70, 25,
  22, -61, 85, -90, 73, -38, -4, 46, -78, 90, -82, 54, -13, -31, 67, -88, 88, -67, 31, 13, -54, 82, -90, 78, -46, 4, 38, -73, 90, -85, 61, -22,
  18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18, 18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18,
  13, -38, 61, -78, 88, -90, 85, -73, 54, -31, 4, 22, -46, 67, -82, 90, -90, 82, -67, 46, -22, -4, 31, -54, 73, -85, 90, -88, 78, -61, 38, -13,
  9, -25, 43, -57, 70, -80, 87, -90, 90, -87, 80, -70, 57, -43, 25, -9, -9, 25, -43, 57, -70, 80, -87, 90, -90, 87, -80, 70, -57, 43, -25, 9,
  4, -13, 22, -31, 38, -46, 54, -61, 67, -73, 78, -82, 85, -88, 90, -90, 90, -90, 88, -85, 82, -78, 73, -67, 61, -54, 46, -38, 31, -22, 13, -4
];

// 4x4 DST, row k = input index, column i = output index: D[k][i] (inverse_transform.rs:347 tr_4x4_luma written as a matrix).
export const IDCT_DST = [29, 55, 74, 84, 74, 74, 0, -74, 84, -29, -74, 55, 55, -84, 74, -29];

// Uniform table: 1024 DCT entries (row k of the 32 point matrix, 32 entries per row) then 16 DST entries.
export const IDCT_TABLE = new Int32Array([...IDCT_MATRIX, ...IDCT_DST]);

export const idctWgsl = /* wgsl */ `
struct Params {
  tuBase : u32,
  tuCount : u32,
  wgPerRow : u32,
  pad : u32,
};

@group(0) @binding(0) var<uniform> params : Params;
@group(0) @binding(1) var<uniform> tbl : array<vec4<i32>, 260>;
@group(0) @binding(2) var<storage, read> tus : array<u32>;
@group(0) @binding(3) var<storage, read> coefs : array<u32>;
@group(0) @binding(4) var<storage, read> lists : array<u32>;
@group(0) @binding(5) var<storage, read_write> residual : array<i32>;

// Row stride n + 1 (<= 33) keeps the column and row passes free of bank conflicts.
var<workgroup> blk : array<i32, 1056>;
var<workgroup> tmp : array<i32, 1056>;
var<workgroup> nzx : atomic<u32>;
var<workgroup> nzy : atomic<u32>;

fn tcoef(idx : u32) -> i32 {
  return tbl[idx >> 2u][idx & 3u];
}

fn clip16(v : i32) -> i32 {
  return clamp(v, -32768, 32767);
}

fn levelScale(r : u32) -> u32 {
  switch (r) {
    case 0u: { return 40u; }
    case 1u: { return 45u; }
    case 2u: { return 51u; }
    case 3u: { return 57u; }
    case 4u: { return 64u; }
    default: { return 72u; }
  }
}

// (level * scale * scaleM + add) >> shift clamped to 16 bits, exact for |level| <= 32768, scale * scaleM < 2^27.
// The product is split so no step leaves 32 bits; a product at or above 2^(15 + shift) clamps whatever the sign handling.
fn dequant(level : i32, x : u32, y : u32, log2 : u32, scale : u32, listBase : u32, flat : bool, shift : u32) -> i32 {
  var sm = 16u;
  if (!flat) {
    if (log2 >= 4u && x == 0u && y == 0u) {
      sm = lists[listBase + 16u] & 255u;
    } else {
      var mpos = (y << 2u) + x;
      if (log2 == 3u) { mpos = (y << 3u) + x; }
      else if (log2 == 4u) { mpos = ((y >> 1u) << 3u) + (x >> 1u); }
      else if (log2 == 5u) { mpos = ((y >> 2u) << 3u) + (x >> 2u); }
      sm = (lists[listBase + (mpos >> 2u)] >> ((mpos & 3u) * 8u)) & 255u;
    }
  }
  let s = scale * sm;
  let neg = level < 0;
  let a = u32(abs(level));
  let hi = a * (s >> 14u);
  if (hi >= (1u << (shift + 1u))) {
    return select(32767, -32768, neg);
  }
  let prod = i32((hi << 14u) + a * (s & 16383u));
  let add = 1i << (shift - 1u);
  let v = select((prod + add) >> shift, (add - prod) >> shift, neg);
  return clip16(v);
}

@compute @workgroup_size(64)
fn main(@builtin(local_invocation_index) t : u32, @builtin(workgroup_id) wid : vec3<u32>) {
  let tuIdx = wid.y * params.wgPerRow + wid.x;
  if (tuIdx >= params.tuCount) { return; }

  let rb = (params.tuBase + tuIdx) * 6u;
  let w1 = tus[rb + 1u];
  let coefOff = tus[rb + 2u];
  let num = tus[rb + 3u];
  let resOff = tus[rb + 4u];
  let log2 = (w1 & 3u) + 2u;
  let n = 1u << log2;
  let isDst = ((w1 >> 4u) & 1u) != 0u;
  let isTs = ((w1 >> 5u) & 1u) != 0u;
  let isByp = ((w1 >> 6u) & 1u) != 0u;
  let bd = ((w1 >> 8u) & 7u) + 8u;
  let listIdx = (w1 >> 11u) & 31u;
  let qp = (w1 >> 16u) & 127u;
  let direct = isTs || isByp;
  let S = n + 1u;
  let area = n * n;

  if (t == 0u) {
    atomicStore(&nzx, 0u);
    atomicStore(&nzy, 0u);
  }
  for (var i = t; i < n * S; i += 64u) {
    blk[i] = 0;
  }
  workgroupBarrier();

  // Dequantise and scatter the coefficient list.
  let shiftQ = bd + log2 - 5u;
  let scale = levelScale(qp % 6u) << (qp / 6u);
  let flat = listIdx == 31u || (isTs && log2 > 2u);
  let listBase = listIdx * 17u;
  let tsShift = 15i - i32(bd) - i32(log2);
  for (var e = t; e < num; e += 64u) {
    let entry = coefs[coefOff + e];
    let pos = entry & 1023u;
    let level = bitcast<i32>(entry) >> 16u;
    let x = pos & (n - 1u);
    let y = pos >> log2;
    var v = level;
    if (!isByp) {
      v = dequant(level, x, y, log2, scale, listBase, flat, shiftQ);
      if (isTs) {
        if (tsShift > 0) { v = (v + (1i << u32(tsShift - 1))) >> u32(tsShift); }
        else { v = v << u32(-tsShift); }
        v = (v << 16u) >> 16u;
      }
    }
    blk[y * S + x] = v;
    atomicMax(&nzx, x);
    atomicMax(&nzy, y);
  }
  workgroupBarrier();

  let nzC = min(n, ((atomicLoad(&nzx) >> 2u) + 1u) * 4u);
  let nzR = min(n, ((atomicLoad(&nzy) >> 2u) + 1u) * 4u);
  let shift2 = 20u - bd;
  let add2 = 1i << (shift2 - 1u);
  let dcOnly = !direct && !isDst && num == 1u && (coefs[coefOff] & 1023u) == 0u;
  let half = n >> 1u;
  let step = 32u >> log2;

  // Pass 1: vertical (columns), (sum + 64) >> 7, written transposed (tmp[col * S + row]).
  if (dcOnly) {
    // Both passes of a DC only block fold into one shift (idct_dc): (64 * ((dc + 1) >> 1) + 2^(19-bd)) >> (20-bd).
    let shiftD = 14u - bd;
    let r = clip16((((blk[0] + 1) >> 1) + (1i << (shiftD - 1u))) >> shiftD);
    for (var i = t; i < area; i += 64u) {
      residual[resOff + i] = r;
    }
  } else if (!direct) {
    if (isDst) {
      for (var it = t; it < nzC * 4u; it += 64u) {
        let c = it % nzC;
        let i = it / nzC;
        var sum = 0;
        for (var k = 0u; k < nzR; k++) {
          sum += tcoef(1024u + k * 4u + i) * blk[k * S + c];
        }
        tmp[c * S + i] = clip16((sum + 64) >> 7);
      }
    } else {
      for (var it = t; it < nzC * half; it += 64u) {
        let c = it % nzC;
        let i = it / nzC;
        var e = 0;
        var o = 0;
        for (var k = 0u; k < nzR; k += 2u) {
          e += tcoef((k * step) * 32u + i) * blk[k * S + c];
          o += tcoef(((k + 1u) * step) * 32u + i) * blk[(k + 1u) * S + c];
        }
        tmp[c * S + i] = clip16((e + o + 64) >> 7);
        tmp[c * S + (n - 1u - i)] = clip16((e - o + 64) >> 7);
      }
    }
  }
  workgroupBarrier();

  // Pass 2: horizontal (rows), (sum + add) >> (20 - bitDepth); the result replaces blk (dead after pass 1).
  if (!dcOnly && !direct) {
    if (isDst) {
      for (var it = t; it < n * 4u; it += 64u) {
        let r = it % n;
        let i = it / n;
        var sum = 0;
        for (var k = 0u; k < nzC; k++) {
          sum += tcoef(1024u + k * 4u + i) * tmp[k * S + r];
        }
        blk[r * S + i] = clip16((sum + add2) >> shift2);
      }
    } else {
      for (var it = t; it < n * half; it += 64u) {
        let r = it % n;
        let i = it / n;
        var e = 0;
        var o = 0;
        for (var k = 0u; k < nzC; k += 2u) {
          e += tcoef((k * step) * 32u + i) * tmp[k * S + r];
          o += tcoef(((k + 1u) * step) * 32u + i) * tmp[(k + 1u) * S + r];
        }
        blk[r * S + i] = clip16((e + o + add2) >> shift2);
        blk[r * S + (n - 1u - i)] = clip16((e - o + add2) >> shift2);
      }
    }
  }
  workgroupBarrier();

  if (!dcOnly) {
    for (var i = t; i < area; i += 64u) {
      residual[resOff + i] = blk[(i >> log2) * S + (i & (n - 1u))];
    }
  }
}
`;
