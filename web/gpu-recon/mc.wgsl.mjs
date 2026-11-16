// HEVC inter prediction (motion compensation) in WGSL, bit exact with rust_h265.
//
// Mirrors rust/aether-hevc/vendor/rust_h265/src/inter_pred.rs:
//   mc_luma / mc_chroma (uni, unweighted: PixelSink rounding :577, copy_block :733),
//   mc_luma_i16 / mc_chroma_i16 (14-bit intermediates :863 / :906, interp_block :325, interp_block16 :269),
//   motion_compensation_pu (:1009): explicit weighted uni (put_weighted :196), default bi (bi_average_row :609,
//   avg_block :771), explicit weighted bi (the weighted arm of its bi closure), chroma_mv (:141), ref_sample edge clamp (:148, ref_window :679),
//   and the i16 narrowing of mc_simd.rs (row / narrow32). Record layout and narrowing rule: FORMAT-mc.md.
//
// Two entry points over one bind group:
//   main       general records (any PU size). Two workgroups per record: luma in tiles of up to 32x16, then Cb and
//              Cr together in tiles of up to 16x16 per plane. Per tile every active window (per list, per chroma
//              plane) is loaded edge clamped into workgroup memory, the horizontal pass goes to a second workgroup
//              array, and each thread runs the vertical pass of a 2x4 block in registers, combines the lists,
//              rounds, clips and writes its words. Three barriers per tile.
//              A part whose lists all have whole-sample MVs copies straight from the reference instead.
//   mainSmall  records with both dimensions <= 8, four per workgroup, one 2x4 block per lane filtered straight
//              from the reference (no workgroup memory, no barrier).
// Every PU starts on an even x and has an even width in every plane, so each thread owns whole u32 words of the
// destination: no atomics.
//
// Measured on an M4 Pro (Chrome 157, 4K 10-bit B picture of 16860 PUs, GPU timestamps, back to back): one
// workgroup per PU with the planes and lists one after another, i32 workgroup arrays and runtime filter loops took
// 5.5 ms; luma/chroma workgroups with vec4 filters 4.5 ms; u16-pair workgroup memory with loads issued before stores
// 2.5 ms; the small-PU entry point 1.97 ms. Tried and dropped: per-lane filtering of every PU straight from the
// reference (2.2 to 2.5 ms on the 5441-PU picture against 1.5), the two lists one after another to halve workgroup
// memory (1.73 against 1.55 ms: the extra barriers cost more than the occupancy gains). Doubling workgroup memory
// to 13 KB made it 1.7x slower, so the kernel is latency bound and its footprint matters. Current numbers:
// FORMAT-mc.md.

export const MC_WORKGROUP = 64;
export const MC_TILE_W = 32; // luma tile; chroma tiles are 16x16 per plane
export const MC_TILE_H = 16;

// Spec tables 8-12 / 8-13 (inter_pred.rs:22 QPEL_FILTER, :31 EPEL_FILTER).
const QPEL = [
  [0, 0, 0, 64, 0, 0, 0, 0],
  [-1, 4, -10, 58, 17, -5, 1, 0],
  [-1, 4, -11, 40, 40, -11, 4, -1],
  [0, 1, -5, 17, 58, -10, 4, -1],
];
const EPEL = [
  [0, 64, 0, 0],
  [-2, 58, 10, -2],
  [-4, 54, 16, -2],
  [-6, 46, 28, -4],
  [-4, 36, 36, -4],
  [-4, 28, 46, -6],
  [-2, 16, 54, -4],
  [-2, 10, 58, -2],
];

// Constant-tap filter expression, one WGSL function per phase (symmetric filters add their tap pairs first, as
// mc_simd.rs does). Exact in i32 for every input here. Measured as fast as vec4 dot products with runtime taps
// (within noise); kept because the phase switch sits outside the loops.
function tapExpr(coefs, terms) {
  const n = coefs.length;
  const sym = coefs.every((c, k) => c === coefs[n - 1 - k]);
  const parts = [];
  const term = (c, x) => (c === 1 ? x : c === -1 ? `(-${x})` : `${c} * ${x}`);
  if (sym) {
    for (let k = 0; k < n / 2; k++) if (coefs[k]) parts.push(term(coefs[k], `(${terms[k]} + ${terms[n - 1 - k]})`));
  } else {
    for (let k = 0; k < n; k++) if (coefs[k]) parts.push(term(coefs[k], terms[k]));
  }
  return parts.join(' + ');
}

const v8 = (a, b) => ['x', 'y', 'z', 'w'].map((c) => `${a}.${c}`).concat(['x', 'y', 'z', 'w'].map((c) => `${b}.${c}`));
const v4 = (a) => ['x', 'y', 'z', 'w'].map((c) => `${a}.${c}`);

function filterFns() {
  let s = '';
  for (let f = 1; f < 4; f++) s += `fn fq${f}(a: vec4<i32>, b: vec4<i32>) -> i32 { return ${tapExpr(QPEL[f], v8('a', 'b'))}; }\n`;
  for (let f = 1; f < 8; f++) s += `fn fe${f}(a: vec4<i32>) -> i32 { return ${tapExpr(EPEL[f], v4('a'))}; }\n`;
  return s;
}

// switch over a filter phase, one case per fractional phase, with `body(f)` as the case body.
const phaseSwitch = (v, n, body) =>
  `switch ${v} {\n${Array.from({ length: n - 1 }, (_, i) => `    case ${i + 1}u: {\n${body(i + 1)}\n    }`).join('\n')}\n    default: {}\n  }`;

/**
 * @param {{separateDst?: boolean}} opts
 *   separateDst false: binding 3 is the DPB arena (read_write) and the current picture is the slot at
 *   params.dstBase inside it. true: binding 3 is the arena (read only) and binding 4 the destination picture
 *   (read_write, params.dstBase usually 0).
 */
export function mcWgsl({ separateDst = false } = {}) {
  const refDecl = separateDst
    ? `@group(0) @binding(3) var<storage, read> pic: array<u32>;
@group(0) @binding(4) var<storage, read_write> dst: array<u32>;`
    : `@group(0) @binding(3) var<storage, read_write> pic: array<u32>;`;
  const D = separateDst ? 'dst' : 'pic';
  return /* wgsl */ `
struct Params {
  width: u32, height: u32, strideY: u32, strideC: u32,
  offCb: u32, offCr: u32, slotWords: u32, dstBase: u32,
  bitDepthY: u32, bitDepthC: u32, flags: u32, puCount: u32,
  groupsX: u32, puFirst: u32, smallFirst: u32, smallCount: u32,
  groupsXSmall: u32, pad0: u32, pad1: u32, pad2: u32,
};
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> pus: array<u32>;
@group(0) @binding(2) var<storage, read> wts: array<u32>;
${refDecl}

// Workgroup memory holds two 16-bit values per u32 (window samples, or i16 horizontal-pass results), even
// column in the low half: half the footprint and half the reads of an i32 layout. Window rows start at the even
// reference column ax = wx0 - o (o = wx0 & 1), so interior windows are plain word copies.
// Luma: window 20 words x 23 rows per list, horizontal pass 16 words (32 columns) x 23 rows per list.
// Chroma: window 10 words x 19 rows per plane and list, horizontal pass 8 words x 19 rows.
const WSW: u32 = 20u;
const WINL: u32 = 460u;
const TMPL: u32 = 368u;
const WSWC: u32 = 10u;
const WINC: u32 = 190u;
const TMPC: u32 = 152u;
var<workgroup> win: array<u32, 920>;  // max(2 * WINL, 4 * WINC)
var<workgroup> tmp: array<u32, 736>;  // max(2 * TMPL, 4 * TMPC)

${filterFns()}
fn sat16(v: i32) -> i32 { return clamp(v, -32768, 32767); }
fn lo16(w: u32) -> i32 { return i32(w << 16u) >> 16u; }
fn hi16(w: u32) -> i32 { return i32(w) >> 16u; }
fn lou(w: u32) -> i32 { return i32(w & 0xffffu); }
fn hiu(w: u32) -> i32 { return i32(w >> 16u); }
fn pack16(a: i32, b: i32) -> u32 { return (u32(a) & 0xffffu) | (u32(b) << 16u); }

// The i16 the decoder stores after the second pass of a 2-D filter (FORMAT-mc.md "Narrowing"): the simd128 / NEON
// row kernels saturate (mc_simd.rs narrow32), the scalar code (and the SIMD tail for rows under 4 lanes) wraps.
fn narrow(a: i32, sat: bool) -> i32 {
  if (sat) { return clamp(a, -32768, 32767); }
  return (a << 16u) >> 16u;
}

// One edge clamped reference sample (ref_sample, inter_pred.rs:148); rs = clamped row * stride.
fn refSample(base: u32, refW: i32, x: i32, rs: u32) -> u32 {
  let i = rs + u32(clamp(x, 0, refW - 1));
  return (pic[base + (i >> 1u)] >> ((i & 1u) << 4u)) & 0xffffu;
}

// Reference window of ww x wh samples from (wx0, wy0), stored from the even column wx0 - (wx0 & 1) into
// win[off + r * wsw + word]. Flat index over the words, row = index / nw by a reciprocal (exact for index < 2048,
// nw <= 24); all loads of a thread are issued before its stores (memory parallelism: 2x faster than a strided
// loop that stores after each load).
fn loadWin(base: u32, stride: u32, refW: i32, refH: i32, wx0: i32, wy0: i32, ww: u32, wh: u32, off: u32, wsw: u32,
           iters: u32, lid: u32) {
  let o = u32(wx0) & 1u;
  let ax = wx0 - i32(o);
  let nw = (ww + o + 1u) >> 1u;
  let total = nw * wh;
  let inv = (65536u + nw - 1u) / nw;
  let inside = wx0 >= 0 && wx0 + i32(ww) <= refW;
  var v: array<u32, 8>;
  var d: array<u32, 8>;
  for (var k = 0u; k < 8u; k++) {
    let i = lid + k * 64u;
    d[k] = 0xffffffffu;
    if (k < iters && i < total) {
      let r = (i * inv) >> 16u;
      let c = i - r * nw;
      let rs = u32(clamp(wy0 + i32(r), 0, refH - 1)) * stride;
      d[k] = off + r * wsw + c;
      if (inside) {
        // Whole words: the last one never passes the (even) picture width.
        v[k] = pic[base + ((rs + u32(ax)) >> 1u) + c];
      } else {
        let x = ax + 2 * i32(c);
        v[k] = refSample(base, refW, x, rs) | (refSample(base, refW, x + 1, rs) << 16u);
      }
    }
  }
  for (var k = 0u; k < 8u; k++) {
    if (d[k] != 0xffffffffu) { win[d[k]] = v[k]; }
  }
}

struct Lst {
  base: u32, xi: i32, yi: i32, xf: u32, yf: u32, o: u32,  // o: window start parity (see loadWin)
};

struct Cmb {
  bi: bool, useL0: bool, weighted: bool, bd: u32, maxv: i32,
  w0: i32, o0: i32, w1: i32, o1: i32, denom: u32,
};

// Final sample from the 14-bit intermediates a (L0) and b (L1), motion_compensation_pu (inter_pred.rs:1009).
fn combine(k: Cmb, a: i32, b: i32) -> i32 {
  var v: i32;
  if (k.bi) {
    if (k.weighted) {
      // Explicit weighted bi-prediction (motion_compensation_pu, bi closure).
      let log2wd = k.denom + 14u - k.bd;
      v = (a * k.w0 + b * k.w1 + ((k.o0 + k.o1 + 1) << log2wd)) >> (log2wd + 1u);
    } else {
      // bi_average_row (:609) with its saturating 16-bit adds; avg_block's integer shortcut gives the same value.
      v = sat16(sat16(a + b) + (1 << (14u - k.bd))) >> (15u - k.bd);
    }
  } else {
    let s = select(b, a, k.useL0);
    if (k.weighted) {
      // put_weighted (:196).
      let sh = 14u - k.bd + k.denom;
      v = ((s * select(k.w1, k.w0, k.useL0) + (1 << (sh - 1u))) >> sh) + select(k.o1, k.o0, k.useL0);
    } else {
      // PixelSink (:577); copy_block's integer shortcut gives the same value.
      let sh = 14u - k.bd;
      v = (s + (1 << (sh - 1u))) >> sh;
    }
  }
  return clamp(v, 0, k.maxv);
}

// Window samples at columns X, X + 1 of row r (any parity).
fn winPair(wo: u32, wsw: u32, r: u32, X: u32) -> vec2<i32> {
  let p = wo + r * wsw + (X >> 1u);
  let w0 = win[p];
  if ((X & 1u) == 0u) { return vec2<i32>(lou(w0), hiu(w0)); }
  return vec2<i32>(hiu(w0), lou(win[p + 1u]));
}

// Horizontal 8-tap pass of window wo into tmp at to, rows rA..rB, column pairs below tw / 2. A thread makes the
// pair (c, c + 1) from window samples c + o .. c + o + 8 (five words).
fn hLuma(wo: u32, to: u32, tw: u32, rA: u32, rB: u32, xf: u32, o: u32, shift1: u32, lid: u32) {
  let odd = o != 0u;
  ${phaseSwitch('xf', 4, (f) => `      for (var k = 0u; k < 6u; k++) {
        let i = lid + k * 64u;
        let pc = i & 15u;
        let r = rA + (i >> 4u);
        if (2u * pc < tw && r < rB) {
          let p = wo + r * WSW + pc;
          let w0 = win[p]; let w1 = win[p + 1u]; let w2 = win[p + 2u]; let w3 = win[p + 3u]; let w4 = win[p + 4u];
          let e = vec4<i32>(lou(w0), hiu(w0), lou(w1), hiu(w1));
          let g = vec4<i32>(lou(w2), hiu(w2), lou(w3), hiu(w3));
          let u0 = select(e, vec4<i32>(e.yzw, g.x), odd);
          let u1 = select(g, vec4<i32>(g.yzw, lou(w4)), odd);
          let u2 = select(lou(w4), hiu(w4), odd);
          let a0 = fq${f}(u0, u1);
          let a1 = fq${f}(vec4<i32>(u0.yzw, u1.x), vec4<i32>(u1.yzw, u2));
          tmp[to + r * 16u + pc] = pack16(a0 >> shift1, a1 >> shift1);
        }
      }`)}
}

fn hChroma(wo: u32, to: u32, tw: u32, rA: u32, rB: u32, xf: u32, o: u32, shift1: u32, lid: u32) {
  let odd = o != 0u;
  ${phaseSwitch('xf', 8, (f) => `      for (var k = 0u; k < 3u; k++) {
        let i = lid + k * 64u;
        let pc = i & 7u;
        let r = rA + (i >> 3u);
        if (2u * pc < tw && r < rB) {
          let p = wo + r * WSWC + pc;
          let w0 = win[p]; let w1 = win[p + 1u]; let w2 = win[p + 2u];
          let e = vec4<i32>(lou(w0), hiu(w0), lou(w1), hiu(w1));
          let u0 = select(e, vec4<i32>(e.yzw, lou(w2)), odd);
          let u1 = select(lou(w2), hiu(w2), odd);
          tmp[to + r * 8u + pc] = pack16(fe${f}(u0) >> shift1, fe${f}(vec4<i32>(u0.yzw, u1)) >> shift1);
        }
      }`)}
}

// Vertical filter over the column pairs t (rows 0..10): raw sums, out[jj * 2 + e].
fn vSumL(t: array<vec2<i32>, 11>, yf: u32) -> array<i32, 8> {
  var s: array<i32, 8>;
  ${phaseSwitch('yf', 4, (f) => `      for (var jj = 0u; jj < 4u; jj++) {
        let a = vec4<i32>(t[jj].x, t[jj + 1u].x, t[jj + 2u].x, t[jj + 3u].x);
        let b = vec4<i32>(t[jj + 4u].x, t[jj + 5u].x, t[jj + 6u].x, t[jj + 7u].x);
        let c = vec4<i32>(t[jj].y, t[jj + 1u].y, t[jj + 2u].y, t[jj + 3u].y);
        let d = vec4<i32>(t[jj + 4u].y, t[jj + 5u].y, t[jj + 6u].y, t[jj + 7u].y);
        s[jj * 2u] = fq${f}(a, b);
        s[jj * 2u + 1u] = fq${f}(c, d);
      }`)}
  return s;
}

fn vSumC(t: array<vec2<i32>, 7>, yf: u32) -> array<i32, 8> {
  var s: array<i32, 8>;
  ${phaseSwitch('yf', 8, (f) => `      for (var jj = 0u; jj < 4u; jj++) {
        s[jj * 2u] = fe${f}(vec4<i32>(t[jj].x, t[jj + 1u].x, t[jj + 2u].x, t[jj + 3u].x));
        s[jj * 2u + 1u] = fe${f}(vec4<i32>(t[jj].y, t[jj + 1u].y, t[jj + 2u].y, t[jj + 3u].y));
      }`)}
  return s;
}

// Second stage shared by every path: rows of a 2x4 block from the per-row values t (window samples when xf is 0,
// first-pass results otherwise), with the row margin m (3 luma, 1 chroma).
fn finishL(t: array<vec2<i32>, 11>, xf: u32, yf: u32, bd: u32, sat: bool) -> array<i32, 8> {
  var out: array<i32, 8>;
  if (yf == 0u) {
    for (var jj = 0u; jj < 4u; jj++) {
      var v = t[jj + 3u];
      if (xf == 0u) { v = v << vec2<u32>(14u - bd); }
      out[jj * 2u] = v.x;
      out[jj * 2u + 1u] = v.y;
    }
    return out;
  }
  let s = vSumL(t, yf);
  for (var k = 0u; k < 8u; k++) {
    if (xf == 0u) { out[k] = s[k] >> (bd - 8u); } else { out[k] = narrow(s[k] >> 6u, sat); }
  }
  return out;
}

fn finishC(t: array<vec2<i32>, 7>, xf: u32, yf: u32, bd: u32, sat: bool) -> array<i32, 8> {
  var out: array<i32, 8>;
  if (yf == 0u) {
    for (var jj = 0u; jj < 4u; jj++) {
      var v = t[jj + 1u];
      if (xf == 0u) { v = v << vec2<u32>(14u - bd); }
      out[jj * 2u] = v.x;
      out[jj * 2u + 1u] = v.y;
    }
    return out;
  }
  let s = vSumC(t, yf);
  for (var k = 0u; k < 8u; k++) {
    if (xf == 0u) { out[k] = s[k] >> (bd - 8u); } else { out[k] = narrow(s[k] >> 6u, sat); }
  }
  return out;
}

// 14-bit intermediates of the 2x4 block at tile column c (even), rows j0..j0+3, from workgroup memory.
fn vLuma(L: Lst, wo: u32, to: u32, c: u32, j0: u32, bd: u32, sat: bool) -> array<i32, 8> {
  var t: array<vec2<i32>, 11>;
  let X = c + 3u + L.o;
  let rA = select(3u, 0u, L.yf != 0u);
  let rB = select(7u, 11u, L.yf != 0u);
  // Constant bounds so the loop unrolls and t stays in registers (runtime bounds put it in memory: 0.85 vs 0.66 ms
  // on the static picture).
  for (var k = 0u; k < 11u; k++) {
    if (k < rA || k >= rB) { continue; }
    if (L.xf == 0u) {
      t[k] = winPair(wo, WSW, j0 + k, X);
    } else {
      let w = tmp[to + (j0 + k) * 16u + (c >> 1u)];
      t[k] = vec2<i32>(lo16(w), hi16(w));
    }
  }
  return finishL(t, L.xf, L.yf, bd, sat);
}

fn vChroma(L: Lst, wo: u32, to: u32, c: u32, j0: u32, bd: u32, sat: bool) -> array<i32, 8> {
  var t: array<vec2<i32>, 7>;
  let X = c + 1u + L.o;
  let rA = select(1u, 0u, L.yf != 0u);
  let rB = select(5u, 7u, L.yf != 0u);
  for (var k = 0u; k < 7u; k++) {
    if (k < rA || k >= rB) { continue; }
    if (L.xf == 0u) {
      t[k] = winPair(wo, WSWC, j0 + k, X);
    } else {
      let w = tmp[to + (j0 + k) * 8u + (c >> 1u)];
      t[k] = vec2<i32>(lo16(w), hi16(w));
    }
  }
  return finishC(t, L.xf, L.yf, bd, sat);
}

@compute @workgroup_size(${MC_WORKGROUP})
fn main(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
  let gi = wid.x + wid.y * P.groupsX;
  let pi = gi >> 1u;
  if (pi >= P.puCount) { return; }
  let chroma = (gi & 1u) != 0u;
  let rb = (P.puFirst + pi) * 6u;
  let r0 = pus[rb];
  let r1 = pus[rb + 1u];
  let r2 = pus[rb + 2u];
  let flags = (r1 >> 16u) & 0xffu;
  let useL = vec2<bool>((flags & 1u) != 0u, (flags & 2u) != 0u);
  var slot = array<u32, 2>(r2 & 0xffffu, r2 >> 16u);
  var mvw = array<u32, 2>(pus[rb + 3u], pus[rb + 4u]);
  var wti = array<u32, 2>(pus[rb + 5u] & 0xffffu, pus[rb + 5u] >> 16u);
  let cs = select(0u, 1u, chroma);           // 4:2:0 subsampling shift of this part's planes
  let pw = (r1 & 0xffu) >> cs;
  let ph = ((r1 >> 8u) & 0xffu) >> cs;
  let px = (r0 & 0xffffu) >> cs;
  let py = (r0 >> 16u) >> cs;
  let refW = i32(P.width >> cs);
  let refH = i32(P.height >> cs);
  let stride = select(P.strideY, P.strideC, chroma);
  let bd = select(P.bitDepthY, P.bitDepthC, chroma);
  // The SIMD row kernels saturate from 4 lanes up (mc_simd.rs row()); narrower rows run the scalar tail.
  let sat = (P.flags & 1u) != 0u && (P.flags & 2u) == 0u && pw >= 4u;
  let shift1 = bd - 8u;
  let dn = wts[wti[select(1u, 0u, useL.x)] * 4u + 3u];
  // MVs in this plane's units: chroma_mv (inter_pred.rs:141) is (mv * 2) >> 1 in 4:2:0, the luma MV read in
  // eighth chroma samples.
  let mvx = vec2<i32>(lo16(mvw[0]), lo16(mvw[1]));
  let mvy = vec2<i32>(hi16(mvw[0]), hi16(mvw[1]));

  // Combine parameters per plane (luma part: index 0 only; chroma part: Cb, Cr).
  var K: array<Cmb, 2>;
  for (var q = 0u; q < 2u; q++) {
    let c = select(0u, q + 1u, chroma);
    let we0 = wts[wti[0] * 4u + c];
    let we1 = wts[wti[1] * 4u + c];
    K[q] = Cmb(useL.x && useL.y, useL.x, (flags & 4u) != 0u, bd, (1 << bd) - 1,
               lo16(we0), hi16(we0), lo16(we1), hi16(we1), select(dn & 0xffu, (dn >> 8u) & 0xffu, chroma));
  }

  // Integer MVs on both axes for every list of this part (zero motion, static areas): no filter, so each thread
  // copies sample pairs straight from the reference, no workgroup memory and no barrier (the static picture:
  // 0.82 ms through the tiles, 0.49 ms here).
  let fm = select(3, 7, chroma);
  let intOnly = (!useL.x || ((mvx[0] | mvy[0]) & fm) == 0) && (!useL.y || ((mvx[1] | mvy[1]) & fm) == 0);
  if (intOnly) {
    let fs = select(2u, 3u, chroma);
    let hp = pw >> 1u;
    let n = hp * ph;
    let inv = (65536u + hp - 1u) / hp; // exact row = i / hp for i < 2048, hp <= 32
    let planes = select(1u, 2u, chroma);
    for (var q = 0u; q < planes; q++) {
      let planeOff = select(0u, select(P.offCb, P.offCr, q == 1u), chroma);
      for (var i = lid; i < n; i += 64u) {
        let r = (i * inv) >> 16u;
        let c = (i - r * hp) * 2u;
        var pr: array<i32, 4>;
        for (var l = 0u; l < 2u; l++) {
          if (!useL[l]) { continue; }
          let base = slot[l] * P.slotWords + planeOff;
          let x = i32(px + c) + (mvx[l] >> fs);
          let rs = u32(clamp(i32(py + r) + (mvy[l] >> fs), 0, refH - 1)) * stride;
          var v: vec2<i32>;
          if (x >= 0 && x + 2 <= refW) {
            let p = base + ((rs + u32(x)) >> 1u);
            let w0 = pic[p];
            if ((x & 1) == 0) { v = vec2<i32>(lou(w0), hiu(w0)); } else { v = vec2<i32>(hiu(w0), lou(pic[p + 1u])); }
          } else {
            v = vec2<i32>(i32(refSample(base, refW, x, rs)), i32(refSample(base, refW, x + 1, rs)));
          }
          v = v << vec2<u32>(14u - bd);
          pr[l * 2u] = v.x;
          pr[l * 2u + 1u] = v.y;
        }
        let v0 = combine(K[q], pr[0], pr[2]);
        let v1 = combine(K[q], pr[1], pr[3]);
        ${D}[P.dstBase + planeOff + (((py + r) * stride + px + c) >> 1u)] = u32(v0) | (u32(v1) << 16u);
      }
    }
    return;
  }

  if (!chroma) {
    for (var ty = 0u; ty < ph; ty += 16u) {
      for (var tx = 0u; tx < pw; tx += 32u) {
        let tw = min(32u, pw - tx);
        let th = min(16u, ph - ty);
        var Ls: array<Lst, 2>;
        workgroupBarrier(); // the previous tile is done with win and tmp
        for (var l = 0u; l < 2u; l++) {
          if (!useL[l]) { continue; }
          let xi = i32(px + tx) + (mvx[l] >> 2u);
          Ls[l] = Lst(slot[l] * P.slotWords, xi, i32(py + ty) + (mvy[l] >> 2u), u32(mvx[l] & 3), u32(mvy[l] & 3),
                      u32(xi - 3) & 1u);
          loadWin(Ls[l].base, stride, refW, refH, xi - 3, Ls[l].yi - 3, tw + 7u, th + 7u, l * WINL, WSW, 8u, lid);
        }
        workgroupBarrier();
        for (var l = 0u; l < 2u; l++) {
          if (useL[l] && Ls[l].xf != 0u) {
            // Only the rows the vertical pass reads: all of them for a vertical phase, else the tile rows.
            let v = Ls[l].yf != 0u;
            hLuma(l * WINL, l * TMPL, tw, select(3u, 0u, v), select(th + 3u, th + 7u, v), Ls[l].xf, Ls[l].o, shift1, lid);
          }
        }
        workgroupBarrier();
        let c = (lid & 15u) * 2u;
        let j0 = (lid >> 4u) * 4u;
        if (c < tw && j0 < th) {
          var a: array<i32, 8>;
          var b: array<i32, 8>;
          if (useL.x) { a = vLuma(Ls[0], 0u, 0u, c, j0, bd, sat); }
          if (useL.y) { b = vLuma(Ls[1], WINL, TMPL, c, j0, bd, sat); }
          for (var jj = 0u; jj < 4u; jj++) {
            if (j0 + jj < th) {
              let v0 = combine(K[0], a[jj * 2u], b[jj * 2u]);
              let v1 = combine(K[0], a[jj * 2u + 1u], b[jj * 2u + 1u]);
              ${D}[P.dstBase + (((py + ty + j0 + jj) * stride + px + tx + c) >> 1u)] = u32(v0) | (u32(v1) << 16u);
            }
          }
        }
      }
    }
  } else {
    let planeOff = vec2<u32>(P.offCb, P.offCr);
    for (var ty = 0u; ty < ph; ty += 16u) {
      for (var tx = 0u; tx < pw; tx += 16u) {
        let tw = min(16u, pw - tx);
        let th = min(16u, ph - ty);
        var Ls: array<Lst, 2>;
        workgroupBarrier();
        for (var l = 0u; l < 2u; l++) {
          if (!useL[l]) { continue; }
          let xi = i32(px + tx) + (mvx[l] >> 3u);
          Ls[l] = Lst(slot[l] * P.slotWords, xi, i32(py + ty) + (mvy[l] >> 3u), u32(mvx[l] & 7), u32(mvy[l] & 7),
                      u32(xi - 1) & 1u);
          for (var q = 0u; q < 2u; q++) {
            loadWin(Ls[l].base + planeOff[q], stride, refW, refH, xi - 1, Ls[l].yi - 1, tw + 3u, th + 3u,
                    (q * 2u + l) * WINC, WSWC, 3u, lid);
          }
        }
        workgroupBarrier();
        for (var w = 0u; w < 4u; w++) {
          let l = w & 1u;
          if (useL[l] && Ls[l].xf != 0u) {
            let v = Ls[l].yf != 0u;
            hChroma(w * WINC, w * TMPC, tw, select(1u, 0u, v), select(th + 1u, th + 3u, v), Ls[l].xf, Ls[l].o, shift1, lid);
          }
        }
        workgroupBarrier();
        // Thread halves: lid >> 5 is the plane (Cb, Cr).
        let q = lid >> 5u;
        let t = lid & 31u;
        let c = (t & 7u) * 2u;
        let j0 = (t >> 3u) * 4u;
        if (c < tw && j0 < th) {
          var a: array<i32, 8>;
          var b: array<i32, 8>;
          if (useL.x) { a = vChroma(Ls[0], (q * 2u) * WINC, (q * 2u) * TMPC, c, j0, bd, sat); }
          if (useL.y) { b = vChroma(Ls[1], (q * 2u + 1u) * WINC, (q * 2u + 1u) * TMPC, c, j0, bd, sat); }
          for (var jj = 0u; jj < 4u; jj++) {
            if (j0 + jj < th) {
              let v0 = combine(K[q], a[jj * 2u], b[jj * 2u]);
              let v1 = combine(K[q], a[jj * 2u + 1u], b[jj * 2u + 1u]);
              ${D}[P.dstBase + planeOff[q] + (((py + ty + j0 + jj) * stride + px + tx + c) >> 1u)] = u32(v0) | (u32(v1) << 16u);
            }
          }
        }
      }
    }
  }
}

// ---- Small PUs (both dimensions <= 8: 8x8, 8x4, 4x8). A workgroup per PU would leave most lanes idle (an 8x8 PU
// is 8 luma and 4 chroma 2x4 blocks), so four PUs share a workgroup, 16 lanes each: lanes 0..7 luma blocks, 8..11
// chroma (Cb, Cr), and each lane filters its block straight from the reference. Measured on the 83444-PU dense
// picture: 8.3 ms through main, 4.5 ms with this entry point.

// n words of the reference row rs from the even column ax, as sample pairs (edge clamped unless inside).
fn refWords(base: u32, rs: u32, refW: i32, ax: i32, inside: bool, n: u32) -> array<vec2<i32>, 5> {
  var s: array<vec2<i32>, 5>;
  if (inside) {
    let p = base + ((rs + u32(ax)) >> 1u);
    for (var k = 0u; k < n; k++) { let w = pic[p + k]; s[k] = vec2<i32>(lou(w), hiu(w)); }
  } else {
    for (var k = 0u; k < n; k++) {
      let x = ax + 2 * i32(k);
      s[k] = vec2<i32>(i32(refSample(base, refW, x, rs)), i32(refSample(base, refW, x + 1, rs)));
    }
  }
  return s;
}

// 14-bit intermediates of the luma 2x4 block whose top-left reference position is (xi, yi).
fn blockLuma(base: u32, stride: u32, refW: i32, refH: i32, xi: i32, yi: i32, xf: u32, yf: u32, bd: u32, sat: bool) -> array<i32, 8> {
  let wx0 = xi - 3;
  let o = u32(wx0) & 1u;
  let odd = o != 0u;
  let ax = wx0 - i32(o);
  let inside = wx0 >= 0 && wx0 + 9 <= refW;
  let shift1 = bd - 8u;
  var t: array<vec2<i32>, 11>;
  let rA = select(3u, 0u, yf != 0u);
  let rB = select(7u, 11u, yf != 0u);
  for (var r = 0u; r < 11u; r++) {
    if (r < rA || r >= rB) { continue; }
    let rs = u32(clamp(yi - 3 + i32(r), 0, refH - 1)) * stride;
    let s = refWords(base, rs, refW, ax, inside, 5u);
    let e = vec4<i32>(s[0], s[1]);
    let g = vec4<i32>(s[2], s[3]);
    let u0 = select(e, vec4<i32>(e.yzw, g.x), odd);
    let u1 = select(g, vec4<i32>(g.yzw, s[4].x), odd);
    let u2 = select(s[4].x, s[4].y, odd);
    var h = vec2<i32>(u0.w, u1.x);
    ${phaseSwitch('xf', 4, (f) => `        h = vec2<i32>(fq${f}(u0, u1), fq${f}(vec4<i32>(u0.yzw, u1.x), vec4<i32>(u1.yzw, u2))) >> vec2<u32>(shift1);`)}
    t[r] = h;
  }
  return finishL(t, xf, yf, bd, sat);
}

fn blockChroma(base: u32, stride: u32, refW: i32, refH: i32, xi: i32, yi: i32, xf: u32, yf: u32, bd: u32, sat: bool) -> array<i32, 8> {
  let wx0 = xi - 1;
  let o = u32(wx0) & 1u;
  let odd = o != 0u;
  let ax = wx0 - i32(o);
  let inside = wx0 >= 0 && wx0 + 5 <= refW;
  let shift1 = bd - 8u;
  var t: array<vec2<i32>, 7>;
  let rA = select(1u, 0u, yf != 0u);
  let rB = select(5u, 7u, yf != 0u);
  for (var r = 0u; r < 7u; r++) {
    if (r < rA || r >= rB) { continue; }
    let rs = u32(clamp(yi - 1 + i32(r), 0, refH - 1)) * stride;
    let s = refWords(base, rs, refW, ax, inside, 3u);
    let e = vec4<i32>(s[0], s[1]);
    let u0 = select(e, vec4<i32>(e.yzw, s[2].x), odd);
    let u1 = select(s[2].x, s[2].y, odd);
    var h = vec2<i32>(u0.y, u0.z);
    ${phaseSwitch('xf', 8, (f) => `        h = vec2<i32>(fe${f}(u0), fe${f}(vec4<i32>(u0.yzw, u1))) >> vec2<u32>(shift1);`)}
    t[r] = h;
  }
  return finishC(t, xf, yf, bd, sat);
}

@compute @workgroup_size(${MC_WORKGROUP})
fn mainSmall(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
  let pi = (wid.x + wid.y * P.groupsXSmall) * 4u + (lid >> 4u);
  let s = lid & 15u;
  if (pi >= P.smallCount || s >= 12u) { return; }
  let rb = (P.smallFirst + pi) * 6u;
  let r0 = pus[rb];
  let r1 = pus[rb + 1u];
  let r2 = pus[rb + 2u];
  let flags = (r1 >> 16u) & 0xffu;
  let useL = vec2<bool>((flags & 1u) != 0u, (flags & 2u) != 0u);
  var slot = array<u32, 2>(r2 & 0xffffu, r2 >> 16u);
  var mvw = array<u32, 2>(pus[rb + 3u], pus[rb + 4u]);
  var wti = array<u32, 2>(pus[rb + 5u] & 0xffffu, pus[rb + 5u] >> 16u);
  let chroma = s >= 8u;
  let q = (s >> 1u) & 1u;                       // chroma plane (Cb, Cr) for lanes 8..11
  let cs = select(0u, 1u, chroma);
  let pw = (r1 & 0xffu) >> cs;
  let ph = ((r1 >> 8u) & 0xffu) >> cs;
  let c = select((s & 3u) * 2u, (s & 1u) * 2u, chroma);
  let j0 = select((s >> 2u) * 4u, 0u, chroma);
  if (c >= pw || j0 >= ph) { return; }
  let px = ((r0 & 0xffffu) >> cs) + c;
  let py = ((r0 >> 16u) >> cs) + j0;
  let refW = i32(P.width >> cs);
  let refH = i32(P.height >> cs);
  let stride = select(P.strideY, P.strideC, chroma);
  let bd = select(P.bitDepthY, P.bitDepthC, chroma);
  let sat = (P.flags & 1u) != 0u && (P.flags & 2u) == 0u && pw >= 4u;
  let planeOff = select(0u, select(P.offCb, P.offCr, q == 1u), chroma);
  let wc = select(0u, q + 1u, chroma);
  let dn = wts[wti[select(1u, 0u, useL.x)] * 4u + 3u];
  let we0 = wts[wti[0] * 4u + wc];
  let we1 = wts[wti[1] * 4u + wc];
  let K = Cmb(useL.x && useL.y, useL.x, (flags & 4u) != 0u, bd, (1 << bd) - 1,
              lo16(we0), hi16(we0), lo16(we1), hi16(we1), select(dn & 0xffu, (dn >> 8u) & 0xffu, chroma));
  var pr: array<array<i32, 8>, 2>;
  for (var l = 0u; l < 2u; l++) {
    if (!useL[l]) { continue; }
    let mx = lo16(mvw[l]);
    let my = hi16(mvw[l]);
    let base = slot[l] * P.slotWords + planeOff;
    if (chroma) {
      pr[l] = blockChroma(base, stride, refW, refH, i32(px) + (mx >> 3u), i32(py) + (my >> 3u), u32(mx & 7), u32(my & 7), bd, sat);
    } else {
      pr[l] = blockLuma(base, stride, refW, refH, i32(px) + (mx >> 2u), i32(py) + (my >> 2u), u32(mx & 3), u32(my & 3), bd, sat);
    }
  }
  for (var jj = 0u; jj < 4u; jj++) {
    if (j0 + jj < ph) {
      let v0 = combine(K, pr[0][jj * 2u], pr[1][jj * 2u]);
      let v1 = combine(K, pr[0][jj * 2u + 1u], pr[1][jj * 2u + 1u]);
      ${D}[P.dstBase + planeOff + (((py + jj) * stride + px) >> 1u)] = u32(v0) | (u32(v1) << 16u);
    }
  }
}
`;
}

export const MC_WGSL = mcWgsl();
export default MC_WGSL;
