// HEVC intra prediction + residual add for 4:2:0, bit exact with rust_h265 (layouts in FORMAT-intra.md).
// Mirrors rust/aether-hevc/vendor/rust_h265/src:
//   intra_pred.rs:80-167   build_reference_samples_fast (availability ranges + substitution)
//   intra_pred.rs:171-318  build_reference_samples_general (constrained intra pred: per sample availability)
//   intra_pred.rs:594-695  filter_reference_samples ([1,2,1] and strong 32x32 smoothing)
//   intra_pred.rs:328-359  predict_planar, 366-417 predict_dc (+ luma edge filter), 428-578 predict_angular
//                          (+ mode 10 / 26 luma boundary filters)
//   intra_pred.rs:701-723  add_residual (clip to [0, (1 << bitDepth) - 1])
//   cu_tree.rs predict_intra_luma / predict_intra_chroma (which filters run, edge filter switch)
//   cu_tree.rs cip_masks (a reference sample is usable under constrained intra pred only if its 4x4 luma
//                          block is intra)
//
// Execution model: one workgroup runs a list of STEPS; the TUs of one step are independent of each other, a step may
// read anything earlier steps of the same workgroup or earlier dispatches wrote. Per step (two barriers): phase 1
// builds every TU's reference array (4 * size + 1 samples, substituted and filtered straight from the picture) in
// workgroup memory and sums DC; phase 2 predicts two horizontally adjacent samples per invocation, adds the residual
// and stores one whole u32 word, while the next step's TU headers load into the other header buffer. TUs start at x
// multiples of 4 in every plane, so a TU owns whole words and no atomics are needed on the picture.
// Measured (Apple M-series, synthetic 4K I frame g-4k-i, CTU schedule, 8647 serial steps): fusing substitution and
// filtering into one phase took 21.8 -> 19.5 ms; prefetching headers and the first residuals 23.3 -> 21.8 ms; workgroups of 128,
// 512, 1024 invocations were slower than 256 (24.6, 28.5, 29.4 ms against 23.3).
export const INTRA_WG = 256;
export const INTRA_MAXT = 64;     // TUs per step
export const INTRA_REFCAP = 1536; // reference samples per step (sum of 4 * size + 1)

export const intraWgsl = /* wgsl */ `
struct Params {
  wY : u32, hY : u32, wC : u32, hC : u32,
  strideY : u32, strideC : u32, offCb : u32, offCr : u32,
  bitDepthY : u32, bitDepthC : u32,
  flags : u32,      // bit 0 strong_intra_smoothing_enabled_flag, bit 1 intra_smoothing_disabled_flag
  imapStride : u32, // 4x4 luma blocks per intra map row (= width / 4)
  stepBase : u32,   // word offset of the step table in sched[]
  tuBase : u32,     // word offset of the scheduled TU table in sched[]
  pad0 : u32, pad1 : u32,
};
struct Disp { groupBase : u32, groupCount : u32, pad0 : u32, pad1 : u32 };

@group(0) @binding(0) var<uniform> P : Params;
@group(0) @binding(1) var<storage, read_write> pic : array<u32>;
@group(0) @binding(2) var<storage, read> sched : array<u32>;
@group(0) @binding(3) var<storage, read> res : array<i32>;
@group(0) @binding(4) var<storage, read> imap : array<u32>;
@group(0) @binding(5) var<uniform> D : Disp;

const WG : u32 = ${INTRA_WG}u;
const MAXT : u32 = ${INTRA_MAXT}u;
const REFCAP : u32 = ${INTRA_REFCAP}u;
const NO_RES : u32 = 0xffffffffu;

var<workgroup> sFil : array<u32, REFCAP>; // reference samples after substitution and filtering (order: refPos)
// TU headers and step sizes, double buffered so the next step's records load while this step predicts.
var<workgroup> tXY : array<u32, 2 * MAXT>;
var<workgroup> tInfo : array<u32, 2 * MAXT>;
var<workgroup> tRes : array<u32, 2 * MAXT>;
var<workgroup> tRefOff : array<u32, 2 * MAXT>;
var<workgroup> tPairOff : array<u32, 2 * MAXT>;
var<workgroup> tDc : array<atomic<u32>, 2 * MAXT>;
var<workgroup> sStep : array<u32, 8>; // per buffer: tuCount, refTotal, pairTotal

// intra_pred.rs:443 and :449
const ANGLE = array<i32, 33>(32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13,
  -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32);
const INV_ANGLE = array<i32, 15>(-4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638,
  -4096);

struct Tu {
  x0 : i32, y0 : i32, log2 : u32, s : i32, c : u32, mode : u32, avail : u32, edgeOff : bool, cip : bool,
  base : u32, stride : u32, pw : i32, ph : i32, bd : u32, ro : u32,
};

fn tuAt(t : u32) -> Tu {
  var u : Tu;
  let xy = tXY[t];
  let info = tInfo[t];
  u.x0 = i32(xy & 0xffffu);
  u.y0 = i32(xy >> 16u);
  u.log2 = info & 7u;
  u.s = 1 << u.log2;
  u.c = (info >> 3u) & 3u;
  u.mode = (info >> 5u) & 63u;
  u.avail = (info >> 11u) & 31u;
  u.edgeOff = ((info >> 16u) & 1u) != 0u;
  u.cip = ((info >> 17u) & 1u) != 0u;
  if (u.c == 0u) {
    u.base = 0u; u.stride = P.strideY; u.pw = i32(P.wY); u.ph = i32(P.hY); u.bd = P.bitDepthY;
  } else {
    u.base = select(P.offCr, P.offCb, u.c == 1u); u.stride = P.strideC; u.pw = i32(P.wC); u.ph = i32(P.hC);
    u.bd = P.bitDepthC;
  }
  u.ro = tRefOff[t];
  return u;
}

fn loadSample(u : Tu, x : i32, y : i32) -> u32 {
  let i = u32(y) * u.stride + u32(x);
  let w = pic[u.base + (i >> 1u)];
  return select(w & 0xffffu, w >> 16u, (i & 1u) == 1u);
}

// Unified reference index (intra_pred.rs:92): 0 = bottom-most left sample, 2s - 1 = left sample next to the
// corner, 2s = corner, 2s + 1 .. 4s = above row left to right.
fn refPos(u : Tu, i : i32) -> vec2<i32> {
  let s2 = 2 * u.s;
  if (i < s2) { return vec2<i32>(u.x0 - 1, u.y0 + s2 - 1 - i); }
  if (i == s2) { return vec2<i32>(u.x0 - 1, u.y0 - 1); }
  return vec2<i32>(u.x0 + i - s2 - 1, u.y0 - 1);
}

// The five availability ranges of build_reference_samples_fast (intra_pred.rs:104-137) as [start, end) pairs.
// avail bits: 0 bottom_left, 1 left, 2 up_left, 3 up, 4 up_right.
struct Ranges { st : array<i32, 5>, en : array<i32, 5> };
fn ranges(u : Tu) -> Ranges {
  var r : Ranges;
  let s = u.s;
  let hasX = u.x0 > 0;
  let hasY = u.y0 > 0;
  var bl = 0;
  if (((u.avail & 1u) != 0u) && hasX) { bl = min(max(u.ph - (u.y0 + s), 0), s); }
  r.st[0] = s - bl; r.en[0] = s;
  r.st[1] = s; r.en[1] = select(s, 2 * s, ((u.avail & 2u) != 0u) && hasX);
  r.st[2] = 2 * s; r.en[2] = select(2 * s, 2 * s + 1, ((u.avail & 4u) != 0u) && hasX && hasY);
  r.st[3] = 2 * s + 1; r.en[3] = select(2 * s + 1, 3 * s + 1, ((u.avail & 8u) != 0u) && hasY);
  var ur = 0;
  if (((u.avail & 16u) != 0u) && hasY) { ur = min(max(u.pw - (u.x0 + s), 0), s); }
  r.st[4] = 3 * s + 1; r.en[4] = 3 * s + 1 + ur;
  return r;
}

fn inRanges(r : Ranges, i : i32) -> bool {
  var a = false;
  for (var k = 0; k < 5; k++) { a = a || (i >= r.st[k] && i < r.en[k]); }
  return a;
}

// cip_masks (cu_tree.rs): the 4x4 luma block holding the sample is intra.
fn intraAt(u : Tu, p : vec2<i32>) -> bool {
  let sh = select(1u, 0u, u.c == 0u);
  let lx = u32(p.x) << sh;
  let ly = u32(p.y) << sh;
  let idx = (ly >> 2u) * P.imapStride + (lx >> 2u);
  return ((imap[idx >> 5u] >> (idx & 31u)) & 1u) != 0u;
}

fn availCip(u : Tu, r : Ranges, i : i32) -> bool {
  return inRanges(r, i) && intraAt(u, refPos(u, i));
}

// Substituted reference sample i (spec 8.4.4.2.2): its own value when available, else the nearest available sample
// below it in the unified order, else the first available one, else 1 << (bitDepth - 1).
fn substituted(u : Tu, r : Ranges, i : i32) -> i32 {
  let n = 4 * u.s + 1;
  var src = -1;
  if (!u.cip) {
    if (inRanges(r, i)) {
      src = i;
    } else {
      var prev = -1;
      var first = n;
      for (var k = 0; k < 5; k++) {
        if (r.en[k] > r.st[k]) {
          first = min(first, r.st[k]);
          if (r.en[k] <= i) { prev = max(prev, r.en[k] - 1); }
        }
      }
      src = select(select(-1, first, first < n), prev, prev >= 0);
    }
  } else {
    // Constrained intra pred (build_reference_samples_general): availability per sample, so scan.
    for (var j = i; j >= 0; j--) {
      if (availCip(u, r, j)) { src = j; break; }
    }
    if (src < 0) {
      for (var j = i + 1; j < n; j++) {
        if (availCip(u, r, j)) { src = j; break; }
      }
    }
  }
  if (src < 0) { return 1 << (u.bd - 1u); }
  let p = refPos(u, src);
  return i32(loadSample(u, p.x, p.y));
}

// TU of flat reference index f in the step whose headers start at hb (largest t with tRefOff[t] <= f).
fn findRef(f : u32, hb : u32, n : u32) -> u32 {
  var lo = hb;
  var hi = hb + n;
  while (hi - lo > 1u) {
    let mid = (lo + hi) >> 1u;
    if (tRefOff[mid] <= f) { lo = mid; } else { hi = mid; }
  }
  return lo;
}

fn findPair(f : u32, hb : u32, n : u32) -> u32 {
  var lo = hb;
  var hi = hb + n;
  while (hi - lo > 1u) {
    let mid = (lo + hi) >> 1u;
    if (tPairOff[mid] <= f) { lo = mid; } else { hi = mid; }
  }
  return lo;
}

// filter_reference_samples (intra_pred.rs:594) for unified index i, computed from the picture rather than from
// workgroup memory so that substitution and filtering share one phase: [1,2,1] needs the substituted neighbours
// i - 1 and i + 1, the strong 32x32 smoothing the anchors 0, s, 2s, 3s, 4s. Chroma is never filtered in 4:2:0
// (chroma_format_idc != 3), DC never, 4x4 never; [1,2,1] over the unified array equals the separate top / left
// filters with the corner rule.
fn reference(u : Tu, r : Ranges, i : i32, own : i32) -> i32 {
  let s = u.s;
  let n4 = 4 * s;
  if (u.mode == 1u || (P.flags & 2u) != 0u || s == 4 || u.c != 0u) { return own; }
  let m = i32(u.mode);
  let dist = min(abs(m - 26), abs(m - 10));
  let thresh = select(select(0, 1, u.log2 == 4u), 7, u.log2 == 3u);
  if (dist <= thresh) { return own; }
  if ((P.flags & 1u) != 0u && u.log2 == 5u) {
    let thr = 1 << (u.bd - 5u);
    let c = substituted(u, r, 2 * s);
    let t = substituted(u, r, n4);
    let l = substituted(u, r, 0);
    if (abs(c + t - 2 * substituted(u, r, 3 * s)) < thr && abs(c + l - 2 * substituted(u, r, s)) < thr) {
      if (i == 0 || i == 2 * s || i == n4) { return own; }
      if (i > 2 * s) {
        let k = i - 2 * s;
        return ((64 - k) * c + k * t + 32) >> 6;
      }
      let k = 2 * s - i;
      return ((64 - k) * c + k * l + 32) >> 6;
    }
  }
  if (i == 0 || i == n4) { return own; }
  return (substituted(u, r, i - 1) + 2 * own + substituted(u, r, i + 1) + 2) >> 2;
}

fn top(u : Tu, k : i32) -> i32 { return i32(sFil[u.ro + u32(2 * u.s + k)]); }
fn left(u : Tu, k : i32) -> i32 { return i32(sFil[u.ro + u32(2 * u.s - k)]); }

// Angular reference row (intra_pred.rs:461-476 / 518-532) as an index into the unified array: the main side for
// k >= 0 (above row for modes >= 18, left column below), the other side projected through invAngle for k < 0.
fn angRef(u : Tu, k : i32, dir : i32, inv : i32) -> i32 {
  let j = select((k * inv + 128) >> 8, k, k >= 0);
  let side = select(-dir, dir, k >= 0);
  return i32(sFil[u.ro + u32(2 * u.s + side * j)]);
}

fn angular(u : Tu, a : i32, b : i32, angle : i32, dir : i32, inv : i32) -> i32 {
  let pos = (b + 1) * angle;
  let idx = pos >> 5;
  let fact = pos & 31;
  let ri = a + idx + 1;
  let r0 = angRef(u, ri, dir, inv);
  if (fact == 0) { return r0; }
  return ((32 - fact) * r0 + fact * angRef(u, ri + 1, dir, inv) + 16) >> 5;
}

// Prediction of samples (x, y) and (x + 1, y).
fn predictPair(u : Tu, x : i32, y : i32, dc : i32, maxv : i32) -> vec2<i32> {
  let s = u.s;
  let lumaEdge = u.c == 0u && !u.edgeOff && s < 32;
  if (u.mode == 0u) {
    let l = left(u, y + 1);
    let tr = top(u, s + 1);
    let bl = left(u, s + 1);
    let base = (y + 1) * bl + s;
    let sh = u.log2 + 1u;
    let p0 = ((s - 1 - x) * l + (x + 1) * tr + (s - 1 - y) * top(u, x + 1) + base) >> sh;
    let p1 = ((s - 2 - x) * l + (x + 2) * tr + (s - 1 - y) * top(u, x + 2) + base) >> sh;
    return vec2<i32>(p0, p1);
  }
  if (u.mode == 1u) {
    var v = vec2<i32>(dc, dc);
    if (lumaEdge) {
      if (y == 0) {
        v = (vec2<i32>(top(u, x + 1), top(u, x + 2)) + 3 * dc + 2) >> vec2<u32>(2u);
        if (x == 0) { v.x = (left(u, 1) + 2 * dc + top(u, 1) + 2) >> 2; }
      } else if (x == 0) {
        v.x = (left(u, y + 1) + 3 * dc + 2) >> 2;
      }
    }
    return v;
  }
  let m = i32(u.mode);
  let angle = ANGLE[m - 2];
  let inv = select(0, INV_ANGLE[clamp(m - 11, 0, 14)], m >= 11 && m <= 25);
  var v : vec2<i32>;
  if (m >= 18) {
    // Rows over the above reference: one (idx, fact) per row.
    v = vec2<i32>(angular(u, x, y, angle, 1, inv), angular(u, x + 1, y, angle, 1, inv));
    if (lumaEdge && m == 26 && x == 0) { v.x = clamp(top(u, 1) + ((left(u, y + 1) - top(u, 0)) >> 1), 0, maxv); }
  } else {
    // Columns over the left reference: one (idx, fact) per column.
    v = vec2<i32>(angular(u, y, x, angle, -1, inv), angular(u, y, x + 1, angle, -1, inv));
    if (lumaEdge && m == 10 && y == 0) {
      v = clamp(left(u, 1) + ((vec2<i32>(top(u, x + 1), top(u, x + 2)) - top(u, 0)) >> vec2<u32>(1u)), vec2<i32>(0),
        vec2<i32>(maxv));
    }
  }
  return v;
}

// Loads the TU records of the step at stepIdx (TUs from tuStart) into header buffer hb: issue (registers) ...
struct Next { step : vec4<u32>, tu : vec4<u32> };
fn fetchStep(stepIdx : u32, tuStart : u32, lid : u32) -> Next {
  var n : Next;
  let sb = P.stepBase + 4u * stepIdx;
  n.step = vec4<u32>(sched[sb], sched[sb + 1u], sched[sb + 2u], sched[sb + 3u]);
  if (lid < MAXT) {
    let tb = P.tuBase + 4u * (tuStart + lid);
    n.tu = vec4<u32>(sched[tb], sched[tb + 1u], sched[tb + 2u], sched[tb + 3u]);
  }
  return n;
}
// ... and commit (workgroup memory).
fn commitStep(n : Next, hb : u32, lid : u32) {
  if (lid < n.step.y) {
    tXY[hb + lid] = n.tu.x;
    tInfo[hb + lid] = n.tu.y;
    tRes[hb + lid] = n.tu.z;
    tRefOff[hb + lid] = n.tu.w & 0xffffu;
    tPairOff[hb + lid] = n.tu.w >> 16u;
    atomicStore(&tDc[hb + lid], 0u);
  }
  if (lid == 0u) {
    let sb = select(0u, 4u, hb != 0u);
    sStep[sb] = n.step.y;
    sStep[sb + 1u] = n.step.z;
    sStep[sb + 2u] = n.step.w;
  }
}

fn residualPair(u : Tu, t : u32, x : i32, y : i32) -> vec2<i32> {
  let ro = tRes[t];
  if (ro == NO_RES) { return vec2<i32>(0); }
  let rb = ro + u32(y * u.s + x);
  return vec2<i32>(res[rb], res[rb + 1u]);
}

@compute @workgroup_size(${INTRA_WG})
fn main(@builtin(workgroup_id) wid : vec3<u32>, @builtin(local_invocation_index) lid : u32) {
  let gl = wid.y * 65535u + wid.x;
  if (gl >= D.groupCount) { return; }
  let g = D.groupBase + gl;
  let stepStart = sched[3u * g];
  let stepCount = sched[3u * g + 1u];
  var tuStart = sched[3u * g + 2u];
  commitStep(fetchStep(stepStart, tuStart, lid), 0u, lid);
  workgroupBarrier();
  for (var si = 0u; si < stepCount; si++) {
    let hb = select(0u, MAXT, (si & 1u) != 0u);
    let sb = select(0u, 4u, (si & 1u) != 0u);
    let tuCount = sStep[sb];
    let refTotal = sStep[sb + 1u];
    let pairTotal = sStep[sb + 2u];
    // Residuals of this invocation's first two pairs, issued now so their latency hides behind the reference phases.
    var res0 = vec2<i32>(0);
    var res1 = vec2<i32>(0);
    if (lid < pairTotal) {
      let t = findPair(lid, hb, tuCount);
      let u = tuAt(t);
      let p = lid - tPairOff[t];
      res0 = residualPair(u, t, i32(p & ((1u << (u.log2 - 1u)) - 1u)) * 2, i32(p >> (u.log2 - 1u)));
    }
    if (lid + WG < pairTotal) {
      let f = lid + WG;
      let t = findPair(f, hb, tuCount);
      let u = tuAt(t);
      let p = f - tPairOff[t];
      res1 = residualPair(u, t, i32(p & ((1u << (u.log2 - 1u)) - 1u)) * 2, i32(p >> (u.log2 - 1u)));
    }
    for (var f = lid; f < refTotal; f += WG) {
      let t = findRef(f, hb, tuCount);
      let u = tuAt(t);
      let r = ranges(u);
      let i = i32(f - u.ro);
      let own = substituted(u, r, i);
      sFil[f] = u32(reference(u, r, i, own));
      // DC sum (intra_pred.rs:378): above row and left column (DC is never filtered).
      if (u.mode == 1u && ((i >= u.s && i < 2 * u.s) || (i > 2 * u.s && i <= 3 * u.s))) {
        atomicAdd(&tDc[t], u32(own));
      }
    }
    workgroupBarrier();
    let hasNext = si + 1u < stepCount;
    var next : Next;
    if (hasNext) { next = fetchStep(stepStart + si + 1u, tuStart + tuCount, lid); }
    for (var f = lid; f < pairTotal; f += WG) {
      let t = findPair(f, hb, tuCount);
      let u = tuAt(t);
      let p = f - tPairOff[t];
      let y = i32(p >> (u.log2 - 1u));
      let x = i32(p & ((1u << (u.log2 - 1u)) - 1u)) * 2;
      let maxv = (1 << u.bd) - 1;
      let dc = (i32(atomicLoad(&tDc[t])) + u.s) >> (u.log2 + 1u);
      var r : vec2<i32>;
      if (f == lid) { r = res0; } else if (f == lid + WG) { r = res1; } else { r = residualPair(u, t, x, y); }
      let v = clamp(predictPair(u, x, y, dc, maxv) + r, vec2<i32>(0), vec2<i32>(maxv));
      let i = u32(u.y0 + y) * u.stride + u32(u.x0 + x);
      pic[u.base + (i >> 1u)] = u32(v.x) | (u32(v.y) << 16u);
    }
    if (hasNext) { commitStep(next, MAXT - hb, lid); }
    tuStart += tuCount;
    storageBarrier();
    workgroupBarrier();
  }
}
`;
