// SPDX-License-Identifier: LGPL-3.0-only
// Realtime Dolby Vision compositor for Chrome (WebGPU). See README.md for the pipeline and measured numbers.
//
//   const comp = await FelCompositor.create(device, canvas.getContext('webgpu'), { peakNits: 1000 });
//   comp.present(blFrame, elFrame, gpuTables, rpuDisplayMeta, displayInfo);   // P7 FEL / MEL
//   comp.present(blFrame, null, null, meta, { profile: 'p8' });               // HDR10 / P8.x / (P5 with meta)
//
// The caller owns the VideoFrames (close them after present returns, the commands are already submitted).
import { RECOVER_WGSL, COMPOSE_WGSL, DISPLAY_WGSL, DISPLAY_LEAN_WGSL } from './fel-shaders.mjs';
import { hdr10plusDisplayBlock } from '../engine/hdr10plus.mjs';
import { buildDisplayUniforms, normalizeRpuDisplayMeta, buildReshapeLut, buildMappingLut, UNIFORM_FLOATS } from './fel-meta.mjs';

const DEFAULT_DISPLAY = Object.freeze({
  peakNits: 1000,          // XDR MacBook panel in HDR mode
  sdrWhiteNits: 203,       // canvas value 1.0
  sourcePeakNits: 1000,    // used when no RPU L1 / DM metadata exists (plain HDR10)
  toneMapping: 'l1',       // 'l1' per frame RPU Level 1 max, 'static' mastering display (Level 6), 'off'
  chromaSiting: 'left-center',
  trims: true,
  dm: 'bt2390',            // display management: 'bt2390' (BT.2390 EETF on max(RGB)) or 'st2094-10' (Dolby style, fel-dm.mjs), options in `dmOptions`
});

/** Canvas colour spaces tried by `probeCanvasColorSpaces`, in preference order for `canvasColorSpace: 'auto'`. */
export const CANVAS_COLOR_SPACES = ['rec2100-pq', 'rec2100-linear', 'rec2100-hlg', 'display-p3-linear', 'srgb-linear', 'display-p3', 'srgb'];

/** Tries every colour space on a scratch canvas. Returns {name: {accepted, error?}}. Never touches the real canvas. */
export function probeCanvasColorSpaces(device, { format = 'rgba16float', toneMapping = { mode: 'extended' }, size = 64 } = {}) {
  const result = {};
  for (const name of CANVAS_COLOR_SPACES) {
    const canvas = typeof OffscreenCanvas !== 'undefined' ? new OffscreenCanvas(size, size) : Object.assign(document.createElement('canvas'), { width: size, height: size });
    try {
      const ctx = canvas.getContext('webgpu');
      ctx.configure({ device, format, colorSpace: name, toneMapping, alphaMode: 'opaque' });
      const c = ctx.getConfiguration?.();
      result[name] = { accepted: true, retained: c ? c.colorSpace === name : null, toneMapping: c?.toneMapping?.mode ?? null };
      ctx.unconfigure();
    } catch (e) { result[name] = { accepted: false, error: String(e.message ?? e).slice(0, 160) }; }
  }
  return result;
}

const align = (n, a) => Math.ceil(n / a) * a;
// Staging slots for exact planes. A slot is free again one mapAsync after its copy was submitted; the pool delivers at most one picture per
// display tick, so two slots should do (a slot still being remapped falls back to writeBuffer, see stats.stageMisses). A third slot costs
// 31 MB (BL + EL planes, 4K FEL) of memory shared with the GPU process.
const STAGE_SLOTS = 2;

/**
 * Layer input. Either a WebCodecs `VideoFrame` (hardware decoded, recovered on the GPU) or exact decoded planes
 * `{ planes: { Y, U, V }, width, height }` (Uint16Array or ArrayBuffer of yuv420p10le codes, Y full size, U and V half size each
 * way, e.g. the frames of web/exact `ExactStream`). Planes skip RECOVER and are copied straight into the code buffer COMPOSE reads.
 */
const isPlanes = f => !!(f && f.planes);
/**
 * Codes already on the GPU: `{ gpu: {buffer, offset, size}, width, height, layout? }`, yuv420 16-bit codes, two per u32, e.g. a DPB
 * slot of web/gpu-recon GpuRecon. width and height are the displayed (cropped) size. `layout` (in samples, from the start of the
 * range) places the planes: `{strideY, strideC, offU, offV, x, y}`, x and y (even) the luma position of the top left displayed
 * sample (conformance window); without it the planes are tight like the code buffers (stride = width, back to back, no crop).
 * COMPOSE and DISPLAY bind the range directly (no copy) and read it exactly as the cropped planes; the caller keeps it unchanged
 * until the present that used it was submitted.
 */
const isGpu = f => !!(f && f.gpu);
const H10P_OFF = new Float32Array(36);   // HDR10+ block with the valid flag 0, one instance so an unchanged block is not rewritten
// The 8 layout words of COMPOSE (bls, blo / els, elo) and DISPLAY (lay, org) for one layer of w x h displayed samples.
const tightLayout = (w, h) => [w, w / 2, w * h, w * h + (w / 2) * (h / 2), 0, 0, 0, 0];
const layoutOf = (f, w, h) => {
  const l = isGpu(f) && f.layout;
  return l ? [l.strideY, l.strideC, l.offU, l.offV, l.x, l.y, l.x / 2, l.y / 2] : tightLayout(w, h);
};
const DISPLAY_LAYOUT_OFFSET = UNIFORM_FLOATS * 4;   // DISPLAY_WGSL DP.lay, right after the fel-meta.mjs block
const frameSize = f => (isPlanes(f) || isGpu(f) ? [f.width, f.height] : [f.displayWidth, f.displayHeight]);
let bufferIds = new WeakMap(), nextBufferId = 1;
const bufferId = b => { let id = bufferIds.get(b); if (!id) { id = nextBufferId++; bufferIds.set(b, id); } return id; };

export class FelCompositor {
  /**
   * @param {GPUDevice} device
   * @param {GPUCanvasContext|null} canvasContext  null renders into `options.target` (a GPUTexture, rgba16float) only
   * @param {object} options display defaults (peakNits, sdrWhiteNits, ...), canvasColorSpace ('display-p3' | 'srgb' |
   *   'rec2100-pq' | 'rec2100-linear' | 'auto'), canvasMaxWidth (default 1920, see README "Canvas findings": a 4K HDR canvas froze Chrome),
   *   format ('rgba16float'), target, lean (true: plain HDR10 / HDR10+ / HLG and profile 5 hardware pictures take one render pass, see `present`)
   */
  constructor(device, canvasContext, options = {}) {
    this.device = device;
    this.ctx = canvasContext;
    this.options = { canvasColorSpace: 'display-p3', canvasMaxWidth: 1920, format: 'rgba16float', ...options };
    this.display = { ...DEFAULT_DISPLAY, ...options };
    this.target = options.target ?? null;
    this.geometry = null;
    this.canvasConfig = null;
    this.stats = { presented: 0, uploadedFrames: 0, uploadCpuMs: 0, uploadBytes: 0 };
    this.lutVersion = null;
    this.h10p = { params: null, peak: 0, block: null };   // last HDR10+ parameters, reused for pictures without a packet
  }

  static async create(device, canvasContext, options) {
    const c = new FelCompositor(device, canvasContext, options);
    await c.init();
    return c;
  }

  async init() {
    const d = this.device;
    const mk = (label, code) => d.createShaderModule({ label, code });
    this.modules = { recover: mk('fel recover', RECOVER_WGSL), compose: mk('fel compose', COMPOSE_WGSL), display: mk('fel display', DISPLAY_WGSL) };
    if (this.options.lean) this.modules.lean = mk('fel display lean', DISPLAY_LEAN_WGSL);
    for (const [k, m] of Object.entries(this.modules)) {
      const info = await m.getCompilationInfo();
      const errors = info.messages.filter(x => x.type === 'error');
      if (errors.length) throw new Error(`WGSL ${k}: ` + errors.map(e => `${e.lineNum}:${e.linePos} ${e.message}`).join('; '));
    }
    const C = GPUShaderStage.COMPUTE;
    this.composeLayout = d.createBindGroupLayout({ entries: [
      { binding: 0, visibility: C, buffer: { type: 'uniform' } },
      { binding: 1, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 2, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 3, visibility: C, buffer: { type: 'read-only-storage' } },
      { binding: 4, visibility: C, buffer: { type: 'storage' } }] });
    const composeLayout = d.createPipelineLayout({ bindGroupLayouts: [this.composeLayout] });
    const compute = (module, entryPoint, layout = 'auto') => d.createComputePipelineAsync({ layout, compute: { module, entryPoint } });
    [this.pRecover, this.pLuma, this.pChroma] = await Promise.all([compute(this.modules.recover, 'main'), compute(this.modules.compose, 'luma', composeLayout), compute(this.modules.compose, 'chroma', composeLayout)]);
    this.sampler = d.createSampler({ magFilter: 'nearest', minFilter: 'nearest' });
    this.tablesBuf = d.createBuffer({ label: 'fel tables', size: 6832 * 4, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
    this.composeUniform = d.createBuffer({ size: 128, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.displayUniform = d.createBuffer({ size: 1024, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.h10pBuf = d.createBuffer({ label: 'fel hdr10plus', size: 144, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.lutBuf = d.createBuffer({ label: 'fel reshape lut', size: 3072 * 4, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
    for (const w of ['bl', 'el']) this[w + 'Uniform'] = d.createBuffer({ size: 16, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    if (this.ctx) this.configureCanvas(this.options.canvasColorSpace);
    else if (!this.target) throw new Error('FelCompositor needs a canvas context or options.target');
    const render = module => d.createRenderPipelineAsync({
      layout: 'auto',
      vertex: { module, entryPoint: 'vertex' },
      fragment: { module, entryPoint: 'fragment', targets: [{ format: this.options.format }] },
      primitive: { topology: 'triangle-list' },
    });
    [this.pDisplay, this.pLean] = await Promise.all([render(this.modules.display), this.modules.lean ? render(this.modules.lean) : null]);
    this.leanGroupLayout = this.pLean?.getBindGroupLayout(0) ?? null;
    return this;
  }

  /** Configures the real canvas. Returns the colour space the browser retained. Logs refusals in `canvasLog`. */
  configureCanvas(colorSpace) {
    const order = colorSpace === 'auto' ? ['rec2100-pq', 'display-p3', 'srgb'] : [colorSpace, 'display-p3', 'srgb'];
    this.canvasLog = [];
    for (const name of [...new Set(order)]) {
      try {
        this.ctx.configure({ device: this.device, format: this.options.format, colorSpace: name, alphaMode: 'opaque',
          toneMapping: { mode: 'extended' }, usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
        const c = this.ctx.getConfiguration?.();
        this.canvasConfig = { colorSpace: c?.colorSpace ?? name, format: c?.format ?? this.options.format, toneMapping: c?.toneMapping?.mode ?? 'extended' };
        this.canvasLog.push({ colorSpace: name, accepted: true });
        return this.canvasConfig.colorSpace;
      } catch (e) { this.canvasLog.push({ colorSpace: name, accepted: false, error: String(e.message ?? e).slice(0, 160) }); }
    }
    throw new Error('canvas configure failed: ' + JSON.stringify(this.canvasLog));
  }

  setDisplay(patch) { this.display = { ...this.display, ...patch }; }

  get outputColorSpace() { return this.canvasConfig?.colorSpace ?? 'display-p3'; }

  /** (Re)allocates the working buffers when the decoded geometry changes. */
  ensureGeometry(w, h, ew = 0, eh = 0) {
    const g = this.geometry;
    if (g && g.w === w && g.h === h && g.ew === ew && g.eh === eh) return g;
    if ((w & 3) || (h & 1) || (ew & 1) || (eh & 1)) throw new Error(`unsupported geometry ${w}x${h} (width must be a multiple of 4, height even)`);
    this.release();
    const d = this.device, U = GPUBufferUsage;
    const planes = (a, b) => (a * b + 2 * (a / 2) * (b / 2)) * 2;       // bytes of one yuv420 16-bit frame
    const blBytes = planes(w, h), elBytes = ew ? planes(ew, eh) : 16, compSamples = w * h + 2 * (w / 2) * (h / 2);
    // The code buffers are created on first use (#codes): GPU input (isGpu) never needs them, 31 MB at 4K FEL.
    this.#codeBufs = { bl: null, el: null };
    const cw = w / 2, ch = h / 2, ecw = ew / 2, ech = eh / 2;
    d.queue.writeBuffer(this.composeUniform, 0, new Uint32Array([w, h, 0, 0, cw, ch, 0, 0, ew, eh, 0, 0, ecw, ech, 0, 0]));
    d.queue.writeBuffer(this.blUniform, 0, new Uint32Array([w, h, 0, 0]));
    d.queue.writeBuffer(this.elUniform, 0, new Uint32Array([ew, eh, 0, 0]));
    this.composeGroups = new Map();
    this.layoutKeys = { compose: '', display: '' };
    this.geometry = { w, h, ew, eh, compSamples, blBytes, elBytes };
    this.blFullApplied = false;
    // canvas size: integer supersampling keeps the DM stage cheap and the canvas below canvasMaxWidth
    this.supersample = Math.max(1, Math.ceil(w / this.options.canvasMaxWidth));
    this.canvasSize = [Math.ceil(w / this.supersample), Math.ceil(h / this.supersample)];
    if (this.ctx) {
      const c = this.ctx.canvas;
      if (c && (c.width !== this.canvasSize[0] || c.height !== this.canvasSize[1])) { c.width = this.canvasSize[0]; c.height = this.canvasSize[1]; }
    }
    this.displayGroups = {};
    this.recoverGroupLayout = this.pRecover.getBindGroupLayout(0);
    return this.geometry;
  }

  /**
   * The composed signal buffer (2 bytes per sample, 24 MB at 4K; DISPLAY reads only the raw value, so the quantised one is not stored)
   * exists only for the paths that run COMPOSE (P7 FEL / MEL tables).
   * Single layer pictures (P8.x, HDR10, P5 reshaped in DISPLAY) read the base layer codes directly and never touch it: allocating it
   * for them cost 50 MB of GPU process memory (zz-dv-p81 4K: 468 MB GPU process footprint with it).
   */
  #ensureComp() {
    if (this.compBuf) return;
    const d = this.device;
    this.compBuf = d.createBuffer({ label: 'fel composed', size: this.geometry.compSamples * 2, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC | GPUBufferUsage.COPY_DST });
  }

  #codeBufs = { bl: null, el: null };
  #codes(layer) {
    const g = this.geometry;
    if (!g) return null;
    return this.#codeBufs[layer] ??= this.device.createBuffer({ label: `fel ${layer} codes`, size: layer === 'bl' ? g.blBytes : g.elBytes, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC });
  }
  /** Code buffers of the planes / hardware inputs (created on first use). */
  get blBuf() { return this.#codes('bl'); }
  get elBuf() { return this.#codes('el'); }
  // A layer's code source: the caller's GPU range, else the own code buffer.
  #src(frame, layer) { return isGpu(frame) ? frame.gpu : { buffer: this.#codes(layer) }; }
  #srcKey(r) { return `${bufferId(r.buffer)}:${r.offset ?? 0}:${r.size ?? -1}`; }

  // Bind groups per (BL source, EL source): a GPU input names a DPB slot, so a handful of keys repeat; reused, not rebuilt per picture.
  #composeGroup(blSrc, elSrc) {
    const key = this.#srcKey(blSrc) + '|' + this.#srcKey(elSrc);
    let g = this.composeGroups.get(key);
    if (!g) {
      if (this.composeGroups.size > 64) this.composeGroups.clear();
      g = this.device.createBindGroup({ layout: this.composeLayout, entries: [{ buffer: this.composeUniform }, blSrc, elSrc, { buffer: this.tablesBuf }, { buffer: this.compBuf }].map((r, i) => ({ binding: i, resource: r })) });
      this.composeGroups.set(key, g);
    }
    return g;
  }

  release() {
    for (const sl of this.stage?.ring || []) sl.buf.destroy();
    this.stage = null;
    for (const b of [this.#codeBufs.bl, this.#codeBufs.el, this.compBuf]) b?.destroy();
    this.#codeBufs = { bl: null, el: null };
    this.compBuf = null;
    this.composeGroups = new Map();
    this.displayGroups = {};
    this.geometry = null;
  }

  dispose() {
    this.release();
    for (const b of [this.tablesBuf, this.composeUniform, this.displayUniform, this.lutBuf, this.h10pBuf, this.blUniform, this.elUniform]) b?.destroy();
    this.ctx?.unconfigure?.();
  }

  #recoverPass(pass, frame, uniform, dst) {
    const group = this.device.createBindGroup({ layout: this.recoverGroupLayout, entries: [
      { binding: 0, resource: { buffer: uniform } },
      { binding: 1, resource: this.device.importExternalTexture({ source: frame, colorSpace: 'srgb' }) },
      { binding: 2, resource: this.sampler },
      { binding: 3, resource: { buffer: dst } }] });
    const [w, h] = uniform === this.blUniform ? [this.geometry.w, this.geometry.h] : [this.geometry.ew, this.geometry.eh];
    pass.setPipeline(this.pRecover);
    pass.setBindGroup(0, group);
    pass.dispatchWorkgroups(Math.ceil(w / 4 / 16), Math.ceil(h / 2 / 16));
  }

  /**
   * Exact planes through a ring of persistently mapped staging buffers (options.upload 'map'): one copy into memory the GPU
   * process already shares, then a buffer copy in this frame's encoder. A slot still being remapped falls back to writeBuffer.
   */
  #stagePlanes(enc, bl, el) {
    const none = { bl: false, el: false, after() {} };
    if (this.options.upload !== 'map' || (!bl && !el)) return none;
    const g = this.geometry, size = (bl ? g.blBytes : 0) + (el ? g.elBytes : 0);
    if (!this.stage || this.stage.size < size) {
      for (const sl of this.stage?.ring || []) sl.buf.destroy();
      this.stage = { size, ring: Array.from({ length: STAGE_SLOTS }, () => ({ buf: this.device.createBuffer({ label: 'fel staging', size, usage: GPUBufferUsage.MAP_WRITE | GPUBufferUsage.COPY_SRC, mappedAtCreation: true }), mapped: true })) };
    }
    const slot = this.stage.ring.find(x => x.mapped);
    if (!slot) { this.stats.stageMisses = (this.stats.stageMisses || 0) + 1; return none; }
    const t = performance.now();
    const range = new Uint8Array(slot.buf.getMappedRange());
    let off = 0;
    const put = (planes, dst, bytes) => {
      const start = off;
      for (const p of [planes.Y, planes.U, planes.V]) { const v = ArrayBuffer.isView(p) ? new Uint8Array(p.buffer, p.byteOffset, p.byteLength) : new Uint8Array(p); range.set(v, off); off += v.byteLength; }
      if (off - start !== bytes) throw new Error(`plane upload is ${off - start} bytes, expected ${bytes}`);
      enc.copyBufferToBuffer(slot.buf, start, dst, 0, bytes);
    };
    if (bl) put(bl, this.blBuf, g.blBytes);
    if (el) put(el, this.elBuf, g.elBytes);
    slot.buf.unmap(); slot.mapped = false;
    this.stats.uploadedFrames++; this.stats.uploadBytes += off; this.stats.uploadCpuMs += performance.now() - t;
    const stage = this.stage;
    return { bl: !!bl, el: !!el, after: () => { slot.buf.mapAsync(GPUMapMode.WRITE).then(() => { if (this.stage === stage) slot.mapped = true; }, () => {}); } };
  }

  /** Queue-ordered copy of exact planes into a code buffer (safe while the previous frame is still being composed). */
  #uploadPlanes(dst, planes, bytes) {
    const t = performance.now();
    const q = this.device.queue;
    let off = 0;
    for (const p of [planes.Y, planes.U, planes.V]) {
      const v = ArrayBuffer.isView(p) ? p : new Uint8Array(p);
      q.writeBuffer(dst, off, v.buffer, v.byteOffset, v.byteLength);     // byte offsets: data is an ArrayBuffer
      off += v.byteLength;
    }
    if (off !== bytes) throw new Error(`plane upload is ${off} bytes, expected ${bytes}`);
    this.stats.uploadedFrames++; this.stats.uploadBytes += off; this.stats.uploadCpuMs += performance.now() - t;
  }

  // Layout words are written only when they change: a stream keeps one layout, so this is one string compare per present.
  #writeLayout(which, words) {
    const key = words.join(',');
    if (this.layoutKeys[which] === key) return;
    this.layoutKeys[which] = key;
    if (which === 'compose') this.device.queue.writeBuffer(this.composeUniform, 64, new Uint32Array(words));
    else this.device.queue.writeBuffer(this.displayUniform, DISPLAY_LAYOUT_OFFSET, new Uint32Array(words));
  }

  #composePass(pass, blSrc = { buffer: this.#codes('bl') }, elSrc = { buffer: this.#codes('el') }) {
    const g = this.geometry;
    this.#ensureComp();
    const group = this.#composeGroup(blSrc, elSrc);
    pass.setPipeline(this.pLuma);
    pass.setBindGroup(0, group);
    pass.dispatchWorkgroups(Math.ceil(g.w / 32), Math.ceil(g.h / 16));     // a thread composes two samples (one word)
    pass.setPipeline(this.pChroma);
    pass.setBindGroup(0, group);
    pass.dispatchWorkgroups(Math.ceil(g.w / 64), Math.ceil(g.h / 32), 2);
  }

  // src: a binding resource ({buffer} or a GPU input's {buffer, offset, size})
  #displayGroup(src) {
    const key = this.#srcKey(src);
    if (!this.displayGroups[key] && Object.keys(this.displayGroups).length > 64) this.displayGroups = {};
    return this.displayGroups[key] ??= this.device.createBindGroup({ layout: this.pDisplay.getBindGroupLayout(0), entries: [
      { binding: 0, resource: { buffer: this.displayUniform } }, { binding: 1, resource: src }, { binding: 2, resource: { buffer: this.lutBuf } }, { binding: 3, resource: { buffer: this.h10pBuf } }] });
  }

  // The lean path: the picture's external texture is the only per picture binding, everything else is the DISPLAY uniform state.
  #leanPass(encoder, frame) {
    const group = this.device.createBindGroup({ layout: this.leanGroupLayout, entries: [
      { binding: 0, resource: { buffer: this.displayUniform } },
      { binding: 1, resource: this.device.importExternalTexture({ source: frame, colorSpace: 'srgb' }) },
      { binding: 2, resource: this.sampler },
      { binding: 3, resource: { buffer: this.h10pBuf } },
      { binding: 4, resource: { buffer: this.blUniform } },
      { binding: 5, resource: { buffer: this.lutBuf } }] });
    const pass = encoder.beginRenderPass({ colorAttachments: [{ view: (this.target ?? this.ctx.getCurrentTexture()).createView(), clearValue: { r: 0, g: 0, b: 0, a: 1 }, loadOp: 'clear', storeOp: 'store' }] });
    pass.setPipeline(this.pLean);
    pass.setBindGroup(0, group);
    pass.draw(3);
    pass.end();
  }

  #renderPass(encoder, src) {
    // options.diag 'offscreen' (measurement only): the same passes into a texture that is never shown, to separate the canvas
    // presentation from the compute and render work.
    if (this.options.diag === 'offscreen' && !this.target) {
      const [w, h] = this.canvasSize;
      if (!this.diagTex || this.diagTex.width !== w || this.diagTex.height !== h) { this.diagTex?.destroy(); this.diagTex = this.device.createTexture({ size: [w, h], format: this.options.format, usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC }); }
    }
    const tex = this.target ?? (this.options.diag === 'offscreen' ? this.diagTex : this.ctx.getCurrentTexture());
    const pass = encoder.beginRenderPass({ colorAttachments: [{ view: tex.createView(), clearValue: { r: 0, g: 0, b: 0, a: 1 }, loadOp: 'clear', storeOp: 'store' }] });
    pass.setPipeline(this.pDisplay);
    pass.setBindGroup(0, this.#displayGroup(src));
    pass.draw(3);
    pass.end();
  }

  /**
   * Composes and displays one picture.
   * @param {VideoFrame|{planes,width,height}} blFrame  hardware decoded base layer (SPS relabelled, see web/engine relabelForChrome)
   *   or exact planes (hybrid / bit exact input, see `isPlanes` above). Mixed inputs are allowed (BL planes + EL VideoFrame and back).
   * @param {VideoFrame|{planes,width,height}|null} elFrame enhancement layer; required when the tables carry a residual, ignored otherwise
   * @param {Int32Array|null} gpuTables  aether_fel_gpu_tables words (6832) for P7; null for single layer content
   * @param {object|null} rpuDisplayMeta libdovi `vdr_dm_data` JSON or the normalised form (fel-meta.mjs), may be null (HDR10)
   * @param {object} displayInfo  {profile:'p7'|'p8'|'p5'|'hdr10', transfer:'pq'|'hlg', sourceFullRange (BL SPS full range flag, see sps-range.mjs), peakNits, sdrWhiteNits, sourcePeakNits, toneMapping, chromaSiting,
   *   trims, dm ('bt2390' | 'st2094-10'), dmOptions ({curve:'spline'|'sigmoid', space:'auto'|'ictcp'|'ipt', ...} see fel-dm.mjs DM_DEFAULTS), rpuJson (P5 reshaping), canvasColorSpace override is not per frame}
   * @returns {{profile:string, mode:string, canvas:number[], tone:object}}
   */
  present(blFrame, elFrame, gpuTables, rpuDisplayMeta, displayInfo = {}) {
    const d = this.device;
    const disp = { ...this.display, ...displayInfo };
    const meta = normalizeRpuDisplayMeta(rpuDisplayMeta);
    const hlg = displayInfo.transfer === 'hlg';
    const h10 = this.#hdr10plus(displayInfo.hdr10plus, disp.peakNits, !rpuDisplayMeta && !gpuTables && !hlg);
    if (h10.noCurveSceneMax) disp.sourcePeakNits = h10.noCurveSceneMax;
    const profile = displayInfo.profile ?? (gpuTables ? 'p7' : 'hdr10');
    // transfer 'hlg': the base layer is BT.2100 HLG and is displayed as it is (inverse OETF, OOTF at the display peak, then the PQ content
    // path). The RPU mapping tables are not applied (see README "HLG and range"); `rpuDisplayMeta` is only used for the Level 5 active area.
    if (hlg) gpuTables = null;
    const [w, h] = frameSize(blFrame);
    const exact = !!gpuTables;
    const residual = exact && gpuTables[5] === 0;
    if (residual && !elFrame) throw new Error('RPU carries a residual but no enhancement frame was supplied');
    const [ew, eh] = residual ? frameSize(elFrame) : [0, 0];
    this.ensureGeometry(w, h, ew, eh);
    // sourceFullRange: the base layer SPS carries video_full_range_flag = 1, so Chrome converted it with the full range matrix whatever the codes
    // really are (sps-range.mjs); RECOVER inverts that conversion to get the codes back
    const full = !!displayInfo.sourceFullRange;
    if (full !== this.blFullApplied) { d.queue.writeBuffer(this.blUniform, 8, new Uint32Array([full ? 1 : 0])); this.blFullApplied = full; }
    // The caller passes the same table object for every picture of a scene (dv-path.js reuses an unchanged RPU's result): 27 KB
    // written per picture otherwise. Callers never modify a table in place after presenting it.
    if (exact && gpuTables !== this.tablesWritten) { d.queue.writeBuffer(this.tablesBuf, 0, gpuTables); this.tablesWritten = gpuTables; this.vdrDepth = gpuTables[4]; }
    let reshape = false, mapLean = false;
    // Profile 5 from a hardware picture, mapping of one curve per component and no residual: the mapping becomes a LUT and the
    // picture takes the lean pass below, no RECOVER, no COMPOSE, no code buffers (options.lean false keeps the three stages).
    if (profile === 'p5' && exact && !residual && this.pLean && !isGpu(blFrame) && !isPlanes(blFrame) && !this.options.diag) {
      if (this.lutVersion !== gpuTables) {
        this.mapLut = buildMappingLut(gpuTables, this.mapLutData ??= new Float32Array(3072));
        if (this.mapLut) d.queue.writeBuffer(this.lutBuf, 0, this.mapLut);
        this.lutVersion = gpuTables;
      }
      mapLean = reshape = !!this.mapLut;
    }
    if (!mapLean && profile === 'p5' && displayInfo.rpuJson) {
      if (this.lutVersion !== displayInfo.rpuJson) { d.queue.writeBuffer(this.lutBuf, 0, buildReshapeLut(displayInfo.rpuJson)); this.lutVersion = displayInfo.rpuJson; }
      reshape = true;
    }
    const g = this.geometry;
    // The DISPLAY uniforms and the HDR10+ block are rebuilt and written only when an input changed: plain HDR10 / HLG keep one set for
    // the whole stream. HDR10 4K24 on a canvas: renderer 0.249 -> 0.247 G instructions/s (proc_pid_rusage, A B x3 headless, x2
    // headed), small. The compositor route as a whole retired the same instructions as Chrome's import within 2 % (0.575 vs 0.573 to
    // 0.590 headed); CPU % from `ps` swung 16 to 28 points between identical runs (core placement), so it cannot resolve this.
    const uKey = [rpuDisplayMeta, profile, exact, reshape, hlg, this.outputColorSpace, g, this.supersample];
    for (const k in disp) if (k !== 'hdr10plus' && k !== 'rpuJson') uKey.push(k, disp[k]);
    const prev = this.uniformCache;
    let u;
    if (prev && prev.key.length === uKey.length && prev.key.every((v, i) => v === uKey[i])) u = prev.u;
    else {
      u = buildDisplayUniforms({ meta, display: disp, profile, scale: exact ? 65536 : 1024, kind: 1, canvasColorSpace: this.outputColorSpace,
        dims: [g.w, g.h, g.w / 2, g.h / 2], reshape, supersample: this.supersample, transfer: hlg ? 'hlg' : 'pq' });
      d.queue.writeBuffer(this.displayUniform, 0, u.data);
      this.uniformCache = { key: uKey, u };
    }
    if (h10.block !== this.h10pWritten) { d.queue.writeBuffer(this.h10pBuf, 0, h10.block); this.h10pWritten = h10.block; }
    // Lean path (options.lean): a plain HDR10 / HDR10+ / HLG hardware picture is decoded from its external texture inside the one
    // render pass (DISPLAY_LEAN_WGSL), no RECOVER compute pass and no 12 MB code buffer. Not bit identical to RECOVER + DISPLAY
    // (chroma is the import's own, not resampled): 0.007 to 0.022 apart in linear light at p99 (README "Lean path").
    // Profile 5 with a mapping LUT (mapLean, above) takes this pass too; other Dolby Vision, planes / GPU input and the measurement
    // modes (options.diag) keep the full path.
    if (mapLean || (this.pLean && profile === 'hdr10' && !exact && !reshape && !isGpu(blFrame) && !isPlanes(blFrame) && !this.options.diag)) {
      const enc = d.createCommandEncoder();
      this.#leanPass(enc, blFrame);
      d.queue.submit([enc.finish()]);
      this.stats.presented++;
      return { profile, input: 'hw', transfer: hlg ? 'hlg' : 'pq', mode: mapLean ? 'mapping-lean' : 'lean', canvas: this.canvasSize, tone: u.info };
    }
    const blLayout = layoutOf(blFrame, w, h);
    if (exact) this.#writeLayout('compose', [...blLayout, ...(residual ? layoutOf(elFrame, ew, eh) : tightLayout(ew, eh))]);
    this.#writeLayout('display', exact ? tightLayout(w, h) : blLayout);
    const enc = d.createCommandEncoder();
    const staged = this.#stagePlanes(enc, isPlanes(blFrame) ? blFrame.planes : null, residual && isPlanes(elFrame) ? elFrame.planes : null);
    // the slot is remapped whatever happens below: a throw before submit (a closed frame, an unconfigured canvas) lost it for good
    try {
      const pass = enc.beginComputePass();
      if (isGpu(blFrame)) { /* codes already on the GPU */ }
      else if (isPlanes(blFrame)) { if (!staged.bl) this.#uploadPlanes(this.blBuf, blFrame.planes, g.blBytes); }
      else if (this.options.diag !== 'noimport' || !this.diagImported) { this.#recoverPass(pass, blFrame, this.blUniform, this.blBuf); this.diagImported = true; }   // 'noimport' (measurement only): the first picture's codes are reused
      if (residual) {
        if (isGpu(elFrame)) { /* codes already on the GPU */ }
        else if (isPlanes(elFrame)) { if (!staged.el) this.#uploadPlanes(this.elBuf, elFrame.planes, g.elBytes); }
        else this.#recoverPass(pass, elFrame, this.elUniform, this.elBuf);
      }
      const blSrc = this.#src(blFrame, 'bl');
      if (exact) this.#composePass(pass, blSrc, residual ? this.#src(elFrame, 'el') : { buffer: this.#codes('el') });
      pass.end();
      this.#renderPass(enc, exact ? { buffer: this.compBuf } : blSrc);
      d.queue.submit([enc.finish()]);
    } finally { staged.after(); }
    this.stats.presented++;
    const kind = f => (isGpu(f) ? 'gpu' : isPlanes(f) ? 'planes' : 'hw');
    const input = `${kind(blFrame)}${residual ? '+' + kind(elFrame) : ''}`;
    return { profile, input, transfer: hlg ? 'hlg' : 'pq', mode: exact ? (residual ? 'fel-exact' : 'mapping-exact') : 'single-layer', canvas: this.canvasSize, tone: u.info };
  }

  /**
   * HDR10+ (ST 2094-40) state. `params` is the result of `extractHdr10Plus(packet, track)` for this picture: an object sets the
   * dynamic metadata, null or undefined reuses the previous picture's (HDR10+ is sent per scene or frame, a packet may carry none),
   * `false` clears it (seek, new stream, switch to another profile). Applies to plain HDR10 only: with a DV RPU or FEL tables
   * it is ignored. Returns the 144 byte uniform block and, when the metadata has statistics but no tone curve, the scene maximum
   * in nits, which then drives the BT.2390 source peak.
   */
  #hdr10plus(params, peakNits, singleLayerHdr10) {
    const st = this.h10p;
    if (params === false) { st.params = null; st.block = null; }
    else if (params) { st.params = params; st.block = null; }
    const off = H10P_OFF;
    if (!singleLayerHdr10 || !st.params) return { block: off };
    if (!st.block || st.peak !== peakNits) { st.block = hdr10plusDisplayBlock(st.params, peakNits, this.options.h10pMap); st.peak = peakNits; }
    if (!st.block || !(st.block[0] > 0.5)) return { block: off };
    return { block: st.block, noCurveSceneMax: st.block[1] > 0.5 ? 0 : st.block[2] };
  }

  /** Waits until everything submitted so far has finished on the GPU. */
  whenIdle() { return this.device.queue.onSubmittedWorkDone(); }

  // ---- verification hooks (used by fel-reference.test.html) ----

  /** Writes exact 16-bit planar yuv420p10le planes (same layout the recover pass produces) and runs only COMPOSE. */
  composeExact(blBytes, elBytes, gpuTables, w = 3840, h = 2160, ew = 1920, eh = 1080) {
    const d = this.device;
    this.ensureGeometry(w, h, ew, eh);
    d.queue.writeBuffer(this.blBuf, 0, blBytes);
    if (elBytes) d.queue.writeBuffer(this.elBuf, 0, elBytes);
    d.queue.writeBuffer(this.tablesBuf, 0, gpuTables);
    this.tablesWritten = null;
    this.vdrDepth = gpuTables[4];
    this.#writeLayout('compose', [...tightLayout(w, h), ...tightLayout(ew, eh)]);
    const enc = d.createCommandEncoder(), pass = enc.beginComputePass();
    this.#composePass(pass);
    pass.end();
    d.queue.submit([enc.finish()]);
  }

  /**
   * Reads the composed signal back. {raw16, vdr} are planar Y,U,V Uint16Array (chroma at half resolution); vdr is raw16 rounded to
   * vdr_bit_depth (table word 4), computed here on the CPU.
   */
  async readComposed() {
    const d = this.device, n = this.geometry.compSamples;
    const rb = d.createBuffer({ size: n * 2, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
    const enc = d.createCommandEncoder();
    enc.copyBufferToBuffer(this.compBuf, 0, rb, 0, n * 2);
    d.queue.submit([enc.finish()]);
    await rb.mapAsync(GPUMapMode.READ);
    const raw16 = new Uint16Array(rb.getMappedRange().slice(0)), vdr = new Uint16Array(n);
    rb.unmap(); rb.destroy();
    const depth = this.vdrDepth, shift = 16 - depth, max = (1 << depth) - 1;
    for (let i = 0; i < n; i++) vdr[i] = shift > 0 ? Math.min((raw16[i] + (1 << (shift - 1))) >> shift, max) : raw16[i];
    return { raw16, vdr };
  }

  /** Reads the recovered 10-bit codes of the base layer (Uint16Array planar Y,U,V), for precision measurements. */
  async readBaseCodes() {
    const d = this.device, n = this.geometry.blBytes;
    const rb = d.createBuffer({ size: n, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
    const enc = d.createCommandEncoder();
    enc.copyBufferToBuffer(this.blBuf, 0, rb, 0, n);
    d.queue.submit([enc.finish()]);
    await rb.mapAsync(GPUMapMode.READ);
    const out = new Uint16Array(rb.getMappedRange().slice(0));
    rb.unmap(); rb.destroy();
    return out;
  }

  /** Reads the recovered or uploaded 10-bit codes of the enhancement layer (Uint16Array planar Y,U,V). */
  async readEnhancementCodes() {
    const d = this.device, n = this.geometry.elBytes;
    const rb = d.createBuffer({ size: n, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
    const enc = d.createCommandEncoder();
    enc.copyBufferToBuffer(this.elBuf, 0, rb, 0, n);
    d.queue.submit([enc.finish()]);
    await rb.mapAsync(GPUMapMode.READ);
    const out = new Uint16Array(rb.getMappedRange().slice(0));
    rb.unmap(); rb.destroy();
    return out;
  }

  /** Renders the already resident buffers (after composeExact or a present) into `target` and reads rgba16float back. */
  async readTarget(width, height) {
    const d = this.device, bpr = align(width * 8, 256);
    const rb = d.createBuffer({ size: bpr * height, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
    const enc = d.createCommandEncoder();
    enc.copyTextureToBuffer({ texture: this.target }, { buffer: rb, bytesPerRow: bpr }, [width, height]);
    d.queue.submit([enc.finish()]);
    await rb.mapAsync(GPUMapMode.READ);
    const out = new Uint16Array(rb.getMappedRange().slice(0));
    rb.unmap(); rb.destroy();
    return { halfWords: out, bytesPerRow: bpr };
  }

  /** Runs only the DISPLAY pass on resident buffers (test hook). */
  displayOnly({ exact, meta, displayInfo = {}, profile = 'hdr10' }) {
    const d = this.device, g = this.geometry;
    if (exact) this.#ensureComp();
    const u = buildDisplayUniforms({ meta: normalizeRpuDisplayMeta(meta), display: { ...this.display, ...displayInfo }, profile, scale: exact ? 65536 : 1024, kind: 1,
      canvasColorSpace: this.outputColorSpace, dims: [g.w, g.h, g.w / 2, g.h / 2], reshape: false, supersample: this.supersample });
    d.queue.writeBuffer(this.displayUniform, 0, u.data);
    this.uniformCache = null;
    this.#writeLayout('display', tightLayout(g.w, g.h));
    const enc = d.createCommandEncoder();
    this.#renderPass(enc, exact ? { buffer: this.compBuf } : { buffer: this.blBuf });
    d.queue.submit([enc.finish()]);
    return u;
  }
}
