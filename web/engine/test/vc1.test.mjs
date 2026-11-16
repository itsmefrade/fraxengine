// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { openDemuxer } from '../demuxer.mjs';
import { createVc1Decoder, isVc1Track, vc1Extradata, vc1FourCC } from '../vc1.mjs';
import { labFiles, have, hasFfmpeg } from './_helpers.mjs';

// First N frames of each lab VC-1 file against `ffmpeg -f framemd5` (the Rust decoder is bit exact on all frames, see rust/aether-vc1).
const N = 120;
async function run(file) {
  const d = await openDemuxer({ path: file });
  const t = d.tracks().find(isVc1Track);
  assert.ok(t, 'VC-1 track found');
  assert.equal(vc1FourCC(t), 'WVC1');
  assert.equal(vc1Extradata(t).subarray(0, 4).join(), '0,0,1,15');   // sequence header start code 0x0F
  const dec = await createVc1Decoder(t);
  const out = [];
  for (let p; out.length < N && (p = d.nextPacket());) {
    if (p.trackId !== t.id) continue;
    for (const f of dec.decode(p.data, p.ptsUs)) out.push(f);
  }
  d.close();
  const info = dec.info();
  dec.close();
  return { out: out.slice(0, N), info, errors: 0 };
}

for (const [name, file] of [['VC1-LPCM7.1', labFiles.vc1Lpcm], ['DTS-HRA5.1_VC1', labFiles.dtsHra51]]) {
  test(`vc1 decoder ${name}: framemd5 identical, timestamps ascending`, { skip: !have(file) || !hasFfmpeg }, async () => {
    const { out, info } = await run(file);
    assert.equal(out.length, N);
    assert.equal(info.width, 1920);
    const ref = spawnSync('ffmpeg', ['-v', 'error', '-i', file, '-map', '0:v:0', '-frames:v', String(N), '-f', 'framemd5', '-'], { encoding: 'utf8', maxBuffer: 1 << 24 });
    const want = ref.stdout.split('\n').filter(l => l && l[0] !== '#').map(l => l.split(',').pop().trim());
    assert.equal(want.length, N);
    out.forEach((f, i) => {
      assert.equal(crypto.createHash('md5').update(f.y).update(f.u).update(f.v).digest('hex'), want[i], `frame ${i}`);
      if (i) assert.ok(f.ptsUs > out[i - 1].ptsUs, `pts ascending at ${i}`);
      assert.equal(f.interlaced, false);
    });
  });
}

// ---- timestamps: frame pts equal FFmpeg's best_effort_timestamp (the first frame of the lab files is at 83 ms, not 0)
import fs from 'node:fs';
import { vc1DisplayDurationUs, vc1RepeatPict } from '../vc1.mjs';
// fixtures/vc1-ffprobe-pts.json is git-ignored (ffprobe output of the lab media); without it the pts tests skip.
const PTS_URL = new URL('./fixtures/vc1-ffprobe-pts.json', import.meta.url);
const PTS_REF = fs.existsSync(PTS_URL) ? JSON.parse(fs.readFileSync(PTS_URL, 'utf8')).files : null;

for (const [key, name, file] of [['lpcm71', 'VC1-LPCM7.1', labFiles.vc1Lpcm], ['dtshra51', 'DTS-HRA5.1_VC1', labFiles.dtsHra51]]) {
  test(`vc1 pts ${name}: first 300 frames match ffprobe best_effort_timestamp`, { skip: !have(file) || !PTS_REF }, async () => {
    const want = PTS_REF[key];
    assert.equal(want.length, 300);
    const d = await openDemuxer({ path: file });
    const t = d.tracks().find(isVc1Track);
    const dec = await createVc1Decoder(t, { copy: false });
    const got = [];
    for (let p; got.length < 300 && (p = d.nextPacket());) {
      if (p.trackId !== t.id) continue;
      for (const f of dec.decode(p.data, p.ptsUs)) got.push(f.ptsUs);
    }
    d.close(); dec.close();
    assert.deepEqual(got.slice(0, 300), want);
    assert.ok(got[0] > 0, 'video does not start at 0');
  });
}

test('vc1 pull-down: duration follows FFmpeg repeat_pict', () => {
  assert.equal(vc1RepeatPict(false, 0), 0);
  assert.equal(vc1RepeatPict(true, 0), 1);
  assert.equal(vc1RepeatPict(false, 2), 4);
  assert.equal(vc1RepeatPict(true, 3), 1);                      // RFF wins, as in vc1dec.c
  const ft = 1e6 * 1001 / 24000;
  assert.equal(vc1DisplayDurationUs(ft, 0), 41708);             // plain frame
  assert.equal(vc1DisplayDurationUs(ft, 1), 62563);             // RFF: one extra field, 1.5 frames
  assert.equal(vc1DisplayDurationUs(ft, 2), 83417);             // RPTFRM 1: twice
  // 2-3 telecine: four film frames (RFF on every second one) span ten fields = five frame times
  const sum = [0, 1, 0, 1].reduce((a, r) => a + vc1DisplayDurationUs(ft, r), 0);
  assert.ok(Math.abs(sum - 5 * ft) <= 2);
});
