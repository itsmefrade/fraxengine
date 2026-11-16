// Driver for the motion compensation kernel (mc.wgsl.mjs). Record layout: FORMAT-mc.md.
//
// At most two dispatches per picture, one compute pass, one bind group: `main` over the general range (two
// workgroups per PU record, luma and chroma) and `mainSmall` over the small range (PUs with both dimensions <= 8,
// four per workgroup). The bind group depends only on the buffers (DPB arena, record buffers, destination), so a
// caller keeps one per destination and reuses it; per picture only the 80-byte parameter block is written.
import { mcWgsl, MC_WORKGROUP, MC_TILE_W, MC_TILE_H } from './mc.wgsl.mjs';

export const MC_RECORD_WORDS = 6;
export const MC_WEIGHT_WORDS = 4;
export { MC_WORKGROUP, MC_TILE_W, MC_TILE_H };

const align64 = (v) => (v + 63) & ~63;

/** README picture layout (offsets and sizes in u32 words). */
export function pictureLayout(width, height) {
  const strideY = align64(width);
  const strideC = align64(width >> 1);
  const offCb = (strideY * height) / 2;
  const offCr = offCb + (strideC * (height >> 1)) / 2;
  const words = offCr + (strideC * (height >> 1)) / 2;
  return { width, height, strideY, strideC, offCb, offCr, words };
}

/** A record belongs in the small range when both PU dimensions are at most 8 (8x8, 8x4, 4x8). */
export const isSmallRecord = (w1) => (w1 & 0xff) <= 8 && ((w1 >> 8) & 0xff) <= 8;

/**
 * Reorders a mixed record list into [small..., general...] (for producers that do not emit the two ranges
 * themselves; G0 should write them directly).
 * @param {Uint32Array} words records, MC_RECORD_WORDS each
 * @returns {{words: Uint32Array, smallCount: number, generalCount: number}}
 */
export function partitionRecords(words) {
  const n = words.length / MC_RECORD_WORDS;
  let small = 0;
  for (let i = 0; i < n; i++) if (isSmallRecord(words[i * MC_RECORD_WORDS + 1])) small++;
  const out = new Uint32Array(words.length);
  let a = 0, b = small;
  for (let i = 0; i < n; i++) {
    const rec = words.subarray(i * MC_RECORD_WORDS, (i + 1) * MC_RECORD_WORDS);
    if (isSmallRecord(rec[1])) out.set(rec, MC_RECORD_WORDS * a++);
    else out.set(rec, MC_RECORD_WORDS * b++);
  }
  return { words: out, smallCount: small, generalCount: n - small };
}

const MAX_GROUPS_X = 65535;
const grid = (groups) => {
  const x = Math.max(1, Math.min(groups, MAX_GROUPS_X));
  return [x, Math.ceil(groups / x)];
};

export class GpuMc {
  /**
   * @param {GPUDevice} device
   * @param {{separateDst?: boolean, code?: string}} opts separateDst: see mcWgsl; code: a WGSL override with the
   *   same interface (experiments only)
   */
  constructor(device, { separateDst = false, code = null } = {}) {
    this.device = device;
    this.separateDst = separateDst;
    const entries = [
      { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'uniform' } },
      { binding: 1, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
      { binding: 2, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
      { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: separateDst ? 'read-only-storage' : 'storage' } },
    ];
    if (separateDst) entries.push({ binding: 4, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } });
    this.bgl = device.createBindGroupLayout({ entries });
    this.module = device.createShaderModule({ code: code ?? mcWgsl({ separateDst }) });
    const layout = device.createPipelineLayout({ bindGroupLayouts: [this.bgl] });
    this.pipeline = device.createComputePipeline({ layout, compute: { module: this.module, entryPoint: 'main' } });
    this.pipelineSmall = device.createComputePipeline({ layout, compute: { module: this.module, entryPoint: 'mainSmall' } });
    this.params = device.createBuffer({ size: 80, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.paramWords = new Uint32Array(20);
  }

  /** Shader compile messages (for tests). */
  async compileInfo() {
    return (await this.module.getCompilationInfo()).messages.map((m) => `${m.type} ${m.lineNum}:${m.linePos} ${m.message}`);
  }

  /**
   * @param {{arena: GPUBuffer, pus: GPUBuffer, wts: GPUBuffer, dst?: GPUBuffer}} b
   *   arena: all DPB slots back to back (slot s at s * slotWords words); pus: PU records; wts: weight entries.
   */
  bindGroup({ arena, pus, wts, dst }) {
    const entries = [
      { binding: 0, resource: { buffer: this.params } },
      { binding: 1, resource: { buffer: pus } },
      { binding: 2, resource: { buffer: wts } },
      { binding: 3, resource: { buffer: arena } },
    ];
    if (this.separateDst) entries.push({ binding: 4, resource: { buffer: dst } });
    return this.device.createBindGroup({ layout: this.bgl, entries });
  }

  /**
   * Writes the parameter block (queue timeline: once per submit) and records the dispatches into `pass`.
   * @param {GPUComputePassEncoder} pass
   * @param {GPUBindGroup} bindGroup
   * @param {object} p {width, height, bitDepthY, bitDepthC, slotWords, dstBase (words),
   *   puFirst = 0, puCount = 0 (general range, any PU size), smallFirst = 0, smallCount = 0 (only PUs with both
   *   dimensions <= 8), u16Samples = max(bitDepthY, bitDepthC) > 8, scalarNarrowing = false}
   */
  encode(pass, bindGroup, p) {
    const puCount = p.puCount ?? 0;
    const smallCount = p.smallCount ?? 0;
    if (!puCount && !smallCount) return;
    const L = pictureLayout(p.width, p.height);
    const [gx, gy] = grid(2 * puCount); // luma and chroma workgroup per record
    const [sx, sy] = grid(Math.ceil(smallCount / 4)); // four small records per workgroup
    const u16 = p.u16Samples ?? Math.max(p.bitDepthY, p.bitDepthC) > 8;
    this.paramWords.set([
      p.width, p.height, L.strideY, L.strideC,
      L.offCb, L.offCr, p.slotWords ?? L.words, p.dstBase ?? 0,
      p.bitDepthY, p.bitDepthC, (u16 ? 1 : 0) | (p.scalarNarrowing ? 2 : 0), puCount,
      gx, p.puFirst ?? 0, p.smallFirst ?? 0, smallCount,
      sx, 0, 0, 0,
    ]);
    this.device.queue.writeBuffer(this.params, 0, this.paramWords);
    pass.setBindGroup(0, bindGroup);
    if (puCount) {
      pass.setPipeline(this.pipeline);
      pass.dispatchWorkgroups(gx, gy);
    }
    if (smallCount) {
      pass.setPipeline(this.pipelineSmall);
      pass.dispatchWorkgroups(sx, sy);
    }
  }
}
