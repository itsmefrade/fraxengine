// GpuRecon: reconstructs one HEVC picture per call on the GPU from G0's command stream (FORMAT-STREAM.md), in one
// command encoder, one compute pass and one submit:
//
//   [bS from edge codes] -> MC (inter PUs) -> PCM -> IDCT (all TUs) -> add-residual (inter TUs) -> intra waves
//   -> deblock V, H -> SAO
//
// A stream of version 2 (header flag bit 6) carries deblocking edge codes in the block map instead of boundary
// strengths; the bS kernel (bs.wgsl.mjs) resolves them in place from the PU and TU records first.
//
// DPB: one arena storage buffer, slot s at s * slotWords words (FORMAT-mc.md), every slot the README picture layout
// with slotWords rounded up to the storage binding offset alignment so a slot can also be bound on its own
// ({buffer: arena, offset, size}) by the stages whose kernels address one picture from word 0 (intra, deblock, SAO,
// add-residual).
//
// Where a picture is reconstructed: SAO cannot filter in place (it reads deblocked neighbours), and a buffer cannot be
// bound read only and writable in one dispatch, so a picture that uses SAO is reconstructed in a separate work
// buffer (MC in its separateDst mode: arena read only, work writable), deblocked there, and SAO writes the finished
// picture straight into its DPB slot. A picture without SAO is reconstructed directly in its slot (MC with
// dstBase = slot * slotWords, the arena writable). Neither path copies a picture. The work buffer is overwritten by
// every picture (every sample belongs to a PU, a TU or a PCM CU), so it needs no clearing.
//
// Bind groups are created once per (buffer generation, target) and reused; a generation changes only when a record
// buffer has to grow or the geometry changes. Per picture: one writeBuffer per non-empty section, the small uniform
// blocks, and the intra schedule: sections 13 and 14 of the stream (G0 computes it while decoding); only a stream
// without them makes this file run analyze + buildSchedule in JS (15 to 55 ms on a 4K I picture).
//
// Pictures the kernels cannot reconstruct exactly (FORMAT-STREAM.md "Unsupported pictures" and every "not covered"
// case of the stage formats) return verdict 'cpu' and leave the GPU untouched; the host then decodes that picture on
// the CPU and hands the result back with uploadPicture(slot, planes) if it is used as a reference.
import { IdctPipeline, LIST_WORDS } from './idct.mjs';
import { GpuMc, pictureLayout, partitionRecords } from './mc.mjs';
import { IntraStage, buildSchedule, fromStreamIntra } from './intra.mjs';
import { createDeblock, makeParams } from './deblock.mjs';
import { createBs, makeBsParams, cellsBytes } from './bs.mjs';
import { createSao } from './sao.mjs';
import { addResidualWgsl, ADD_RESIDUAL_WG } from './add-residual.wgsl.mjs';

export const STREAM_MAGIC = 0x31534352; // "RCS1"
/** Stream file versions this reader accepts (1: block map with strengths, 2: with edge codes, header flag bit 6). */
export const STREAM_VERSIONS = [1, 2];
export const EV = { PICTURE: 1, GENERATED: 2, OUTPUT: 3, DISCARD: 4 };
export const SEC = { TU: 0, COEFF: 1, PU: 2, WEIGHTS: 3, INTRA: 4, PCM: 5, PCM_SAMPLES: 6, BLOCK_MAP: 7, DEBLOCK_CTB: 8, SAO: 9, CTB_INFO: 10, SCALING: 11, INTRA_MAP: 12, INTRA_SCHED: 13, INTRA_DISPATCH: 14 };
const SECTIONS = 15;
const HEADER_WORDS = 24;
const NONE = 0xffffffff;
const MAX_GROUPS = 65535;

/** A device with the limits the arena needs (FORMAT-mc.md: a 4K 10-bit slot is 24.9 MB). */
export async function requestReconDevice(adapter = null, { timestamps = false } = {}) {
  adapter ??= await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  const requiredFeatures = timestamps && adapter.features.has('timestamp-query') ? ['timestamp-query'] : [];
  const L = adapter.limits;
  return adapter.requestDevice({
    requiredFeatures,
    requiredLimits: { maxStorageBufferBindingSize: L.maxStorageBufferBindingSize, maxBufferSize: L.maxBufferSize },
  });
}

/** Iterates the events of a stream file (`stream.rcs`): yields {type, payload (Uint32Array view), word}. */
export function* streamEvents(words) {
  if (words[0] !== STREAM_MAGIC) throw new Error('not an RCS1 stream');
  if (!STREAM_VERSIONS.includes(words[1])) throw new Error(`RCS1 stream version ${words[1]} not supported`);
  let p = 2;
  while (p + 2 <= words.length) {
    const type = words[p], n = words[p + 1];
    yield { type, payload: words.subarray(p + 2, p + 2 + n), word: p };
    p += 2 + n;
  }
}

/**
 * Parses a Picture payload into header fields and section views (no copies). Accepts the 15-section layout of
 * FORMAT-STREAM.md (sections 13 and 14: the intra schedule computed by G0; without them, the 13-section layout, the
 * schedule is built here in JS) and the first 12-section one (no intra map, 6-word intra records covering Cb + Cr,
 * PUs not split into the small and general ranges), which the first dumps use; `legacy` is set then.
 */
export function parsePicture(p) {
  const nSec = (p[HEADER_WORDS] - HEADER_WORDS) >> 1;
  if (nSec < 12 || nSec > SECTIONS) throw new Error(`picture payload: unexpected directory (${nSec} sections)`);
  const secs = [];
  for (let i = 0; i < SECTIONS; i++) {
    if (i >= nSec) { secs.push({ words: new Uint32Array(0), count: 0 }); continue; }
    const off = p[HEADER_WORDS + 2 * i], count = p[HEADER_WORDS + 2 * i + 1];
    const end = i + 1 < nSec ? p[HEADER_WORDS + 2 * (i + 1)] : p.length;
    secs.push({ words: p.subarray(off, end), count });
  }
  const h = p;
  const flags = h[7];
  return {
    legacy: nSec === 12,
    picId: h[0], slot: h[1], poc: h[2] | 0, width: h[3], height: h[4],
    bitDepthY: h[5] & 0xff, bitDepthC: (h[5] >>> 8) & 0xff, chromaFormatIdc: (h[5] >>> 16) & 0xff,
    log2Ctb: h[6] & 0xff, log2MinCb: (h[6] >>> 8) & 0xff, log2MinTb: (h[6] >>> 16) & 0xff,
    flags,
    saoEnabled: !!(flags & 1), strongIntraSmoothing: !!(flags & 2), intraSmoothingDisabled: !!(flags & 4),
    output: !!(flags & 8), anyNoFilter: !!(flags & 16), edgeCodes: !!(flags & 64), unsupported: !!(flags & 0x80000000),
    cbQpOffset: h[8] | 0, crQpOffset: h[9] | 0,
    confWin: [h[10], h[11], h[12], h[13]],
    waves: h[14], widthInCtbs: h[15], heightInCtbs: h[16], smallCount: nSec >= 13 ? h[19] : 0,
    schedStepBase: h[20], schedTuBase: h[21],
    secs,
  };
}

const grid = (groups) => {
  const x = Math.max(1, Math.min(groups, MAX_GROUPS));
  return [x, Math.ceil(groups / x)];
};

export class GpuRecon {
  /**
   * @param {GPUDevice} device from requestReconDevice (or with equivalent limits)
   * @param {{slots?: number, schedule?: 'list'|'ctu'|'wave', streamSchedule?: boolean}} opts slots: initial DPB slot
   *   count (grows on demand, copying the arena; sps_max_dec_pic_buffering + 1 avoids that); schedule: intra schedule
   *   mode when built in JS; streamSchedule: use the stream's schedule when it has one ('list' only)
   */
  constructor(device, { slots = 4, schedule = 'list', streamSchedule = true } = {}) {
    this.device = device;
    this.scheduleMode = schedule;
    this.streamSchedule = streamSchedule;
    this.initialSlots = slots;
    this.timestamps = device.features.has('timestamp-query');
    const U = GPUBufferUsage;
    this.idct = new IdctPipeline(device);
    this.mcSep = new GpuMc(device, { separateDst: true });
    this.mcDirect = new GpuMc(device);
    this.intra = new IntraStage(device);
    this.deblock = createDeblock(device);
    this.bs = createBs(device);
    this.sao = createSao(device);
    const arMod = device.createShaderModule({ code: addResidualWgsl, label: 'add-residual' });
    const C = GPUShaderStage.COMPUTE;
    this.arBgl = device.createBindGroupLayout({
      label: 'add-residual',
      entries: [
        { binding: 0, visibility: C, buffer: { type: 'uniform' } },
        { binding: 1, visibility: C, buffer: { type: 'storage' } },
        { binding: 2, visibility: C, buffer: { type: 'read-only-storage' } },
        { binding: 3, visibility: C, buffer: { type: 'read-only-storage' } },
        { binding: 4, visibility: C, buffer: { type: 'read-only-storage' } },
        { binding: 5, visibility: C, buffer: { type: 'read-only-storage' } },
      ],
    });
    const arLayout = device.createPipelineLayout({ bindGroupLayouts: [this.arBgl] });
    this.arPipeline = device.createComputePipeline({ layout: arLayout, compute: { module: arMod, entryPoint: 'addResidual' } });
    this.pcmPipeline = device.createComputePipeline({ layout: arLayout, compute: { module: arMod, entryPoint: 'pcmMain' } });
    this.arParams = device.createBuffer({ size: 64, usage: U.UNIFORM | U.COPY_DST });
    this.arWords = new Uint32Array(16);
    this.filterParams = device.createBuffer({ size: 64, usage: U.UNIFORM | U.COPY_DST });
    this.bsParams = device.createBuffer({ size: 32, usage: U.UNIFORM | U.COPY_DST });
    this.lists = device.createBuffer({ size: 24 * LIST_WORDS * 4, usage: U.STORAGE | U.COPY_DST });
    this.dummy = device.createBuffer({ size: 256, usage: U.STORAGE | U.COPY_DST });
    this.bufs = {};
    this.gen = 0;
    this.geom = null;
    this.arena = null;
    this.nSlots = 0;
    this.slotValid = [];
    this.bg = new Map();
    this.slotRes = [];
    if (this.timestamps) {
      this.querySet = device.createQuerySet({ type: 'timestamp', count: 2 });
      this.resolveBuf = device.createBuffer({ size: 16, usage: U.QUERY_RESOLVE | U.COPY_SRC });
      this.tsPool = [];
    }
  }

  // A record buffer of at least `bytes`; growing invalidates every cached bind group. Small buffers double; from 1 MB on they get
  // 25% headroom rounded up to 1 MB (doubling left FEL_TEST_V6's residual buffer at 80 MB for about 45 MB of use).
  #buf(name, bytes) {
    const b = this.bufs[name];
    if (b && b.size >= bytes) return b;
    b?.destroy();
    const MB = 1 << 20;
    const size = bytes < MB ? Math.max(4096, 2 ** Math.ceil(Math.log2(Math.max(bytes, 1)))) : Math.ceil(bytes * 1.25 / MB) * MB;
    this.bufs[name] = this.device.createBuffer({ label: name, size, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
    this.gen++;
    return this.bufs[name];
  }

  #upload(name, words) {
    const b = this.#buf(name, words.byteLength);
    if (words.length) this.device.queue.writeBuffer(b, 0, words);
    return b;
  }

  // Geometry: README layout, slot size rounded to the binding offset alignment. A change drops every slot.
  #ensureGeometry(pic) {
    const g = this.geom;
    if (g && g.width === pic.width && g.height === pic.height) return;
    const L = pictureLayout(pic.width, pic.height);
    const alignWords = Math.max(64, this.device.limits.minStorageBufferOffsetAlignment / 4);
    const slotWords = Math.ceil(L.words / alignWords) * alignWords;
    this.geom = { width: pic.width, height: pic.height, L, slotWords, slotBytes: slotWords * 4 };
    this.arena?.destroy();
    this.work?.destroy();
    this.arena = null;
    this.nSlots = 0;
    this.slotValid = [];
    this.work = this.device.createBuffer({ label: 'recon work', size: this.geom.slotBytes, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC | GPUBufferUsage.COPY_DST });
    this.workRes = { buffer: this.work, offset: 0, size: this.geom.slotBytes };
    this.#ensureSlots(this.initialSlots);
    this.gen++;
  }

  #ensureSlots(n) {
    if (n <= this.nSlots) return;
    const { slotBytes } = this.geom;
    const max = Math.min(this.device.limits.maxStorageBufferBindingSize, this.device.limits.maxBufferSize);
    if (n * slotBytes > max) throw new Error(`DPB arena of ${n} slots (${n * slotBytes} bytes) exceeds the device limit ${max}`);
    const arena = this.device.createBuffer({ label: 'dpb arena', size: n * slotBytes, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC | GPUBufferUsage.COPY_DST });
    if (this.arena) {
      const enc = this.device.createCommandEncoder();
      enc.copyBufferToBuffer(this.arena, 0, arena, 0, this.nSlots * slotBytes);
      this.device.queue.submit([enc.finish()]);
      this.arena.destroy();
    }
    this.arena = arena;
    this.nSlots = n;
    this.slotRes = Array.from({ length: n }, (_, s) => ({ buffer: arena, offset: s * slotBytes, size: slotBytes }));
    this.gen++;
  }

  /** The binding resource of a DPB slot ({buffer, offset, size}), e.g. for the compositor. */
  slotResource(slot) {
    return this.slotRes[slot];
  }

  // Bind group cache, cleared whenever a buffer generation changes.
  #bind(key, make) {
    if (this.bgGen !== this.gen) { this.bg.clear(); this.bgGen = this.gen; }
    let g = this.bg.get(key);
    if (!g) { g = make(); this.bg.set(key, g); }
    return g;
  }

  /**
   * Handles one event of the stream. Picture: see decodePicture. Generated: fills the slot with mid-grey.
   * Output / Discard: returned for the host's display queue ({type, picId, slot}).
   */
  event(type, payload, opts) {
    if (type === EV.PICTURE) return { type, ...this.decodePicture(payload, opts) };
    if (type === EV.GENERATED) {
      const [picId, slot, poc, width, height, bd] = payload;
      this.fillGrey(slot, { width, height, bitDepthY: bd & 0xff, bitDepthC: (bd >>> 8) & 0xff });
      return { type, picId, slot, poc: poc | 0 };
    }
    return { type, picId: payload[0], slot: payload[1] };
  }

  fillGrey(slot, pic) {
    this.#ensureGeometry(pic);
    this.#ensureSlots(slot + 1);
    const { L } = this.geom;
    const words = new Uint32Array(L.words);
    const gy = 1 << (pic.bitDepthY - 1), gc = 1 << (pic.bitDepthC - 1);
    words.fill((gy | (gy << 16)) >>> 0, 0, L.offCb);
    words.fill((gc | (gc << 16)) >>> 0, L.offCb);
    this.device.queue.writeBuffer(this.arena, slot * this.geom.slotBytes, words);
    this.slotValid[slot] = true;
  }

  /** Writes a CPU decoded picture (planes without stride: Uint16Array Y, Cb, Cr) into a slot (CPU fallback). */
  uploadPicture(slot, pic, planes) {
    this.#ensureGeometry(pic);
    this.#ensureSlots(slot + 1);
    const { L } = this.geom;
    const out = new Uint16Array(L.words * 2);
    const dims = [[pic.width, pic.height, L.strideY, 0], [pic.width >> 1, pic.height >> 1, L.strideC, L.offCb * 2], [pic.width >> 1, pic.height >> 1, L.strideC, L.offCr * 2]];
    dims.forEach(([w, h, stride, base], c) => {
      for (let y = 0; y < h; y++) out.set(planes[c].subarray(y * w, (y + 1) * w), base + y * stride);
    });
    this.device.queue.writeBuffer(this.arena, slot * this.geom.slotBytes, out);
    this.slotValid[slot] = true;
  }

  /**
   * Reconstructs one Picture event payload into its DPB slot (one submit).
   * @param {Uint32Array} payload
   * @param {{stopAfter?: 'recon'|'deblock'}} opts test only: end the pass early; the partial picture is in `target`
   *   (the work buffer with SAO, else the slot) and the slot is not final
   * @returns {{verdict: 'gpu'|'cpu', reason?: string, picId, slot, poc, kind: 'I'|'P'|'B', output, target,
   *   cpu: {parseMs, scheduleMs, uploadMs, encodeMs, totalMs}, gpuMs?: Promise<number>, counts}}
   */
  decodePicture(payload, { stopAfter = null } = {}) {
    const t0 = performance.now();
    const pic = parsePicture(payload);
    const S = pic.secs;
    const base = { picId: pic.picId, slot: pic.slot, poc: pic.poc, output: pic.output };
    const cpu = (reason) => ({ ...base, verdict: 'cpu', reason, kind: '?' });
    // Unsupported features: route the picture to the CPU (FORMAT-STREAM.md, FORMAT-idct/mc/intra/deblock/sao.md).
    if (pic.unsupported) return cpu('stream flags it unsupported');
    if (pic.chromaFormatIdc !== 1) return cpu(`chroma_format_idc ${pic.chromaFormatIdc}`);
    if (pic.bitDepthY < 8 || pic.bitDepthY > 12 || pic.bitDepthC < 8 || pic.bitDepthC > 12) return cpu('bit depth');
    if (pic.width < 16 || pic.width % 8 || pic.height % 8 || pic.width > 65535 || pic.height > 65535) return cpu('picture size');

    const tus = S[SEC.TU].words, nTu = S[SEC.TU].count;
    let interTus = 0, resWords = 0;
    for (let i = 0; i < nTu; i++) {
      const w1 = tus[i * 6 + 1];
      if ((w1 & 0x80) || (w1 >>> 23)) return cpu('TU uses a reserved tool (rotation, RDPCM, extended precision)');
      if (tus[i * 6 + 5] === NONE) interTus++;
    }
    if (nTu) {
      const last = (nTu - 1) * 6;
      const n = 4 << (tus[last + 1] & 3);
      resWords = tus[last + 4] + n * n;
    }
    let pus = S[SEC.PU].words, nPu = S[SEC.PU].count, smallCount = pic.smallCount, nB = 0;
    if (pic.legacy && nPu) {
      const r = partitionRecords(pus.subarray(0, nPu * 6));
      pus = r.words;
      smallCount = r.smallCount;
    }
    for (let i = 0; i < nPu; i++) {
      const f = pus[i * 6 + 1] >>> 16, sl = pus[i * 6 + 2];
      if (f & 2) nB++;
      if ((f & 1) && !this.slotValid[sl & 0xffff]) return cpu(`reference slot ${sl & 0xffff} holds no picture`);
      if ((f & 2) && !this.slotValid[sl >>> 16]) return cpu(`reference slot ${sl >>> 16} holds no picture`);
    }
    const kind = nPu === 0 ? 'I' : nB ? 'B' : 'P';
    const nPcm = S[SEC.PCM].count;
    let intraTus = S[SEC.INTRA].words, nIntra = S[SEC.INTRA].count;
    if (nIntra) {
      const recWords = S[SEC.INTRA].words.length / nIntra;
      if (pic.legacy && recWords === 6) {
        intraTus = fromStreamIntra(S[SEC.INTRA].words, tus);
        nIntra = intraTus.length / 4;
      } else if (recWords !== 4) {
        throw new Error(`intra section: ${recWords} words per record`);
      } else {
        intraTus = intraTus.subarray(0, nIntra * 4);
      }
    }
    let anySao = false;
    if (pic.saoEnabled) {
      const sao = S[SEC.SAO].words;
      for (let i = 0; i < S[SEC.SAO].count && !anySao; i++) if (sao[i * 4] & 0x7e00) anySao = true;
    }
    const t1 = performance.now();
    let schedule = null;
    if (nIntra && S[SEC.INTRA_SCHED].count && this.scheduleMode === 'list' && this.streamSchedule) {
      // G0's schedule (sections 13 and 14): exactly buildSchedule(..., 'list').
      const dw = S[SEC.INTRA_DISPATCH].words, dispatches = new Array(S[SEC.INTRA_DISPATCH].count);
      for (let i = 0; i < dispatches.length; i++) dispatches[i] = { groupBase: dw[2 * i], groupCount: dw[2 * i + 1] };
      schedule = { sched: S[SEC.INTRA_SCHED].words.subarray(0, S[SEC.INTRA_SCHED].count), stepBase: pic.schedStepBase, tuBase: pic.schedTuBase, dispatches };
    } else if (nIntra) {
      schedule = buildSchedule(intraTus, pic.width, pic.height, pic.log2Ctb, this.scheduleMode);
    }
    const t2 = performance.now();

    // Geometry, slots, uploads.
    this.#ensureGeometry(pic);
    this.#ensureSlots(pic.slot + 1);
    const { L, slotWords } = this.geom;
    const q = this.device.queue;
    const tuBuf = this.#upload('tus', tus.subarray(0, nTu * 6));
    const coefBuf = this.#upload('coefs', S[SEC.COEFF].words.subarray(0, S[SEC.COEFF].count));
    const resBuf = this.#buf('residual', resWords * 4);
    const puBuf = this.#upload('pus', pus.subarray(0, nPu * 6));
    const wtBuf = this.#upload('weights', S[SEC.WEIGHTS].words.subarray(0, S[SEC.WEIGHTS].count * 4));
    const pcmBuf = this.#upload('pcm', S[SEC.PCM].words.subarray(0, nPcm * 3));
    const pcmSBuf = this.#upload('pcmSamples', S[SEC.PCM_SAMPLES].words.subarray(0, (S[SEC.PCM_SAMPLES].count + 1) >> 1));
    const mapBuf = this.#upload('blockMap', S[SEC.BLOCK_MAP].words.subarray(0, S[SEC.BLOCK_MAP].count));
    const dbBuf = this.#upload('deblockCtb', S[SEC.DEBLOCK_CTB].words.subarray(0, S[SEC.DEBLOCK_CTB].count));
    const saoBuf = this.#upload('saoCtb', S[SEC.SAO].words.subarray(0, S[SEC.SAO].count * 4));
    if (S[SEC.SCALING].count) q.writeBuffer(this.lists, 0, S[SEC.SCALING].words.subarray(0, 24 * LIST_WORDS));
    const imapBuf = S[SEC.INTRA_MAP].count ? this.#upload('intraMap', S[SEC.INTRA_MAP].words.subarray(0, S[SEC.INTRA_MAP].count)) : this.dummy;
    const fp = makeParams({
      width: pic.width, height: pic.height, bitDepthY: pic.bitDepthY, bitDepthC: pic.bitDepthC, log2Ctb: pic.log2Ctb,
      cbQpOffset: pic.cbQpOffset, crQpOffset: pic.crQpOffset, anyNoFilter: pic.anyNoFilter,
    });
    q.writeBuffer(this.filterParams, 0, fp);
    const bsDims = pic.edgeCodes && { width: pic.width, height: pic.height, nPu, nTu };
    const cellBuf = bsDims && this.#buf('bsCells', cellsBytes(pic.width, pic.height));
    if (bsDims) q.writeBuffer(this.bsParams, 0, makeBsParams(bsDims));
    const [tgx] = grid(nTu), [pgx] = grid(nPcm);
    this.arWords.set([L.strideY, L.strideC, L.offCb, L.offCr, nTu, tgx, nPcm, pgx, pic.bitDepthY, pic.bitDepthC]);
    q.writeBuffer(this.arParams, 0, this.arWords);
    const t3 = performance.now();

    // The target picture: the work buffer when SAO writes the slot, else the slot itself.
    const useSao = anySao;
    const slotR = this.slotRes[pic.slot];
    const target = useSao ? this.workRes : slotR;
    const tKey = useSao ? 'work' : `slot${pic.slot}`;

    const enc = this.device.createCommandEncoder();
    if (bsDims) this.bs.clear(enc, cellBuf, pic.width, pic.height, nPu);
    const passDesc = {};
    if (this.timestamps) passDesc.timestampWrites = { querySet: this.querySet, beginningOfPassWriteIndex: 0, endOfPassWriteIndex: 1 };
    const pass = enc.beginComputePass(passDesc);
    if (bsDims) {
      // Edge codes to boundary strengths, in the uploaded block map (deblocking and SAO read it later in the pass).
      const bg = this.#bind('bs', () => this.bs.bindGroup({ params: this.bsParams, cells: cellBuf, pus: puBuf, tus: tuBuf, map: mapBuf }));
      this.bs.encode(pass, bg, bsDims);
    }
    if (nPu) {
      const mc = useSao ? this.mcSep : this.mcDirect;
      const bg = this.#bind(`mc-${tKey === 'work' ? 'sep' : 'direct'}`, () => mc.bindGroup({ arena: this.arena, pus: puBuf, wts: wtBuf, dst: this.work }));
      mc.encode(pass, bg, {
        width: pic.width, height: pic.height, bitDepthY: pic.bitDepthY, bitDepthC: pic.bitDepthC, slotWords,
        dstBase: useSao ? 0 : pic.slot * slotWords,
        smallFirst: 0, smallCount, puFirst: smallCount, puCount: nPu - smallCount,
      });
    }
    const arBg = (nPcm || interTus) && this.#bind(`ar-${tKey}`, () => this.device.createBindGroup({
      layout: this.arBgl,
      entries: [
        { binding: 0, resource: { buffer: this.arParams } },
        { binding: 1, resource: target },
        { binding: 2, resource: { buffer: tuBuf } },
        { binding: 3, resource: { buffer: resBuf } },
        { binding: 4, resource: { buffer: pcmBuf } },
        { binding: 5, resource: { buffer: pcmSBuf } },
      ],
    }));
    if (nPcm) {
      pass.setPipeline(this.pcmPipeline);
      pass.setBindGroup(0, arBg);
      pass.dispatchWorkgroups(...grid(nPcm));
    }
    if (nTu) {
      const ib = this.#bind('idct', () => this.idct.createBindings({ tus: tuBuf, coefs: coefBuf, lists: this.lists, residual: resBuf }));
      this.idct.dispatch(pass, ib, nTu);
    }
    if (interTus) {
      pass.setPipeline(this.arPipeline);
      pass.setBindGroup(0, arBg);
      pass.dispatchWorkgroups(...grid(nTu));
    }
    if (schedule) {
      this.intra.prepare(pic, schedule, { picture: target, residual: resBuf, intraMap: imapBuf });
      this.intra.encode(pass);
    }
    if (stopAfter !== 'recon') {
      const dbg = this.#bind(`db-${tKey}`, () => this.deblock.bindGroup({ params: this.filterParams, picture: target, map: mapBuf, ctb: dbBuf }));
      this.deblock.encode(pass, dbg, pic.width, pic.height);
      if (useSao && stopAfter !== 'deblock') {
        const sbg = this.#bind(`sao-${pic.slot}`, () => this.sao.bindGroup({ params: this.filterParams, src: this.work, dst: slotR, map: mapBuf, ctb: saoBuf }));
        this.sao.encode(pass, sbg, fp);
      }
    }
    pass.end();
    let gpuMs, rb;
    if (this.timestamps) {
      rb = this.tsPool.pop() ?? this.device.createBuffer({ size: 16, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
      enc.resolveQuerySet(this.querySet, 0, 2, this.resolveBuf, 0);
      enc.copyBufferToBuffer(this.resolveBuf, 0, rb, 0, 16);
    }
    q.submit([enc.finish()]);
    if (rb) {
      // mapAsync only after the submit that writes rb (a buffer pending a map cannot be used in a submit).
      gpuMs = (async () => {
        await rb.mapAsync(GPUMapMode.READ);
        const t = new BigUint64Array(rb.getMappedRange());
        const ms = Number(t[1] - t[0]) / 1e6;
        rb.unmap();
        this.tsPool.push(rb);
        return ms;
      })();
    }
    if (!stopAfter) this.slotValid[pic.slot] = true;
    const t4 = performance.now();
    return {
      ...base, verdict: 'gpu', kind, target, useSao, legacy: pic.legacy, streamSchedule: schedule ? !schedule.stats : null, gpuMs,
      cpu: { parseMs: t1 - t0, scheduleMs: t2 - t1, uploadMs: t3 - t2, encodeMs: t4 - t3, totalMs: t4 - t0 },
      counts: { tus: nTu, interTus, coefs: S[SEC.COEFF].count, pus: nPu, smallPus: smallCount, intra: nIntra, pcm: nPcm, intraDispatches: schedule?.dispatches.length ?? 0 },
      pic: { width: pic.width, height: pic.height, bitDepthY: pic.bitDepthY, bitDepthC: pic.bitDepthC },
    };
  }

  /** Reads a binding resource ({buffer, offset, size}) back (tests). */
  async read(res) {
    const rb = this.device.createBuffer({ size: res.size, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
    const enc = this.device.createCommandEncoder();
    enc.copyBufferToBuffer(res.buffer, res.offset, rb, 0, res.size);
    this.device.queue.submit([enc.finish()]);
    await rb.mapAsync(GPUMapMode.READ);
    const out = new Uint32Array(rb.getMappedRange().slice(0));
    rb.unmap();
    rb.destroy();
    return out;
  }

  /** README layout of the current geometry. */
  get layout() {
    return this.geom?.L;
  }

  destroy() {
    for (const b of Object.values(this.bufs)) b.destroy();
    this.arena?.destroy();
    this.work?.destroy();
  }
}
