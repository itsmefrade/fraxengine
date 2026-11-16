// SPDX-License-Identifier: LGPL-3.0-only
// usage: node rpu-survey.mjs <name> : table build (decode order) for every RPU of media/<name>, counts of profile / FEL / MEL / MMR / polynomial curves.
import fs from 'node:fs';
import { FelTables } from '../fel-tables.mjs';
import { AetherWasm } from '../../engine/aether-wasm.mjs';
const name = process.argv[2], dir = new URL('../media/', import.meta.url);
const idx = JSON.parse(fs.readFileSync(new URL(`${name}-index.json`, dir))), rpu = fs.readFileSync(new URL(`${name}-rpu.bin`, dir));
const wasm = new URL('../../wasm/aether_wasm.wasm', import.meta.url);
const t = await FelTables.create(wasm);
const w = await AetherWasm.create({}, wasm);
const stats = { frames: idx.aus.length, rpuCount: 0, profile: {}, enh: {}, disableResidual: 0, mmrFrames: 0, mmrMask: {}, errors: 0, firstError: null, pieces: {}, usePrev: 0, buildMs: 0, vdrDepth: {}, blDepth: {}, elDepth: {} };
const T0 = performance.now();
for (let i = 0; i < idx.aus.length; i++) {
  const a = idx.aus[i]; if (!a.rpu) continue;
  const nal = rpu.subarray(a.rpu[0], a.rpu[0] + a.rpu[1]);
  stats.rpuCount++;
  try {
    const n = w.withBytes(nal, (p, l) => w.x.aether_dovi_inspect_rpu(p, l));
    if (n === 0) { const r = new Uint32Array(w.result().slice().buffer); stats.profile[r[0]] = (stats.profile[r[0]] ?? 0) + 1; stats.enh[r[1]] = (stats.enh[r[1]] ?? 0) + 1; stats.usePrev += r[3]; stats.vdrDepth[r[8]] = (stats.vdrDepth[r[8]] ?? 0) + 1; stats.blDepth[r[6]] = (stats.blDepth[r[6]] ?? 0) + 1; stats.elDepth[r[7]] = (stats.elDepth[r[7]] ?? 0) + 1; }
    const words = t.tables(nal);
    if (words[5]) stats.disableResidual++;
    if (words[6]) { stats.mmrFrames++; stats.mmrMask[words[6]] = (stats.mmrMask[words[6]] ?? 0) + 1; }
    for (let c = 0; c < 3; c++) { const d = 64 + c * 32, n = words[d]; const k = `c${c}:${n - 1}pieces`; stats.pieces[k] = (stats.pieces[k] ?? 0) + 1; }
  } catch (e) { stats.errors++; stats.firstError ??= { frame: i, msg: String(e.message ?? e) }; }
}
stats.buildMs = +((performance.now() - T0) / stats.rpuCount).toFixed(3);
console.log(JSON.stringify(stats));
