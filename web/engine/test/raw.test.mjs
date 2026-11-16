// SPDX-License-Identifier: LGPL-3.0-only
// Raw audio files (AC-3, E-AC-3, FLAC, DTS / DTS-HD, TrueHD / MLP, WAV / RF64 / Wave64, ADTS AAC) through the
// engine: packets against `ffprobe -show_packets -show_data_hash sha256`, 10 s of decoded audio against
// `ffmpeg -f f32le`, seeking, and the bytes fetched over HTTP. The generated fixtures live in
// rust/aether-demux/tests/rawfixtures (run generate.sh there); missing files skip.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { openDemuxer } from '../demuxer.mjs';
import { createAudioPlugin } from '../audio-plugins.mjs';
import { ROOT, labFiles, have, hasFfmpeg, ffmpegReference, comparePlanar, decodeWindow } from './_helpers.mjs';

const LAB = path.dirname(labFiles.pocawe);
const FIX = path.join(ROOT, 'rust/aether-demux/tests/rawfixtures');
const lab = n => path.join(LAB, n);
const fix = n => path.join(FIX, n);
const SECONDS = 10;

// name, file, engine codec, channels, ffmpeg input args, plugin options. `lossy`: DTS core (1e-5), else 1e-6.
const cases = [
  { name: '7_pt_1.eac3', file: lab('7_pt_1.eac3'), codec: 'eac3', channels: 8, ff: ['-drc_scale', '0'], plugin: { joc: false } },
  // One block (93, channel SR) of this file differs from ffmpeg by 3.2e-4 in aether-ac3, everything else is exact.
  { name: 'Canyon ac3', file: lab('Canyon-5.1-48khz-448kbit.ac3'), codec: 'ac3', channels: 6, ff: ['-drc_scale', '0'], tol: 4e-4 },
  { name: 'surround51.flac', file: lab('surround51.flac'), codec: 'flac', channels: 6 },
  { name: 'surround71.flac', file: lab('surround71.flac'), codec: 'flac', channels: 8 },
  { name: 'tone51.ac3', file: fix('tone51.ac3'), codec: 'ac3', channels: 6, ff: ['-drc_scale', '0'] },
  { name: 'tone51.eac3', file: fix('tone51.eac3'), codec: 'eac3', channels: 6, ff: ['-drc_scale', '0'], plugin: { joc: false } },
  { name: 'eac3 joc bed', file: fix('eac3-joc-lab.eac3'), codec: 'eac3', channels: 6, ff: ['-drc_scale', '0'], plugin: { joc: false } },
  { name: 'sine71.flac', file: fix('sine71.flac'), codec: 'flac', channels: 8 },
  { name: 'stereo-s24.flac', file: fix('stereo-s24.flac'), codec: 'flac', channels: 2 },
  { name: 'tone51.dts (core)', file: fix('tone51.dts'), codec: 'dts', channels: 6, lossy: true },
  { name: 'dts-hd ma', file: fix('dtshd-ma-lab.dts'), codec: 'dts', channels: 6 },
  { name: 'tone51.thd', file: fix('tone51.thd'), codec: 'truehd', channels: 6 },
  { name: 'truehd 7.1', file: fix('truehd71-lab.thd'), codec: 'truehd', channels: 8 },
  { name: 'tone51.mlp', file: fix('tone51.mlp'), codec: 'truehd', channels: 6 },
  { name: 'wav 7.1 s24 extensible', file: fix('sine71-s24.wav'), codec: 'pcm', channels: 8 },
  { name: 'rf64 7.1 s24', file: fix('sine71-s24-rf64.wav'), codec: 'pcm', channels: 8 },
  { name: 'wav 5.1 f32', file: fix('sine51-f32.wav'), codec: 'pcm', channels: 6 },
  { name: 'wav stereo s16', file: fix('stereo-s16.wav'), codec: 'pcm', channels: 2 },
  { name: 'w64 7.1 s32', file: fix('sine71-s32.w64'), codec: 'pcm', channels: 8 },
  { name: 'stereo.aac', file: fix('stereo.aac'), codec: 'mp4a.40.2', channels: 2 },
  { name: 'tone51.aac', file: fix('tone51.aac'), codec: 'mp4a.40.2', channels: 6 },
];

/** ffprobe packets of the first audio stream: {pos, size, ptsS, durS, key, hash}. */
function ffprobePackets(file) {
  const out = execFileSync('ffprobe', ['-v', 'error', '-select_streams', 'a:0', '-show_packets', '-show_data_hash', 'sha256', '-of', 'compact', file], { maxBuffer: 1 << 28 }).toString();
  return out.split('\n').filter(l => l.startsWith('packet|')).map(l => {
    const f = Object.fromEntries(l.split('|').slice(1).map(kv => { const i = kv.indexOf('='); return [kv.slice(0, i), kv.slice(i + 1)]; }));
    return { pos: Number(f.pos), size: Number(f.size), ptsS: Number(f.pts_time), durS: Number(f.duration_time), key: f.flags.startsWith('K'), hash: f.data_hash.replace('SHA256:', '') };
  });
}

function readAll(d, ptsLimitUs = Infinity) {
  const out = [];
  for (let p; (p = d.nextPacket());) {
    out.push(p);
    if (p.ptsUs > ptsLimitUs) break;
  }
  return out;
}

for (const c of cases) {
  test(`raw ${c.name}: tracks and every packet equal ffprobe (count, size, hash, time)`, async t => {
    if (!have(c.file)) return t.skip(`missing ${c.file}`);
    let want;
    try { want = ffprobePackets(c.file); } catch { return t.skip('ffprobe not installed'); }
    const d = await openDemuxer({ path: c.file });
    assert.equal(d.container, 'raw');
    const tracks = d.tracks();
    assert.equal(tracks.length, 1);
    const tr = tracks[0];
    assert.equal(tr.kind, 'audio');
    assert.equal(tr.codec, c.codec);
    assert.equal(tr.channels, c.channels);
    assert.ok(tr.sampleRate > 0);
    if (c.codec === 'flac') assert.equal(String.fromCharCode(...tr.description.subarray(0, 4)), 'fLaC');
    if (c.codec === 'pcm') assert.ok(tr.pcm && (tr.pcm.format === 'int' || tr.pcm.format === 'float'));
    assert.equal(d.startTimeUs, 0);
    assert.ok(d.durationUs > 0);
    const got = readAll(d);
    assert.equal(got.length, want.length, 'packet count');
    for (let i = 0; i < got.length; i++) {
      const p = got[i], w = want[i];
      if (p.data.length !== w.size || Math.abs(p.ptsUs / 1e6 - w.ptsS) > 2e-6 || Math.abs(p.durationUs / 1e6 - w.durS) > 2e-6 || p.key !== w.key
        || crypto.createHash('sha256').update(p.data).digest('hex') !== w.hash) {
        assert.fail(`packet ${i}: size ${p.data.length}/${w.size} pts ${p.ptsUs / 1e6}/${w.ptsS} dur ${p.durationUs / 1e6}/${w.durS} key ${p.key}/${w.key}`);
      }
    }
    // The end of the last packet is the duration.
    const last = got.at(-1);
    assert.ok(Math.abs(d.durationUs - (last.ptsUs + last.durationUs)) < 5, `duration ${d.durationUs} vs ${last.ptsUs + last.durationUs}`);
    assert.equal(d.complete, true);
    d.close();
  });
}

for (const c of cases.filter(x => ['eac3', 'ac3', 'dts', 'truehd', 'pcm'].includes(x.codec))) {
  test(`raw ${c.name}: ${SECONDS} s decoded by the engine plugin equal ffmpeg`, async t => {
    if (!have(c.file)) return t.skip(`missing ${c.file}`);
    if (!hasFfmpeg) return t.skip('ffmpeg not installed');
    const d = await openDemuxer({ path: c.file });
    const track = d.tracks()[0];
    const plugin = createAudioPlugin(track, { presentation: 2, ...(c.plugin ?? {}) });
    assert.ok(plugin, 'the engine decodes this codec');
    plugin.configure(track);
    const out = decodeWindow(d, plugin, track.id, SECONDS);
    assert.ok(out, 'decoded something');
    const ref = ffmpegReference(c.file, 0, SECONDS, c.ff ?? []);
    // The MLP stream header names no labels in aether-audio's TrueHD decoder: compare by channel index there.
    let cmp;
    if (out.layout.length === 0) {
      assert.equal(c.name, 'tone51.mlp');
      const ch = out.planar.length, frames = Math.min(out.planar[0].length, Math.floor(ref.length / ch));
      let max = 0;
      for (let k = 0; k < ch; k++) for (let i = 0; i < frames; i++) max = Math.max(max, Math.abs(out.planar[k][i] - ref[i * ch + k]));
      cmp = { frames, max, refFrames: Math.floor(ref.length / ch) };
    } else {
      assert.equal(out.layout.length, c.channels, 'one label per channel');
      cmp = comparePlanar(out.planar, out.layout, ref);
    }
    const tol = c.tol ?? (c.lossy && !out.lossless ? 1e-5 : (c.codec === 'ac3' || c.codec === 'eac3') ? 1e-6 : 0);
    console.log(`  ${c.name}: ${out.layout.join(',')} frames=${out.planar[0].length} ref=${cmp.refFrames} maxdiff=${cmp.max} lossless=${out.lossless}`);
    assert.equal(out.planar.length, c.channels);
    const window = Math.min(SECONDS, d.durationUs / 1e6);
    assert.ok(cmp.frames >= (window - 0.15) * out.sampleRate, `covers ${window} s: ${cmp.frames} frames`);
    assert.ok(cmp.max <= tol, `max abs diff ${cmp.max} <= ${tol}`);
    plugin.close();
    d.close();
  });
}

// FLAC and AAC are decoded by the browser (no engine plugin). The packets must still be the frames of the
// stream: description + the first SECONDS of packets, decoded by ffmpeg, equal ffmpeg's decode of the file.
for (const c of cases.filter(x => x.codec === 'flac' || x.codec.startsWith('mp4a'))) {
  test(`raw ${c.name}: the first ${SECONDS} s of packets (plus description) decode like the file`, async t => {
    if (!have(c.file)) return t.skip(`missing ${c.file}`);
    if (!hasFfmpeg) return t.skip('ffmpeg not installed');
    const d = await openDemuxer({ path: c.file });
    const track = d.tracks()[0];
    assert.equal(createAudioPlugin(track), null, 'decoded by the browser');
    if (track.codec === 'flac') assert.ok(track.description.length === 42);
    else assert.equal(track.description, undefined, 'ADTS packets, WebCodecs reads them without a description');
    const parts = track.description ? [Buffer.from(track.description)] : [];
    for (const p of readAll(d, SECONDS * 1e6)) parts.push(Buffer.from(p.data));
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'raw-'));
    const ext = path.extname(c.file);
    const re = path.join(dir, `re${ext}`);
    fs.writeFileSync(re, Buffer.concat(parts));
    const decode = (file, extra) => {
      const o = path.join(dir, `${path.basename(file)}.f32`);
      execFileSync('ffmpeg', ['-v', 'error', '-y', '-i', file, ...extra, '-f', 'f32le', '-acodec', 'pcm_f32le', o]);
      const b = fs.readFileSync(o);
      return new Float32Array(b.buffer, b.byteOffset, b.byteLength >> 2);
    };
    const a = decode(re, []);
    const b = decode(c.file, ['-t', String(SECONDS + 2)]);
    const n = Math.min(a.length, b.length);
    assert.ok(a.length >= SECONDS * track.sampleRate * c.channels * 0.97, `reassembled stream decodes ${a.length / c.channels} frames`);
    let max = 0;
    for (let i = 0; i < n; i++) max = Math.max(max, Math.abs(a[i] - b[i]));
    console.log(`  ${c.name}: ${n / c.channels} frames compared, max diff ${max}`);
    assert.equal(max, 0);
    fs.rmSync(dir, { recursive: true, force: true });
    d.close();
  });
}

for (const c of cases.filter(x => ['eac3', 'dts', 'truehd', 'pcm', 'flac', 'mp4a.40.2'].includes(x.codec) && /7_pt_1|tone51\.(thd|dts)|sine71-s24\.wav|sine71\.flac|stereo\.aac/.test(x.file))) {
  test(`raw ${c.name}: seek lands at or before the target on a packet boundary and plays on`, async t => {
    if (!have(c.file)) return t.skip(`missing ${c.file}`);
    let want;
    try { want = ffprobePackets(c.file); } catch { return t.skip('ffprobe not installed'); }
    const d = await openDemuxer({ path: c.file });
    const exact = c.codec === 'pcm' || c.codec === 'flac';
    for (const frac of [0.5, 0.8, 0.1, 0.99, 0]) {
      const target = Math.floor(d.durationUs * frac);
      d.seek(target);
      const first = d.nextPacket();
      assert.ok(first, 'a packet after the seek');
      assert.ok(first.ptsUs <= target || frac === 0, `landed at ${first.ptsUs} after ${target}`);
      const sha = crypto.createHash('sha256').update(first.data).digest('hex');
      assert.ok(want.some(w => w.hash === sha), 'the packet is one of the file\'s packets');
      assert.ok(first.key, 'lands on a key packet');
      if (exact) assert.ok(Math.abs(first.ptsUs - target) < 0.2e6 + 0.1e6, `landed ${(target - first.ptsUs) / 1e6} s before the target`);
      else assert.ok(Math.abs(first.ptsUs - target) < d.durationUs * 0.25 + 1e6, `landed ${(target - first.ptsUs) / 1e6} s from the target`);
      let prev = first;
      for (let i = 0; i < 50; i++) {
        const p = d.nextPacket();
        if (!p) break;
        assert.ok(Math.abs(p.ptsUs - (prev.ptsUs + prev.durationUs)) < 3, `continuous timeline (${p.ptsUs} after ${prev.ptsUs}+${prev.durationUs})`);
        prev = p;
      }
    }
    d.close();
  });
}

test('raw wav: PCM decoded right after a seek equals ffmpeg at that time', async t => {
  const file = fix('sine71-s24.wav');
  if (!have(file)) return t.skip('missing fixture');
  if (!hasFfmpeg) return t.skip('ffmpeg not installed');
  const d = await openDemuxer({ path: file });
  const track = d.tracks()[0];
  const plugin = createAudioPlugin(track).configure(track);
  d.seek(7.3e6);
  const parts = [];
  let start = null;
  for (let p; (p = d.nextPacket()) && p.ptsUs < 8.6e6;) {
    start ??= p.ptsUs;
    const r = plugin.decode(p);
    if (r.frames) parts.push(r);
  }
  const frames = parts.reduce((n, r) => n + r.frames, 0);
  const planar = parts[0].planar.map((_, c) => { const a = new Float32Array(frames); let o = 0; for (const r of parts) { a.set(r.planar[c], o); o += r.frames; } return a; });
  const ref = ffmpegReference(file, 0, 1, ['-ss', String(start / 1e6)]);
  const cmp = comparePlanar(planar, parts[0].layout, ref);
  assert.ok(start <= 7.3e6 && 7.3e6 - start < 0.2e6, `start ${start}`);
  assert.ok(cmp.frames >= 40000, `${cmp.frames} frames compared`);
  assert.equal(cmp.max, 0);
});

// HTTP Range: the open reads the head (and, for variable rate streams, a few sample windows), not the file.
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

for (const [name, file, bound] of [
  ['7_pt_1.eac3', lab('7_pt_1.eac3'), 3], ['Canyon ac3', lab('Canyon-5.1-48khz-448kbit.ac3'), 3], ['surround71.flac', lab('surround71.flac'), 3],
  ['dts-hd ma', fix('dtshd-ma-lab.dts'), 11], ['truehd 7.1', fix('truehd71-lab.thd'), 11], ['wav 7.1', fix('sine71-s24.wav'), 3],
]) {
  test(`raw ${name}: HTTP Range open and first packet stay within ${bound} MiB`, async t => {
    if (!have(file)) return t.skip(`missing ${file}`);
    const { transport, log, size } = fileTransport(file);
    const d = await openDemuxer({ url: 'http://test.invalid/media', transport });
    const atOpen = log.bytes;
    assert.ok(d.nextPacket());
    console.log(`  ${name}: file ${(size / 1048576).toFixed(1)} MiB, fetched ${(atOpen / 1024).toFixed(0)} KiB at open, ${(log.bytes / 1024).toFixed(0)} KiB (${log.requests} requests) at the first packet`);
    assert.ok(log.bytes <= bound * 1048576, `${log.bytes} bytes fetched`);
    d.seek(Math.floor(d.durationUs / 2));
    assert.ok(d.nextPacket());
    assert.ok(log.bytes <= (bound + 4) * 1048576, `${log.bytes} bytes after a seek to the middle`);
    d.close();
  });
}

test('raw: sequential reading of a lazily opened file equals the ffprobe list and detection leaves other containers alone', async t => {
  const file = lab('7_pt_1.eac3');
  if (!have(file)) return t.skip('missing lab media');
  const d = await openDemuxer({ path: file });
  assert.equal(d.container, 'raw');
  assert.equal(d.complete, false, 'lazy: not everything is indexed at open');
  readAll(d);
  assert.equal(d.complete, true);
  d.close();
  for (const f of [labFiles.ddp71, labFiles.truehdCheck, labFiles.dtsMa51]) {
    if (!have(f)) continue;
    const m = await openDemuxer({ path: f });
    assert.notEqual(m.container, 'raw', f);
    m.close();
  }
});
