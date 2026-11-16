// HEVC sample adaptive offset (spec 8.7.3) in WGSL, bit exact with rust_h265 src/sao.rs. Input layout: FORMAT-sao.md.
//
// Mirrors (file:line in rust/aether-hevc/vendor/rust_h265/src/sao.rs at commit 9515fd59): sao_band_filter 232, sao_edge_filter 283
// (per-sample neighbour availability through the 3x3 CTB `avail` table, built by sao_ctb_rows_at 642 from
// sao_skip_slice_boundary 437 and precomputed per CTB on the CPU), edge_offset_px 416, restore_no_filter 760.
//
// Reads the fully deblocked picture `src` and writes every word of every plane row of `dst` (a second buffer):
// SAO reads deblocked neighbours, so it cannot run in place. One invocation produces one u32 word (two samples);
// samples past the plane width (stride padding) are copied.

export const SAO_WGSL = /* wgsl */ `
struct Params {
  width: u32, height: u32, strideY: u32, strideC: u32,
  offCb: u32, offCr: u32, bitDepthY: u32, bitDepthC: u32,
  log2Ctb: u32, widthInCtbs: u32, cbQpOffset: i32, crQpOffset: i32,
  anyNoFilter: u32, heightInCtbs: u32, mapStride: u32, reserved: u32,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
@group(0) @binding(3) var<storage, read> blk: array<u32>;
@group(0) @binding(4) var<storage, read> sao: array<vec4<u32>>;

fn i8At(v: u32, sh: u32) -> i32 { return bitcast<i32>(v << (24u - sh)) >> 24u; }

struct Plane { off: u32, rs: u32, w: i32, h: i32, cs: u32, bd: u32 }

fn sgn(a: i32, b: i32) -> i32 { return select(0, 1, a > b) - select(0, 1, a < b); }

// Sample xo (0..5 = columns x0-2..x0+3) of the three words [x0-2, x0+3] of a row.
fn pick(r: vec3<u32>, xo: u32) -> i32 { return i32((r[xo >> 1u] >> (16u * (xo & 1u))) & 0xffffu); }

// The three words around word column xw of row y (clamped to the plane: neighbours outside the picture are
// rejected by the availability test, so a clamped word is never used).
fn rowWords(p: Plane, y: i32, xw: u32) -> vec3<u32> {
  let r = p.off + u32(clamp(y, 0, p.h - 1)) * p.rs;
  return vec3<u32>(src[r + max(xw, 1u) - 1u], src[r + xw], src[r + min(xw + 1u, p.rs - 1u)]);
}

// SAO of the two samples of word xw (columns x0, x0 + 1) of row y of plane c. Both lie in one CTB and one 4x4
// no-filter block (CTB sizes and 4x4 blocks are even in luma and chroma).
fn saoWord(p: Plane, c: u32, y: i32, xw: u32, w: u32) -> u32 {
  let x0 = i32(xw << 1u);
  if (x0 >= p.w) { return w; }
  let l2 = P.log2Ctb - p.cs;
  let cx = x0 >> l2;
  let cy = y >> l2;
  let rec = sao[u32(cy) * P.widthInCtbs + u32(cx)];
  let typ = (rec.x >> (9u + 2u * c)) & 3u;
  if (typ == 0u) { return w; }
  if (P.anyNoFilter != 0u) {
    // restore_no_filter: lossless / PCM samples keep their deblocked value.
    let lx = u32(x0) << p.cs;
    let ly = u32(y) << p.cs;
    let b = blk[(ly >> 3u) * P.mapStride + (lx >> 3u)];
    if (((b >> (8u + ((lx >> 2u) & 1u) + 2u * ((ly >> 2u) & 1u))) & 1u) != 0u) { return w; }
  }
  let cls = (rec.x >> (15u + 5u * c)) & 31u;
  let offs = rec[1u + c];
  let maxV = (1 << p.bd) - 1;
  var s = vec2<i32>(i32(w & 0xffffu), i32(w >> 16u));
  let n = select(1, 2, x0 + 1 < p.w);
  if (typ == 1u) {
    // Band offset: bands cls..cls+3 (mod 32) get offsets 1..4.
    for (var k = 0; k < n; k++) {
      let band = ((u32(s[k]) >> (p.bd - 5u)) - cls) & 31u;
      if (band < 4u) { s[k] = clamp(s[k] + i8At(offs, 8u * band), 0, maxV); }
    }
    return u32(s.x) | (u32(s.y) << 16u);
  }
  // Edge offset. Neighbour a at (x + dx, y + dy), b at (x - dx, y - dy); classes horizontal, vertical, 135, 45.
  var dx = -1;
  var dy = 0;
  if (cls == 1u) { dx = 0; dy = -1; } else if (cls == 2u) { dx = -1; dy = -1; } else if (cls == 3u) { dx = 1; dy = -1; }
  let ra = rowWords(p, y + dy, xw);
  let rb = rowWords(p, y - dy, xw);
  let ay = y + dy;
  let by = y - dy;
  for (var k = 0; k < n; k++) {
    let x = x0 + k;
    let ax = x + dx;
    let bx = x - dx;
    if (ax < 0 || ay < 0 || ax >= p.w || ay >= p.h || bx < 0 || by < 0 || bx >= p.w || by >= p.h) { continue; }
    // Neighbour CTBs (at most one CTB away): the per-sample avail check of sao_edge_filter.
    let ia = u32(((ay >> l2) - cy + 1) * 3 + ((ax >> l2) - cx + 1));
    let ib = u32(((by >> l2) - cy + 1) * 3 + ((bx >> l2) - cx + 1));
    if (((rec.x >> ia) & (rec.x >> ib) & 1u) == 0u) { continue; }
    let sum = sgn(s[k], pick(ra, u32(ax - x0 + 2))) + sgn(s[k], pick(rb, u32(bx - x0 + 2)));
    if (sum == 0) { continue; }
    // edge_idx: sum -2, -1, 1, 2 -> categories 1, 2, 3, 4 (offset bytes 0..3).
    s[k] = clamp(s[k] + i8At(offs, 8u * u32(select(sum + 2, sum + 1, sum > 0))), 0, maxV);
  }
  return u32(s.x) | (u32(s.y) << 16u);
}

@compute @workgroup_size(64)
fn saoMain(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
  var t = gid.x + gid.y * nwg.x * 64u;
  let rsY = P.strideY >> 1u;
  let rsC = P.strideC >> 1u;
  let hC = P.height >> 1u;
  let nY = rsY * P.height;
  let nC = rsC * hC;
  var p: Plane;
  var c = 0u;
  if (t < nY) {
    p = Plane(0u, rsY, i32(P.width), i32(P.height), 0u, P.bitDepthY);
  } else if (t < nY + 2u * nC) {
    t -= nY;
    c = 1u + t / nC;
    t = t % nC;
    p = Plane(select(P.offCr, P.offCb, c == 1u), rsC, i32(P.width >> 1u), i32(hC), 1u, P.bitDepthC);
  } else {
    return;
  }
  let wi = p.off + t;
  dst[wi] = saoWord(p, c, i32(t / p.rs), t % p.rs, src[wi]);
}
`;

// Invocations the entry point needs: one per u32 word of the three planes.
export function saoThreads(params) {
  const [, height, strideY, strideC] = params;
  return (strideY >> 1) * height + 2 * (strideC >> 1) * (height >> 1);
}
