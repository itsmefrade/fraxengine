// SPDX-License-Identifier: LGPL-3.0-only
// The RPU to shader parameters of the single pass Dolby Vision profile 5 route (fel-meta.mjs): the mapping LUT from the GPU table
// packet, and the IPT-PQ-C2 colour stage (RPU matrices) through the f64 model of the DISPLAY shader.
import test from 'node:test';
import assert from 'node:assert/strict';
import { buildMappingLut, buildDisplayUniforms, fromEngineDisplay, modelDisplayPixel, GPU_TABLE_WORDS, GPU_TABLE_MAGIC, GPU_PRED_OFFSET, GPU_DESC_OFFSET } from '../../fel/fel-meta.mjs';
import { withNormalized } from '../../engine/dovi.mjs';

// Table packet as rust/aether-dovi/src/fel/gpu.rs writes it: `pieces` polynomial pieces per component, prediction = f(component, code).
function packet(f, { pieces = 1, residual = false } = {}) {
  const t = new Int32Array(GPU_TABLE_WORDS);
  t[0] = GPU_TABLE_MAGIC; t[1] = 1; t[2] = GPU_TABLE_WORDS; t[3] = 23; t[4] = 12; t[5] = residual ? 0 : 1;
  for (let c = 0; c < 3; c++) {
    const d = GPU_DESC_OFFSET + c * 32;
    t[d] = pieces + 1;
    for (let p = 0; p <= pieces; p++) t[d + 2 + p] = Math.round(1023 * p / pieces);
    t[d + 27] = 0; t[d + 28] = 1023;
    for (let code = 0; code < 1024; code++) t[GPU_PRED_OFFSET + c * 1024 + code] = f(c, code);
  }
  return t;
}
// The RPU of the dv-p5 clip (profile 5, IPT-PQ-C2, full range 12 bit signal) as `aether_dovi_rpu_display` returns it.
const P5_DISPLAY = () => withNormalized({
  profile: 5, hasDm: true, compressed: false,
  yccToRgb: [8192, 799, 1681, 8192, -933, 1091, 8192, 267, -5545], yccToRgbOffset: [0, 134217728, 134217728],
  rgbToLms: [17081, -349, -349, -349, 17081, -349, -349, -349, 17081],
  signal: { eotf: 65535, eotfParam: [0, 0, 0], bitDepth: 12, colorSpace: 2, chromaFormat: 0, fullRange: 1 },
  source: { minPq: 7, maxPq: 3079, diagonal: 42 }, l1: { minPq: 0, avgPq: 1419, maxPq: 2457 }, l2: [], l8: [],
});

test('mapping LUT: one entry per component and code, the Reference16 prediction over 65536', () => {
  const lut = buildMappingLut(packet((c, code) => Math.min(65535, code * 64 + c)));
  assert.equal(lut.length, 3072);
  for (const c of [0, 1, 2]) for (const code of [0, 1, 64, 512, 940, 1023]) assert.equal(lut[c * 1024 + code], Math.fround((code * 64 + c) / 65536));
  // a curve of several polynomial pieces is still one prediction per code
  const bent = buildMappingLut(packet((c, code) => (code < 512 ? code * 32 : 16384 + (code - 512) * 96), { pieces: 3 }));
  assert.equal(bent[511], 511 * 32 / 65536);
  assert.equal(bent[1024 + 1023], (16384 + 511 * 96) / 65536);
});

test('mapping LUT: refused when one pass cannot carry the mapping', () => {
  const ok = packet((c, code) => code * 64);
  assert.ok(buildMappingLut(ok));
  assert.equal(buildMappingLut(packet((c, code) => code * 64, { residual: true })), null, 'residual needs the enhancement layer');
  const mmr = ok.slice(); mmr[6] = 2; mmr[GPU_DESC_OFFSET + 32 + 11] = 1;
  assert.equal(buildMappingLut(mmr), null, 'MMR reads three components');
  const kind = ok.slice(); kind[GPU_DESC_OFFSET + 2 * 32 + 11] = 1;
  assert.equal(buildMappingLut(kind), null, 'piece kind checked without the summary word');
  const magic = ok.slice(); magic[0] = 0;
  assert.equal(buildMappingLut(magic), null);
  assert.equal(buildMappingLut(null), null);
  assert.equal(buildMappingLut(new Int32Array(100)), null);
});

test('profile 5: LUT signal through the RPU matrices gives neutral grey at the PQ level of I', () => {
  const lut = buildMappingLut(packet((c, code) => code * 64));
  const meta = fromEngineDisplay(P5_DISPLAY());
  const display = { peakNits: 10000, sdrWhiteNits: 203, sourcePeakNits: 1000, toneMapping: 'off', trims: false };
  const u = buildDisplayUniforms({ meta, display, profile: 'p5', scale: 65536, kind: 1, dims: [3840, 2160, 1920, 1080], canvasColorSpace: 'rec2100-pq', reshape: true });
  assert.equal(u.data[55], 1, 'fin.w tells the shader to read the LUT');
  assert.deepEqual([...u.data.subarray(12, 15)], [0, 0.5, 0.5], 'chroma offsets of the IPT signal');
  for (const code of [64, 300, 512, 700]) {
    const sig = [lut[code], lut[1024 + 512], lut[2048 + 512]];
    const out = modelDisplayPixel(sig, u.data, 'rec2100-pq');
    assert.ok(Math.abs(out[0] - out[1]) < 1e-6 && Math.abs(out[1] - out[2]) < 1e-6, `neutral at ${code}: ${out}`);
    // L' = M' = S' = I; the crosstalk matrix rows sum to (17081 - 698) / 16384, so the light is within 0.01 % of PQ(I)
    assert.ok(Math.abs(out[0] - code / 1024) < 2e-4, `level at ${code}: ${out[0]} vs ${code / 1024}`);
  }
  // P (second component) above the centre moves away from grey: the matrices are in use, not an identity
  const tinted = modelDisplayPixel([lut[512], lut[1024 + 600], lut[2048 + 512]], u.data, 'rec2100-pq');
  assert.ok(Math.abs(tinted[0] - tinted[1]) > 0.01);
});
