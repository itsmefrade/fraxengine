// SPDX-License-Identifier: LGPL-3.0-only
// Dolby Vision and HEVC WebCodecs strings from real MP4 and Matroska files.
import test from 'node:test';
import assert from 'node:assert/strict';
import { openDemuxer, hevcCodecString, dolbyVisionCodecString } from '../demuxer.mjs';
import { have } from './_helpers.mjs';

const LAB = (process.env.AETHER_LAB || decodeURIComponent(new URL('../../../test-media', import.meta.url).pathname)) + '/';
const WEBCODECS_HEVC = /^(hvc1|hev1)\.([ABC]?\d+)\.[0-9A-F]+\.[LH]\d+(\.[0-9A-F]{2})+$/;
const WEBCODECS_DV = /^(dvh1|dvhe|dva1|dvav|dav1)\.\d{2}\.\d{2}$/;
const CASES = [
  ['Test Jellyfin 4K DV P5.mp4', 'dvh1.05.09', 'hvc1.2.4.H153.B0'],
  ['Test Jellyfin 4K DV P8.1.mp4', 'dvh1.08.09', 'hvc1.2.4.H153.B0'],
  ['Test Jellyfin 1080p DV P8.4.mp4', 'dvh1.08.05', 'hvc1.2.4.L123.B0'],
  ['DV8_TEST.mp4', 'dvhe.08.06', 'hev1.2.4.H153.90'],
  ['DV FEL Test DT DL P7 CMV4.0 4000nits v3.mp4', 'dvhe.07.06', 'hev1.2.4.H153.90'],
  ['DV FEL Power Rangers Credits Test.mkv', /^dv(h1|he)\.07\.06$/, 'hvc1.2.4.H153.90'],
  ['awaken-girl.4K.HDR.DV.mkv', /^dv(h1|he)\.07\.06$/, 'hvc1.2.4.H153.90'],
];
for (const [file, dv, hevc] of CASES) {
  test(`codec strings: ${file}`, async t => {
    if (!have(LAB + file)) return t.skip('missing media');
    const d = await openDemuxer({ path: LAB + file });
    const v = d.tracks().find(x => x.kind === 'video');
    assert.match(v.dolbyVisionCodecString, WEBCODECS_DV);
    if (dv instanceof RegExp) assert.match(v.dolbyVisionCodecString, dv); else assert.equal(v.dolbyVisionCodecString, dv);
    assert.equal(v.fallbackCodecString, hevc);
    assert.match(v.fallbackCodecString, WEBCODECS_HEVC);
    // the hvcC rebuild agrees with the container-level string
    assert.equal(hevcCodecString(v.description, v.fallbackCodecString.slice(0, 4)), v.fallbackCodecString);
    d.close();
  });
}
test('dolbyVisionCodecString families', () => {
  const dv = { profile: 8, level: 9 };
  assert.equal(dolbyVisionCodecString(dv), 'dvh1.08.09');
  assert.equal(dolbyVisionCodecString(dv, { inband: true }), 'dvhe.08.09');
  assert.equal(dolbyVisionCodecString({ profile: 9, level: 4 }, { base: 'avc' }), 'dva1.09.04');
  assert.equal(dolbyVisionCodecString({ profile: 10, level: 6 }, { base: 'av1' }), 'dav1.10.06');
});

import { labFiles } from './_helpers.mjs';
import { execFileSync } from 'node:child_process';
test('Matroska H.264 key flags equal ffprobe (POCAWE: muxer flags 72 pictures, 25 are IDR)', async t => {
  if (!have(labFiles.pocawe)) return t.skip('missing media');
  let ff;
  try { ff = execFileSync('ffprobe', ['-v', 'error', '-select_streams', 'v:0', '-show_entries', 'packet=pts,flags', '-of', 'csv=p=0', labFiles.pocawe], { maxBuffer: 1 << 26 }).toString().trim().split('\n'); }
  catch { return t.skip('no ffprobe'); }
  const want = new Set(ff.filter(l => l.includes('K')).map(l => Number(l.split(',')[0])));
  const d = await openDemuxer({ path: labFiles.pocawe });
  const v = d.tracks().find(x => x.kind === 'video');
  const got = new Set();
  let n = 0;
  for (let p; (p = d.nextPacket());) if (p.trackId === v.id) { n++; if (p.key) got.add(Math.round(p.ptsUs / 1000)); }
  assert.equal(n, ff.length);
  assert.deepEqual([...got].sort((a, b) => a - b), [...want].sort((a, b) => a - b));
  assert.equal(want.size, 25);
  d.close();
});
