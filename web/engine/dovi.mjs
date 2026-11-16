// SPDX-License-Identifier: LGPL-3.0-only
// Dolby Vision RPU helpers over the Rust/WASM engine: display metadata (JSON from libdovi, guarded by
// DoviRpuGuard before the parse) and the FEL / mapping GPU tables, with use_prev_vdr_rpu reuse.
//
//   const s = await createDoviSession();
//   s.reset();                                  // load and every seek
//   const { tables, display } = s.parseRpu(rpuNal);   // rpuNal: one type-62 NAL, 2 byte header, no start code
//
// Replaces web/fel/fel-tables.mjs. Plain ES module, works in Workers, Node and the main thread.

import { AetherWasm, AetherError } from './aether-wasm.mjs';

export const GPU_TABLE_WORDS = 6832;

const PQ = { m1: 2610 / 16384, m2: 2523 / 4096 * 128, c1: 3424 / 4096, c2: 2413 / 4096 * 32, c3: 2392 / 4096 * 32 };

/** ST 2084 inverse EOTF for a 12 bit PQ code (0..4095) to nits. */
export function pq12ToNits(code) {
  const e = Math.pow(Math.max(0, code) / 4095, 1 / PQ.m2);
  return 10000 * Math.pow(Math.max(e - PQ.c1, 0) / (PQ.c2 - PQ.c3 * e), 1 / PQ.m1);
}

/**
 * Adds floating point views to the integer JSON of `aether_dovi_rpu_display`:
 * `norm = {yccToRgb (/2^13), yccToRgbOffset (/2^28), rgbToLms (/2^14), sourceMinPq, sourceMaxPq (0..1), sourceMinNits, sourceMaxNits}`.
 */
export function withNormalized(display) {
  display.norm = {
    yccToRgb: display.yccToRgb.map(v => v / 8192),
    yccToRgbOffset: display.yccToRgbOffset.map(v => v / 268435456),
    rgbToLms: display.rgbToLms.map(v => v / 16384),
    sourceMinPq: display.source.minPq / 4095,
    sourceMaxPq: display.source.maxPq / 4095,
    sourceMinNits: pq12ToNits(display.source.minPq),
    sourceMaxNits: pq12ToNits(display.source.maxPq),
  };
  return display;
}

export class DoviSession {
  /** @param {AetherWasm} wasm one instance holds one mapping history: one session per playback / worker */
  constructor(wasm) {
    this.wasm = wasm;
    this.epoch = 1n;
    this.lastFull = null;
    wasm.felReset(this.epoch);
  }

  /** Forget the mapping history and the inherited display fields (call on load and on every seek). */
  reset(epoch = this.epoch + 1n) {
    this.epoch = BigInt(epoch);
    this.lastFull = null;
    this.wasm.felReset(this.epoch);
  }

  /**
   * Display metadata of one RPU. `compressed` RPUs (DM data repeated by id) carry no matrices or signal
   * fields: they are copied from the last full RPU of this epoch and `inherited` is set (when none was seen
   * yet they stay zero and `inherited` is false).
   */
  display(rpuNal) {
    const d = this.wasm.doviRpuDisplay(rpuNal);
    d.inherited = false;
    if (d.compressed) {
      const f = this.lastFull;
      if (f) {
        for (const k of ['yccToRgb', 'yccToRgbOffset', 'rgbToLms', 'signal', 'source']) d[k] = f[k];
        d.inherited = true;
      }
    } else if (d.hasDm) {
      this.lastFull = d;
    }
    return withNormalized(d);
  }

  /** GPU tables for this epoch (Int32Array of GPU_TABLE_WORDS). Throws AetherError, e.g. use_prev_vdr_rpu with no history. */
  tables(rpuNal) { return this.wasm.felGpuTables(this.epoch, rpuNal); }

  /** Summary words of `aether_dovi_inspect_rpu`: profile, enhancement ('fel'|'mel'|'unknown'), depths... */
  inspect(rpuNal) { return this.wasm.doviInspectRpu(rpuNal); }

  /**
   * `{display, tables, tablesError}`. A table failure does not hide the display metadata: `tables` is null and
   * `tablesError` the AetherError (`{tables: false}` skips the table build, e.g. when only the metadata is wanted).
   */
  parseRpu(rpuNal, { tables = true } = {}) {
    const display = this.display(rpuNal);
    let t = null, tablesError = null;
    if (tables) {
      try { t = this.tables(rpuNal); } catch (e) { if (!(e instanceof AetherError)) throw e; tablesError = e; }
    }
    return { display, tables: t, tablesError };
  }
}

/** New session on its own wasm instance (`url` overrides the module location). */
export async function createDoviSession(url) {
  return new DoviSession(await AetherWasm.create({}, url));
}

let shared = null;
/** Stateless convenience: `{display, tables, tablesError}` with a throw-away mapping history (no use_prev reuse). */
export async function parseRpu(rpuNal, opts) {
  shared ??= await createDoviSession();
  shared.reset();
  return shared.parseRpu(rpuNal, opts);
}
