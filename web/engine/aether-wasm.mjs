// SPDX-License-Identifier: LGPL-3.0-only
// Thin loader for the raw C ABI of rust/aether-wasm (web/wasm/aether_wasm.wasm).
// Works in browsers, Workers and Node. No wasm-bindgen: the instance owns linear memory and every
// call copies bytes in through aether_alloc and out through the module result buffer.
//
// One module instance holds ONE open demuxer and ONE open audio decoder (thread-local state in
// Rust), so create one `AetherWasm` per demuxer and per audio decoder. The compiled
// `WebAssembly.Module` is cached and shared, instantiation is synchronous and cheap.

const IS_NODE = typeof process !== 'undefined' && !!process.versions?.node && typeof window === 'undefined'
  && typeof WorkerGlobalScope === 'undefined';

export const ABI_VERSION = 1;
export const DEFAULT_WASM_URL = new URL('../wasm/aether_wasm.wasm', import.meta.url);

let modulePromise = null;
let moduleUrl = null;
let compiled = null;

/** Compile (once) and cache the module. `url` may be a URL, string, ArrayBuffer or Uint8Array. */
export function loadModule(url = DEFAULT_WASM_URL) {
  if (modulePromise && (url === moduleUrl || url === DEFAULT_WASM_URL)) return modulePromise;
  moduleUrl = url;
  modulePromise = (async () => {
    let bytes;
    if (url instanceof ArrayBuffer || ArrayBuffer.isView(url)) {
      bytes = url;
    } else if (IS_NODE && (url instanceof URL ? url.protocol === 'file:' : !/^[a-z]+:\/\//i.test(String(url)))) {
      const fs = await import('node:fs/promises');
      bytes = await fs.readFile(url instanceof URL ? url : String(url));
    } else {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`aether wasm fetch failed: ${res.status}`);
      if (WebAssembly.compileStreaming && /wasm/.test(res.headers.get('content-type') || '')) {
        compiled = await WebAssembly.compileStreaming(res);
        return compiled;
      }
      bytes = await res.arrayBuffer();
    }
    compiled = await WebAssembly.compile(bytes);
    return compiled;
  })();
  modulePromise.catch(() => { modulePromise = null; });
  return modulePromise;
}

/** Synchronous module access after `loadModule` resolved. */
export function moduleSync() {
  if (!compiled) throw new Error('aether wasm not loaded: await loadModule() first');
  return compiled;
}

/** Error raised for a negative status. `code` is the raw ABI status. */
export class AetherError extends Error {
  /** `detail` is the engine's own message (`aether_last_error`) and is the headline when present. */
  constructor(message, code, op, detail = '') {
    super(`${op}: ${detail || message} (status ${code})`);
    this.name = 'AetherError';
    this.code = code;
    this.op = op;
    this.detail = detail;
  }
}

const DEMUX_ERRORS = { '-1': 'demux not open / open failed', '-2': 'no such packet or seek failed', '-3': 'host read failed' };
const RPU_ERRORS = ['RPU too large', 'RPU CRC or envelope invalid', 'unsupported or hostile RPU syntax', 'RPU parse rejected'];
const AUDIO_ERRORS = { '-1': 'decoder not open or bad codec', '-2': 'decoder open failed', '-200': 'DTS decode error', '-300': 'PCM decode error', '-400': 'AC-3 decode error' };

function describe(table, code) {
  if (table[String(code)]) return table[String(code)];
  if (code <= -100 && code > -200) return `TrueHD decode error ${-code - 100}`;
  return 'error';
}

export class AetherWasm {
  /**
   * @param {WebAssembly.Module} module
   * @param {{read?: (offset:number, dest:Uint8Array)=>number}} [host] synchronous read callback,
   *   called from inside demux calls; returns bytes written or a negative value.
   */
  constructor(module, host = {}) {
    this.host = host;
    this.instance = new WebAssembly.Instance(module, {
      env: {
        aether_host_read: (offset, ptr, len) => {
          if (!this.host.read) return -1;
          try {
            return this.host.read(Number(offset), new Uint8Array(this.x.memory.buffer, ptr, len));
          } catch (e) {
            this.hostError = e;
            return -1;
          }
        },
      },
    });
    this.x = this.instance.exports;
    this.hostError = null;
    const v = this.x.aether_abi_version();
    if (v !== ABI_VERSION) throw new Error(`aether wasm ABI ${v}, adapter expects ${ABI_VERSION}`);
  }

  static async create(host, url) {
    return new AetherWasm(await loadModule(url), host);
  }

  get mem() { return new Uint8Array(this.x.memory.buffer); }

  /** Copy `bytes` into module memory, run `fn(ptr, len)`, free. */
  withBytes(bytes, fn) {
    const len = bytes.byteLength;
    if (len === 0) return fn(0, 0);
    const ptr = this.x.aether_alloc(len);
    if (!ptr) throw new Error(`aether_alloc(${len}) failed`);
    try {
      this.mem.set(bytes, ptr);
      return fn(ptr, len);
    } finally {
      this.x.aether_free(ptr, len);
    }
  }

  /** Copy of the module result buffer (valid until the next call, so always copied). */
  result() {
    const len = this.x.aether_result_len();
    if (!len) return new Uint8Array(0);
    const ptr = this.x.aether_result_ptr();
    return this.mem.slice(ptr, ptr + len);
  }

  resultText() { return new TextDecoder().decode(this.result()); }

  /** Result as an ArrayBuffer-backed DataView-friendly copy. */
  resultView() {
    const r = this.result();
    return new DataView(r.buffer, r.byteOffset, r.byteLength);
  }

  /** Message of the most recent failing audio or demux call (`aether_last_error`), '' when none. */
  lastError() {
    const fn = this.x.aether_last_error;
    if (!fn) return '';
    let cap = 256;
    for (;;) {
      const size = cap;
      const ptr = this.x.aether_alloc(size);
      if (!ptr) return '';
      try {
        const n = fn(ptr, size);
        if (n <= size) return new TextDecoder().decode(this.mem.slice(ptr, ptr + n));
        cap = n;
      } finally {
        this.x.aether_free(ptr, size);
      }
    }
  }

  clearError() { this.x.aether_clear_error?.(); }

  fail(op, code, table = {}) {
    if (this.hostError) { const e = this.hostError; this.hostError = null; throw e; }
    throw new AetherError(describe(table, code), code, op, this.lastError());
  }

  // ---- demux ----
  demuxOpen(length) {
    const s = this.x.aether_demux_open(BigInt(length));
    if (s < 0) {
      const msg = this.lastError() || this.resultText();
      if (this.hostError) { const e = this.hostError; this.hostError = null; throw e; }
      throw new AetherError(msg || 'open failed', s, 'demux_open');
    }
  }
  demuxInfo() {
    const s = this.x.aether_demux_info();
    if (s < 0) this.fail('demux_info', s, DEMUX_ERRORS);
    return JSON.parse(this.resultText());
  }
  demuxPacketCount() { return this.x.aether_demux_packet_count(); }
  /** 48 byte records, see rust/aether-wasm/src/demux.rs. */
  demuxPackets(start, count) {
    const n = this.x.aether_demux_packets(start, count);
    if (n < 0) this.fail('demux_packets', n, DEMUX_ERRORS);
    return { count: n, view: this.resultView() };
  }
  /** Payload of packet `index` (storage order), copied out. */
  demuxRead(index) {
    const n = this.x.aether_demux_read(index);
    if (n < 0) this.fail('demux_read', n, DEMUX_ERRORS);
    return this.result();
  }
  demuxSeek(track, pts) {
    const i = this.x.aether_demux_seek(BigInt(track), BigInt(pts));
    if (i < 0) this.fail('demux_seek', i, DEMUX_ERRORS);
    return i;
  }
  demuxClose() { this.x.aether_demux_close(); }

  // ---- dovi ----
  /** framing: 0 Annex B, 1..4 length prefix size. Returns [{offset, length, kind:0|1|2}] */
  doviSplit(bytes, framing) {
    const n = this.withBytes(bytes, (p, l) => this.x.aether_dovi_split(p, l, framing));
    if (n < 0) throw new AetherError('framing error', n, 'dovi_split');
    const v = this.resultView();
    const out = [];
    for (let i = 0; i < n; i++) {
      out.push({ offset: v.getUint32(i * 12, true), length: v.getUint32(i * 12 + 4, true), kind: v.getUint32(i * 12 + 8, true) });
    }
    return out;
  }
  doviInspect(bytes, framing) {
    const s = this.withBytes(bytes, (p, l) => this.x.aether_dovi_inspect(p, l, framing));
    if (s !== 0) throw new AetherError('framing error', s, 'dovi_inspect');
    const v = this.resultView();
    return { total: v.getUint32(0, true), base: v.getUint32(4, true), enhancement: v.getUint32(8, true), rpu: v.getUint32(12, true), crcValidRpu: v.getUint32(16, true) };
  }
  /** Returns {nal, original:{primaries, transfer, matrix}} */
  doviRelabelSps(nal, patch) {
    const [p, t, m, f] = patchArgs(patch);
    const n = this.withBytes(nal, (ptr, len) => this.x.aether_dovi_relabel_sps(ptr, len, p, t, m, f));
    if (n < 0) throw new AetherError('SPS relabel failed', n, 'dovi_relabel_sps');
    return splitRelabel(this.result());
  }
  doviRelabelHvcc(hvcc, patch) {
    const [p, t, m, f] = patchArgs(patch);
    const n = this.withBytes(hvcc, (ptr, len) => this.x.aether_dovi_relabel_hvcc(ptr, len, p, t, m, f));
    if (n < 0) throw new AetherError('hvcC relabel failed', n, 'dovi_relabel_hvcc');
    return splitRelabel(this.result());
  }
  /** One type-62 NAL rewritten to Profile 8.1. */
  doviTo81(nal) {
    const n = this.withBytes(nal, (p, l) => this.x.aether_dovi_to_81(p, l));
    if (n < 0) throw new AetherError(RPU_ERRORS[-n - 300] || 'RPU conversion failed', n, 'dovi_to_81');
    return this.result().slice();
  }
  doviInspectRpu(nal) {
    const s = this.withBytes(nal, (p, l) => this.x.aether_dovi_inspect_rpu(p, l));
    if (s !== 0) throw new AetherError('RPU parse failed', s, 'dovi_inspect_rpu');
    const v = this.resultView();
    const w = i => v.getUint32(i * 4, true);
    return { profile: w(0), enhancement: ['unknown', 'fel', 'mel'][w(1)], disableResidual: !!w(2), usePreviousMapping: !!w(3), hasMapping: !!w(4), hasDisplayMetadata: !!w(5), blDepth: w(6), elDepth: w(7), vdrDepth: w(8), coefficientLog2Denom: w(9) };
  }

  /** Display metadata JSON of one type-62 RPU NAL (layout in rust/aether-dovi/src/display.rs), parsed. */
  doviRpuDisplay(nal) {
    const s = this.withBytes(nal, (p, l) => this.x.aether_dovi_rpu_display(p, l));
    if (s < 0) throw new AetherError(RPU_ERRORS[-s - 300] || 'RPU display parse failed', s, 'dovi_rpu_display');
    return JSON.parse(this.resultText());
  }
  /** Starts a new FEL / mapping epoch in this instance (load and every seek). */
  felReset(epoch) { this.x.aether_fel_reset(BigInt(epoch)); }
  /** GPU tables (GPU_TABLE_WORDS i32) of one RPU NAL for `epoch`; negative engine status throws AetherError. */
  felGpuTables(epoch, nal) {
    const n = this.withBytes(nal, (p, l) => this.x.aether_fel_gpu_tables(BigInt(epoch), p, l));
    if (n < 0) throw new AetherError('FEL table build failed', n, 'fel_gpu_tables');
    const r = this.result();
    return new Int32Array(r.buffer, r.byteOffset, r.byteLength >> 2).slice();
  }

  // ---- audio ----
  /** codec 0 TrueHD (arg = presentation), 1 DTS, 2 DTS:X, 3 AC-3, 4 E-AC-3 */
  audioOpen(codec, arg = 0) {
    const s = this.x.aether_audio_open(codec, arg);
    if (s < 0) this.fail('audio_open', s, AUDIO_ERRORS);
  }
  /**
   * LPCM decoder. `format` 0 signed int LE, 1 signed int BE, 2 float LE, 3 float BE, 4 unsigned 8 bit,
   * 5 Blu-ray HDMV LPCM (parameters come from each packet's header). `layoutMask` is a WAVE mask, 0 = default.
   */
  audioOpenPcm(format, bits, channels, sampleRate, layoutMask = 0) {
    const s = this.x.aether_audio_open_pcm(format, bits, channels, sampleRate, layoutMask);
    if (s < 0) this.fail('audio_open', s, AUDIO_ERRORS);
  }
  /** Canonical channel layout tables of the engine, see rust/README.md. */
  audioLayoutTable() {
    this.x.aether_audio_layout_table();
    return JSON.parse(this.resultText());
  }
  audioPush(bytes) {
    const s = this.withBytes(bytes, (p, l) => this.x.aether_audio_push(p, l));
    if (s < 0) this.fail('audio_push', s, AUDIO_ERRORS);
  }
  /**
   * null when more input is needed, else {sampleRate, frames, channels, labels, lossless, joc, planar}.
   * `joc` is false, or for an E-AC-3 stream that signals Dolby Atmos `{objects, rendered}`: `rendered` true when
   * the block is the objects rendered to 7.1.4 (12 channels), false when it is the 5.1/7.1 bed.
   */
  audioPull() {
    const s = this.x.aether_audio_pull();
    if (s === 1) return null;
    if (s < 0) this.fail('audio_pull', s, AUDIO_ERRORS);
    const r = this.result();
    const v = new DataView(r.buffer, r.byteOffset, r.byteLength);
    const sampleRate = v.getUint32(0, true), frames = v.getUint32(4, true), channels = v.getUint32(8, true);
    const labelBytes = v.getUint32(12, true), lossless = v.getUint32(16, true) !== 0;
    const labels = labelBytes ? new TextDecoder().decode(r.subarray(20, 20 + labelBytes)).split(',') : [];
    let off = 20 + ((labelBytes + 3) & ~3);
    const planar = [];
    for (let c = 0; c < channels; c++) {
      planar.push(new Float32Array(r.buffer.slice(r.byteOffset + off, r.byteOffset + off + frames * 4)));
      off += frames * 4;
    }
    const flags = this.x.aether_audio_block_flags ? this.x.aether_audio_block_flags() : (lossless ? 1 : 0);
    const joc = (flags & 2) === 0 ? false : { objects: this.x.aether_audio_block_joc_objects ? this.x.aether_audio_block_joc_objects() : 0, rendered: (flags & 4) !== 0 };
    return { sampleRate, frames, channels, labels, lossless, joc, planar };
  }
  /** E-AC-3 JOC object rendering on (default) or off (bed only). Remembered for decoders opened later. */
  audioSetJoc(enabled) { this.x.aether_audio_set_joc(enabled ? 1 : 0); }
  audioClose() { this.x.aether_audio_close(); }
}

/** The SDR carrier label: primaries 1, transfer 13, matrix kept. */
export const SDR_CARRIER = Object.freeze({ primaries: 1, transfer: 13, matrix: -1 });
/**
 * The carrier of a Dolby Vision profile 5 base layer: its SPS says matrix 2 (unspecified, the signal is IPT-PQ-C2), so the matrix
 * Chrome converts the picture with would be its own guess. BT.2020 NCL (9) is the one the compositor inverts to get the codes back.
 */
// fullRange: the range written where the stream states no signal type (streaming profile 5). Profile 5 is full range by definition, and a
// limited range label makes Chrome clip every code below 64 and above 940 on import, which lifts black.
export const P5_CARRIER = Object.freeze({ primaries: 1, transfer: 13, matrix: 9, fullRange: true });

function patchArgs(p) {
  return [p.primaries ?? -1, p.transfer ?? -1, p.matrix ?? -1, p.fullRange ? 1 : 0];
}

function splitRelabel(r) {
  return { original: { primaries: r[0], transfer: r[1], matrix: r[2] }, nal: r.slice(4) };
}
