// Driver for intra.wgsl.mjs: dependency analysis, wave scheduling and dispatch (layouts in FORMAT-intra.md).
import { intraWgsl, INTRA_WG, INTRA_MAXT, INTRA_REFCAP } from './intra.wgsl.mjs';

export const TU_WORDS = 4;
export const NO_RESIDUAL = 0xffffffff;
// Pairs per workgroup in 'wave' mode: enough work per workgroup without starving the GPU of workgroups.
const WAVE_PAIRCAP = 1024;
// Rounds per dispatch of the 'list' schedule (see analyze). Measured on a 4K I frame (g-4k-i): see FORMAT-intra.md.
export const LIST_ROUNDS = 8;

export function packTu(out, at, { x, y, log2, plane, mode, avail, edgeFiltersOff = false, cip = false, resOffset = NO_RESIDUAL, wave = 0 }) {
  out[at] = (x | (y << 16)) >>> 0;
  out[at + 1] = (log2 | (plane << 3) | (mode << 5) | (avail << 11) | (edgeFiltersOff << 16) | (cip << 17)) >>> 0;
  out[at + 2] = resOffset >>> 0;
  out[at + 3] = wave >>> 0;
}

// TU records of this format from G0's reconstruction stream (FORMAT-STREAM.md): `intra` is the picture's intra section
// (6 words per block: one record covers Cb and Cr), `tus` its TU section (6-word FORMAT-idct.md records, word 4 =
// resOffset). The stream's wave goes to word 3; its availability bit order (0 up-left, 1 up, 2 up-right, 3 left,
// 4 bottom-left) is remapped to this format's.
export function fromStreamIntra(intra, tus) {
  const n = intra.length / 6;
  let m = 0;
  for (let r = 0; r < n; r++) m += (intra[r * 6 + 1] & 3) ? 2 : 1;
  const out = new Uint32Array(m * TU_WORDS);
  const resOf = i => (i === 0xffffffff ? NO_RESIDUAL : tus[i * 6 + 4]);
  let k = 0;
  for (let r = 0; r < n; r++) {
    const xy = intra[r * 6], w1 = intra[r * 6 + 1], wave = intra[r * 6 + 2];
    const c = w1 & 3, log2 = (w1 >>> 2) & 7, mode = (w1 >>> 5) & 63, a = (w1 >>> 11) & 31;
    const avail = ((a >> 4) & 1) | (((a >> 3) & 1) << 1) | ((a & 1) << 2) | (((a >> 1) & 1) << 3) | (((a >> 2) & 1) << 4);
    const flags = (((w1 >>> 17) & 1) << 16) | (((w1 >>> 16) & 1) << 17);
    const planes = c ? [1, 2] : [0];
    planes.forEach((p, j) => {
      out[k * 4] = xy;
      out[k * 4 + 1] = (log2 | (p << 3) | (mode << 5) | (avail << 11) | flags) >>> 0;
      out[k * 4 + 2] = resOf(intra[r * 6 + 3 + j]);
      out[k * 4 + 3] = wave;
      k++;
    });
  }
  return out;
}

// Params for one picture. `pic`: { width, height, bitDepthY, bitDepthC, strongIntraSmoothing, intraSmoothingDisabled }.
export function pictureLayout(width, height) {
  const align64 = v => (v + 63) & ~63;
  const strideY = align64(width), strideC = align64(width >> 1);
  const offCb = (strideY * height) >> 1;
  const offCr = offCb + ((strideC * (height >> 1)) >> 1);
  const words = offCr + ((strideC * (height >> 1)) >> 1);
  return { width, height, strideY, strideC, offCb, offCr, words };
}

// Dependency analysis in decoding order (one pass, O(perimeter / 4) per TU). A TU reads the reconstructed samples its
// availability bits and the picture size allow (build_reference_samples); every 4x4 block of those regions that an
// earlier intra TU of the picture wrote is a dependency. Inter samples are final before this stage (wave 0).
// Returns per TU: global wave (1 + max wave of the TUs read), the CTU, the local wave inside the CTU, and per CTU
// its level (1 + max level of other CTUs read; only CTUs that hold intra TUs get one). Also the 'list' schedule slot
// (dispatch, round): as early as possible, a TU runs after the TUs it reads, in a later round of the same dispatch if
// they belong to its CTU (same workgroup), in a later dispatch otherwise; at most `rounds` rounds per dispatch.
export function analyze(tus, width, height, log2Ctb, rounds = LIST_ROUNDS) {
  const n = tus.length / TU_WORDS;
  const gw = [width >> 2, width >> 3, width >> 3];
  const gh = [height >> 2, height >> 3, height >> 3];
  const grid = [new Int32Array(gw[0] * gh[0]).fill(-1), new Int32Array(gw[1] * gh[1]).fill(-1), new Int32Array(gw[2] * gh[2]).fill(-1)];
  const wave = new Uint32Array(n), local = new Uint32Array(n), ctuOf = new Uint32Array(n);
  const wCtb = (width + (1 << log2Ctb) - 1) >> log2Ctb, hCtb = (height + (1 << log2Ctb) - 1) >> log2Ctb;
  const level = new Uint32Array(wCtb * hCtb);
  const disp = new Uint32Array(n), round = new Uint32Array(n);
  let maxWave = 0, maxDisp = 0;
  for (let t = 0; t < n; t++) {
    const xy = tus[t * 4], info = tus[t * 4 + 1];
    const x0 = xy & 0xffff, y0 = xy >>> 16, log2 = info & 7, s = 1 << log2, c = (info >>> 3) & 3, av = (info >>> 11) & 31;
    const pw = c ? width >> 1 : width, ph = c ? height >> 1 : height, sh = c ? 1 : 0;
    const ctu = (((y0 << sh) >> log2Ctb) * wCtb) + ((x0 << sh) >> log2Ctb);
    ctuOf[t] = ctu;
    const g = grid[c], W = gw[c];
    let w = 0, lw = 0, lv = 1, sd = 0, sr = 0;
    const dep = d => {
      if (d < 0) return;
      if (wave[d] > w) w = wave[d];
      let qd = disp[d], qr = round[d] + 1;
      if (ctuOf[d] === ctu) { if (local[d] + 1 > lw) lw = local[d] + 1; }
      else { qd++; qr = 0; if (level[ctuOf[d]] + 1 > lv) lv = level[ctuOf[d]] + 1; }
      if (qd > sd || (qd === sd && qr > sr)) { sd = qd; sr = qr; }
    };
    if (x0 > 0) {
      let first = s, rows = 0;
      if (av & 2) { first = 0; rows = s; }
      if (av & 1) rows = s + Math.min(Math.max(ph - y0 - s, 0), s);
      const bx = (x0 - 1) >> 2;
      for (let r = first; r < rows; r += 4) dep(g[((y0 + r) >> 2) * W + bx]);
      if ((av & 4) && y0 > 0) dep(g[((y0 - 1) >> 2) * W + bx]);
    }
    if (y0 > 0) {
      const row = ((y0 - 1) >> 2) * W;
      if (av & 8) for (let cc = 0; cc < s; cc += 4) dep(g[row + ((x0 + cc) >> 2)]);
      if (av & 16) {
        const cnt = Math.min(Math.max(pw - x0 - s, 0), s);
        for (let cc = 0; cc < cnt; cc += 4) dep(g[row + ((x0 + s + cc) >> 2)]);
      }
    }
    wave[t] = w + 1;
    local[t] = lw;
    if (sr >= rounds) { sd++; sr = 0; }
    disp[t] = sd; round[t] = sr;
    if (sd > maxDisp) maxDisp = sd;
    if (lv > level[ctu]) level[ctu] = lv;
    if (w + 1 > maxWave) maxWave = w + 1;
    for (let by = y0 >> 2; by < (y0 + s) >> 2; by++) g.fill(t, by * W + (x0 >> 2), by * W + ((x0 + s) >> 2));
  }
  return { wave, local, ctuOf, level, maxWave, disp, round, maxDisp, rounds };
}

// Build the GPU schedule. mode 'list' (default): one workgroup per CTU and dispatch, the CTU's TUs of that dispatch in
// rounds (analyze()'s slots), one step per round. mode 'ctu': one dispatch per CTU level, one workgroup per CTU holding
// intra TUs, one step per local wave (CTU wavefront: fewest dispatches, but a CTU waits for the whole of the CTUs it
// reads). mode 'wave': one dispatch per global wave, TUs of the wave packed into independent workgroups (one dispatch
// per wave). The kernel is the same for all three; they differ in dispatch count against serial steps.
// Returns { sched (Uint32Array), stepBase, tuBase, dispatches: [{ groupBase, groupCount }], stats }.
export function buildSchedule(tus, width, height, log2Ctb, mode = 'list', analysis = null) {
  const a = analysis ?? analyze(tus, width, height, log2Ctb);
  const n = tus.length / TU_WORDS;
  const refsOf = t => 4 * (1 << (tus[t * 4 + 1] & 7)) + 1;
  const pairsOf = t => { const s = 1 << (tus[t * 4 + 1] & 7); return (s * s) >> 1; };
  const groups = [], steps = [], order = [], dispatches = [];
  // Split an ordered list of independent TUs into steps that fit the workgroup memory.
  const pushSteps = (list, pairCap) => {
    let st = [], refs = 0, pairs = 0;
    const flush = () => { if (st.length) { steps.push(st); st = []; refs = 0; pairs = 0; } };
    for (const t of list) {
      const r = refsOf(t), p = pairsOf(t);
      if (st.length === INTRA_MAXT || refs + r > INTRA_REFCAP || (pairCap && st.length && pairs + p > pairCap)) flush();
      st.push(t); refs += r; pairs += p;
    }
    flush();
  };
  if (mode === 'list') {
    // Bucket by dispatch, then CTU (first appearance in decoding order), then round.
    const perDisp = Array.from({ length: n ? a.maxDisp + 1 : 0 }, () => new Map());
    for (let t = 0; t < n; t++) {
      const m = perDisp[a.disp[t]];
      let b = m.get(a.ctuOf[t]);
      if (!b) { b = []; m.set(a.ctuOf[t], b); }
      (b[a.round[t]] ??= []).push(t);
    }
    for (const m of perDisp) {
      if (!m.size) continue;
      const groupBase = groups.length;
      for (const b of m.values()) {
        const s0 = steps.length;
        for (const r of b) if (r) pushSteps(r, 0);
        groups.push([s0, steps.length - s0]);
      }
      dispatches.push({ groupBase, groupCount: groups.length - groupBase });
    }
  } else if (mode === 'ctu') {
    const byLevel = new Map();
    const ctuTus = new Map();
    for (let t = 0; t < n; t++) {
      const c = a.ctuOf[t];
      if (!ctuTus.has(c)) { ctuTus.set(c, []); const L = a.level[c]; if (!byLevel.has(L)) byLevel.set(L, []); byLevel.get(L).push(c); }
      ctuTus.get(c).push(t);
    }
    for (const L of [...byLevel.keys()].sort((p, q) => p - q)) {
      const groupBase = groups.length;
      for (const c of byLevel.get(L)) {
        const list = ctuTus.get(c);
        let maxL = 0;
        for (const t of list) if (a.local[t] > maxL) maxL = a.local[t];
        const buckets = Array.from({ length: maxL + 1 }, () => []);
        for (const t of list) buckets[a.local[t]].push(t);
        const s0 = steps.length;
        for (const b of buckets) pushSteps(b, 0);
        groups.push([s0, steps.length - s0]);
      }
      dispatches.push({ groupBase, groupCount: groups.length - groupBase });
    }
  } else {
    const buckets = Array.from({ length: a.maxWave + 1 }, () => []);
    for (let t = 0; t < n; t++) buckets[a.wave[t]].push(t);
    for (let w = 1; w <= a.maxWave; w++) {
      if (!buckets[w].length) continue;
      const groupBase = groups.length, s0 = steps.length;
      pushSteps(buckets[w], WAVE_PAIRCAP);
      for (let s = s0; s < steps.length; s++) groups.push([s, 1]);
      dispatches.push({ groupBase, groupCount: groups.length - groupBase });
    }
  }
  const stepBase = groups.length * 3;
  const tuBase = stepBase + steps.length * 4;
  const sched = new Uint32Array(tuBase + n * TU_WORDS);
  let at = 0, maxSteps = 0, critical = 0;
  const stepTu = new Uint32Array(steps.length);
  steps.forEach((st, i) => {
    let refs = 0, pairs = 0;
    const b = stepBase + 4 * i;
    stepTu[i] = at;
    sched[b] = at;
    sched[b + 1] = st.length;
    for (const t of st) {
      const o = tuBase + at * 4;
      sched[o] = tus[t * 4]; sched[o + 1] = tus[t * 4 + 1]; sched[o + 2] = tus[t * 4 + 2];
      sched[o + 3] = (refs | (pairs << 16)) >>> 0;
      refs += refsOf(t); pairs += pairsOf(t); at++;
    }
    sched[b + 2] = refs;
    sched[b + 3] = pairs;
  });
  groups.forEach(([s, c], i) => { sched[3 * i] = s; sched[3 * i + 1] = c; sched[3 * i + 2] = stepTu[s]; });
  for (const d of dispatches) {
    let m = 0;
    for (let g = d.groupBase; g < d.groupBase + d.groupCount; g++) m = Math.max(m, groups[g][1]);
    maxSteps = Math.max(maxSteps, m);
    critical += m;
  }
  const stats = { mode, tus: n, dispatches: dispatches.length, groups: groups.length, steps: steps.length, maxStepsPerGroup: maxSteps, criticalSteps: critical, maxWave: a.maxWave };
  return { sched, stepBase, tuBase, dispatches, stats, analysis: a };
}

export class IntraStage {
  // `code` overrides the shader source (test page experiments only).
  constructor(device, { code = intraWgsl } = {}) {
    this.device = device;
    this.module = device.createShaderModule({ code, label: 'intra' });
    this.bgl = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'uniform' } },
        { binding: 1, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 2, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 4, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 5, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'uniform', hasDynamicOffset: true } },
      ],
    });
    this.pipeline = device.createComputePipeline({
      layout: device.createPipelineLayout({ bindGroupLayouts: [this.bgl] }),
      compute: { module: this.module, entryPoint: 'main' },
    });
    this.params = device.createBuffer({ size: 64, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    this.align = device.limits.minUniformBufferOffsetAlignment;
    this.disp = null;
    this.sched = null;
  }

  #ensure(name, size, usage) {
    if (!this[name] || this[name].size < size) {
      this[name]?.destroy();
      this[name] = this.device.createBuffer({ size: Math.max(256, size), usage });
    }
    return this[name];
  }

  // Upload one picture's schedule. `pic`: { width, height, bitDepthY, bitDepthC, strongIntraSmoothing,
  // intraSmoothingDisabled }. Buffers: `picture` (read_write u32), `residual` (i32), `intraMap` (u32 bits).
  prepare(pic, schedule, { picture, residual, intraMap }) {
    const L = pictureLayout(pic.width, pic.height);
    const p = new Uint32Array(16);
    p.set([pic.width, pic.height, pic.width >> 1, pic.height >> 1, L.strideY, L.strideC, L.offCb, L.offCr,
      pic.bitDepthY, pic.bitDepthC, (pic.strongIntraSmoothing ? 1 : 0) | (pic.intraSmoothingDisabled ? 2 : 0),
      pic.width >> 2, schedule.stepBase, schedule.tuBase, 0, 0]);
    const q = this.device.queue;
    q.writeBuffer(this.params, 0, p);
    const sched = this.#ensure('sched', schedule.sched.byteLength, GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST);
    q.writeBuffer(sched, 0, schedule.sched);
    const nd = Math.max(1, schedule.dispatches.length);
    const disp = this.#ensure('disp', nd * this.align, GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST);
    const d = new Uint32Array((nd * this.align) / 4);
    schedule.dispatches.forEach((x, i) => { d[(i * this.align) / 4] = x.groupBase; d[(i * this.align) / 4 + 1] = x.groupCount; });
    q.writeBuffer(disp, 0, d);
    // Bind groups are cached per buffer set (creating one per frame is measurable in the GPU process), one per
    // picture target: `picture` is a GPUBuffer or a binding resource {buffer, offset, size} (a DPB arena slot), and
    // a host that alternates between targets passes the same object for the same target.
    const key = [residual, intraMap, sched, disp];
    if (!this.bgKey || this.bgKey.some((b, i) => b !== key[i])) {
      this.bgKey = key;
      this.bgs = new Map();
    }
    let bg = this.bgs.get(picture);
    if (!bg) {
      bg = this.device.createBindGroup({
        layout: this.bgl,
        entries: [
          { binding: 0, resource: { buffer: this.params } },
          { binding: 1, resource: picture.buffer ? picture : { buffer: picture } },
          { binding: 2, resource: { buffer: sched } },
          { binding: 3, resource: { buffer: residual } },
          { binding: 4, resource: { buffer: intraMap } },
          { binding: 5, resource: { buffer: disp, size: 16 } },
        ],
      });
      this.bgs.set(picture, bg);
    }
    this.bg = bg;
    this.dispatches = schedule.dispatches;
  }

  // Record the dispatches into an open compute pass (one dispatch per schedule level; WebGPU orders storage writes
  // between dispatches of one pass).
  encode(pass) {
    pass.setPipeline(this.pipeline);
    this.dispatches.forEach((d, i) => {
      pass.setBindGroup(0, this.bg, [i * this.align]);
      const x = Math.min(d.groupCount, 65535), y = Math.ceil(d.groupCount / 65535);
      pass.dispatchWorkgroups(x, y);
    });
  }
}

export { INTRA_WG, INTRA_MAXT, INTRA_REFCAP };
