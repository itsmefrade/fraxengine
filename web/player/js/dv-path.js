// SPDX-License-Identifier: LGPL-3.0-only
// Compositor path of the video worker: Dolby Vision (P5, P7 FEL/MEL, P8.x) and HDR10 / HDR10+ through web/fel FelCompositor.
//
//   quality 'fast'      BL + EL on WebCodecs hardware decoders          (about 60 dB against the exact result)
//   quality 'high'      BL on the WASM pool (bit exact), EL on hardware (about 75.8 dB)
//   quality 'reference' BL + EL on the WASM pool                        (bit exact)
//
// The demux worker delivers one message per access unit: BL (SPS relabelled for Chrome), optional EL, RPU NAL, HDR10+ params.
// Layers are paired by presentation time; RPU tables are built in decode order (mapping reuse needs the history).
import { FelCompositor } from '../../fel/fel-compositor.mjs';
import { hvccFullRange, packetFullRange } from '../../fel/sps-range.mjs';
import { createDoviSession } from '../../engine/dovi.mjs';
import { LiveExact } from './exact-live.js';
import { ReconLayer } from './gpu-recon-path.js';
import { isIrap, toAnnexB, hvccAnnexB, concat, lengthSizeOfHvcc, firstVclType, GopBuilder } from './dv-util.js';
import { stage, stageNow } from './stages.js';

export const QUALITY_INFO = {
  native: { label: 'Native (BL as HDR10)', psnr: 'no compositor: base layer only, Chrome tone mapping' },
  fast: { label: 'Fast (hw)', psnr: 'about 60 dB (min 59.5) against exact' },
  high: { label: 'High (exact BL)', psnr: 'about 75.8 dB against exact' },
  reference: { label: 'Reference (exact both)', psnr: 'bit exact' },
};
const QMAX = 12;   // queued + pairing + in-flight pictures: a hardware decoder keeps up to 5 or 6 back for reordering and the worker needs 4 queued to report ready
// Exact pool: the player holds this many composed-ready pictures (4K 16-bit planes, 25 MB each). 32 held Rangers FEL at 2.3 GB in
// the renderer; 8 brought it to 1.5 GB but a seek into the middle of a GOP then took 4 s (the pictures before the target filled
// the queue), 16 with READY 8 seeks in 0.7 to 1.1 s at about 1.7 GB. The cushion for the 57 to 78 fps pool lives inside it (EXACT_AHEAD).
export const EXACT_QMAX = 16;
export const EXACT_AHEAD = 16;  // ... and this many more decoded inside the pool (the next GOP must already be under way)
// GPU reconstruction (on by default, ?gpurecon=0 turns it off): composed-ready pictures held per layer. Every held picture is a 25 MB DPB slot on the GPU (4K),
// and the entropy decoder runs at 3 to 7 times real time, so the cushion is smaller than the pool's; it must stay above GPU_READY.
export const GPU_QMAX = 6;   // display queue; each 4K picture held is a 25 MB arena slot (12 kept the BL arena at about 380 MB)
export const GPU_READY = 4;  // pictures queued before an epoch of the GPU path reports ready (below GPU_QMAX: 6 with the pool's 8 made every seek wait 4 s)
const GPU_BACKLOG = 6;      // access units sent to an entropy worker or pictures waiting for reconstruction, per layer
export const EXACT_READY = 8;  // pictures queued before an epoch of the exact path reports ready

function sameBytes(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

export class DvPath {
  constructor(o) {
    Object.assign(this, o);       // track, device, gctx, quality, display, log, post, push(item), ack(n), epoch()
    this.ls = lengthSizeOfHvcc(this.track.description);
    this.inq = []; this.elQ = []; this.eof = false;
    this.meta = new Map();         // ptsUs -> {tables, display (RPU display metadata), h10p}
    this.blPending = []; this.elEarly = new Map();   // ordered items waiting for their EL; EL frames that arrived first
    this.hw = { bl: null, el: null };
    this.counts = { blOut: 0, elOut: 0, blSub: 0, elSub: 0, noTables: 0, elMissing: 0, blPre: 0, elPre: 0, composed: 0, presentCpuMs: 0, tablesMs: 0, tablesN: 0 };
    this.elKind = null;            // 'fel' | 'mel' | 'none', decided from the first RPU
    this.dvInfo = {};
    this.needKey = true; this.ackDebt = 0; this.pktSeen = 0; this.recoveries = 0; this.recovering = false;
    this.gb = null;
    this.elKeyNeeded = true;
    this.targetUs = 0;
    this.maxBlPts = -Infinity;
  }

  async init() {
    // Profile 5 on the hardware route with no hardware HEVC decoder for this stream: the exact software base layer instead.
    if (this.quality === 'fast' && this.track.dolbyVision?.profile === 5 && !(await this.#probeBlCodec())) {
      this.quality = 'high'; this.hwUnavailable = true;
      this.log('Profile 5: no hardware HEVC decoder for this stream, base layer through the WASM decoder');
    }
    const q = this.quality;
    this.exactBl = q === 'high' || q === 'reference';
    this.exactEl = q === 'reference';
    this.dovi = this.track.dolbyVision ? await createDoviSession() : null;   // own mapping history: reset() on every seek
    this.comp = await FelCompositor.create(this.device, this.gctx, { peakNits: this.display.peakNits, sdrWhiteNits: this.display.sdrWhiteNits, h10pMap: this.display.h10pMap, canvasColorSpace: this.display.canvasColorSpace || 'display-p3', elementCurve: !!this.display.elementCurve, canvasMaxWidth: this.display.canvasMaxWidth || 3840, upload: this.display.upload || 'map', diag: this.display.diag, lean: this.display.lean !== false });
    this.canvasLog = this.comp.canvasLog;
    const cores = navigator.hardwareConcurrency || 4;
    this.exactN = this.display.exactN || Math.max(2, Math.min(5, cores - 3));   // 5 on a 12 core machine (aether-hevc: P5 4K60 57 to 78 fps at N 5 to 6)
    if (this.exactBl && this.display.gpuRecon) await this.#startGpu();
    if ((this.exactBl || this.exactEl) && !this.recon) { this.exact = await this.#startPool(); this.#makeBuilders(); }
    this.dvInfo = { profile: this.track.dolbyVision?.profile ?? null, level: this.track.dolbyVision?.level ?? null, elPresent: !!this.track.dolbyVision?.elPresent, kind: this.track.dolbyVision ? `P${this.track.dolbyVision.profile}` : (this.track.hdr10plus ? 'HDR10+' : 'HDR10') };
    await this.#makeDecoders();
  }

  // 512 MB of decoded pictures ahead: Rangers FEL peaked at 605 MB instead of 1.37 GB with 1280, in real time, seeks 0.9 to
  // 1.4 s. The GOP being shown always decodes whatever the budget (exact-live.js), so a long GOP can exceed it.
  #startPool() { return new LiveExact({ n: this.exactN, budgetMB: this.display.exactMB || 512 }).start(); }
  #makeBuilders() {
    const ps = hvccAnnexB(this.track.description);
    this.ps = ps;
    // On the GPU path the builders only keep the GOPs whose pictures did not all come out yet: the exact pool decodes them again
    // when the session falls back (#gpuFallback).
    const sink = layer => g => { if (this.recon) this.gpuGops[layer].push({ layer, ...g }); else this.exact.submit({ layer, ...g }); };
    this.gpuGops = { bl: [], el: [] };
    this.gpuLast = { bl: -Infinity, el: -Infinity };
    this.gb = { bl: new GopBuilder(ps, sink('bl')), el: new GopBuilder(new Uint8Array(0), sink('el')) };
  }

  // -------------------------------------------------------------- GPU reconstruction (?gpurecon=0 turns it off)
  async #startGpu() {
    const layers = { bl: null, el: null };
    try {
      const wasmUrl = new URL('../../exact/wasm/aether_hevc_simd_recon.wasm', import.meta.url).href;
      // ?entropythreads=N: the base layer's WPP rows on N threads (EL stays single-thread; cross-origin isolated pages only).
      const mtUrl = new URL('../../exact/wasm/aether_hevc_simd_recon_mt.wasm', import.meta.url).href;
      const mk = (name, slots) => new ReconLayer({ name, device: this.device, slots, onOutput: f => this.#onGpuOutput(name, f), onError: m => this.#gpuFallback(m), onActivity: () => this.pump(), failAt: name === 'bl' ? this.display.gpuFailAt : 0 }).start(wasmUrl, name === 'bl' ? { mtUrl, threads: this.display.entropyThreads || 1 } : {});
      layers.bl = await mk('bl', GPU_QMAX);
      // EL slots: the display queue's pictures, up to 4 enhancement pictures decoded ahead of their base picture, and the references.
      if (this.exactEl) layers.el = await mk('el', GPU_QMAX + 6);
      this.onGpuError = ev => this.#gpuFallback('GPU error: ' + (ev.error?.message || ev.error));
      this.device.addEventListener('uncapturederror', this.onGpuError);
      this.recon = layers;
      this.#makeBuilders();
    } catch (e) {
      this.log('WARNING: GPU reconstruction unavailable, exact pool instead: ' + (e.message || e));
      this.gpuFailed = String(e.message || e);
      for (const l of Object.values(layers)) l?.destroy();
    }
  }
  #onGpuOutput(layer, f) {
    this.gpuLast[layer] = Math.max(this.gpuLast[layer], f.ptsUs);
    const gops = this.gpuGops[layer];
    while (gops.length && Math.max(...gops[0].ptsList) <= this.gpuLast[layer]) gops.shift();
    if (f.ptsUs < this.targetUs) { f.layer.release(f); return; }
    if (layer === 'bl') { this.counts.blOut++; this.#onBl(f.ptsUs, f); } else { this.counts.elOut++; this.#onEl(f.ptsUs, f); }
  }
  #drainGpu() {
    const { bl, el } = this.recon;
    try {
      bl?.process(() => this.qlen() + this.blPending.length < GPU_QMAX);
      // Enhancement pictures wait in elEarly for their base picture; past 4 of them only a base picture that needs one pulls more
      // (8 kept 2 more 4K-EL-sized arena slots alive with no effect on drops or seeks).
      el?.process(() => this.elEarly.size < 4 || this.blPending.some(e => e.needEl && !e.el));
    } catch (e) { this.#gpuFallback(String(e.message || e)); }
  }
  #feedGpu(m, data) {
    const { bl, el } = this.recon;
    const elStarved = el && el.backlog() < 2 && this.blPending.some(e => e.needEl && !e.el);
    if ((bl.backlog() >= GPU_BACKLOG || (el && el.backlog() >= GPU_BACKLOG)) && !(elStarved && bl.backlog() < 4 * GPU_BACKLOG)) return 'wait';
    const type = firstVclType(data, this.ls);
    const irap = type >= 16 && type <= 23;
    if (this.needKey && !irap) return 'skip';
    this.needKey = false;
    const t = stageNow();
    const annexB = toAnnexB(data, this.ls);
    this.gb.bl.push(m.ptsUs, annexB, type);
    bl.feed(m.ptsUs, irap ? concat([this.ps, annexB]) : annexB.slice(), type);   // parameter sets at every IRAP, as GopBuilder
    stage('video.feed.bl', t, 2 * annexB.byteLength);   // two copies: Annex B, then the worker's own
    if (el && m.el && this.wantsEl) {
      const t2 = stageNow();
      const e = new Uint8Array(m.el), et = firstVclType(e, this.ls), eb = toAnnexB(e, this.ls);
      this.gb.el.push(m.ptsUs, eb, et);
      el.feed(m.ptsUs, eb.slice(), et);
      stage('video.feed.el', t2, 2 * eb.byteLength);
    }
    return 'ok';
  }
  /**
   * A picture the GPU path cannot reconstruct exactly ('cpu' verdict), an entropy or GPU error: the session continues on the exact
   * pool, never with a non exact picture. The pool decodes again every GOP whose pictures did not all come out of the GPU path and
   * drops the pictures that did (minPts); pictures already queued from the GPU stay.
   */
  async #gpuFallback(reason) {
    if (!this.recon || this.closed) return;
    this.gpuFailed = reason;
    this.log('WARNING: GPU reconstruction off for this session, exact pool instead: ' + reason);
    const layers = this.recon;
    this.recon = null; this.reconOld = layers;
    this.device.removeEventListener('uncapturederror', this.onGpuError);
    for (const l of Object.values(layers)) l?.stop();
    this.switching = true;
    try {
      this.exact = await this.#startPool();
      this.minPts = { ...this.gpuLast };
      for (const l of ['bl', 'el']) for (const g of this.gpuGops[l]) this.exact.submit(g);
      this.gpuGops = { bl: [], el: [] };
    } catch (e) { this.post({ type: 'error', where: 'dv-gpu-fallback', message: String(e) }); }
    this.switching = false;
    this.pump();
  }

  // -------------------------------------------------------------- decoders
  async #makeDecoders() {
    this.hwGen = (this.hwGen || 0) + 1;
    const gen = this.hwGen;
    for (const l of ['bl', 'el']) if (this.hw[l] && this.hw[l].state !== 'closed') { try { this.hw[l].close(); } catch {} }
    this.hw = { bl: null, el: null };
    const t = this.track;
    if (!this.exactBl) {
      // The compositor wants the plain HEVC base layer: the dvcC / hvcC base layer string first (not the Dolby Vision string, that would
      // ask the browser for its own Dolby Vision handling), then the container codec, each also with the level in the main tier.
      const codec = this.blCodec ??= (await this.#probeBlCodec()) || this.#blCodecList()[0];
      const cfg = { codec, codedWidth: t.width, codedHeight: t.height, hardwareAcceleration: 'prefer-hardware', optimizeForLatency: true };
      if (t.description) cfg.description = t.description;
      this.hw.bl = this.#decoder('bl', cfg, gen);
    }
    this.blCfg = this.blCfg || null;
  }
  #blCodecList() {
    const t = this.track, list = [];
    for (const c of [t.fallbackCodecString, t.codec]) if (c && !list.includes(c)) { list.push(c); if (/^hvc1\..*\.H/.test(c)) list.push(c.replace('.H', '.L')); }
    return list;
  }
  // The first base layer codec string a hardware decoder accepts, null when none does.
  async #probeBlCodec() {
    if (typeof VideoDecoder === 'undefined') return null;
    const t = this.track, probe = { codedWidth: t.width, codedHeight: t.height, hardwareAcceleration: 'prefer-hardware' };
    if (t.description) probe.description = t.description;
    for (const c of this.#blCodecList()) { try { if ((await VideoDecoder.isConfigSupported({ ...probe, codec: c })).supported) return c; } catch {} }
    return null;
  }
  #ensureElDecoder() {
    if (this.hw.el || this.exactEl) return;
    const w = this.track.dolbyVision?.profile === 7 ? Math.round(this.track.width / 2) : this.track.width;
    const h = Math.round(this.track.height / 2);
    const cfg = { codec: this.track.height > 1100 ? 'hvc1.2.4.L123.90' : 'hvc1.2.4.L93.90', codedWidth: w, codedHeight: h, hardwareAcceleration: 'prefer-hardware', optimizeForLatency: true };
    VideoDecoder.isConfigSupported(cfg).then(s => { this.elHwSupported = !!s.supported; }).catch(() => {});
    this.hw.el = this.#decoder('el', cfg, this.hwGen);
  }
  #decoder(layer, cfg, gen) {
    const d = new VideoDecoder({
      output: f => this.#onHwFrame(layer, f, gen),
      error: e => { if (gen === this.hwGen) this.#recover(`dv-${layer}-decoder`, e); },
    });
    d.configure(cfg);
    this.dvInfo[layer + 'Decoder'] = { codec: cfg.codec, hw: cfg.hardwareAcceleration };
    return d;
  }
  #onHwFrame(layer, f, gen) {
    const t = stageNow();
    try { this.#onHwFrame2(layer, f, gen); } finally { stage(`video.out.${layer}(incl)`, t); }
  }
  #onHwFrame2(layer, f, gen) {
    this.createdFrames = (this.createdFrames || 0) + 1;
    if (gen !== this.hwGen) { this.#close(f); return; }
    // frames before the seek target are decoded and dropped; they still left the decoder, so they count against the in-flight limit
    if (f.timestamp < this.targetUs) { this.counts[layer === 'bl' ? 'blPre' : 'elPre']++; this.#close(f); this.pump(); return; }
    if (layer === 'bl') { this.counts.blOut++; if (this.recoveries && this.counts.blOut >= 30) this.recoveries = 0; this.#onBl(f.timestamp, f); }
    else { this.counts.elOut++; this.#onEl(f.timestamp, f); }
    this.pump();
  }
  #close(f) { try { f.close(); } catch {} this.closedFrames = (this.closedFrames || 0) + 1; }

  // -------------------------------------------------------------- intake (decode order)
  accept(m) {
    // RPU tables in decode order, whatever the decoder does later
    let tables = null, display = null;
    if (this.dovi && m.rpu) {
      const a = performance.now();
      const rpu = m.rpu instanceof Uint8Array ? m.rpu : new Uint8Array(m.rpu);
      // An RPU byte identical to the previous one (one RPU per scene, repeated on every picture) maps to the same tables and display
      // metadata: the mapping history it would update holds that RPU already. Reusing the same objects also lets the compositor skip
      // the table upload (same reference).
      const prev = this.lastRpu;
      if (prev && sameBytes(prev.bytes, rpu)) { tables = prev.tables; display = prev.display; this.counts.rpuReused = (this.counts.rpuReused || 0) + 1; }
      else {
        this.lastRpu = null;
        try {
          const r = this.dovi.parseRpu(rpu);
          tables = r.tables; display = r.display;
          if (r.tablesError && !this.tablesErr) { this.tablesErr = String(r.tablesError.message || r.tablesError); this.log('WARNING: RPU table build failed: ' + this.tablesErr); }
          if (tables && !r.tablesError) this.lastRpu = { bytes: rpu.slice(), tables, display };
        } catch (e) { if (!this.rpuErr) { this.rpuErr = String(e.message || e); this.log('WARNING: RPU rejected: ' + this.rpuErr); } }
      }
      this.counts.tablesMs += performance.now() - a; this.counts.tablesN++;
      stage('video.rpu', a, rpu.byteLength);
    }
    if (tables && this.elKind === null) {
      this.elKind = !m.el ? 'none' : tables[5] === 0 ? 'fel' : 'mel';
      this.dvInfo.elKind = this.elKind;
      if (this.elKind === 'mel') this.log('MEL: mapping only, the enhancement layer is not decoded');
    }
    if (!tables && this.dovi) this.counts.noTables++;
    this.meta.set(m.ptsUs, { tables, display, h10p: m.h10p || null });
    if (this.meta.size > 400) this.meta.delete(this.meta.keys().next().value);
    this.inq.push(m);
  }
  get wantsEl() { return this.elKind === 'fel'; }

  pump() {
    const t = stageNow();
    try { this.#pump(); } finally { stage('video.pump(incl)', t); }
  }
  #pump() {
    if (this.closed || this.resetting || this.recovering || this.switching) return;
    // 1. frames from the exact pool (or the GPU reconstruction) into pairing
    if (this.recon) this.#drainGpu();
    if (this.switching) return;   // #drainGpu fell back: the pool is starting
    if (this.exact) this.#drainExact();
    // 2. packets
    while (this.inq.length) {
      const m = this.inq[0];
      const data = new Uint8Array(m.data);
      if (this.exactBl) {
        const r = this.recon ? this.#feedGpu(m, data) : this.#feedExact(m, data);
        if (r === 'wait') break;
        this.inq.shift();
        if (r === 'skip') this.ack(1); else { this.#feedEl(m); this.#ackOrHold(); }
        continue;
      }
      const bl = this.hw.bl;
      if (!bl || bl.state !== 'configured') break;
      if (bl.decodeQueueSize >= 4 || this.qlen() + this.blPending.length + this.#inflightBl() >= QMAX) break;
      const irap = isIrap(data, this.ls);
      // The relabelled in-band SPS is what Chrome converts by; the record in the description may hold no parameter sets at all.
      if (irap) { const range = packetFullRange(data, this.ls, { primaries: 1, transfer: 13 }); if (range !== null) this.fullRange = range; }
      // start at an IRAP or at the packet the demuxer's seek landed on (m.key: container flag AND payload check); WebCodecs refuses a
      // 'key' chunk that is not an IRAP, so a non-IRAP first picture cannot be a start
      const startOk = irap || m.key;
      this.pktSeen++;
      if (this.needKey && !startOk) { this.inq.shift(); this.ack(1); continue; }
      const asKey = irap || (this.needKey && startOk);
      this.needKey = false;
      const t = stageNow();
      try { bl.decode(new EncodedVideoChunk({ type: asKey ? 'key' : 'delta', timestamp: m.ptsUs, duration: m.durationUs, data })); }
      catch (e) { this.#recover('dv-bl-decode', e); return; }
      stage('video.feed.bl', t, data.byteLength);
      this.counts.blSub++; this.lastBlPts = m.ptsUs;
      this.#feedEl(m);
      this.inq.shift(); this.ack(1);
    }
    this.#pumpEl();
    this.#releaseAcks();
    if (this.exactEl && this.exact) this.#drainExact();
    if (this.recon) this.#drainGpu();
    this.#flushPairs();
  }
  /**
   * A hardware decoder error (a corrupt access unit, on either layer) is not fatal, like in the single layer path: both decoders are
   * rebuilt and both layers resume at the next BL IRAP, so BL and EL restart together. Pictures that were inside a decoder are lost
   * and counted in counts.recDropped; pictures already decoded and waiting for pairing are kept. More than 6 errors without 30
   * pictures of progress in between end playback.
   */
  #recover(where, e) {
    if (this.closed || this.resetting || this.exactBl) { if (!this.closed) this.post({ type: 'error', where, message: String(e) }); return; }
    if (this.recovering) return;   // the second layer's error for the same bad unit
    this.counts.decodeErrors = (this.counts.decodeErrors || 0) + 1; this.totalRecoveries = (this.totalRecoveries || 0) + 1;
    if (++this.recoveries > 6) { this.post({ type: 'error', where, message: String(e) }); return; }
    // The decoder reports late and without a position: how far the layers were says which access unit to look at.
    this.lastError = `${where}: ${e} (base layer ${this.counts.blSub} submitted, ${this.counts.blOut + this.counts.blPre} out, last pts ${this.lastBlPts ?? null})`;
    this.log(`WARNING: DV decoder error, recovering (${this.recoveries}/6): ${this.lastError}`);
    this.recovering = true;
    const c = this.counts;
    this.counts.recDropped = (c.recDropped || 0) + Math.max(0, this.#inflightBl()) + this.elQ.length;
    this.recoverGoodOut = c.blOut;
    this.elQ = []; this.needKey = true; this.elKeyNeeded = true; this.elFlushed = false;
    Object.assign(c, { blSub: 0, blOut: 0, blPre: 0, elSub: 0, elOut: 0, elPre: 0 });
    this.hw.el = null;
    this.#makeDecoders().then(() => { this.recovering = false; this.pump(); }, err => { this.recovering = false; this.post({ type: 'error', where, message: String(err) }); });
  }
  #inflightBl() { return this.counts.blSub - this.counts.blOut - this.counts.blPre; }
  // EL packets wait in elQ and are decoded just in time: the hardware EL decoder must not run ahead of the BL (exact BL arrives
  // a GOP late), or its frames would pile up as unpaired hardware surfaces.
  #feedEl(m) {
    if (!this.wantsEl || !m.el || this.exactEl) return;
    const data = new Uint8Array(m.el);
    const irap = isIrap(data, this.ls);
    if (this.elKeyNeeded && !irap) return;
    this.elKeyNeeded = false;
    const t = stageNow();
    this.elQ.push({ pts: m.ptsUs, dur: m.durationUs, irap, annexB: toAnnexB(data, this.ls) });
    stage('video.feed.el', t, data.byteLength);
  }
  #pumpEl() {
    if (!this.elQ.length) return;
    if (!this.hw.el) this.#ensureElDecoder();
    const el = this.hw.el;
    if (!el || el.state !== 'configured') return;
    while (this.elQ.length && el.decodeQueueSize < 4 && this.elEarly.size + (this.counts.elSub - this.counts.elOut - this.counts.elPre) < 10) {
      const p = this.elQ.shift();
      if (this.elFlushed && !p.irap) continue;
      this.elFlushed = false;
      const t = stageNow();
      try { el.decode(new EncodedVideoChunk({ type: p.irap ? 'key' : 'delta', timestamp: p.pts, duration: p.dur, data: p.annexB })); this.counts.elSub++; stage('video.feed.el', t, p.annexB.byteLength); }
      catch (e) { this.#recover('dv-el-decode', e); return; }
    }
  }
  // exact path: cut GOPs per layer (see GopBuilder), one Annex B blob each
  #feedExact(m, data) {
    if (this.exact.pendingGops('bl') > this.exactN + 1 && !this.gb.bl.cur) return 'wait';   // pool is full of work
    const type = firstVclType(data, this.ls);
    const irap = type >= 16 && type <= 23;
    if (this.needKey && !irap) return 'skip';
    this.needKey = false;
    this.gb.bl.push(m.ptsUs, toAnnexB(data, this.ls), type);
    if (this.exactEl && m.el && this.wantsEl) {
      const el = new Uint8Array(m.el), et = firstVclType(el, this.ls);
      this.gb.el.push(m.ptsUs, toAnnexB(el, this.ls), et);
    }
    return 'ok';
  }
  #ackOrHold() { this.ackDebt++; }
  #releaseAcks() {
    if (!this.exactBl) return;
    if (this.recon) { if (this.ackDebt) { this.ack(this.ackDebt); this.ackDebt = 0; } return; }   // #feedGpu waits instead
    // acknowledge as long as the pool has room, so the demuxer reads ahead by about the pool's capacity
    if (this.exact.pendingGops('bl') <= this.exactN + 1 || this.eof) { if (this.ackDebt) { this.ack(this.ackDebt); this.ackDebt = 0; } }
  }
  #drainExact() {
    let guard = 0;
    while (this.qlen() + this.blPending.length + this.#bufferedExact() < EXACT_QMAX && guard++ < 16) {
      let any = false;
      if (this.exactBl) { const f = this.exact.take('bl'); if (f) { any = true; if (this.#fresh('bl', f)) { this.counts.blOut++; this.#onBl(f.ptsUs, f); } } }
      if (this.exactEl) { const f = this.exact.take('el'); if (f) { any = true; if (this.#fresh('el', f)) { this.counts.elOut++; this.#onEl(f.ptsUs, f); } } }
      if (!any) break;
    }
    // Enhancement pictures are also taken past the queue limit while a base picture waits for its own: with 16 base pictures
    // waiting, the loop above stopped taking enhancement pictures too, and a paused start never showed a frame.
    guard = 0;
    while (this.exactEl && guard++ < 32 && this.blPending.some(e => e.needEl && !e.el)) {
      const f = this.exact.take('el');
      if (!f) break;
      if (this.#fresh('el', f)) { this.counts.elOut++; this.#onEl(f.ptsUs, f); }
    }
  }
  // A pool picture at or after the seek target that the GPU path did not already deliver (minPts: set by #gpuFallback). A dropped
  // picture goes back to its worker.
  #fresh(layer, f) {
    if (f.ptsUs >= this.targetUs && !(this.minPts && f.ptsUs <= this.minPts[layer])) return true;
    this.exact.recycle(f);
    return false;
  }
  #bufferedExact() { return 0; }
  /** Pictures queued before this epoch may report ready. */
  readyNeed() { return this.recon ? GPU_READY : EXACT_READY; }
  /** Pictures decoded by the exact pool and waiting for the player (BL layer). */
  ahead() { return this.recon ? EXACT_AHEAD : this.exact ? this.exact.lookahead('bl') : 0; }
  // The lookahead target is in pictures, the pool's budget in bytes: a P7 file with a full resolution enhancement layer (about
  // 50 MB per picture pair) fills 512 MB before 16 pictures, and the start then waited for the 4 s fallback.
  aheadFull() { return !!this.exact?.budgetFull(); }

  // -------------------------------------------------------------- pairing
  #onBl(ts, bl) {
    (this.dbgBl ??= []).length < 14 && this.dbgBl.push(ts);
    const meta = this.meta.get(ts) || { tables: null, display: null, h10p: null };
    const needEl = this.wantsEl && !!meta.tables && meta.tables[5] === 0;
    this.blPending.push({ timestamp: ts, bl, el: null, needEl, meta, born: performance.now() });
    const early = this.elEarly.get(ts);
    if (early) { this.elEarly.delete(ts); this.blPending[this.blPending.length - 1].el = early; }
  }
  #onEl(ts, el) {
    (this.dbgEl ??= []).length < 14 && this.dbgEl.push(ts);
    const e = this.blPending.find(x => x.timestamp === ts);
    if (e) e.el = el; else if (ts >= this.maxBlPts - 1) this.elEarly.set(ts, el); else this.#releaseLayer(el);
  }
  #releaseLayer(f) { if (f && typeof f.close === 'function') this.#close(f); else if (f?.gpu) f.layer.release(f); }
  #flushPairs() {
    while (this.blPending.length) {
      const e = this.blPending[0];
      // An exact enhancement layer is still being decoded while its pool has work: the picture waits for it however long that
      // takes. A 2 s limit let the first 10 to 15 pictures after a seek go out as base layer only (mapping, no residual), so the
      // same frame came out two different ways from run to run. The limit stays for a layer that is idle and never sent it.
      if (e.needEl && !e.el) {
        const elBusy = this.exactEl && ((this.exact && this.exact.pendingGops('el') > 0) || (this.recon?.el && this.recon.el.busy()));
        if (elBusy || performance.now() - e.born < 2000) break;
        this.counts.elMissing++;
      }
      this.blPending.shift();
      this.maxBlPts = Math.max(this.maxBlPts, e.timestamp);
      this.push(this.#item(e));
    }
  }
  #item(e) {
    const self = this;
    return {
      timestamp: e.timestamp, dv: true,
      present() {
        const { tables, display, h10p } = e.meta;
        const dvProfile = self.track.dolbyVision?.profile;
        const profile = !self.track.dolbyVision ? 'hdr10' : dvProfile === 5 ? 'p5' : dvProfile === 7 ? 'p7' : 'p8';
        // HLG base layer (Profile 8.4: colorOriginal comes from the worker, the SPS was relabelled): displayed through BT.2100 HLG, not the RPU mapping
        // (fel/README.md "HLG"). The SPS range flag decides how Chrome converted the frame (some files carry it wrong), see fel/sps-range.mjs.
        self.fullRange ??= hvccFullRange(self.track.description, { primaries: 1, transfer: 13 }) === true;
        const info = { profile, hdr10plus: h10p || (self.h10pAfterSeek ? false : undefined), transfer: self.colorOriginal?.transfer === 18 ? 'hlg' : 'pq', sourceFullRange: self.fullRange };
        let el = e.el;
        let t = tables;
        if (t && t[5] === 0 && !el) { self.counts.elMissing++; t = tables.slice(); t[5] = 1; }   // residual wanted but no EL frame: mapping only
        self.h10pAfterSeek = false;
        const a = performance.now();
        const bl = e.bl.gpu ? e.bl.layer.input(e.bl) : e.bl.planes ? { planes: e.bl.planes, width: e.bl.width, height: e.bl.height } : e.bl;
        const elIn = el?.gpu ? el.layer.input(el) : el && el.planes ? { planes: el.planes, width: el.width, height: el.height } : el;
        const r = self.comp.present(bl, elIn, t, display, info);   // display: the RPU's own matrices (P5 IPTPQc2 -> LMS -> RGB), L1/L2/L5/L6/L8
        self.counts.presentCpuMs += performance.now() - a; self.counts.composed++;
        stage('video.present', a);
        self.lastPresent = r;
      },
      release() {
        for (const l of [e.bl, e.el]) { if (l && typeof l.close === 'function') self.#close(l); else if (l?.gpu) l.layer.release(l); else if (l?.planes) self.exact?.recycle(l); }
        e.bl = e.el = null;
      },
    };
  }

  // -------------------------------------------------------------- lifecycle
  async finish() {
    this.eof = true;
    if (this.gb) { this.gb.bl.flush(); this.gb.el.flush(); }
    this.pump();
    // EL packets are decoded just in time; flushing the EL decoder first made the rest of them fail ('a key frame is required after
    // flush()') when a seek landed close to the end of the file. Let the queue drain (bounded), then flush.
    for (let i = 0; i < 600 && this.elQ.length && !this.closed && !this.resetting; i++) { this.pump(); await new Promise(r => setTimeout(r, 5)); }
    this.elQ = [];
    const flushes = [];
    for (const l of ['bl', 'el']) if (this.hw[l] && this.hw[l].state === 'configured') flushes.push(this.hw[l].flush().catch(() => {}));
    this.elFlushed = true;
    await Promise.all(flushes);
    // GPU path: the entropy workers output their last pictures, then reconstruction drains like the pool below
    if (this.recon) for (const l of Object.values(this.recon)) l?.finish();
    for (let i = 0; i < 6000 && this.recon && !this.closed && !this.resetting; i++) {
      this.pump();
      if (!Object.values(this.recon).some(l => l?.busy()) && !this.inq.length) break;
      await new Promise(r => setTimeout(r, 10));
    }
    // exact frames still decoding
    for (let i = 0; i < 6000 && this.exact && !this.closed; i++) {
      this.pump();
      const left = (this.exactBl && this.exact.pendingGops('bl')) + (this.exactEl && this.exact.pendingGops('el'));
      if (!left && !this.inq.length) break;
      await new Promise(r => setTimeout(r, 10));
    }
    this.pump();
    // pairs whose EL never came
    for (const e of this.blPending) e.needEl = false;
    this.#flushPairs();
  }

  /** Seek: drop everything in flight. Packets of the new epoch that arrive while this awaits are kept (inq is cleared first). */
  async reset(targetUs) {
    this.resetting = true;
    this.targetUs = targetUs;
    this.inq = []; this.elQ = []; this.eof = false; this.meta.clear(); this.needKey = true; this.pktSeen = 0; this.elKeyNeeded = true; this.elFlushed = false; this.maxBlPts = -Infinity;
    for (const e of this.blPending) { this.#releaseLayer(e.bl); this.#releaseLayer(e.el); }
    this.blPending = [];
    for (const f of this.elEarly.values()) this.#releaseLayer(f);
    this.elEarly.clear();
    this.ackDebt = 0;
    this.dovi?.reset(); this.lastRpu = null;
    this.h10pAfterSeek = true;   // a packet may carry no metadata, the first picture after a seek must not inherit the previous scene's
    this.hw.el = null;
    // decoder generations are replaced: nothing is in flight any more (frames of the old generation never reach the counters)
    Object.assign(this.counts, { blSub: 0, blOut: 0, blPre: 0, elSub: 0, elOut: 0, elPre: 0 });
    // the WASM workers stay (compiled instance, session): in-flight GOPs are aborted instead of respawning the pool
    this.minPts = null;
    if (this.recon) { for (const l of Object.values(this.recon)) l?.reset(this.reconEpoch = (this.reconEpoch || 0) + 1); this.#makeBuilders(); }
    if (this.exact) { await this.exact.seek(); this.#makeBuilders(); }
    await this.#makeDecoders();
    this.resetting = false;
    this.pump();
  }

  info() {
    const c = this.counts, ex = this.exact?.stats;
    const layer = (exactFlag, hwDec) => exactFlag ? (this.recon ? 'WASM entropy + WebGPU reconstruction (bit exact)' : `WASM HEVC x${this.exactN} (bit exact)`) : hwDec ? `WebCodecs ${hwDec.hw} (${hwDec.codec})` : 'not decoded';
    const dv = this.track.dolbyVision;
    return {
      quality: this.quality, qualityLabel: QUALITY_INFO[this.quality]?.label, psnr: QUALITY_INFO[this.quality]?.psnr,
      kind: this.dvInfo.kind, dvProfile: dv?.profile ?? null, dvLevel: dv?.level ?? null, elKind: this.elKind,
      blPath: layer(this.exactBl, this.dvInfo.blDecoder),
      elPath: this.elKind === 'fel' ? layer(this.exactEl, this.dvInfo.elDecoder) : this.elKind === 'mel' ? 'not decoded (MEL, mapping only)' : dv?.elPresent === false || !dv ? 'none (single layer)' : 'pending',
      tables: this.dovi ? 'web/engine dovi.mjs (aether_fel_gpu_tables + aether_dovi_rpu_display)' : 'none', tablesErr: this.tablesErr || null, rpuErr: this.rpuErr || null, noTables: c.noTables, elMissing: c.elMissing,
      composed: c.composed, upload: this.comp?.stats, presentCpuMsPerFrame: c.composed ? Math.round(c.presentCpuMs / c.composed * 100) / 100 : 0,
      tablesMsPerRpu: c.tablesN ? Math.round(c.tablesMs / c.tablesN * 100) / 100 : 0, rpuReused: c.rpuReused || 0,
      exact: ex ? { ...ex, mismatchedGops: this.exact.mismatches || 0 } : null,
      gpuRecon: this.display.gpuRecon ? { on: !!this.recon, failed: this.gpuFailed || null, bl: (this.recon || this.reconOld)?.bl?.info() ?? null, el: (this.recon || this.reconOld)?.el?.info() ?? null } : null,
      canvasLog: this.canvasLog, canvas: this.comp?.canvasConfig, last: this.lastPresent ? { mode: this.lastPresent.mode, input: this.lastPresent.input, canvas: this.lastPresent.canvas, tone: this.lastPresent.tone } : null,
      dbg: { gops: this.exact?.dbg, bl: this.dbgBl, el: this.dbgEl, elSub: c.elSub, elOut: c.elOut, blOut: c.blOut, blSub: c.blSub, blPre: c.blPre, dqs: this.hw.bl?.decodeQueueSize, blPending: this.blPending.length, dvInq: this.inq.length, needKey: this.needKey, early: this.elEarly.size, elQ: this.elQ.length },
      queue: this.blPending.length, createdFrames: this.createdFrames || 0, closedFrames: this.closedFrames || 0,
    };
  }

  async close() {
    this.closed = true; this.hwGen++;
    for (const l of ['bl', 'el']) if (this.hw[l] && this.hw[l].state !== 'closed') { try { this.hw[l].close(); } catch {} }
    for (const e of this.blPending) { this.#releaseLayer(e.bl); this.#releaseLayer(e.el); }
    for (const f of this.elEarly.values()) this.#releaseLayer(f);
    this.blPending = []; this.elEarly.clear();
    try { await this.exact?.stop(); } catch {}
    if (this.onGpuError) this.device.removeEventListener('uncapturederror', this.onGpuError);
    for (const l of [...Object.values(this.recon || {}), ...Object.values(this.reconOld || {})]) { try { await l?.destroy(); } catch {} }
    if (this.recon || this.reconOld) await new Promise(r => setTimeout(r, 400));   // nested worker teardown, see exact-live.js stop()
    try { this.comp?.dispose(); } catch {}
  }
}
