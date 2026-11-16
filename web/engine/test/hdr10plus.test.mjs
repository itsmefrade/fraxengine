// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { openDemuxer } from '../demuxer.mjs';
import { extractHdr10Plus, hdr10plusDisplayBlock, hdr10plusGpuParams, hdr10plusApply, initHdr10Plus, HDR10PLUS_WGSL, HDR10PLUS_GPU_PARAM_LENGTH } from '../hdr10plus.mjs';
import { have } from './_helpers.mjs';

const LAB = (process.env.AETHER_LAB || decodeURIComponent(new URL('../../../test-media', import.meta.url).pathname));
const files = {
  mkv: path.join(LAB, 'HDR10Plus_PB_EAC3JOC.mkv'),
  hevc: path.join(LAB, 'Test Jellyfin 4K HEVC HDR10 40M.mp4'),
  av1: path.join(LAB, 'Test Jellyfin 4K AV1 HDR10 40M.mp4'),
  lake: '/tmp/aether-hdr10plus/hdr10+test_lake_2021_02_01.mp4',
};
const hasProbe = (() => { try { return spawnSync('ffprobe', ['-version']).status === 0; } catch { return false; } })();

// ---- synthetic payload built bit by bit (same values as the Rust tests) ----
function bitWriter() {
  const bytes = []; let n = 0;
  return {
    put(v, bits) { for (let i = bits - 1; i >= 0; i--) { if (n % 8 === 0) bytes.push(0); bytes[bytes.length - 1] |= ((v >>> i) & 1) << (7 - n % 8); n++; } },
    bytes: () => Uint8Array.from(bytes),
  };
}
function t35Sample() {
  const w = bitWriter();
  for (const b of [0xB5, 0, 0x3C, 0, 1, 4, 1]) w.put(b, 8);
  w.put(1, 2); w.put(400, 27); w.put(0, 1);
  for (const v of [10568, 10410, 11038, 24]) w.put(v, 17);
  w.put(2, 4); w.put(50, 7); w.put(5, 17); w.put(99, 7); w.put(8412, 17); w.put(0, 10);
  w.put(0, 1);
  w.put(1, 1); w.put(69, 12); w.put(147, 12); w.put(9, 4);
  for (const a of [180, 362, 546, 696, 743, 787, 842, 898, 936]) w.put(a, 10);
  w.put(0, 1);
  return w.bytes();
}
function escapeEpb(b) {
  const o = []; let z = 0;
  for (const x of b) { if (z >= 2 && x <= 3) { o.push(3); z = 0; } o.push(x); z = x === 0 ? z + 1 : 0; }
  return Uint8Array.from(o);
}
function hevcPacket(t35) {
  const rbsp = Uint8Array.from([4, t35.length, ...t35, 0x80]);
  const nal = Uint8Array.from([39 << 1, 1, ...escapeEpb(rbsp)]);
  const slice = Uint8Array.from([0x26, 1, 0xAF]);
  const out = [];
  for (const n of [slice, nal]) out.push(0, 0, (n.length >> 8) & 255, n.length & 255, ...n);
  return Uint8Array.from(out);
}

test('synthetic HEVC and AV1 packets parse and map', async () => {
  await initHdr10Plus();
  const t35 = t35Sample();
  const hevcTrack = { codec: 'hvc1.2.4.L150.B0', description: new Uint8Array(23).fill(0).map((_, i) => (i === 21 ? 3 : 0)) };
  const p = extractHdr10Plus({ data: hevcPacket(t35) }, hevcTrack);
  assert.ok(p, 'found in HEVC');
  assert.equal(p.targetedSystemDisplayMaximumLuminance, 400);
  assert.deepEqual(p.windows[0].maxscl, [10568, 10410, 11038]);
  assert.deepEqual(p.windows[0].kneePoint, { x: 69, y: 147 });
  assert.equal(p.windows[0].bezierCurveAnchors.length, 9);
  assert.deepEqual([...p.raw], [...t35]);

  const obu = Uint8Array.from([0x12, 0x00, (5 << 3) | 2, t35.length + 1, 4, ...t35]);
  const a = extractHdr10Plus(obu, { codec: 'av01.0.13M.10' });
  assert.deepEqual(a.windows, p.windows);

  assert.equal(extractHdr10Plus({ data: hevcPacket(t35) }, { codec: 'avc1.640028' }), null, 'H.264 is not HDR10+');
  assert.equal(extractHdr10Plus(new Uint8Array(0), hevcTrack), null);
  assert.equal(extractHdr10Plus(Uint8Array.from([1, 2, 3]), hevcTrack), null);

  const g = hdr10plusGpuParams(p, 400);
  assert.equal(g.length, HDR10PLUS_GPU_PARAM_LENGTH);
  assert.equal(g[0], 1); assert.equal(g[1], 1); assert.equal(g[4], 400); assert.equal(g[7], 9);
  assert.ok(Math.abs(g[2] - 1103.8) < 0.01);
  const peak = hdr10plusApply(p, 400, [0.6, 0.6, 0.6]);
  assert.ok(peak.every(v => v > 0 && v < 0.6), 'bright pixel is compressed');
  assert.ok(HDR10PLUS_WGSL.includes('fn hdr10plus_map'));
});

test('the HDR display block adapts to the panel unless the target display is asked for', async () => {
  await initHdr10Plus();
  const hevcTrack = { codec: 'hvc1.2.4.L150.B0', description: new Uint8Array(23).fill(0).map((_, i) => (i === 21 ? 3 : 0)) };
  const p = extractHdr10Plus({ data: hevcPacket(t35Sample()) }, hevcTrack);
  const unset = Array.from(hdr10plusDisplayBlock(p, 1000));
  assert.deepEqual(unset, Array.from(hdr10plusDisplayBlock(p, 1000, 'panel')));
  assert.notDeepEqual(unset, Array.from(hdr10plusDisplayBlock(p, 1000, 'real')));
});

test('mutated packets never throw', async () => {
  await initHdr10Plus();
  const base = hevcPacket(t35Sample());
  const track = { codec: 'hvc1.2.4.L150.B0' };
  let seed = 99;
  const rnd = () => (seed = (seed * 1103515245 + 12345) >>> 0);
  for (let r = 0; r < 800; r++) {
    const d = base.slice(0, r % 7 === 0 ? rnd() % base.length : base.length);
    for (let k = 0; k < 1 + r % 4 && d.length; k++) d[rnd() % d.length] = rnd() & 255;
    const p = extractHdr10Plus(d, track);
    if (p) hdr10plusGpuParams(p, 1000);
    extractHdr10Plus(d, { codec: 'av01.0.13M.10' });
  }
});

// ---- ffprobe reference (default writer; JSON would collapse the repeated keys) ----
function ffprobeFrames(file, n) {
  const r = spawnSync('ffprobe', ['-v', 'error', '-select_streams', 'v:0', '-read_intervals', `%+#${n}`, '-show_frames',
    '-show_entries', 'frame=side_data_list', file], { encoding: 'utf8', maxBuffer: 1 << 28 });
  const frames = [];
  for (const block of r.stdout.split('[FRAME]').slice(1)) {
    let hp = null;
    for (const sd of block.split('[SIDE_DATA]').slice(1)) {
      if (!/HDR10\+/.test(sd)) continue;
      hp = { maxscl: [], pct: [], anchors: [] };
      let lastPct = null;
      for (const line of sd.split('\n')) {
        const m = /^([\w ]+)=(.*)$/.exec(line.trim());
        if (!m) continue;
        const [, k, v] = m;
        const frac = x => { const [a, b] = x.split('/').map(Number); return { n: a, d: b }; };
        if (k === 'application version') hp.version = Number(v);
        else if (k === 'num_windows') hp.windows = Number(v);
        else if (k === 'targeted_system_display_maximum_luminance') hp.target = frac(v);
        else if (k === 'maxscl') hp.maxscl.push(frac(v));
        else if (k === 'average_maxrgb') hp.avg = frac(v);
        else if (k === 'distribution_maxrgb_percentage') lastPct = Number(v);
        else if (k === 'distribution_maxrgb_percentile') hp.pct.push({ percentage: lastPct, ...frac(v) });
        else if (k === 'fraction_bright_pixels') hp.fbp = frac(v);
        else if (k === 'knee_point_x') hp.kx = frac(v);
        else if (k === 'knee_point_y') hp.ky = frac(v);
        else if (k === 'bezier_curve_anchors') hp.anchors.push(frac(v));
        else if (k === 'color_saturation_weight') hp.sat = frac(v);
      }
    }
    frames.push(hp);
  }
  return frames;
}

/** First `n` video frames of the file in presentation order, HDR10+ extracted from their packets. */
async function ourFrames(file, n) {
  const d = await openDemuxer({ path: file });
  const video = d.tracks().find(t => t.kind === 'video');
  const pk = [];
  let p;
  while (pk.length < n + 24 && (p = d.nextPacket())) if (p.trackId === video.id) pk.push(p);
  const out = pk.sort((a, b) => a.ptsUs - b.ptsUs).slice(0, n).map(x => extractHdr10Plus(x, video));
  d.close();
  return out;
}

function same(ours, ref, what) {
  if (!ref) return assert.equal(ours, null, `${what}: expected none`);
  assert.ok(ours, `${what}: expected HDR10+`);
  const w = ours.windows[0];
  assert.equal(ours.applicationVersion, ref.version, what);
  assert.equal(ours.numWindows, ref.windows, what);
  assert.equal(ref.target.d, 1, what);
  assert.equal(ours.targetedSystemDisplayMaximumLuminance, ref.target.n, what);
  assert.deepEqual(w.maxscl, ref.maxscl.map(f => f.n), what);
  assert.ok(ref.maxscl.every(f => f.d === 100000));
  assert.equal(w.averageMaxrgb, ref.avg.n, what);
  assert.deepEqual(w.distributionMaxrgb.map(x => [x.percentage, x.percentile]), ref.pct.map(x => [x.percentage, x.n]), what);
  assert.equal(w.fractionBrightPixels, ref.fbp.n, what);
  if (ref.kx) {
    assert.deepEqual(w.kneePoint, { x: ref.kx.n, y: ref.ky.n }, what);
    assert.deepEqual(w.bezierCurveAnchors, ref.anchors.map(f => f.n), what);
  } else {
    assert.equal(w.kneePoint, null, what);
  }
  if (ref.sat) assert.equal(w.colorSaturationWeight, ref.sat.n, what);
}

for (const [name, expectSome] of [['mkv', true], ['lake', true], ['hevc', false], ['av1', false]]) {
  test(`first 20 frames of ${name} match ffprobe`, async t => {
    if (!have(files[name]) || !hasProbe) return t.skip('missing media or ffprobe');
    const ref = ffprobeFrames(files[name], 20);
    assert.equal(ref.length, 20);
    const ours = await ourFrames(files[name], 20);
    ref.forEach((r, i) => same(ours[i], r, `${name} frame ${i}`));
    assert.equal(ref.some(Boolean), expectSome, `${name} presence`);
    if (expectSome) {
      const g = hdr10plusGpuParams(ours[0], 1000);
      assert.equal(g[0], 1);
    }
  });
}
