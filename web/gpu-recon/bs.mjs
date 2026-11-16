// Driver for the boundary strength kernel (bs.wgsl.mjs): turns the edge codes of a block map (FORMAT-deblock.md
// "Edge codes") into boundary strengths in place, from the picture's PU and TU records.
//
//   const bs = createBs(device);
//   const bg = bs.bindGroup({ params, cells, pus, tus, map });   // GPUBuffers; params: 32 bytes, see makeBsParams
//   bs.clear(encoder, cells, width, height, nPu);                 // before the compute pass
//   bs.encode(pass, bg, { width, height, nPu, nTu });             // before deblocking reads the map
//
// cells: one u32 per 4x4 luma block (cellsBytes(width, height)); the kernel's scratch, contents not needed after.

import { BS_WGSL, BS_WG } from './bs.wgsl.mjs';

const MAX_GROUPS = 65535;

function groups(n) {
  const g = Math.ceil(n / BS_WG);
  const x = Math.max(1, Math.min(g, MAX_GROUPS));
  return [x, Math.max(1, Math.ceil(g / x))];
}

export const cellsBytes = (width, height) => (width >> 2) * (height >> 2) * 4;

/// The 8-word uniform block (BsParams in bs.wgsl.mjs).
export function makeBsParams({ width, height, nPu, nTu }) {
  return new Uint32Array([width, height, width >> 2, width >> 3, nPu, nTu, (width >> 3) * (height >> 3), nPu ? 0 : 1]);
}

export function createBs(device) {
  const module = device.createShaderModule({ code: BS_WGSL, label: 'bs' });
  const C = GPUShaderStage.COMPUTE;
  const layout = device.createBindGroupLayout({
    label: 'bs',
    entries: [
      { binding: 0, visibility: C, buffer: { type: 'uniform' } },
      { binding: 1, visibility: C, buffer: { type: 'storage' } },
      { binding: 2, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 3, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 4, visibility: C, buffer: { type: 'storage' } },
    ],
  });
  const pipelineLayout = device.createPipelineLayout({ bindGroupLayouts: [layout] });
  const mk = (entryPoint) => device.createComputePipeline({ label: entryPoint, layout: pipelineLayout, compute: { module, entryPoint } });
  const pu = mk('bsPu'), tu = mk('bsTu'), main = mk('bsMain');
  return {
    module,
    bindGroup({ params, cells, pus, tus, map }) {
      return device.createBindGroup({
        layout,
        entries: [params, cells, pus, tus, map].map((b, binding) => ({ binding, resource: b.buffer ? b : { buffer: b } })),
      });
    },
    /** Zeroes the cells of a picture with PUs (outside a pass); a picture without PUs reads no cells. */
    clear(encoder, cells, width, height, nPu) {
      if (nPu) encoder.clearBuffer(cells, 0, cellsBytes(width, height));
    },
    encode(pass, bindGroup, { width, height, nPu, nTu }) {
      pass.setBindGroup(0, bindGroup);
      if (nPu) {
        pass.setPipeline(pu);
        pass.dispatchWorkgroups(...groups(nPu));
        if (nTu) {
          pass.setPipeline(tu);
          pass.dispatchWorkgroups(...groups(nTu));
        }
      }
      pass.setPipeline(main);
      pass.dispatchWorkgroups(...groups((width >> 3) * (height >> 3)));
    },
  };
}
