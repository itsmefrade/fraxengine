// SPDX-License-Identifier: LGPL-3.0-only
// One layer (BL or EL) of the GPU reconstruction path of the exact DV qualities (on by default, ?gpurecon=0 turns it off; web/gpu-recon/INTEGRATION.md):
// an entropy worker (recon-entropy-worker.js) decodes the layer sequentially and posts each access unit's command stream; this
// class reconstructs the pictures with web/gpu-recon GpuRecon on the video worker's device (the FelCompositor's) and hands
// the output pictures, in display order, to the pairing of dv-path.js as {ptsUs, gpu: true, layer, slot, width, height, layout}
// (width x height the conformance window, layout where it sits in the slot: FelCompositor reads it from there, no copy).
//
// DPB slots: the recorder reuses a slot as soon as its picture was output and is no longer a reference, but the player still
// holds an output picture in its display queue. So recorder slots are renamed to physical arena slots here: a Picture event
// takes the lowest physical slot that no recorder slot maps to and no queued picture holds, and its PU records are rewritten to
// the physical slots of their references (a few words per PU, in the payload this worker owns). A held slot is freed by
// release() after the compositor's submit that read it (later reconstruction writes are queue ordered behind that submit).
//
// Presentation order: pictures get the presentation times of their access units in sorted order, as the exact pool does per GOP
// (exact-live.js): Output events come in display order, and RASL pictures the decoder skips (after the first CRA of an epoch or a
// BLA) never enter the list.
import { GpuRecon, EV } from '../../gpu-recon/recon.mjs';
import { stage, stageNow, stageAdd } from './stages.js';

const HEADER_WORDS = 24;
const SEC_PU = 2;
const HELD_RESERVE = 4;   // arena slots for output pictures the recorder has released but the display queue still holds (#sizeArena)

export class ReconLayer {
  /**
   * @param {{name: 'bl'|'el', device: GPUDevice, slots?: number, onOutput: (f) => void, onError: (msg: string) => void, onActivity: () => void}} o
   */
  constructor(o) {
    Object.assign(this, o);
    this.recon = new GpuRecon(this.device, { slots: o.slots || 8 });
    this.epoch = 0; this.seq = 0; this.sent = 0; this.acked = 0; this.finishSent = false; this.finished = false;
    this.events = []; this.pictureEvents = 0;
    this.pts = [];                 // presentation times of access units sent and not yet output, sorted
    this.map = new Map();          // recorder slot -> physical slot
    this.mapped = []; this.held = [];   // per physical slot: a recorder slot names it / a queued picture holds it
    this.geom = [];                // per physical slot: {width, height, crop: [left, right, top, bottom] (chroma units), chroma format}
    this.first = true; this.skipRasl = false;
    this.stats = { pictures: 0, outputs: 0, entropyMs: 0, reconCpuMs: 0, maxSlots: 0, errors: 0, dispatches: 0 };
  }

  /** @param {string} wasmUrl single-thread module; @param {{mtUrl?: string, threads?: number}} [mt] threaded module for WPP rows */
  async start(wasmUrl, mt = {}) {
    this.worker = new Worker(new URL('./recon-entropy-worker.js', import.meta.url), { type: 'module' });
    await new Promise((resolve, reject) => {
      this.worker.onerror = e => reject(new Error('recon worker: ' + (e.message || 'error')));
      this.worker.onmessage = ev => {
        if (ev.data.type === 'ready') { this.threads = ev.data.threads || 1; resolve(); }
        else if (ev.data.type === 'warn') this.warning = ev.data.message;
        else if (ev.data.type === 'error') reject(new Error(ev.data.message));
      };
      this.worker.postMessage({ type: 'init', wasmUrl, mtUrl: mt.mtUrl, threads: mt.threads || 1 });
    });
    this.worker.onerror = e => this.onError('recon worker: ' + (e.message || 'error'));
    this.worker.onmessage = ev => this.#onMsg(ev.data);
    this.reset(0);
    return this;
  }

  /** Seek: a fresh decoder for a new epoch; pictures still held by the display queue stay until released. */
  reset(epoch) {
    this.epoch = epoch; this.seq = 0; this.sent = 0; this.acked = 0; this.finishSent = false; this.finished = false;
    this.events = []; this.pictureEvents = 0; this.pts = [];
    this.map.clear(); this.mapped = [];
    this.first = true; this.skipRasl = false;
    this.worker?.postMessage({ type: 'reset', epoch });
  }

  /** Access units sent but not decoded yet, plus pictures decoded but not reconstructed yet. */
  backlog() { return (this.sent - this.acked) + this.pictureEvents; }
  /** True while sent work has not come out of this layer (a picture can still be on its way). */
  busy() { return this.sent > this.acked || this.events.length > 0 || (this.finishSent && !this.finished); }

  /** One access unit (Annex B, owned: its buffer is transferred) with the first VCL NAL type. */
  feed(ptsUs, annexB, type) {
    const irap = type >= 16 && type <= 23;
    if (irap && this.dpb === undefined) this.#sizeArena(annexB);
    if (irap) { this.skipRasl = this.first || type <= 18; this.first = false; }
    if ((type === 8 || type === 9) && this.skipRasl) return;   // RASL of a CRA that starts the stream or of a BLA: not decodable, never output
    // An end of sequence (or bitstream) NAL makes the next CRA start a new coded video sequence: its RASL pictures are skipped
    // by the decoder like after the first CRA. The NAL closes its access unit, so only the tail is looked at.
    if (endsSequence(annexB)) this.first = true;
    let i = this.pts.length;
    while (i > 0 && this.pts[i - 1] > ptsUs) i--;
    this.pts.splice(i, 0, ptsUs);
    const bytes = annexB.byteOffset === 0 && annexB.byteLength === annexB.buffer.byteLength ? annexB.buffer : annexB.slice().buffer;
    const t = stageNow(), n = bytes.byteLength;
    this.worker.postMessage({ type: 'au', epoch: this.epoch, seq: this.seq++, bytes }, [bytes]);
    this.sent++;
    stage(`video.post.${this.name}`, t, n);
  }

  // The arena is sized before the first picture from the SPS: growing it later copies it while the old one is still alive, and the
  // grows of the first second (6 -> 10 slots one at a time on FEL_TEST_V6) pushed the GPU process peak 490 MB above its steady
  // footprint (1349 against 857 MB; sized up front: 860 to 873). Physical slots in use reach the recorder's (up to
  // sps_max_dec_pic_buffering + 1) plus the pictures only the display queue still holds (at most 4 for the base layer measured on
  // Rangers, FEL_TEST_V6, fel-old-dt, dv-p5: 8, 10, 11, 9 slots with a DPB of 3, 5, 5, 5). A bigger need still grows the arena.
  #sizeArena(annexB) {
    this.dpb = spsMaxDecPicBuffering(annexB);
    if (this.dpb && !this.recon.geom) this.recon.initialSlots = Math.max(this.recon.initialSlots, this.dpb + 1 + HELD_RESERVE);
  }

  finish() {
    if (this.finishSent) return;
    this.finishSent = true;
    this.worker.postMessage({ type: 'finish', epoch: this.epoch });
  }

  #onMsg(m) {
    if (m.epoch !== this.epoch) return;
    if (m.type === 'error') { this.stats.errors++; this.onError(m.message); return; }
    if (m.type !== 'events') return;
    if (m.finished) this.finished = true; else this.acked++;
    this.stats.entropyMs += m.ms || 0;
    // The entropy worker's own time, under this worker's stage table (docs/perf/CORE-V2.md F): `wasm` is the decode itself, `out`
    // the command stream copied out (its bytes), `other` the NAL split and allocation around them.
    if (m.ms != null && !m.finished) {
      stageAdd(`entropy.${this.name}.wasm`, m.pushMs || 0, 1, m.inBytes || 0);
      stageAdd(`entropy.${this.name}.out`, m.takeMs || 0, 1, m.outBytes || 0);
      stageAdd(`entropy.${this.name}.other`, m.ms - (m.pushMs || 0) - (m.takeMs || 0), 1, 0);
    }
    for (const e of m.events) { this.events.push(e); if (e.type === EV.PICTURE) this.pictureEvents++; }
    this.onActivity();
  }

  #phys(recSlot) {
    const p = this.map.get(recSlot);
    if (p === undefined) throw new Error(`${this.name}: recorder slot ${recSlot} holds no picture`);
    return p;
  }
  #assign(recSlot) {
    const old = this.map.get(recSlot);
    if (old !== undefined) { this.map.delete(recSlot); this.mapped[old] = false; }
    let p = 0;
    while (this.mapped[p] || this.held[p]) p++;
    this.mapped[p] = true; this.map.set(recSlot, p);
    this.stats.maxSlots = Math.max(this.stats.maxSlots, p + 1);
    return p;
  }

  /**
   * Applies queued events in order. An Output waits while canOutput() is false (the display queue is full); everything after it
   * waits too. Throws on anything the GPU path cannot reconstruct exactly (the caller falls back to the exact pool).
   */
  process(canOutput) {
    while (this.events.length) {
      const ev = this.events[0], p = ev.payload;
      if (ev.type === EV.OUTPUT && !canOutput()) return;
      this.events.shift();
      if (ev.type === EV.PICTURE) {
        this.pictureEvents--;
        const t = performance.now();
        // References first (the recorder never picks a slot the picture references), then the picture's own slot.
        const pu = p[HEADER_WORDS + 2 * SEC_PU], nPu = p[HEADER_WORDS + 2 * SEC_PU + 1];
        for (let i = 0, o = pu; i < nPu; i++, o += 6) {
          const f = p[o + 1] >>> 16, s = p[o + 2];
          p[o + 2] = ((f & 1 ? this.#phys(s & 0xffff) : 0) | ((f & 2 ? this.#phys(s >>> 16) : 0) << 16)) >>> 0;
        }
        const slot = this.#assign(p[1]);
        p[1] = slot;
        // ?gpufail=N (test only): picture N of this layer gets a 'cpu' verdict, to exercise the fallback to the exact pool
        const r = this.failAt && this.stats.pictures + 1 === this.failAt ? { verdict: 'cpu', picId: p[0], reason: 'gpufail test' } : this.recon.decodePicture(p);
        if (r.verdict !== 'gpu') throw new Error(`${this.name}: picture ${r.picId} needs the CPU (${r.reason})`);
        this.geom[slot] = { width: p[3], height: p[4], crop: [p[10], p[11], p[12], p[13]], cf: (p[5] >>> 16) & 255, depth: p[5] & 0xffff };
        this.stats.pictures++; this.stats.reconCpuMs += performance.now() - t; this.stats.dispatches += r.counts?.intraDispatches ?? 0;
        stage(`video.recon.${this.name}`, t, p.byteLength);
      } else if (ev.type === EV.GENERATED) {
        const slot = this.#assign(p[1]);
        p[1] = slot;
        this.recon.event(EV.GENERATED, p);
        this.geom[slot] = { width: p[3], height: p[4], crop: [0, 0, 0, 0], cf: (p[5] >>> 16) & 255, depth: p[5] & 0xffff };
      } else if (ev.type === EV.OUTPUT) {
        const slot = this.#phys(p[1]), g = this.geom[slot];
        const ptsUs = this.pts.shift();
        if (ptsUs === undefined) throw new Error(`${this.name}: output picture without an access unit`);
        // The compositor reads the slot through its strides and the conformance window (4:2:0: crop offsets are in 2 luma
        // samples), so any coded size works; it needs a displayed width that is a multiple of 4 and an even height, like every input,
        // and 10-bit codes (8-bit codes would be shown as 10-bit ones: the exact pool refuses such streams too).
        const [l, r, t, b] = g.crop, width = g.width - 2 * (l + r), height = g.height - 2 * (t + b);
        if (g.cf !== 1 || g.depth !== (10 | 10 << 8) || (width & 3) || (height & 1) || width <= 0 || height <= 0) {
          throw new Error(`${this.name}: ${g.width}x${g.height} crop ${g.crop} chroma format ${g.cf} bit depth ${g.depth & 255}/${g.depth >> 8} is not a compositor input`);
        }
        const L = this.recon.layout;
        if (!L || L.width !== g.width || L.height !== g.height) throw new Error(`${this.name}: slot ${slot} is not in the arena geometry`);
        const layout = { strideY: L.strideY, strideC: L.strideC, offU: L.offCb * 2, offV: L.offCr * 2, x: 2 * l, y: 2 * t };
        this.held[slot] = true;
        this.stats.outputs++;
        this.onOutput({ ptsUs, gpu: true, layer: this, slot, width, height, layout });
      } else if (ev.type === EV.DISCARD) {
        this.pts.shift();
      }
    }
  }

  /** The compositor input of an output picture (resolved at present time: the arena may have grown since). */
  input(f) { return { gpu: this.recon.slotResource(f.slot), width: f.width, height: f.height, layout: f.layout }; }
  /** The picture's slot may be overwritten (call after the present that read it was submitted, or when it is dropped). */
  release(f) {
    if (f.layer !== this) return;
    this.held[f.slot] = false;
    if (this.stopped && !this.held.some(Boolean)) this.#freeArena();   // after a fallback: the last queued picture is gone
  }
  #freeArena() { if (!this.freed) { this.freed = true; try { this.recon.destroy(); } catch {} } }

  info() {
    const s = this.stats, n = s.pictures || 1;
    return { threads: this.threads || 1, warning: this.warning || null, pictures: s.pictures, outputs: s.outputs, entropyMsPerPic: Math.round(s.entropyMs / n * 100) / 100, reconCpuMsPerPic: Math.round(s.reconCpuMs / n * 100) / 100, intraDispatchesPerPic: Math.round(s.dispatches / n),
      slots: this.recon.nSlots, maxSlots: s.maxSlots, dpb: this.dpb || 0, arenaMB: this.freed ? 0 : this.recon.geom ? Math.round(this.recon.nSlots * this.recon.geom.slotBytes / 1048576) : 0, backlog: this.backlog() };
  }

  /** Stops the entropy worker; the arena stays until destroy() (queued pictures may still read it). */
  // Cooperative shutdown as in exact-live.js stop(): a nested worker terminated mid teardown could keep its parent's thread alive.
  async stop() {
    const w = this.worker; this.worker = null; this.events = []; this.pictureEvents = 0; this.stopped = true;
    if (!this.held.some(Boolean)) this.#freeArena();
    if (!w) return;
    await new Promise(res => {
      const t = setTimeout(res, 1500);
      w.onmessage = ev => { if (ev.data.type === 'bye') { clearTimeout(t); res(); } };
      w.onerror = () => { clearTimeout(t); res(); };
      try { w.postMessage({ type: 'quit' }); } catch { clearTimeout(t); res(); }
    });
    w.terminate();
  }
  async destroy() { await this.stop(); this.#freeArena(); }
}

// True when the access unit ends with an EOS (36) or EOB (37) NAL: a start code followed by header 0x48 or 0x4a near the end.
function endsSequence(b) {
  for (let i = Math.max(0, b.length - 16); i + 4 < b.length; i++) {
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1 && (b[i + 3] === 0x48 || b[i + 3] === 0x4a)) return true;
  }
  return false;
}

// sps_max_dec_pic_buffering_minus1 + 1 of the highest sub-layer of the first SPS (NAL type 33) in an Annex B access unit, 0 when
// there is none or it does not parse.
function spsMaxDecPicBuffering(b) {
  let at = -1;
  for (let i = 0; i + 4 < b.length; i++) if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1 && ((b[i + 3] >> 1) & 63) === 33) { at = i + 5; break; }
  if (at < 0) return 0;
  const r = [];
  for (let i = at, z = 0; i < b.length && r.length < 600; i++) {
    if (z >= 2 && b[i] === 3) { z = 0; continue; }
    if (z >= 2 && b[i] <= 1) break;                       // next start code
    r.push(b[i]); z = b[i] === 0 ? z + 1 : 0;
  }
  let p = 0;
  const u = n => { let v = 0; for (let k = 0; k < n; k++, p++) v = v * 2 + ((r[p >> 3] >> (7 - (p & 7))) & 1); return v; };
  const ue = () => { let z = 0; while (p < r.length * 8 && !u(1)) z++; return z > 31 ? -1 : 2 ** z - 1 + u(z); };
  u(4); const maxSub = u(3); u(1);
  p += 88 + 8;                                            // general profile, tier, level
  const present = [];
  for (let i = 0; i < maxSub; i++) present.push([u(1), u(1)]);
  if (maxSub > 0) p += 2 * (8 - maxSub);
  for (const [pp, lp] of present) p += (pp ? 88 : 0) + (lp ? 8 : 0);
  ue(); const cf = ue(); if (cf === 3) u(1);
  ue(); ue();
  if (u(1)) { ue(); ue(); ue(); ue(); }
  ue(); ue(); ue();
  const ordering = u(1);
  let d = 0;
  for (let i = ordering ? 0 : maxSub; i <= maxSub; i++) { d = ue() + 1; ue(); ue(); }
  return p <= r.length * 8 && d > 0 && d <= 16 ? d : 0;
}
