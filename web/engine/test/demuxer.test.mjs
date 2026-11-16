// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import { openDemuxer } from '../demuxer.mjs';
import { fixtureFiles, labFiles, have } from './_helpers.mjs';

const MAX_PACKETS_BIG = 3000;

for (const [name, file] of Object.entries({ ...fixtureFiles, ...labFiles })) {
  test(`demux ${name}`, async t => {
    if (!have(file)) return t.skip(`missing fixture ${file}`);
    let d;
    try {
      d = await openDemuxer({ path: file });
    } catch (e) {
      // Known engine limit (see README gaps): MKV ContentEncodings are refused by aether-demux.
      if (/UnsupportedTrack/.test(e.message)) return t.skip(`engine refuses this file: ${e.message}`);
      throw e;
    }
    const tracks = d.tracks();
    assert.ok(tracks.length >= 1);
    for (const tr of tracks) {
      assert.ok(['video', 'audio', 'subtitle'].includes(tr.kind));
      assert.equal(typeof tr.codec, 'string');
      assert.ok(tr.timescale > 0);
      if (tr.kind === 'video') assert.ok(tr.width > 0 && tr.height > 0);
      if (tr.kind === 'audio') assert.ok(tr.sampleRate > 0 && tr.channels > 0);
    }
    const video = tracks.find(x => x.kind === 'video');
    console.log(`  ${name}: ${tracks.map(x => `${x.kind}:${x.codec}`).join(' ')} packets=${d.count}`);

    // sequential read: monotonic dts per track, only keyframes may start a video decode
    const big = d.count > 12000;
    const last = new Map(), seen = new Map();
    let n = 0, firstVideo = null, keys = 0;
    for (let p; (p = d.nextPacket()) && (!big || n < MAX_PACKETS_BIG);) {
      n++;
      assert.ok(p.data instanceof Uint8Array && p.data.length > 0, 'packet has data');
      assert.ok(Number.isFinite(p.ptsUs) && Number.isFinite(p.dtsUs) && p.durationUs >= 0);
      assert.ok(p.dtsUs <= p.ptsUs, `dts <= pts (track ${p.trackId})`);
      if (last.has(p.trackId)) assert.ok(p.dtsUs >= last.get(p.trackId), `dts monotonic on track ${p.trackId} at packet ${n}`);
      last.set(p.trackId, p.dtsUs);
      seen.set(p.trackId, (seen.get(p.trackId) ?? 0) + 1);
      if (video && p.trackId === video.id) {
        firstVideo ??= p;
        if (p.key) keys++;
      }
    }
    for (const tr of tracks) assert.ok(seen.get(tr.id) > 0, `track ${tr.id} produced packets`);
    // POCAWE starts on a non-IDR I picture that muxers flag as key; keys are verified (IDR / IRAP), like ffprobe's flags.
    if (video) { assert.ok(firstVideo.key || name === 'pocawe', 'first video packet is a keyframe'); assert.ok(keys >= 1); }

    // seek to the middle: the first video packet after the seek is a keyframe at or before the target
    if (video) {
      const target = d.startTimeUs + Math.floor((d.durationUs ?? last.get(video.id)) / 2);
      d.seek(target);
      let p;
      while ((p = d.nextPacket()) && p.trackId !== video.id);
      assert.ok(p, 'video packet after seek');
      assert.ok(p.key, 'first video packet after seek is a keyframe');
      assert.ok(p.ptsUs <= target, `seek lands at or before target (${p.ptsUs} <= ${target})`);
      // and nothing is lost: the stream continues
      assert.ok(d.nextPacket());
      d.seek(0);
      const q = d.nextPacket();
      assert.ok(q);
    }
    d.close();
  });
}

test('codec strings', async t => {
  if (!have(fixtureFiles.dtsTruehd)) return t.skip('missing fixture');
  const d = await openDemuxer({ path: fixtureFiles.dtsTruehd });
  const tr = d.tracks();
  assert.equal(tr[0].codec, 'hvc1.2.4.H153.90');
  assert.deepEqual(tr[0].dolbyVision, { profile: 7, level: 6, rpuPresent: true, elPresent: true, blPresent: true, blCompat: 6 });
  assert.deepEqual(tr.slice(1).map(x => x.codec), ['dts', 'truehd']);
  assert.equal(tr[1].language, 'eng');
});

test('Blob source (in memory off a Worker)', async t => {
  if (!have(fixtureFiles.dtsx)) return t.skip('missing fixture');
  const { readFileSync } = await import('node:fs');
  const d = await openDemuxer(new Blob([readFileSync(fixtureFiles.dtsx)]));
  assert.equal(d.tracks().length, 2);
  assert.ok(d.nextPacket().key);
});

// The Matroska dts look-ahead follows the reorder depth the HEVC SPS declares instead of a fixed 16 to 64 packets.
for (const [name, depth] of [['awakenGirl', 2], ['eac3Joc', 4], ['felPowerRangers', 1]]) {
  test(`matroska dts look-ahead is the SPS reorder depth (${name})`, async t => {
    const file = labFiles[name];
    if (!have(file)) return t.skip(`missing fixture ${file}`);
    const d = await openDemuxer({ path: file });
    const video = d.tracks().find(x => x.kind === 'video' && !x.enhancementLayer);
    assert.equal(video.reorderDepth, depth);
    let first = null;
    while (!first) { const p = d.nextPacket(); if (p.trackId === video.id) first = p; }
    const ahead = d.queue.filter(q => q.trackId === video.id).length;
    assert.ok(ahead <= depth, `${ahead} video packets read ahead of the first one`);
    let n = 1, last = first.dtsUs;
    for (let p; n < 4000 && (p = d.nextPacket());) {
      if (p.trackId !== video.id) continue;
      assert.ok(p.dtsUs <= p.ptsUs && p.dtsUs >= last, `dts in order and not after pts at video packet ${n}`);
      last = p.dtsUs; n++;
    }
    assert.equal(d.reorderGuess.size, 0, 'the declared depth held');
    d.close?.();
  });
}

// MP4 stores each track in chunks, so the sound that plays at a keyframe can lie in front of it. A seek starts at that sound and
// hands on nothing else before the keyframe, so the player need not seek 3 s early to catch it (a whole GOP early on long GOPs).
test('an MP4 seek starts at the sound stored in front of its keyframe', async t => {
  const file = (process.env.AETHER_LAB || decodeURIComponent(new URL('../../../test-media', import.meta.url).pathname)) + '/DTS-UHD DTSX P2 Sample.mp4';
  if (!have(file)) return t.skip('missing fixture');
  const d = await openDemuxer({ path: file });
  assert.equal(d.seekKeepsAudio, true);
  const video = d.tracks().find(x => x.kind === 'video'), audio = d.tracks().find(x => x.kind === 'audio');
  for (const target of [6_200_000, 9_000_000, 15_000_000]) {
    d.seek(target);
    const before = [];
    let p;
    while ((p = d.nextPacket()) && p.trackId === audio.id) before.push(p);
    assert.equal(p.trackId, video.id, 'only audio comes before the keyframe');
    assert.ok(p.key && p.ptsUs <= target, 'the first video packet is the keyframe at or before the target');
    assert.ok(before.length > 0, 'this file stores the sound of a keyframe before it');
    assert.ok(before[0].ptsUs <= p.ptsUs && before[0].ptsUs + before[0].durationUs > p.ptsUs, `the first packet sounds at the keyframe (${before[0].ptsUs} over ${p.ptsUs})`);
    for (let i = 1; i < before.length; i++) assert.ok(before[i].ptsUs > before[i - 1].ptsUs, 'audio in order');
  }
  d.close();
});
