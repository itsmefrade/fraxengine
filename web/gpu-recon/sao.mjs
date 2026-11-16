// Driver for the WGSL SAO kernel (sao.wgsl.mjs). Input layout: FORMAT-sao.md.
//
//   const sao = createSao(device);
//   const bg = sao.bindGroup({ params, src, dst, map, ctb });  // src: deblocked picture, dst: a second buffer
//   sao.encode(pass, bg, params);                              // params: the Uint32Array from makeParams
//
// SAO reads deblocked neighbours, so src and dst must differ. dst receives every word of every plane row (CTBs
// without SAO and lossless / PCM samples are copied), so it is the finished picture and src can be recycled. A
// caller that needs the result in the deblocked buffer itself copies it to a scratch buffer first
// (copyBufferToBuffer, about 0.2 ms for 4K Main 10 on an M-series GPU) and runs SAO from the scratch.

import { SAO_WGSL, saoThreads } from './sao.wgsl.mjs';
import { groupsFor } from './deblock.mjs';

export { makeParams, pictureWords } from './deblock.mjs';

export function createSao(device) {
  const module = device.createShaderModule({ code: SAO_WGSL, label: 'sao' });
  const C = GPUShaderStage.COMPUTE;
  const layout = device.createBindGroupLayout({
    label: 'sao',
    entries: [
      { binding: 0, visibility: C, buffer: { type: 'uniform' } },
      { binding: 1, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 2, visibility: C, buffer: { type: 'storage' } },
      { binding: 3, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 4, visibility: C, buffer: { type: 'read-only-storage' } },
    ],
  });
  const pipeline = device.createComputePipeline({
    label: 'saoMain',
    layout: device.createPipelineLayout({ bindGroupLayouts: [layout] }),
    compute: { module, entryPoint: 'saoMain' },
  });
  return {
    module,
    bindGroup({ params, src, dst, map, ctb }) {
      return device.createBindGroup({
        layout,
        // A GPUBuffer or a binding resource {buffer, offset, size} (e.g. one slot of a DPB arena).
        entries: [params, src, dst, map, ctb].map((b, binding) => ({ binding, resource: b.buffer ? b : { buffer: b } })),
      });
    },
    encode(pass, bindGroup, params) {
      pass.setBindGroup(0, bindGroup);
      pass.setPipeline(pipeline);
      pass.dispatchWorkgroups(...groupsFor(saoThreads(params)));
    },
  };
}
