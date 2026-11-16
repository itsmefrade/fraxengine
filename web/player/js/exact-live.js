// SPDX-License-Identifier: LGPL-3.0-only
// Live GOP-parallel bit exact HEVC decode for the compositor path. Reuses web/exact/worker-ref.js (aether-hevc, bit exact; one WASM instance per worker,
// whole GOP per task, frames in display order) but, unlike web/exact/stream.js, takes its GOPs from a running demuxer instead of
// a fixture index: submit({layer, bytes (Annex B), ptsList}) per GOP, then take(layer) returns the next display order frame.
// Workers on a GOP the consumer has not reached are paused while more than budgetMB of decoded frames wait.
export class LiveExact {
  constructor({ n = 4, budgetMB = 1024 } = {}) {
    this.n = n; this.budget = budgetMB * 1048576;
    this.workers = []; this.tasks = []; this.queue = []; this.nextId = 0;
    this.layers = {}; this.buffered = 0; this.failed = null;
    this.stats = { decoded: 0, peakBufferedMB: 0, gops: 0, wasmMB: 0, workerInitMs: 0, decodeMsPerFrame: 0 };
    this.wake = null;
    this.minTask = 0;     // tasks below this id belong to a position that was left by seek(): their frames and results are ignored
    this.seeks = 0;
  }
  async start() {
    const t = performance.now();
    await new Promise((resolve, reject) => {
      let left = this.n;
      for (let i = 0; i < this.n; i++) {
        const w = new Worker(new URL('../../exact/worker-ref.js', import.meta.url), { type: 'module' });
        w.task = null; w.run = true; w.id = i;
        w.onerror = e => { this.#fail('exact worker: ' + (e.message || 'error')); reject(new Error(this.failed)); };
        w.onmessage = ev => { if (ev.data.type === 'ready' && --left === 0) resolve(); };
        this.workers.push(w);
        w.postMessage({ type: 'init', id: i, wasmUrl: new URL('../../exact/wasm/aether_hevc_simd.wasm', import.meta.url).href });
      }
    });
    for (const w of this.workers) w.onmessage = ev => this.#onMsg(w, ev.data);
    this.stats.workerInitMs = Math.round(performance.now() - t);
    return this;
  }
  #fail(m) { this.failed ??= m; this.wake?.(); }
  #layer(l) { return this.layers[l] ??= { order: [], cur: 0 }; }
  /** GOPs of one layer are delivered in submission order. ptsList: presentation times of the GOP's access units. */
  submit({ layer, bytes, ptsList, skip = 0 }) {
    const t = { taskId: this.nextId++, layer, bytes, skip, ptsList: ptsList.slice().sort((a, b) => a - b), frames: [], delivered: 0, done: false, count: 0 };
    this.#layer(layer).order.push(t);
    this.tasks.push(t); this.queue.push(t); this.stats.gops++;
    (this.dbg ??= []).length < 6 && this.dbg.push({ layer, n: t.ptsList.length, first: t.ptsList[0], last: t.ptsList.at(-1), skip, bytes: bytes.length });
    this.#assign();
  }
  #assign() {
    for (const w of this.workers) {
      if (w.task || w.aborting != null || !this.queue.length) continue;
      const t = this.queue.shift();
      w.task = t; w.run = true;
      const b = t.bytes; t.bytes = null;
      w.postMessage({ type: 'task', taskId: t.taskId, layer: t.layer, gop: t.taskId, bytes: b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength), hash: false, skip: t.skip }, []);
    }
    this.#gates();
  }
  #gates() {
    for (const w of this.workers) {
      if (!w.task || w.aborting != null) continue;
      const L = this.#layer(w.task.layer);
      const current = L.order[L.cur] === w.task;
      const flag = current || this.buffered < this.budget;
      if (w.run !== flag) { w.run = flag; w.postMessage({ type: 'run', taskId: w.task.taskId, run: flag }); }
    }
  }
  /**
   * Repositions without respawning workers (same protocol as web/exact ExactStream.seek): the tasks in flight are aborted (a worker
   * finishes the NAL it is decoding, then answers 'aborted' and keeps its compiled WASM instance), queued GOPs and decoded frames are
   * dropped. Resolves, with the abort time in ms, once every worker is idle again; GOPs submitted afterwards belong to the new position.
   */
  seek() {
    if (this.failed) return Promise.reject(new Error(this.failed));
    const t0 = performance.now(); this.seeks++;
    this.minTask = this.nextId;
    this.layers = {}; this.queue = []; this.tasks = []; this.buffered = 0; this.dbg = [];
    for (const w of this.workers) {
      if (w.task && w.aborting == null) { w.aborting = w.task.taskId; w.postMessage({ type: 'abort', taskId: w.task.taskId }); }
    }
    return new Promise(resolve => { this.seekDone = { t0, resolve }; this.#settleSeek(); });
  }
  #settleSeek() {
    const d = this.seekDone;
    if (!d || this.workers.some(w => w.aborting != null)) return;
    this.seekDone = null;
    this.stats.lastSeekAbortMs = Math.round((performance.now() - d.t0) * 10) / 10;
    d.resolve(this.stats.lastSeekAbortMs);
    this.#assign();
  }
  #onMsg(w, m) {
    if (m.type === 'aborted' || (m.type === 'taskdone' && w.aborting === m.taskId && !m.error)) {
      // acknowledgement of an abort (a task that finished just before the abort arrived counts too): the worker is idle again
      if (w.aborting === m.taskId) { w.aborting = null; w.task = null; this.#settleSeek(); }
      return;
    }
    if (m.taskId !== undefined && m.taskId < this.minTask) return;   // frame or result of a task from before the last seek()
    if (m.type === 'frame') {
      const t = w.task; if (!t) return;
      t.frames[m.k] = { Y: new Uint16Array(m.Y), U: new Uint16Array(m.U), V: new Uint16Array(m.V), width: m.w, height: m.h, poc: m.poc, bytes: m.bytes, src: w };
      this.buffered += m.bytes; this.stats.decoded++;
      this.stats.peakBufferedMB = Math.max(this.stats.peakBufferedMB, Math.round(this.buffered / 1048576));
      this.wake?.(); this.#gates();
    } else if (m.type === 'taskdone') {
      const t = w.task; w.task = null;
      if (m.error) { this.#fail(m.error); return; }
      t.done = true; t.count = m.count;
      if (m.count !== t.ptsList.length) { t.mismatch = `${m.count} frames for ${t.ptsList.length} packets`; this.mismatches = (this.mismatches || 0) + 1; }
      this.stats.wasmMB = Math.max(this.stats.wasmMB, Math.round(m.wasmBytes / 1048576));
      this.stats.decodeMsPerFrame = m.count ? Math.round(m.wallMs / m.count * 10) / 10 : 0;
      // Work per delivered picture: prefix pictures decoded only to be dropped, and NALs the open-GOP plan left out.
      const w2 = (this.stats.work ??= { tasks: 0, prefixTasks: 0, planned: 0, skippedPictures: 0, deliveredPictures: 0, nals: 0, keptNals: 0, wallMs: 0 });
      w2.tasks++; w2.wallMs += m.wallMs || 0; w2.deliveredPictures += m.count || 0; w2.skippedPictures += m.skipped || 0;
      w2.nals += m.nals || 0; w2.keptNals += m.keptNals || 0; if (m.skipped) w2.prefixTasks++; if (m.planned) w2.planned++;
      this.wake?.(); this.#assign();
    }
  }
  /** True while the decode-ahead budget holds the pool back: no more lookahead will come until pictures are taken. */
  budgetFull() { return this.buffered >= this.budget * 0.9; }
  /** Hands the plane buffers of a taken frame back to the worker that decoded it (after they were uploaded or dropped). */
  recycle(f) {
    if (!f?.planes || !f.src || this.stopped) return;
    const bufs = [f.planes.Y.buffer, f.planes.U.buffer, f.planes.V.buffer].filter(b => b.byteLength);
    f.planes = null;
    try { f.src.postMessage({ type: 'recycle', bufs }, bufs); } catch {}
  }
  /** Next frame of `layer` in display order: {ptsUs, planes:{Y,U,V}, width, height} or null when none is ready yet. */
  take(layer) {
    if (this.failed) throw new Error(this.failed);
    const L = this.#layer(layer);
    for (;;) {
      const t = L.order[L.cur];
      if (!t) return null;
      const k = t.delivered;
      const f = t.frames[k];
      if (f) {
        t.frames[k] = null; t.delivered++; this.buffered -= f.bytes;
        const ptsUs = t.ptsList[Math.min(k, t.ptsList.length - 1)];
        if (t.delivered >= t.ptsList.length || (t.done && t.delivered >= t.count)) { L.cur++; this.#gates(); }
        this.#gates();
        return { ptsUs, planes: { Y: f.Y, U: f.U, V: f.V }, width: f.width, height: f.height, src: f.src };
      }
      if (t.done && t.delivered >= t.count) { L.cur++; continue; }
      return null;
    }
  }
  /** Frames of `layer` decoded and waiting, contiguous from the next one `take` would deliver (same meaning as ExactStream.lookahead in web/exact/stream-ref.js). */
  lookahead(layer) {
    const L = this.layers[layer]; if (!L) return 0;
    let n = 0;
    for (let g = L.cur; g < L.order.length; g++) {
      const t = L.order[g];
      let k = t.delivered;
      for (; t.frames[k]; k++) n++;
      if (!(t.done && k >= t.count)) return n;   // this GOP is still decoding (or has a gap): later GOPs cannot be delivered yet
    }
    return n;
  }
  /** Frames queued or decoding for a layer (GOPs submitted but not fully delivered). */
  pendingGops(layer) { const L = this.#layer(layer); return L.order.length - L.cur; }
  /** Resolves once every worker confirmed shutdown (or after 1.5 s), then terminates them; callers that terminate their own worker afterwards must await it. */
  async stop() {
    const ws = this.workers; this.stopped = true;
    this.workers = []; this.layers = {}; this.queue = []; this.buffered = 0; this.seekDone?.resolve(0); this.seekDone = null;
    await Promise.all(ws.map(w => new Promise(res => {
      const t = setTimeout(res, 1500);
      w.onmessage = ev => { if (ev.data.type === 'bye') { clearTimeout(t); res(); } };
      w.onerror = () => { clearTimeout(t); res(); };
      try { w.postMessage({ type: 'quit' }); } catch { clearTimeout(t); res(); }
    })));
    for (const w of ws) w.terminate();
    // A nested worker needs a moment to finish tearing down; terminating the parent (the video worker) while that is in flight
    // left the parent's thread alive for good (an empty-URL worker target, about 1 in 2 High-mode stops). 400 ms removed it in 12 of 12 runs.
    await new Promise(r => setTimeout(r, 400));
  }
}
