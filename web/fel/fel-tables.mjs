// SPDX-License-Identifier: LGPL-3.0-only
// Thin JS call for aether_fel_* (rust/aether-wasm/src/dovi.rs) on top of web/engine/aether-wasm.mjs.
// Superseded by web/engine/dovi.mjs (createDoviSession); kept for the test pages that still import it.
import { AetherWasm, AetherError } from '../engine/aether-wasm.mjs';

export const GPU_TABLE_WORDS = 6832;

export class FelTables {
  /** @param {AetherWasm} wasm one instance holds one mapping history, create one per playback session */
  constructor(wasm) { this.wasm = wasm; this.epoch = 1n; this.wasm.x.aether_fel_reset(this.epoch); }
  static async create(url) { return new FelTables(await AetherWasm.create({}, url)); }
  /** Call on load and on every seek (mapping reuse via use_prev_vdr_rpu_flag needs the history). */
  reset(epoch = this.epoch + 1n) { this.epoch = BigInt(epoch); this.wasm.x.aether_fel_reset(this.epoch); }
  /**
   * @param {Uint8Array} rpuNal one type-62 NAL (2 byte header, no start code, no length prefix)
   * @returns {Int32Array} GPU_TABLE_WORDS words
   */
  tables(rpuNal) {
    const w = this.wasm;
    const n = w.withBytes(rpuNal, (p, l) => w.x.aether_fel_gpu_tables(this.epoch, p, l));
    if (n < 0) throw new AetherError('FEL table build failed', n, 'fel_gpu_tables');
    const r = w.result();
    return new Int32Array(r.buffer, r.byteOffset, r.byteLength >> 2).slice();
  }
}
