// SPDX-License-Identifier: LGPL-3.0-only
// VC-1 (Advanced Profile) video path of the video worker. Chrome has no VC-1 decoder, so packets go to a WASM decode worker running
// rust/aether-vc1 (vc1-worker.js, bit exact with FFmpeg); the 8 bit planes it returns are uploaded to WebGPU r8unorm textures and converted
// to R'G'B' in a fragment shader (BT.709 limited range for HD, BT.601 below 720 lines), drawn into an sRGB canvas.
//
// One decoder gives about 2.4 to 2.8 times real time at 1080p23.976 on this machine, so there is no GOP-parallel pool; the worker is
// separate from the video worker only so that decode time never delays a presentation tick.
//
// Interlace: frames carry interlaced / top field first / repeat first field. Progressive frames are always shown whole. An interlaced frame is
// shown woven (both fields at once, combing on motion) or bobbed (each field line doubled and shown for half a frame), `deint` weave | bob,
// default bob for interlaced streams. Repeat first field (3:2 pull down flags) is not applied: every picture is shown once at its own time.
const QMAX = 10;       // frames waiting + frames in the decoder before packets stop being submitted
const DEC_QUEUE = 4;   // packets in the decode worker at once

const SHADER = `
struct U { size: vec4<f32>, mode: vec4<u32>, }   // size: luma w, h, chroma w, h. mode: x colour matrix (0 BT.709, 1 BT.601), y field (0 whole frame, 1 top field, 2 bottom field)
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var ty: texture_2d<f32>;
@group(0) @binding(2) var tu: texture_2d<f32>;
@group(0) @binding(3) var tv: texture_2d<f32>;
@group(0) @binding(4) var smp: sampler;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
  var p = array(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
  return vec4f(p[i], 0.0, 1.0);
}
@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  var xy = vec2<i32>(floor(pos.xy));
  if (u.mode.y != 0u) { xy.y = (xy.y & ~1) + i32(u.mode.y) - 1; }    // bob: the line of the shown field
  let y = textureLoad(ty, xy, 0).r;
  // chroma is left sited, vertically centred (MPEG-2 / VC-1 4:2:0): bilinear taps at (x / 2 + 0.5, (y + 0.5) / 2) in chroma texels
  let c = vec2<f32>(f32(xy.x) * 0.5 + 0.5, (f32(xy.y) + 0.5) * 0.5) / u.size.zw;
  let cb = textureSampleLevel(tu, smp, c, 0.0).r;
  let cr = textureSampleLevel(tv, smp, c, 0.0).r;
  let yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
  let pb = (cb - 128.0 / 255.0) * (255.0 / 224.0);
  let pr = (cr - 128.0 / 255.0) * (255.0 / 224.0);
  var rgb: vec3<f32>;
  if (u.mode.x == 0u) { rgb = vec3<f32>(yy + 1.5748 * pr, yy - 0.187324 * pb - 0.468124 * pr, yy + 1.8556 * pb); }       // KR 0.2126, KB 0.0722
  else { rgb = vec3<f32>(yy + 1.402 * pr, yy - 0.344136 * pb - 0.714136 * pr, yy + 1.772 * pb); }                       // KR 0.299, KB 0.114
  return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}`;

export class Vc1Path {
  constructor(o) {
    Object.assign(this, o);     // track, device, gctx, canvas, qlen(), push(item), ack(n), log, post, deint ('weave' | 'bob' | undefined)
    this.inq = []; this.eof = false;
    this.gen = 1; this.targetUs = 0; this.needKey = true; this.pktSeen = 0;
    this.submitted = 0; this.received = 0;
    this.createdFrames = 0; this.closedFrames = 0;
    this.counts = { decoded: 0, preroll: 0, noKey: 0, bobbed: 0, uploadMs: 0, uploads: 0, decodeMs: 0, decodeN: 0, errors: 0, interlaced: 0, progressive: 0 };
    this.streamInfo = null; this.lastFrame = null; this.flushWait = null;
    this.durUs = o.track.defaultDurationUs || 41708;
  }

  async init() {
    const d = this.device;
    this.worker = new Worker(new URL('./vc1-worker.js', import.meta.url), { type: 'module', name: 'vc1-decode' });
    await new Promise((resolve, reject) => {
      this.worker.onerror = e => reject(new Error('vc1 worker: ' + (e.message || 'script error')));
      this.worker.onmessage = ev => { if (ev.data.type === 'ready') { this.streamInfo = ev.data.info; resolve(); } else if (ev.data.type === 'error') reject(new Error(ev.data.message)); };
      this.worker.postMessage({ type: 'init', track: this.track });
    });
    this.worker.onerror = e => this.post({ type: 'error', where: 'vc1-worker', message: e.message || 'script error' });
    this.worker.onmessage = ev => this.#onMsg(ev.data);
    this.worker.postMessage({ type: 'reset', gen: this.gen });
    if (this.streamInfo?.fpsNum) this.durUs = Math.round(1e6 * this.streamInfo.fpsDen / this.streamInfo.fpsNum);
    this.format = navigator.gpu.getPreferredCanvasFormat();
    this.gctx.configure({ device: d, format: this.format, alphaMode: 'opaque', colorSpace: 'srgb', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
    const mod = d.createShaderModule({ code: SHADER });
    const info = await mod.getCompilationInfo();
    const errs = info.messages.filter(m => m.type === 'error');
    if (errs.length) throw new Error('WGSL vc1: ' + errs.map(m => `${m.lineNum}:${m.linePos} ${m.message}`).join('; '));
    this.pipeline = await d.createRenderPipelineAsync({ layout: 'auto', vertex: { module: mod, entryPoint: 'vs' }, fragment: { module: mod, entryPoint: 'fs', targets: [{ format: this.format }] }, primitive: { topology: 'triangle-list' } });
    this.sampler = d.createSampler({ magFilter: 'linear', minFilter: 'linear' });
    this.uniform = d.createBuffer({ size: 64, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.tex = null;
  }

  hdrReport() {
    return { canvas: `${this.format} srgb (Vc1Path, SDR)`, canvasSize: `${this.canvas.width}x${this.canvas.height}`, source: `VC-1 Advanced Profile ${this.lastFrame ? (this.lastFrame.height <= 576 ? 'BT.601' : 'BT.709') : 'BT.709'} limited range (SDR)`, displayHdr: false };
  }

  // -------------------------------------------------------------- intake
  accept(m) { this.inq.push(m); }

  pump() {
    if (this.closed || this.resetting) return;
    while (this.inq.length && this.submitted - this.received < DEC_QUEUE && this.qlen() + (this.submitted - this.received) < QMAX) {
      const m = this.inq.shift();
      // a random access point: the demuxer's key flag, a packet that opens with a sequence header (0x0000010F), or the first packet of the stream
      const d = new Uint8Array(m.data);
      const seq = d.length > 3 && d[0] === 0 && d[1] === 0 && d[2] === 1 && d[3] === 0x0f;
      const first = this.targetUs <= 0 && this.pktSeen === 0;
      this.pktSeen++;
      if (this.needKey && !(m.key || seq || first)) { this.counts.noKey++; this.ack(1); continue; }
      this.needKey = false;
      this.submitted++;
      this.worker.postMessage({ type: 'pkt', gen: this.gen, ptsUs: m.ptsUs, data: m.data }, [m.data]);
      this.ack(1);
    }
    if (this.flushWait && !this.inq.length && !this.flushWait.sent && this.submitted === this.received) {
      this.flushWait.sent = true;
      this.worker.postMessage({ type: 'flush', gen: this.gen });
    }
  }

  #onMsg(m) {
    if (m.type === 'frames') {
      if (m.gen !== this.gen) return;      // from before a seek
      this.counts.decodeMs += m.decodeMs; this.counts.decodeN++; this.counts.errors = m.errors;
      if (m.for === 'pkt') this.received++;
      for (const f of m.frames) this.#frame(f);
      this.pump();
    } else if (m.type === 'flushed') {
      if (m.gen === this.gen && this.flushWait) { this.flushWait.resolve(); this.flushWait = null; }
    } else if (m.type === 'error') {
      this.counts.errors++;
      this.post({ type: 'error', where: 'vc1-decode', message: m.message });
    }
  }

  #frame(f) {
    this.counts.decoded++;
    if (f.interlaced) this.counts.interlaced++; else this.counts.progressive++;
    this.lastFrame = f;
    if (f.ptsUs < this.targetUs) { this.counts.preroll++; return; }
    const bob = f.interlaced && (this.deint || 'bob') === 'bob';
    // RFF (3:2 pull-down) shows the first field a second time: three fields, each half a frame time long
    const fields = bob ? (f.repeatFirstField ? 3 : 2) : 1;
    const shared = { buffer: f.buffer, left: fields };
    const mk = (ts, field) => {
      this.createdFrames++;
      let released = false;
      return {
        timestamp: ts, dv: true,
        present: () => this.#present(f, shared.buffer, field),
        release: () => { if (released) return; released = true; this.closedFrames++; if (--shared.left === 0) shared.buffer = null; },
      };
    };
    if (bob) {
      this.counts.bobbed++;
      const second = f.topFieldFirst ? 2 : 1, first = f.topFieldFirst ? 1 : 2;
      const half = Math.round(this.durUs / 2);
      this.push(mk(f.ptsUs, first)); this.push(mk(f.ptsUs + half, second));
      if (f.repeatFirstField) this.push(mk(f.ptsUs + 2 * half, first));
    } else this.push(mk(f.ptsUs, 0));
  }

  // -------------------------------------------------------------- GPU
  #ensureTextures(w, h) {
    if (this.tex && this.tex.w === w && this.tex.h === h) return;
    this.tex?.list.forEach(t => t.destroy());
    const d = this.device, cw = (w + 1) >> 1, ch = (h + 1) >> 1;
    const mk = (tw, th) => d.createTexture({ size: [tw, th], format: 'r8unorm', usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST });
    const list = [mk(w, h), mk(cw, ch), mk(cw, ch)];
    this.tex = { w, h, cw, ch, list, group: d.createBindGroup({ layout: this.pipeline.getBindGroupLayout(0), entries: [
      { binding: 0, resource: { buffer: this.uniform } }, { binding: 1, resource: list[0].createView() }, { binding: 2, resource: list[1].createView() },
      { binding: 3, resource: list[2].createView() }, { binding: 4, resource: this.sampler }] }) };
    if (this.canvas.width !== w || this.canvas.height !== h) { this.canvas.width = w; this.canvas.height = h; }
  }

  #present(f, buffer, field) {
    const t0 = performance.now();
    const d = this.device, w = f.width, h = f.height;
    this.#ensureTextures(w, h);
    const { cw, ch, list } = this.tex;
    if (this.uploaded !== buffer) {
      // a bobbed frame is drawn twice from the same planes: upload once
      d.queue.writeTexture({ texture: list[0] }, buffer, { offset: 0, bytesPerRow: w }, [w, h]);
      d.queue.writeTexture({ texture: list[1] }, buffer, { offset: w * h, bytesPerRow: cw }, [cw, ch]);
      d.queue.writeTexture({ texture: list[2] }, buffer, { offset: w * h + cw * ch, bytesPerRow: cw }, [cw, ch]);
      this.uploaded = buffer;
    }
    d.queue.writeBuffer(this.uniform, 0, new Float32Array([w, h, cw, ch]));
    d.queue.writeBuffer(this.uniform, 16, new Uint32Array([h <= 576 ? 1 : 0, field, 0, 0]));
    const enc = d.createCommandEncoder();
    const pass = enc.beginRenderPass({ colorAttachments: [{ view: this.gctx.getCurrentTexture().createView(), loadOp: 'clear', clearValue: { r: 0, g: 0, b: 0, a: 1 }, storeOp: 'store' }] });
    pass.setPipeline(this.pipeline); pass.setBindGroup(0, this.tex.group); pass.draw(3); pass.end();
    d.queue.submit([enc.finish()]);
    this.counts.uploadMs += performance.now() - t0; this.counts.uploads++;
  }

  // -------------------------------------------------------------- lifecycle
  finish() {
    this.eof = true;
    return new Promise(resolve => {
      this.flushWait = { resolve, sent: false };
      this.pump();
      if (!this.worker) resolve();
    });
  }

  /** Seek: drop everything in flight (frames of the old generation are ignored when they arrive). */
  async reset(targetUs) {
    this.resetting = true;
    this.targetUs = targetUs; this.inq = []; this.eof = false; this.needKey = true; this.pktSeen = 0;
    this.gen++; this.submitted = 0; this.received = 0; this.uploaded = null;
    this.flushWait?.resolve(); this.flushWait = null;
    this.worker.postMessage({ type: 'reset', gen: this.gen });
    this.resetting = false;
    this.pump();
  }

  info() {
    const c = this.counts, si = this.streamInfo;
    return {
      quality: 'software', qualityLabel: 'VC-1 (Rust/WASM, bit exact)', kind: 'VC-1 Advanced Profile', dvProfile: null,
      blPath: `WASM aether-vc1 x1 (${c.decodeN ? Math.round(1000 * c.decoded / c.decodeMs) : 0} frames/s while decoding)`, elPath: 'none (single layer)',
      deint: this.deint || 'bob', interlacedFrames: c.interlaced, progressiveFrames: c.progressive, bobbedFrames: c.bobbed,
      stream: si ? `${si.width}x${si.height} ${si.interlace ? 'interlaced' : 'progressive'} profile ${si.profile} level ${si.level}` : null,
      decoded: c.decoded, preroll: c.preroll, noKey: c.noKey, errors: c.errors,
      decodeMsPerPacket: c.decodeN ? Math.round(c.decodeMs / c.decodeN * 100) / 100 : 0, uploadMsPerFrame: c.uploads ? Math.round(c.uploadMs / c.uploads * 100) / 100 : 0,
      queue: this.qlen(), createdFrames: this.createdFrames, closedFrames: this.closedFrames,
    };
  }

  async close() {
    this.closed = true;
    this.flushWait?.resolve(); this.flushWait = null;
    if (this.worker) { try { this.worker.postMessage({ type: 'close' }); } catch {} this.worker.terminate(); this.worker = null; }
    this.tex?.list.forEach(t => t.destroy()); this.tex = null;
    try { this.uniform?.destroy(); } catch {}
  }
}
