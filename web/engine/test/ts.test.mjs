// SPDX-License-Identifier: LGPL-3.0-only
// MPEG-TS / M2TS and lazily indexed files through the adapter: track lists, audio decoded by the engine
// plugins against ffmpeg, seeking, and the bytes an HTTP Range source fetches.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { openDemuxer, annexBToLengthPrefixed } from '../demuxer.mjs';
import { HttpRangeSource } from '../sources.mjs';
import { createAudioPlugin, engineCodec } from '../audio-plugins.mjs';
import { labFiles, fixtureFiles, have, hasFfmpeg, ffmpegReference, ffprobeStreams, comparePlanar, decodeWindow } from './_helpers.mjs';

const SECONDS = 3;
const FILES = {
  amaze: labFiles.amaze,
  truehdCheck: labFiles.truehdCheck,
  dtsxDemo: labFiles.dtsxDemo,
  dredd: labFiles.dredd,
  pocawe: labFiles.pocawe,
};

function nalTypes(annexB) {
  const out = [];
  for (let i = 0; i + 3 < annexB.length; i++) {
    if (annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 1) out.push(annexB[i + 3] & 31);
  }
  return out;
}

for (const [name, file] of Object.entries(FILES)) {
  test(`${name}: tracks`, async t => {
    if (!have(file)) return t.skip(`missing ${file}`);
    const d = await openDemuxer({ path: file });
    const tracks = d.tracks();
    console.log(`  ${name}: ${d.container} duration=${(d.durationUs / 1e6).toFixed(2)}s start=${(d.startTimeUs / 1e6).toFixed(3)}s ` +
      tracks.map(x => `${x.id}:${x.kind}:${x.codec}${x.channels ? `/${x.channels}ch` : ''}`).join(' '));
    assert.ok(tracks.some(x => x.kind === 'video'));
    assert.ok(tracks.some(x => x.kind === 'audio'));
    for (const tr of tracks.filter(x => x.kind === 'audio')) assert.ok(tr.channels > 0 && tr.sampleRate > 0, `audio track ${tr.id} has a layout`);
    if (d.container === 'mpegts') {
      for (const tr of tracks) assert.ok(tr.sourcePid > 0);
      const v = tracks.find(x => x.kind === 'video');
      assert.equal(v.annexB, true);
      assert.equal(v.description, undefined);
      assert.match(v.codec, /^avc1\.[0-9A-F]{6}$/);
      const core = tracks.find(x => x.id === (0x10000 | x.sourcePid) && x.codec === 'ac-3');
      if (name === 'truehdCheck' || name === 'amaze') assert.ok(core, 'AC-3 core split out of the TrueHD PID');
    }
    d.close();
  });

  test(`${name}: audio decodes like ffmpeg`, async t => {
    if (!have(file)) return t.skip(`missing ${file}`);
    if (!hasFfmpeg) return t.skip('no ffmpeg');
    const probe = ffprobeStreams(file);
    const audioProbe = probe.filter(s => s.type === 'audio');
    const d0 = await openDemuxer({ path: file });
    const audio = d0.tracks().filter(x => x.kind === 'audio');
    d0.close();
    for (const [n, track] of audio.entries()) {
      const d = await openDemuxer({ path: file });
      const tr = d.tracks().find(x => x.id === track.id);
      const plugin = createAudioPlugin(tr, { presentation: 2, joc: false }); // the bed is what ffmpeg decodes
      if (!plugin) { d.close(); continue; }
      plugin.configure(tr);
      const out = decodeWindow(d, plugin, tr.id, SECONDS);
      assert.ok(out, `track ${tr.id} decoded something`);
      const codec = engineCodec(tr.codec);
      let ref;
      let how;
      if (d.container === 'mpegts' && tr.id >= 0x10000) {
        // The AC-3 core has no ffmpeg stream: write its frames to a raw .ac3 and decode that.
        const d2 = await openDemuxer({ path: file });
        const chunks = [];
        for (let p; (p = d2.nextPacket());) {
          if (p.trackId !== tr.id) continue;
          if (p.ptsUs - d2.startTimeUs > SECONDS * 1e6 + 1e5) break;
          chunks.push(p.data);
        }
        d2.close();
        const raw = path.join(os.tmpdir(), `aether-core-${process.pid}-${name}.ac3`);
        fs.writeFileSync(raw, Buffer.concat(chunks));
        const refOut = `${raw}.f32le`;
        execFileSync('ffmpeg', ['-v', 'error', '-y', '-drc_scale', '0', '-i', raw, '-f', 'f32le', '-acodec', 'pcm_f32le', refOut]);
        const b = fs.readFileSync(refOut);
        ref = new Float32Array(b.buffer, b.byteOffset, b.byteLength >> 2);
        how = 'raw ac3 via ffmpeg';
      } else {
        const sp = d.container === 'mpegts' ? probe.find(s => s.id === tr.sourcePid && s.type === 'audio') : audioProbe[n];
        assert.ok(sp, `ffprobe stream for track ${tr.id}`);
        ref = ffmpegReference(file, sp.index, SECONDS, ['ac3', 'eac3'].includes(codec) ? ['-drc_scale', '0'] : []);
        how = `ffmpeg stream ${sp.index}`;
      }
      const c = comparePlanar(out.planar, out.layout, ref);
      const tol = out.lossless || tr.codec === 'pcm' ? 0 : 1e-4;
      console.log(`  ${name} track ${tr.id} ${tr.codec}: ${out.layout.join(',')} frames=${out.planar[0].length} ref=${c.refFrames} common=${c.frames} maxdiff=${c.max} lossless=${out.lossless} (${how})`);
      assert.ok(c.frames >= (SECONDS - 0.2) * out.sampleRate, 'covers the window');
      assert.ok(c.max <= tol, `max abs diff ${c.max} <= ${tol}`);
      plugin.close();
      d.close();
    }
  });

  test(`${name}: seek to the middle lands on a keyframe`, async t => {
    if (!have(file)) return t.skip(`missing ${file}`);
    const d = await openDemuxer({ path: file });
    const video = d.tracks().find(x => x.kind === 'video' && !x.enhancementLayer);
    const target = d.startTimeUs + Math.floor(d.durationUs / 2);
    const bytesBefore = d.source.bytesFetched;
    d.seek(target);
    let p;
    while ((p = d.nextPacket()) && p.trackId !== video.id);
    assert.ok(p, 'video packet after seek');
    assert.ok(p.key, 'first video packet after seek is a keyframe');
    assert.ok(p.ptsUs <= target, `lands at or before the target (${p.ptsUs} <= ${target})`);
    assert.ok(target - p.ptsUs < 20e6, `and not absurdly far before it (${(target - p.ptsUs) / 1e6}s)`);
    if (video.annexB) {
      const types = nalTypes(p.data);
      // IDR slice, or an I picture introduced by parameter sets and a recovery point SEI (open GOP)
      assert.ok(types.includes(5) || (types.includes(7) && types.includes(6)), `random access point (NAL types ${types.join(',')})`);
      const lp = annexBToLengthPrefixed(p.data);
      assert.ok(lp.length > 0 && lp.length <= p.data.length + 8);
    }
    // keep reading: ordered, and no hole after seeking forward and then back into the skipped part
    const lastDts = new Map();
    const count = async (from, seconds) => {
      d.seek(from);
      let n = 0, startUs = null;
      for (let q; (q = d.nextPacket());) {
        startUs ??= q.ptsUs;
        if (q.ptsUs - startUs > seconds * 1e6) break;
        if (lastDts.has(q.trackId) && q.trackId === video.id) assert.ok(q.dtsUs - lastDts.get(q.trackId) < 0.5e6, `no hole in the video (dts jump ${(q.dtsUs - lastDts.get(q.trackId)) / 1e6}s at ${n})`);
        lastDts.set(q.trackId, q.dtsUs);
        n++;
      }
      lastDts.clear();
      return n;
    };
    assert.ok(await count(d.startTimeUs + Math.floor(d.durationUs * 0.75), 4) > 50);
    assert.ok(await count(d.startTimeUs + Math.floor(d.durationUs * 0.25), 12) > 100, 'seek back into the skipped part reads on without a hole');
    assert.ok(await count(d.startTimeUs + 1e6, 12) > 100);
    d.close();
  });
}

// ---------- HTTP Range source ----------

/** Synchronous stand-in for XHR: serves byte ranges of a local file and counts them. */
function fileTransport(file) {
  const fd = fs.openSync(file, 'r');
  const size = fs.fstatSync(fd).size;
  const log = { requests: 0, bytes: 0 };
  const transport = (_url, range) => {
    const [a, b] = /bytes=(\d+)-(\d+)/.exec(range).slice(1).map(Number);
    const end = Math.min(b, size - 1);
    const buf = Buffer.alloc(end - a + 1);
    fs.readSync(fd, buf, 0, buf.length, a);
    log.requests++;
    log.bytes += buf.length;
    return { status: 206, contentRange: `bytes ${a}-${end}/${size}`, body: buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.length) };
  };
  return { transport, log, size };
}

// Every container needs headers plus the first access units. Matroska walks block headers in bounded
// steps and, for a seek, jumps to the Cue's block (CueClusterPosition + CueRelativePosition). What is left
// above the others is the adapter's dts look-ahead, at least 16 video frames (about 9 MiB for the 4K DV file).
const HTTP_BOUND = { mpegts: 4 * 1024 * 1024, mp4: 4 * 1024 * 1024, matroska: 10 * 1024 * 1024 };
const MKV_SEEK_BOUND = 8 * 1024 * 1024;
const lab = path.dirname(labFiles.pocawe);
const httpCases = {
  ...FILES,
  mp4: path.join(lab, 'Test Jellyfin 4K HEVC HDR10 40M.mp4'),
  mkvDv: labFiles.awakenGirl,
  mkvFel: path.join(lab, 'DV FEL Power Rangers Credits Test.mkv'),
  mkvDtsx: path.join(lab, 'DTSX Gravity.mkv'),
};
for (const [name, file] of Object.entries(httpCases)) {
  test(`${name}: HTTP Range source fetches a bounded number of bytes before the first packet`, async t => {
    if (!have(file)) return t.skip(`missing ${file}`);
    const { transport, log, size } = fileTransport(file);
    const d = await openDemuxer({ url: 'http://test.invalid/media', transport });
    assert.ok(d.source instanceof HttpRangeSource);
    assert.equal(d.source.length, size);
    const atOpen = log.bytes;
    const p = d.nextPacket();
    assert.ok(p && p.data.length > 0);
    console.log(`  ${name}: file ${(size / 1048576).toFixed(0)} MiB, fetched ${(atOpen / 1024).toFixed(0)} KiB at open, ${(log.bytes / 1024).toFixed(0)} KiB (${log.requests} requests) at the first packet`);
    const bound = HTTP_BOUND[d.container];
    assert.ok(log.bytes <= bound, `${log.bytes} bytes fetched before the first packet (bound ${bound})`);
    // and seeking to the middle stays bounded too (no scan of the first half)
    d.seek(d.startTimeUs + Math.floor(d.durationUs / 2));
    const beforeSeek = log.bytes;
    assert.ok(d.nextPacket());
    console.log(`  ${name}: ${(log.bytes / 1048576).toFixed(1)} MiB (${log.requests} requests) after seeking to the middle (${((log.bytes - beforeSeek) / 1048576).toFixed(1)} MiB for the seek)`);
    if (d.container === 'matroska') assert.ok(log.bytes - beforeSeek <= MKV_SEEK_BOUND, `${log.bytes - beforeSeek} bytes for the seek`);
    assert.ok(log.bytes <= 6 * bound, `${log.bytes} bytes after the seek`);
    d.close();
  });
}

test('sequential reading of a transport stream skips and repeats nothing', async t => {
  for (const file of [labFiles.amaze, labFiles.dredd]) {
    if (!have(file)) return t.skip('missing lab media');
    const d = await openDemuxer({ path: file });
    const last = new Map(), n = new Map();
    let bytes = 0;
    for (let p; (p = d.nextPacket()) && bytes < 64 * 1024 * 1024;) {
      bytes += p.data.length;
      if (last.has(p.trackId)) {
        const gap = p.dtsUs - last.get(p.trackId);
        assert.ok(gap > 0 && gap < 0.25e6, `track ${p.trackId}: dts step ${gap} us after ${n.get(p.trackId)} packets`);
      }
      last.set(p.trackId, p.dtsUs);
      n.set(p.trackId, (n.get(p.trackId) ?? 0) + 1);
    }
    assert.ok(d.tracks().every(x => x.kind !== 'audio' || n.get(x.id) > 100));
    d.close();
  }
});

test('lazy index grows while reading and ends complete', async t => {
  if (!have(fixtureFiles.dtsx)) return t.skip('missing fixture');
  const d = await openDemuxer({ path: fixtureFiles.dtsx });
  const before = d.count;
  let n = 0;
  while (d.nextPacket()) n++;
  assert.ok(n > 100);
  assert.equal(d.complete, true);
  assert.ok(d.count >= n && d.count > before);
  d.close();
});
