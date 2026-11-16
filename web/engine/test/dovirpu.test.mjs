// SPDX-License-Identifier: LGPL-3.0-only
// RPU display metadata and GPU tables against dovi_tool (libdovi's own serde output via `dovi_tool export`).
// Needs ffmpeg, dovi_tool and the lab media; missing pieces skip.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { createDoviSession, GPU_TABLE_WORDS, pq12ToNits } from '../dovi.mjs';
import { have, hasFfmpeg } from './_helpers.mjs';

const LAB = (process.env.AETHER_LAB || decodeURIComponent(new URL('../../../test-media', import.meta.url).pathname)) + '/';
const hasTool = spawnSync('dovi_tool', ['--version']).status === 0;
const CASES = [
  ['P5', 'Test Jellyfin 4K DV P5.mp4', 5],
  ['P7 FEL Power Rangers', 'DV FEL Power Rangers Credits Test.mkv', 7],
  ['P7 MEL awaken-girl', 'awaken-girl.4K.HDR.DV.mkv', 7],
  ['P8.1 Jellyfin', 'Test Jellyfin 4K DV P8.1.mp4', 8],
  ['P7 FEL CM v4.0', 'DV FEL Test DT DL P7 CMV4.0 4000nits v3.mp4', 7],
];
const FRAMES = 48;
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'aether-rpu-'));

function extract(file, tag) {
  const rpu = path.join(work, `${tag}.rpu`), json = path.join(work, `${tag}.json`);
  const ff = spawnSync('sh', ['-c', `ffmpeg -v error -y -i "${LAB + file}" -map 0:v:0 -c:v copy -bsf:v hevc_mp4toannexb -frames:v ${FRAMES} -f hevc - | dovi_tool extract-rpu -i - -o "${rpu}"`]);
  assert.equal(ff.status, 0, String(ff.stderr));
  assert.equal(spawnSync('dovi_tool', ['export', '-i', rpu, '-d', `all=${json}`]).status, 0);
  const data = fs.readFileSync(rpu);
  const starts = [];
  for (let i = 0; i + 4 <= data.length; i++) if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 0 && data[i + 3] === 1) starts.push(i);
  const nals = starts.map((s, n) => Buffer.concat([Buffer.from([0x7c, 1]), data.subarray(s + 4, starts[n + 1] ?? data.length)]));
  return { nals: nals.map(b => new Uint8Array(b)), ref: JSON.parse(fs.readFileSync(json, 'utf8')) };
}

const num = a => a.map(Number);
function blocks(sec) { return (sec?.ext_metadata_blocks ?? []).map(b => { const [k, v] = Object.entries(b)[0]; return [k, v]; }); }

function compare(d, ref, i) {
  const v = ref.vdr_dm_data;
  const at = `rpu ${i}`;
  assert.equal(d.profile, ref.dovi_profile, at);
  assert.equal(d.compressed, v.compressed, at);
  assert.deepEqual(d.dmId, { affected: v.affected_dm_metadata_id, current: v.current_dm_metadata_id }, at);
  assert.equal(d.sceneRefresh, v.scene_refresh_flag, at);
  assert.equal(d.usePrevMapping, ref.header.use_prev_vdr_rpu_flag, at);
  assert.deepEqual(d.yccToRgb, num([0, 1, 2, 3, 4, 5, 6, 7, 8].map(k => v[`ycc_to_rgb_coef${k}`])), at);
  assert.deepEqual(d.yccToRgbOffset, [0, 1, 2].map(k => v[`ycc_to_rgb_offset${k}`]), at);
  assert.deepEqual(d.rgbToLms, [0, 1, 2, 3, 4, 5, 6, 7, 8].map(k => v[`rgb_to_lms_coef${k}`]), at);
  assert.deepEqual(d.signal, { eotf: v.signal_eotf, eotfParam: [v.signal_eotf_param0, v.signal_eotf_param1, v.signal_eotf_param2], bitDepth: v.signal_bit_depth, colorSpace: v.signal_color_space, chromaFormat: v.signal_chroma_format, fullRange: v.signal_full_range_flag }, at);
  assert.deepEqual(d.source, { minPq: v.source_min_pq, maxPq: v.source_max_pq, diagonal: v.source_diagonal }, at);
  const all = [...blocks(v.cmv29_metadata), ...blocks(v.cmv40_metadata)];
  const by = lvl => all.filter(([k]) => k === `Level${lvl}`).map(([, x]) => x);
  const one = lvl => by(lvl)[0] ?? null;
  const l1 = one(1);
  assert.deepEqual(d.l1, l1 && { minPq: l1.min_pq, avgPq: l1.avg_pq, maxPq: l1.max_pq }, at);
  assert.deepEqual(d.l2, by(2).map(x => ({ targetMaxPq: x.target_max_pq, slope: x.trim_slope, offset: x.trim_offset, power: x.trim_power, chromaWeight: x.trim_chroma_weight, saturationGain: x.trim_saturation_gain, msWeight: x.ms_weight })), at);
  const l3 = one(3);
  assert.deepEqual(d.l3, l3 && { minPqOffset: l3.min_pq_offset, avgPqOffset: l3.avg_pq_offset, maxPqOffset: l3.max_pq_offset }, at);
  const l4 = one(4);
  assert.deepEqual(d.l4, l4 && { anchorPq: l4.anchor_pq, anchorPower: l4.anchor_power }, at);
  const l5 = one(5);
  assert.deepEqual(d.l5, l5 && { left: l5.active_area_left_offset, right: l5.active_area_right_offset, top: l5.active_area_top_offset, bottom: l5.active_area_bottom_offset }, at);
  const l6 = one(6);
  assert.deepEqual(d.l6, l6 && { maxMasteringLuminance: l6.max_display_mastering_luminance, minMasteringLuminance: l6.min_display_mastering_luminance, maxCll: l6.max_content_light_level, maxFall: l6.max_frame_average_light_level }, at);
  // L8 fields beyond the coded length are libdovi defaults (serde omits them): compare the coded ones.
  const coded = o => ({ length: o.length, idx: o.idx, trims: o.trims, ...(o.length > 10 ? { mid: o.mid } : {}), ...(o.length > 12 ? { clip: o.clip } : {}), ...(o.length > 13 ? { sat: o.sat } : {}), ...(o.length > 19 ? { hue: o.hue } : {}) });
  assert.deepEqual(d.l8.map(x => coded({ length: x.length, idx: x.targetDisplayIndex, trims: [x.slope, x.offset, x.power, x.chromaWeight, x.saturationGain, x.msWeight], mid: x.targetMidContrast, clip: x.clipTrim, sat: x.saturationVector, hue: x.hueVector })),
    by(8).map(x => coded({ length: x.length, idx: x.target_display_index, trims: [x.trim_slope, x.trim_offset, x.trim_power, x.trim_chroma_weight, x.trim_saturation_gain, x.ms_weight], mid: x.target_mid_contrast, clip: x.clip_trim, sat: [0, 1, 2, 3, 4, 5].map(k => x[`saturation_vector_field${k}`]), hue: [0, 1, 2, 3, 4, 5].map(k => x[`hue_vector_field${k}`]) })), at);
  const pr = (x, p) => ['red', 'green', 'blue', 'white'].flatMap(c => [x[`${p}_${c}_x`] ?? 0, x[`${p}_${c}_y`] ?? 0]);
  const l9 = one(9);
  assert.deepEqual(d.l9, l9 && { length: l9.length, primaryIndex: l9.source_primary_index, primaries: pr(l9, 'source_primary') }, at);
  assert.deepEqual(d.l10, by(10).map(x => ({ length: x.length, targetDisplayIndex: x.target_display_index, targetMaxPq: x.target_max_pq, targetMinPq: x.target_min_pq, primaryIndex: x.target_primary_index, primaries: pr(x, 'target_primary') })), at);
  const l11 = one(11);
  assert.deepEqual(d.l11, l11 && { contentType: l11.content_type, whitepoint: l11.whitepoint, referenceMode: l11.reference_mode_flag }, at);
  assert.deepEqual(d.cm, [v.cmv29_metadata && 29, v.cmv40_metadata && 40].filter(Boolean), at);
}

for (const [name, file, profile] of CASES) {
  test(`RPU display metadata matches dovi_tool: ${name}`, async t => {
    if (!hasFfmpeg || !hasTool || !have(LAB + file)) return t.skip('needs ffmpeg, dovi_tool and the lab media');
    const { nals, ref } = extract(file, name.replace(/\W+/g, '_'));
    assert.equal(nals.length, ref.length);
    assert.ok(nals.length >= 12);
    const s = await createDoviSession();
    let withTables = 0, seen = { l2: 0, l8: 0, l9: 0, l10: 0, l11: 0, l5: 0, l6: 0 };
    nals.forEach((nal, i) => {
      const r = s.parseRpu(nal);
      compare(r.display, ref[i], i);
      assert.equal(r.display.profile, profile);
      if (r.tables) { assert.equal(r.tables.length, GPU_TABLE_WORDS); withTables++; }
      for (const k of Object.keys(seen)) if (Array.isArray(r.display[k]) ? r.display[k].length : r.display[k]) seen[k]++;
    });
    assert.equal(withTables, nals.length, 'every RPU yields tables inside one epoch');
    const d0 = s.display(nals[0]);
    assert.ok(Math.abs(d0.norm.yccToRgb[0] - d0.yccToRgb[0] / 8192) < 1e-12);
    assert.ok(d0.norm.sourceMaxNits > 100 && d0.norm.sourceMaxNits <= 10000, `${d0.norm.sourceMaxNits}`);
    console.log(`  ${name}: ${nals.length} RPUs equal to dovi_tool, source ${d0.norm.sourceMinNits.toFixed(4)}..${d0.norm.sourceMaxNits.toFixed(0)} nits, blocks seen ${JSON.stringify(seen)}`);
  });
}

test('pq12ToNits anchors', () => {
  assert.equal(pq12ToNits(0), 0);
  assert.ok(Math.abs(pq12ToNits(4095) - 10000) < 1e-6);
});

test('guard: corrupt and non-RPU input is rejected before libdovi', async () => {
  const s = await createDoviSession();
  assert.throws(() => s.display(new Uint8Array([0x7c, 1, 0x19, 0, 0, 0, 0, 0, 0, 0])), /dovi_rpu_display/);
  assert.throws(() => s.display(new Uint8Array(0)), /dovi_rpu_display/);
  assert.throws(() => s.display(new Uint8Array(70000).fill(1)), /too large/);
});
