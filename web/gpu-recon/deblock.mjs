// Driver for the WGSL deblocking kernel (deblock.wgsl.mjs). Input layout: FORMAT-deblock.md.
//
//   const db = createDeblock(device);
//   const bg = db.bindGroup({ params, picture, map, ctb });   // GPUBuffers
//   db.encode(pass, bg, width, height);                        // inside a compute pass: vertical, then horizontal
//
// Both dispatches go into the caller's compute pass: WebGPU makes the storage writes of one dispatch visible to
// the next, which is the only ordering deblocking needs (every vertical edge before any horizontal edge).

import { DEBLOCK_WGSL, deblockThreads } from './deblock.wgsl.mjs';

export const WORKGROUP = 64;
const MAX_GROUPS = 65535;

/// The 16-word uniform block shared by deblocking and SAO (FORMAT-deblock.md, "Params").
export function makeParams({ width, height, bitDepthY, bitDepthC, log2Ctb, cbQpOffset = 0, crQpOffset = 0, anyNoFilter = false }) {
  const align64 = (v) => (v + 63) & ~63;
  const strideY = align64(width);
  const strideC = align64(width >> 1);
  const offCb = (strideY * height) / 2;
  const offCr = offCb + (strideC * (height >> 1)) / 2;
  const ctb = 1 << log2Ctb;
  const p = new Uint32Array(16);
  p.set([width, height, strideY, strideC, offCb, offCr, bitDepthY, bitDepthC, log2Ctb, Math.ceil(width / ctb)]);
  const i = new Int32Array(p.buffer);
  i[10] = cbQpOffset;
  i[11] = crQpOffset;
  p[12] = anyNoFilter ? 1 : 0;
  p[13] = Math.ceil(height / ctb);
  p[14] = width >> 3;
  return p;
}

/// Size in u32 words of a picture buffer with these params.
export function pictureWords(p) {
  return p[5] + (p[3] * (p[1] >> 1)) / 2;
}

/// Workgroup counts for n invocations (2D past the 65535 per-dimension limit; the kernels fold y back in).
export function groupsFor(n) {
  const g = Math.ceil(n / WORKGROUP);
  const x = Math.min(g, MAX_GROUPS);
  return [Math.max(1, x), Math.ceil(g / Math.max(1, x))];
}

export function createDeblock(device) {
  const module = device.createShaderModule({ code: DEBLOCK_WGSL, label: 'deblock' });
  const C = GPUShaderStage.COMPUTE;
  const layout = device.createBindGroupLayout({
    label: 'deblock',
    entries: [
      { binding: 0, visibility: C, buffer: { type: 'uniform' } },
      { binding: 1, visibility: C, buffer: { type: 'storage' } },
      { binding: 2, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 3, visibility: C, buffer: { type: 'read-only-storage' } },
    ],
  });
  const pipelineLayout = device.createPipelineLayout({ bindGroupLayouts: [layout] });
  const mk = (entryPoint) => device.createComputePipeline({ label: entryPoint, layout: pipelineLayout, compute: { module, entryPoint } });
  const vertical = mk('deblockV');
  const horizontal = mk('deblockH');
  return {
    module,
    bindGroup({ params, picture, map, ctb }) {
      return device.createBindGroup({
        layout,
        // A GPUBuffer or a binding resource {buffer, offset, size} (e.g. one slot of a DPB arena).
        entries: [params, picture, map, ctb].map((b, binding) => ({ binding, resource: b.buffer ? b : { buffer: b } })),
      });
    },
    encode(pass, bindGroup, width, height) {
      const n = deblockThreads(width, height);
      pass.setBindGroup(0, bindGroup);
      pass.setPipeline(vertical);
      pass.dispatchWorkgroups(...groupsFor(n.v));
      pass.setPipeline(horizontal);
      pass.dispatchWorkgroups(...groupsFor(n.h));
    },
  };
}
