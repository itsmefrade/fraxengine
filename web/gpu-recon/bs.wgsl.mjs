// Deblocking boundary strengths (spec 8.7.2.4) from the stream's edge codes, in WGSL. Layout: FORMAT-deblock.md
// "Edge codes". Bit exact with rust_h265: mirrors recon.rs `bs_cells`, `edge_bs`, `motion_bs` and
// `resolve_boundary_strengths`, which reproduce cu_tree.rs `compute_deblocking_boundary_strengths` and
// `inter_boundary_strength_full` (reference pictures compared by DPB slot instead of POC; the recorder marks a picture
// unsupported when two slots it references share a POC).
//
// Three entry points, dispatched in this order in one compute pass, after the host cleared the cell buffer to 0
// (skipped, with noCells = 1, when the picture has no PU: every cell is then intra):
//   bsPu   one invocation per PU record: cells it covers = PU index + 1
//   bsTu   one invocation per TU record: luma TUs set bit 31 (coded residual, cbf_luma) of the cells they cover
//   bsMain one invocation per 8x8 block map word: edge codes in bits 0..7 replaced by boundary strengths, in place
// PUs never overlap and TUs of one plane never overlap, so no two invocations of a dispatch write one cell.

export const BS_WG = 64;

export const BS_WGSL = /* wgsl */ `
struct BsParams {
  width: u32, height: u32, cellStride: u32, mapStride: u32,
  nPu: u32, nTu: u32, nBlk: u32, noCells: u32,
}

@group(0) @binding(0) var<uniform> P: BsParams;
@group(0) @binding(1) var<storage, read_write> cells: array<u32>;
@group(0) @binding(2) var<storage, read> pus: array<u32>;
@group(0) @binding(3) var<storage, read> tus: array<u32>;
@group(0) @binding(4) var<storage, read_write> blk: array<u32>;

fn flatIndex(wg: vec3<u32>, nwg: vec3<u32>, lid: u32) -> u32 {
  return (wg.y * nwg.x + wg.x) * ${BS_WG}u + lid;
}

@compute @workgroup_size(${BS_WG})
fn bsPu(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
  let i = flatIndex(wg, nwg, lid);
  if (i >= P.nPu) { return; }
  let w0 = pus[i * 6u];
  let w1 = pus[i * 6u + 1u];
  let x0 = (w0 & 0xffffu) >> 2u;
  let y0 = w0 >> 18u;
  let x1 = min(x0 + ((w1 & 0xffu) >> 2u), P.cellStride);
  let y1 = min(y0 + (((w1 >> 8u) & 0xffu) >> 2u), P.height >> 2u);
  for (var y = y0; y < y1; y++) {
    for (var x = x0; x < x1; x++) {
      cells[y * P.cellStride + x] = i + 1u;
    }
  }
}

@compute @workgroup_size(${BS_WG})
fn bsTu(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
  let i = flatIndex(wg, nwg, lid);
  if (i >= P.nTu) { return; }
  let w1 = tus[i * 6u + 1u];
  if (((w1 >> 2u) & 3u) != 0u) { return; }
  let w0 = tus[i * 6u];
  let n = 1u << (w1 & 3u);
  let x0 = (w0 & 0xffffu) >> 2u;
  let y0 = w0 >> 18u;
  let x1 = min(x0 + n, P.cellStride);
  let y1 = min(y0 + n, P.height >> 2u);
  for (var y = y0; y < y1; y++) {
    for (var x = x0; x < x1; x++) {
      let c = y * P.cellStride + x;
      cells[c] = cells[c] | 0x80000000u;
    }
  }
}

fn lo16(w: u32) -> i32 { return bitcast<i32>(w << 16u) >> 16u; }
fn hi16(w: u32) -> i32 { return bitcast<i32>(w) >> 16u; }
// Two quarter sample MVs (packed i16 pairs) 4 or more apart in a component (i32 differences).
fn far(a: u32, b: u32) -> bool {
  return abs(lo16(a) - lo16(b)) >= 4 || abs(hi16(a) - hi16(b)) >= 4;
}

// motion_bs: p, q are PU record indices (P side, Q side).
fn motionBs(p: u32, q: u32) -> u32 {
  let pf = (pus[p * 6u + 1u] >> 16u) & 3u;
  let qf = (pus[q * 6u + 1u] >> 16u) & 3u;
  let ps = pus[p * 6u + 2u];
  let qs = pus[q * 6u + 2u];
  let pm0 = pus[p * 6u + 3u];
  let pm1 = pus[p * 6u + 4u];
  let qm0 = pus[q * 6u + 3u];
  let qm1 = pus[q * 6u + 4u];
  var b = true;
  if (pf == 3u && qf == 3u) {
    let p0 = ps & 0xffffu; let p1 = ps >> 16u;
    let q0 = qs & 0xffffu; let q1 = qs >> 16u;
    if (q0 == p0 && q0 == q1 && p0 == p1) {
      b = (far(pm0, qm0) || far(pm1, qm1)) && (far(pm1, qm0) || far(pm0, qm1));
    } else if (q0 == p0 && q1 == p1) {
      b = far(pm0, qm0) || far(pm1, qm1);
    } else if (q1 == p0 && q0 == p1) {
      b = far(pm1, qm0) || far(pm0, qm1);
    }
  } else if (pf != 3u && qf != 3u) {
    let pl0 = (pf & 1u) != 0u;
    let ql0 = (qf & 1u) != 0u;
    let pSlot = select(ps >> 16u, ps & 0xffffu, pl0);
    let qSlot = select(qs >> 16u, qs & 0xffffu, ql0);
    let pm = select(pm1, pm0, pl0);
    let qm = select(qm1, qm0, ql0);
    b = pSlot != qSlot || far(pm, qm);
  }
  return select(0u, 1u, b);
}

// edge_bs: code (FORMAT-deblock.md), cells on the P and Q side.
fn edgeBs(code: u32, cp: u32, cq: u32) -> u32 {
  if (code == 0u) { return 0u; }
  if (code == 3u) { return 2u; }
  let ip = cp & 0x7fffffffu;
  let iq = cq & 0x7fffffffu;
  if (ip == 0u || iq == 0u) { return 2u; }
  if (code == 1u && ((cp | cq) >> 31u) != 0u) { return 1u; }
  if (ip == iq) { return 0u; }
  return motionBs(ip - 1u, iq - 1u);
}

fn cellAt(x: u32, y: u32) -> u32 {
  if (P.noCells != 0u) { return 0u; }
  return cells[y * P.cellStride + x];
}

@compute @workgroup_size(${BS_WG})
fn bsMain(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
  let i = flatIndex(wg, nwg, lid);
  if (i >= P.nBlk) { return; }
  let m = blk[i];
  if ((m & 0xffu) == 0u) { return; }
  let bx = i % P.mapStride;
  let by = i / P.mapStride;
  var out = m & 0xffffff00u;
  for (var k = 0u; k < 4u; k++) {
    let code = (m >> (2u * k)) & 3u;
    if (code == 0u) { continue; }
    // Segments 0, 1: vertical edge at x, rows +0 / +4; 2, 3: horizontal edge at y, columns +0 / +4.
    var qx = 2u * bx;
    var qy = 2u * by + k;
    if (k >= 2u) { qx = 2u * bx + k - 2u; qy = 2u * by; }
    if ((k < 2u && qx == 0u) || (k >= 2u && qy == 0u)) { continue; }
    var px = qx - 1u;
    var py = qy;
    if (k >= 2u) { px = qx; py = qy - 1u; }
    out |= edgeBs(code, cellAt(px, py), cellAt(qx, qy)) << (2u * k);
  }
  blk[i] = out;
}
`;
