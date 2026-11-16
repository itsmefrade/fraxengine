// Driver for idct.wgsl.mjs: pipeline creation, per picture dispatch, and CPU side packers for the formats in FORMAT-idct.md.
import { idctWgsl, IDCT_TABLE } from './idct.wgsl.mjs';

export const TU_WORDS = 6;
export const LIST_WORDS = 17; // per scaling list: 16 words of 64 packed bytes, then the DC word
export const LIST_FLAT = 31;
export const KIND = { DCT: 0, DST: 1, SKIP: 2, BYPASS: 3 };

// Packs one TU record into out[at .. at + 6]. `kind` is a KIND value (DST only for 4x4 intra luma).
export function packTu(out, at, { x, y, log2, plane, kind, bitDepth, listIndex = LIST_FLAT, qp, coefOffset, numCoefs, resOffset }) {
  out[at] = (x | (y << 16)) >>> 0;
  out[at + 1] = ((log2 - 2) | (plane << 2) | ((kind === KIND.DST) << 4) | ((kind === KIND.SKIP) << 5) | ((kind === KIND.BYPASS) << 6) |
    ((bitDepth - 8) << 8) | (listIndex << 11) | (qp << 16)) >>> 0;
  out[at + 2] = coefOffset >>> 0;
  out[at + 3] = numCoefs >>> 0;
  out[at + 4] = resOffset >>> 0;
  out[at + 5] = 0;
}

// One coefficient entry: raster position y * n + x in bits 0..9, the raw level (int16) in bits 16..31.
export function packCoef(pos, level) {
  return ((pos & 1023) | ((level & 0xffff) << 16)) >>> 0;
}

// Scaling list table (24 lists, index sizeId * 6 + matrixId) from the decoder's sl[4][6][64] and sl_dc[2][6] (rust_h265 ScalingList).
export function packScalingLists(sl, slDc) {
  const out = new Uint32Array(24 * LIST_WORDS);
  for (let s = 0; s < 4; s++) {
    for (let m = 0; m < 6; m++) {
      const base = (s * 6 + m) * LIST_WORDS;
      for (let j = 0; j < 16; j++) {
        const b = sl[s][m];
        out[base + j] = (b[4 * j] | (b[4 * j + 1] << 8) | (b[4 * j + 2] << 16) | (b[4 * j + 3] << 24)) >>> 0;
      }
      out[base + 16] = s >= 2 ? slDc[s - 2][m] : sl[s][m][0];
    }
  }
  return out;
}

export class IdctPipeline {
  constructor(device) {
    this.device = device;
    this.pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code: idctWgsl, label: 'idct' }), entryPoint: 'main' },
    });
    this.table = device.createBuffer({ size: IDCT_TABLE.byteLength + 48, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    // 260 vec4 = 1040 entries: 1024 DCT + 16 DST.
    device.queue.writeBuffer(this.table, 0, IDCT_TABLE);
  }

  // Bind the picture's buffers. `tus`, `coefs`, `lists`: STORAGE buffers (u32); `residual`: STORAGE buffer (i32).
  // Each Bindings owns its params uniform, so record at most one dispatch per Bindings between two submits.
  createBindings({ tus, coefs, lists, residual }) {
    const params = this.device.createBuffer({ size: 16, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    const group = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: params } },
        { binding: 1, resource: { buffer: this.table } },
        { binding: 2, resource: { buffer: tus } },
        { binding: 3, resource: { buffer: coefs } },
        { binding: 4, resource: { buffer: lists } },
        { binding: 5, resource: { buffer: residual } },
      ],
    });
    return { params, group };
  }

  // One dispatch covering TU records [tuBase, tuBase + tuCount): a workgroup per TU, 2D grid past 32768 TUs.
  dispatch(pass, bindings, tuCount, tuBase = 0) {
    if (tuCount === 0) return;
    const perRow = Math.min(tuCount, 32768);
    this.device.queue.writeBuffer(bindings.params, 0, new Uint32Array([tuBase, tuCount, perRow, 0]));
    pass.setPipeline(this.pipeline);
    pass.setBindGroup(0, bindings.group);
    pass.dispatchWorkgroups(perRow, Math.ceil(tuCount / perRow));
  }
}
