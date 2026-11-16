// HEVC deblocking (spec 8.7.2) in WGSL, bit exact with rust_h265 src/deblock.rs. Input layout: FORMAT-deblock.md.
//
// Mirrors (file:line in rust/aether-hevc/vendor/rust_h265/src/deblock.rs at commit 9515fd59):
//   beta_lookup 48, tc_lookup 54, chroma_qp_table 61, chroma_tc 75, luma_strong_seg 162, luma_weak_seg 181,
//   loop_filter_chroma_weak 202, filter_luma_edge_scalar 285 (decisions), luma_vertical_edges 583,
//   luma_horizontal_edges 641, chroma_edges 698, skip_*_slice_boundary 462 / 487 (precomputed per CTB on the CPU),
//   edge_no_filter 412, deblock_offsets 427, get_qp_y 435. The SIMD path (deblock_simd.rs) is bit identical to the
//   scalar one (rust_h265 test simd_luma_edge_matches_scalar), so only the scalar one is mirrored.
//
// Two entry points, dispatched in this order in one compute pass: `deblockV` (every vertical luma and chroma edge)
// then `deblockH` (every horizontal edge), the whole-picture order of deblock_picture. One invocation filters one
// 4-line segment of one edge. Segments of one pass never share a u32 word: a luma edge at x (multiple of 8) reads
// and writes only samples x-4..x+3 (words (x-4)/2..(x+2)/2), a chroma edge at xc only xc-2..xc+1, and horizontal
// edges likewise in rows, so whole words are read and written without atomics.

export const DEBLOCK_WGSL = /* wgsl */ `
struct Params {
  width: u32, height: u32, strideY: u32, strideC: u32,
  offCb: u32, offCr: u32, bitDepthY: u32, bitDepthC: u32,
  log2Ctb: u32, widthInCtbs: u32, cbQpOffset: i32, crQpOffset: i32,
  anyNoFilter: u32, heightInCtbs: u32, mapStride: u32, reserved: u32,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> pic: array<u32>;
@group(0) @binding(2) var<storage, read> blk: array<u32>;
@group(0) @binding(3) var<storage, read> ctbs: array<u32>;

// Spec table 8-14 (deblock.rs TC_TABLE).
var<private> TC_TABLE: array<i32, 54> = array<i32, 54>(
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
  1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4,
  5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24);
// QpC for qPi 30..43 (deblock.rs chroma_qp_table, 4:2:0).
var<private> QP_C: array<i32, 14> = array<i32, 14>(29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37);

struct Seg { p3: vec4<i32>, p2: vec4<i32>, p1: vec4<i32>, p0: vec4<i32>,
             q0: vec4<i32>, q1: vec4<i32>, q2: vec4<i32>, q3: vec4<i32> }

fn sr(v: vec4<i32>, n: u32) -> vec4<i32> { return v >> vec4<u32>(n); }
fn lo(w: u32) -> i32 { return i32(w & 0xffffu); }
fn hi(w: u32) -> i32 { return i32(w >> 16u); }
fn pk(a: i32, b: i32) -> u32 { return u32(a) | (u32(b) << 16u); }

fn blkAt(bx: u32, by: u32) -> u32 { return blk[by * P.mapStride + bx]; }
fn ctbAt(x: u32, y: u32) -> u32 { return ctbs[(y >> P.log2Ctb) * P.widthInCtbs + (x >> P.log2Ctb)]; }
// Signed byte at bit offset sh.
fn i8At(v: u32, sh: u32) -> i32 { return bitcast<i32>(v << (24u - sh)) >> 24u; }
fn qpOf(b: u32) -> i32 { return i8At(b, 16u); }
// no-filter flag of the 4x4 luma block holding luma sample (x, y), from that sample's 8x8 block record b.
fn nfAt(b: u32, x: u32, y: u32) -> bool {
  return P.anyNoFilter != 0u && ((b >> (8u + ((x >> 2u) & 1u) + 2u * ((y >> 2u) & 1u))) & 1u) != 0u;
}

// beta_lookup: BETA_TABLE is 0 below 16, qp - 10 up to 28, 2 * qp - 38 above (same 52 values).
fn betaLookup(qp: i32, off: i32, bd: u32) -> i32 {
  let i = clamp(qp + off, 0, 51);
  let b = select(select(2 * i - 38, i - 10, i < 29), 0, i < 16);
  return b << (bd - 8u);
}
fn tcLookup(qp: i32, bs: i32, off: i32, bd: u32) -> i32 {
  return TC_TABLE[clamp(qp + 2 * (bs - 1) + off, 0, 53)] << (bd - 8u);
}
fn chromaTc(qpY: i32, qpOff: i32, tcOff: i32, bd: u32) -> i32 {
  let qpi = clamp(qpY + qpOff, 0, 57);
  var qp = qpi;
  if (qpi > 43) { qp = qpi - 6; } else if (qpi >= 30) { qp = QP_C[qpi - 30]; }
  return TC_TABLE[clamp(qp + 2 + tcOff, 0, 53)] << (bd - 8u);
}

// filter_luma_edge_scalar for one segment (lanes = the 4 lines). Returns the segment to store; p / q taps of a
// no-filter side keep their input values.
fn lumaFilter(s: Seg, beta: i32, tc: i32, nfP: bool, nfQ: bool) -> Seg {
  var o = s;
  let dp = abs(s.p2 - 2 * s.p1 + s.p0);
  let dq = abs(s.q2 - 2 * s.q1 + s.q0);
  let d0 = dp.x + dq.x;
  let d3 = dp.w + dq.w;
  if (d0 + d3 >= beta) { return o; }
  let beta3 = beta >> 3u;
  let beta2 = beta >> 2u;
  let tc25 = (tc * 5 + 1) >> 1u;
  let strong0 = abs(s.p3.x - s.p0.x) + abs(s.q3.x - s.q0.x) < beta3 && abs(s.p0.x - s.q0.x) < tc25 && (d0 << 1u) < beta2;
  let strong3 = abs(s.p3.w - s.p0.w) + abs(s.q3.w - s.q0.w) < beta3 && abs(s.p0.w - s.q0.w) < tc25 && (d3 << 1u) < beta2;
  if (strong0 && strong3) {
    let t2 = vec4<i32>(tc << 1u);
    let p3 = s.p3; let p2 = s.p2; let p1 = s.p1; let p0 = s.p0;
    let q0 = s.q0; let q1 = s.q1; let q2 = s.q2; let q3 = s.q3;
    if (!nfP) {
      o.p2 = p2 + clamp(sr(2 * p3 + 3 * p2 + p1 + p0 + q0 + 4, 3u) - p2, -t2, t2);
      o.p1 = p1 + clamp(sr(p2 + p1 + p0 + q0 + 2, 2u) - p1, -t2, t2);
      o.p0 = p0 + clamp(sr(p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4, 3u) - p0, -t2, t2);
    }
    if (!nfQ) {
      o.q0 = q0 + clamp(sr(p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4, 3u) - q0, -t2, t2);
      o.q1 = q1 + clamp(sr(p0 + q0 + q1 + q2 + 2, 2u) - q1, -t2, t2);
      o.q2 = q2 + clamp(sr(2 * q3 + 3 * q2 + q1 + q0 + p0 + 4, 3u) - q2, -t2, t2);
    }
  } else {
    let side = (beta + (beta >> 1u)) >> 3u;
    let modP1 = dp.x + dp.w < side;
    let modQ1 = dq.x + dq.w < side;
    let p2 = s.p2; let p1 = s.p1; let p0 = s.p0; let q0 = s.q0; let q1 = s.q1; let q2 = s.q2;
    let vtc = vec4<i32>(tc);
    let vtc2 = vec4<i32>(tc >> 1u);
    let raw = sr(9 * (q0 - p0) - 3 * (q1 - p1) + 8, 4u);
    let on = abs(raw) < vec4<i32>(10 * tc);
    let delta0 = clamp(raw, -vtc, vtc);
    let np1 = p1 + clamp(sr(sr(p2 + p0 + 1, 1u) - p1 + delta0, 1u), -vtc2, vtc2);
    let nq1 = q1 + clamp(sr(sr(q2 + q0 + 1, 1u) - q1 - delta0, 1u), -vtc2, vtc2);
    if (!nfP) {
      o.p0 = select(p0, p0 + delta0, on);
      if (modP1) { o.p1 = select(p1, np1, on); }
    }
    if (!nfQ) {
      o.q0 = select(q0, q0 - delta0, on);
      if (modQ1) { o.q1 = select(q1, nq1, on); }
    }
  }
  return o;
}

fn clampSeg(s: Seg, m: i32) -> Seg {
  let z = vec4<i32>(0);
  let v = vec4<i32>(m);
  return Seg(clamp(s.p3, z, v), clamp(s.p2, z, v), clamp(s.p1, z, v), clamp(s.p0, z, v),
             clamp(s.q0, z, v), clamp(s.q1, z, v), clamp(s.q2, z, v), clamp(s.q3, z, v));
}

// Luma vertical edge at x (multiple of 8, > 0), segment rows y..y+3 (y multiple of 4).
fn lumaV(x: u32, y: u32) {
  let b = blkAt(x >> 3u, y >> 3u);
  let bs = i32((b >> (2u * ((y >> 2u) & 1u))) & 3u);
  if (bs == 0) { return; }
  let c = ctbAt(x, y);
  if ((x & ((1u << P.log2Ctb) - 1u)) == 0u && (c & 1u) != 0u) { return; }
  let bp = blkAt((x >> 3u) - 1u, y >> 3u);
  let qp = (qpOf(bp) + qpOf(b) + 1) >> 1u;
  let tc = tcLookup(qp, bs, i8At(c, 16u), P.bitDepthY);
  if (tc == 0) { return; }
  let beta = betaLookup(qp, i8At(c, 8u), P.bitDepthY);
  let rs = P.strideY >> 1u;
  let w0 = y * rs + ((x - 4u) >> 1u);
  var a: array<vec4<u32>, 4>;
  for (var l = 0u; l < 4u; l++) {
    let r = w0 + l * rs;
    a[l] = vec4<u32>(pic[r], pic[r + 1u], pic[r + 2u], pic[r + 3u]);
  }
  let s = Seg(
    vec4<i32>(lo(a[0].x), lo(a[1].x), lo(a[2].x), lo(a[3].x)), vec4<i32>(hi(a[0].x), hi(a[1].x), hi(a[2].x), hi(a[3].x)),
    vec4<i32>(lo(a[0].y), lo(a[1].y), lo(a[2].y), lo(a[3].y)), vec4<i32>(hi(a[0].y), hi(a[1].y), hi(a[2].y), hi(a[3].y)),
    vec4<i32>(lo(a[0].z), lo(a[1].z), lo(a[2].z), lo(a[3].z)), vec4<i32>(hi(a[0].z), hi(a[1].z), hi(a[2].z), hi(a[3].z)),
    vec4<i32>(lo(a[0].w), lo(a[1].w), lo(a[2].w), lo(a[3].w)), vec4<i32>(hi(a[0].w), hi(a[1].w), hi(a[2].w), hi(a[3].w)));
  let o = clampSeg(lumaFilter(s, beta, tc, nfAt(bp, x - 1u, y), nfAt(b, x, y)), (1 << P.bitDepthY) - 1);
  for (var l = 0u; l < 4u; l++) {
    let r = w0 + l * rs;
    pic[r] = pk(o.p3[l], o.p2[l]);
    pic[r + 1u] = pk(o.p1[l], o.p0[l]);
    pic[r + 2u] = pk(o.q0[l], o.q1[l]);
    pic[r + 3u] = pk(o.q2[l], o.q3[l]);
  }
}

// Luma horizontal edge at y (multiple of 8, > 0), segment columns x..x+3 (x multiple of 4).
fn lumaH(x: u32, y: u32) {
  let b = blkAt(x >> 3u, y >> 3u);
  let bs = i32((b >> (4u + 2u * ((x >> 2u) & 1u))) & 3u);
  if (bs == 0) { return; }
  let c = ctbAt(x, y);
  if ((y & ((1u << P.log2Ctb) - 1u)) == 0u && (c & 2u) != 0u) { return; }
  let bp = blkAt(x >> 3u, (y >> 3u) - 1u);
  let qp = (qpOf(bp) + qpOf(b) + 1) >> 1u;
  let tc = tcLookup(qp, bs, i8At(c, 16u), P.bitDepthY);
  if (tc == 0) { return; }
  let beta = betaLookup(qp, i8At(c, 8u), P.bitDepthY);
  let rs = P.strideY >> 1u;
  let w0 = (y - 4u) * rs + (x >> 1u);
  var t: array<vec4<i32>, 8>;
  for (var k = 0u; k < 8u; k++) {
    let r = w0 + k * rs;
    let u = pic[r];
    let v = pic[r + 1u];
    t[k] = vec4<i32>(lo(u), hi(u), lo(v), hi(v));
  }
  let s = Seg(t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7]);
  let o = clampSeg(lumaFilter(s, beta, tc, nfAt(bp, x, y - 1u), nfAt(b, x, y)), (1 << P.bitDepthY) - 1);
  let ov = array<vec4<i32>, 6>(o.p2, o.p1, o.p0, o.q0, o.q1, o.q2);
  for (var k = 0u; k < 6u; k++) {
    let r = w0 + (k + 1u) * rs;
    pic[r] = pk(ov[k].x, ov[k].y);
    pic[r + 1u] = pk(ov[k].z, ov[k].w);
  }
}

// loop_filter_chroma_weak on the 4 lines (lanes): new p0 / q0.
fn chromaDelta(p1: vec4<i32>, p0: vec4<i32>, q0: vec4<i32>, q1: vec4<i32>, tc: i32) -> vec4<i32> {
  return clamp(sr((q0 - p0) * 4 + p1 - q1 + 4, 3u), vec4<i32>(-tc), vec4<i32>(tc));
}

// Chroma vertical edge at chroma column xc (multiple of 8, > 0), segment rows yc..yc+3 (yc multiple of 4).
fn chromaV(xc: u32, yc: u32) {
  let xl = xc << 1u;
  let yl = yc << 1u;
  let b = blkAt(xl >> 3u, yl >> 3u);
  if ((b & 3u) != 2u) { return; }
  let c = ctbAt(xl, yl);
  if ((xl & ((1u << P.log2Ctb) - 1u)) == 0u && (c & 1u) != 0u) { return; }
  let bp = blkAt((xl >> 3u) - 1u, yl >> 3u);
  let qp = (qpOf(bp) + qpOf(b) + 1) >> 1u;
  let tcOff = i8At(c, 16u);
  let nfP = nfAt(bp, xl - 1u, yl);
  let nfQ = nfAt(b, xl, yl);
  let m = vec4<i32>((1 << P.bitDepthC) - 1);
  let rs = P.strideC >> 1u;
  for (var pl = 0u; pl < 2u; pl++) {
    let tc = chromaTc(qp, select(P.crQpOffset, P.cbQpOffset, pl == 0u), tcOff, P.bitDepthC);
    if (tc == 0) { continue; }
    let w0 = select(P.offCr, P.offCb, pl == 0u) + yc * rs + (xc >> 1u) - 1u;
    var u: vec4<u32>;
    var v: vec4<u32>;
    for (var l = 0u; l < 4u; l++) {
      u[l] = pic[w0 + l * rs];
      v[l] = pic[w0 + l * rs + 1u];
    }
    let p1 = vec4<i32>(u & vec4<u32>(0xffffu));
    let p0 = vec4<i32>(u >> vec4<u32>(16u));
    let q0 = vec4<i32>(v & vec4<u32>(0xffffu));
    let q1 = vec4<i32>(v >> vec4<u32>(16u));
    let d = chromaDelta(p1, p0, q0, q1, tc);
    let np0 = select(clamp(p0 + d, vec4<i32>(0), m), p0, nfP);
    let nq0 = select(clamp(q0 - d, vec4<i32>(0), m), q0, nfQ);
    for (var l = 0u; l < 4u; l++) {
      pic[w0 + l * rs] = pk(p1[l], np0[l]);
      pic[w0 + l * rs + 1u] = pk(nq0[l], q1[l]);
    }
  }
}

// Chroma horizontal edge at chroma row yc (multiple of 8, > 0), segment columns xc..xc+3 (xc multiple of 4).
fn chromaH(xc: u32, yc: u32) {
  let xl = xc << 1u;
  let yl = yc << 1u;
  let b = blkAt(xl >> 3u, yl >> 3u);
  if (((b >> 4u) & 3u) != 2u) { return; }
  let c = ctbAt(xl, yl);
  if ((yl & ((1u << P.log2Ctb) - 1u)) == 0u && (c & 2u) != 0u) { return; }
  let bp = blkAt(xl >> 3u, (yl >> 3u) - 1u);
  let qp = (qpOf(bp) + qpOf(b) + 1) >> 1u;
  let tcOff = i8At(c, 16u);
  let nfP = nfAt(bp, xl, yl - 1u);
  let nfQ = nfAt(b, xl, yl);
  let m = vec4<i32>((1 << P.bitDepthC) - 1);
  let rs = P.strideC >> 1u;
  for (var pl = 0u; pl < 2u; pl++) {
    let tc = chromaTc(qp, select(P.crQpOffset, P.cbQpOffset, pl == 0u), tcOff, P.bitDepthC);
    if (tc == 0) { continue; }
    let w0 = select(P.offCr, P.offCb, pl == 0u) + (yc - 2u) * rs + (xc >> 1u);
    var t: array<vec4<i32>, 4>;
    for (var k = 0u; k < 4u; k++) {
      let u = pic[w0 + k * rs];
      let v = pic[w0 + k * rs + 1u];
      t[k] = vec4<i32>(lo(u), hi(u), lo(v), hi(v));
    }
    let d = chromaDelta(t[0], t[1], t[2], t[3], tc);
    let np0 = select(clamp(t[1] + d, vec4<i32>(0), m), t[1], nfP);
    let nq0 = select(clamp(t[2] - d, vec4<i32>(0), m), t[2], nfQ);
    pic[w0 + rs] = pk(np0.x, np0.y);
    pic[w0 + rs + 1u] = pk(np0.z, np0.w);
    pic[w0 + 2u * rs] = pk(nq0.x, nq0.y);
    pic[w0 + 2u * rs + 1u] = pk(nq0.z, nq0.w);
  }
}

fn threadIndex(gid: vec3<u32>, nwg: vec3<u32>) -> u32 { return gid.x + gid.y * nwg.x * 64u; }

@compute @workgroup_size(64)
fn deblockV(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
  var t = threadIndex(gid, nwg);
  let edgesL = (P.width >> 3u) - 1u;
  let nL = edgesL * (P.height >> 2u);
  if (t < nL) {
    lumaV((t % edgesL + 1u) << 3u, (t / edgesL) << 2u);
    return;
  }
  t -= nL;
  let edgesC = (((P.width >> 1u) + 7u) >> 3u) - 1u;
  if (edgesC == 0u) { return; }
  if (t < edgesC * (P.height >> 3u)) {
    chromaV((t % edgesC + 1u) << 3u, (t / edgesC) << 2u);
  }
}

@compute @workgroup_size(64)
fn deblockH(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
  var t = threadIndex(gid, nwg);
  let segsL = P.width >> 2u;
  let nL = segsL * ((P.height >> 3u) - 1u);
  if (t < nL) {
    lumaH((t % segsL) << 2u, (t / segsL + 1u) << 3u);
    return;
  }
  t -= nL;
  let segsC = P.width >> 3u;
  let rowsC = (((P.height >> 1u) + 7u) >> 3u) - 1u;
  if (t < segsC * rowsC) {
    chromaH((t % segsC) << 2u, (t / segsC + 1u) << 3u);
  }
}
`;

// Invocations each entry point needs (the driver dispatches ceil(n / 64) workgroups).
export function deblockThreads(width, height) {
  const edgesL = (width >> 3) - 1;
  const edgesC = (((width >> 1) + 7) >> 3) - 1;
  const v = Math.max(0, edgesL) * (height >> 2) + Math.max(0, edgesC) * (height >> 3);
  const h = (width >> 2) * ((height >> 3) - 1) + (width >> 3) * ((((height >> 1) + 7) >> 3) - 1);
  return { v, h };
}
