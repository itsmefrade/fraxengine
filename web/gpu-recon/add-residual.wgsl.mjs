// Inter add-residual and PCM write (FORMAT-STREAM.md order: motion compensation, PCM, inter residuals, intra waves).
//
// addResidual: one workgroup per TU record (FORMAT-idct.md, 6 words), only records with word 5 = 0xffffffff (no intra
// owner; intra TUs add their residual inside the intra stage). sample = clip(pred + res, 0, (1 << bitDepth) - 1), the
// decoder's add_residual on the already clipped motion compensated prediction (rust_h265 cu_tree.rs, recon.rs
// Replayer `add_tu`). The bit depth is the TU's own (record word 1 bits 8..10).
//
// pcmMain: one workgroup per PCM record (FORMAT-STREAM.md section 5: `x | y << 16`, log2Size, sample offset);
// samples (section 6, two u16 per word, low half first) are n * n luma, then (n / 2)^2 Cb and Cr, already at the
// picture bit depth; written clamped as the replay's `P::from_i32_clamped` does.
//
// Writes: a TU (and a PCM CU) starts at an x multiple of 4 in every plane and is at least 4 wide, so an invocation
// owns whole u32 words; no atomics.
export const ADD_RESIDUAL_WG = 64;

export const addResidualWgsl = /* wgsl */ `
struct Params {
  strideY : u32, strideC : u32, offCb : u32, offCr : u32,
  tuCount : u32, groupsX : u32, pcmCount : u32, pcmGroupsX : u32,
  bitDepthY : u32, bitDepthC : u32, pad0 : u32, pad1 : u32,
  pad2 : u32, pad3 : u32, pad4 : u32, pad5 : u32,
};

@group(0) @binding(0) var<uniform> P : Params;
@group(0) @binding(1) var<storage, read_write> pic : array<u32>;
@group(0) @binding(2) var<storage, read> tus : array<u32>;
@group(0) @binding(3) var<storage, read> res : array<i32>;
@group(0) @binding(4) var<storage, read> pcm : array<u32>;
@group(0) @binding(5) var<storage, read> pcmS : array<u32>;

const WG : u32 = ${ADD_RESIDUAL_WG}u;
const NONE : u32 = 0xffffffffu;

fn planeBase(c : u32) -> u32 {
  if (c == 0u) { return 0u; }
  if (c == 1u) { return P.offCb; }
  return P.offCr;
}

@compute @workgroup_size(WG)
fn addResidual(@builtin(workgroup_id) wid : vec3<u32>, @builtin(local_invocation_index) li : u32) {
  let t = wid.y * P.groupsX + wid.x;
  if (t >= P.tuCount) { return; }
  let r = t * 6u;
  if (tus[r + 5u] != NONE) { return; }
  let xy = tus[r];
  let w1 = tus[r + 1u];
  let log2 = (w1 & 3u) + 2u;
  let c = (w1 >> 2u) & 3u;
  let maxv = i32((1u << (((w1 >> 8u) & 7u) + 8u)) - 1u);
  let n = 1u << log2;
  let stride = select(P.strideC, P.strideY, c == 0u);
  let base = planeBase(c);
  let x0 = xy & 0xffffu;
  let y0 = xy >> 16u;
  let resOff = tus[r + 4u];
  let pairs = (n * n) >> 1u;
  for (var i = li; i < pairs; i += WG) {
    let j = i << 1u;
    let ry = j >> log2;
    let rx = j & (n - 1u);
    let w = base + (((y0 + ry) * stride + x0 + rx) >> 1u);
    let v = pic[w];
    let a = clamp(i32(v & 0xffffu) + res[resOff + j], 0, maxv);
    let b = clamp(i32(v >> 16u) + res[resOff + j + 1u], 0, maxv);
    pic[w] = u32(a) | (u32(b) << 16u);
  }
}

fn pcmSample(k : u32) -> u32 {
  return (pcmS[k >> 1u] >> ((k & 1u) * 16u)) & 0xffffu;
}

@compute @workgroup_size(WG)
fn pcmMain(@builtin(workgroup_id) wid : vec3<u32>, @builtin(local_invocation_index) li : u32) {
  let b = wid.y * P.pcmGroupsX + wid.x;
  if (b >= P.pcmCount) { return; }
  let xy = pcm[b * 3u];
  let log2 = pcm[b * 3u + 1u];
  var k = pcm[b * 3u + 2u];
  for (var c = 0u; c < 3u; c++) {
    let lg = select(log2 - 1u, log2, c == 0u);
    let n = 1u << lg;
    let sh = select(1u, 0u, c == 0u);
    let x0 = (xy & 0xffffu) >> sh;
    let y0 = (xy >> 16u) >> sh;
    let stride = select(P.strideC, P.strideY, c == 0u);
    let maxv = (1u << select(P.bitDepthC, P.bitDepthY, c == 0u)) - 1u;
    let base = planeBase(c);
    let pairs = (n * n) >> 1u;
    for (var i = li; i < pairs; i += WG) {
      let j = i << 1u;
      let ry = j >> lg;
      let rx = j & (n - 1u);
      let a = min(pcmSample(k + j), maxv);
      let d = min(pcmSample(k + j + 1u), maxv);
      pic[base + (((y0 + ry) * stride + x0 + rx) >> 1u)] = a | (d << 16u);
    }
    k += n * n;
  }
}
`;
