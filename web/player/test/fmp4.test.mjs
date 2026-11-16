// SPDX-License-Identifier: LGPL-3.0-only
// node --test web/player/test/fmp4.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Fmp4Muxer, buildVpcC } from '../js/mse/fmp4.js';

// FFMPEG / FFPROBE override the tools found on PATH; without them the tests that check against ffmpeg skip.
const FFMPEG = process.env.FFMPEG || 'ffmpeg';
const FFPROBE = process.env.FFPROBE || 'ffprobe';
const NEEDS_FFMPEG = { skip: spawnSync(FFMPEG, ['-version']).status === 0 && spawnSync(FFPROBE, ['-version']).status === 0 ? false : 'ffmpeg/ffprobe not on PATH' };
const dir = mkdtempSync(join(tmpdir(), 'fmp4-'));
test.after(() => rmSync(dir, { recursive: true, force: true }));

const SRC = ['-y', '-v', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=320x240:rate=30', '-t', '2'];
const ff = (...a) => execFileSync(FFMPEG, a, { stdio: ['ignore', 'pipe', 'pipe'] });
const probe = (file, ...a) => JSON.parse(execFileSync(FFPROBE, ['-v', 'error', '-of', 'json', ...a, file]).toString());

// Tiny box walker: finds the first box of `type` under a path of container types.
function findBox(buf, path, start = 0, end = buf.length) {
  let p = start;
  while (p + 8 <= end) {
    const size = buf.readUInt32BE(p);
    const type = buf.toString('latin1', p + 4, p + 8);
    if (type === path[0]) {
      if (path.length === 1) return buf.subarray(p + 8, p + size);
      // stsd: skip fullbox header + entry count; sample entry: skip 78 (video) or 28 (audio) byte header
      const skip = type === 'stsd' ? 8 : /^(hvc1|hev1|avc1|avc3|av01|vp09)$/.test(type) ? 78
        : /^(mp4a|Opus|fLaC)$/.test(type) ? 28 : 0;
      const r = findBox(buf, path.slice(1), p + 8 + skip, p + size);
      if (r) return r;
    }
    p += size;
  }
  return null;
}

// Packets (decode order) with timing in microseconds, plus sample bytes.
function readPackets(file, tb) {
  const j = probe(file, '-select_streams', 'v:0', '-show_packets',
    '-show_entries', 'packet=pts,dts,duration,flags,pos,size', '-show_entries', 'stream=time_base');
  const [num, den] = j.streams ? j.streams[0].time_base.split('/').map(Number) : tb;
  const us = (v) => Math.round((v * 1e6 * num) / den);
  return j.packets.map((p) => ({
    ptsUs: us(p.pts), dtsUs: us(p.dts), durationUs: us(p.duration),
    key: p.flags.startsWith('K'), pos: Number(p.pos), size: Number(p.size),
  }));
}

function loadMp4(file) {
  const buf = readFileSync(file);
  const pk = readPackets(file);
  for (const p of pk) p.data = new Uint8Array(buf.subarray(p.pos, p.pos + p.size));
  return { buf, pk };
}

function loadIvf(file) {
  const buf = readFileSync(file);
  const pk = readPackets(file);
  let o = 32;
  for (const p of pk) {
    const n = buf.readUInt32LE(o);
    p.data = new Uint8Array(buf.subarray(o + 12, o + 12 + n));
    o += 12 + n;
  }
  return { buf, pk };
}

function splitAtKeys(samples) {
  const groups = [];
  for (const s of samples) {
    if (s.key || !groups.length) groups.push([]);
    groups.at(-1).push(s);
  }
  return groups;
}

function roundTrip(name, muxer, pk, expect) {
  const parts = [muxer.init(), ...splitAtKeys(pk).map((g) => muxer.fragment(g))];
  assert.ok(parts.length >= 3, 'init plus at least two fragments');
  const out = join(dir, `${name}-out.mp4`);
  writeFileSync(out, Buffer.concat(parts));
  const j = probe(out, '-select_streams', 'v:0', '-show_streams', '-show_packets',
    '-show_entries', 'stream=codec_name,width,height,color_transfer,time_base:packet=pts,dts');
  const s = j.streams[0];
  assert.equal(s.codec_name, expect.codec);
  assert.equal(s.width, 320);
  assert.equal(s.height, 240);
  if (expect.transfer) assert.equal(s.color_transfer, expect.transfer);
  assert.equal(j.packets.length, pk.length);
  const [num, den] = s.time_base.split('/').map(Number);
  const tick = (1e6 * num) / den;
  const off = muxer.offsetUs;
  // Output packets come back in file (decode) order, like the input list.
  j.packets.forEach((p, i) => {
    const pts = (p.pts * 1e6 * num) / den - off;
    const dts = (p.dts * 1e6 * num) / den - off;
    assert.ok(Math.abs(pts - pk[i].ptsUs) <= tick + 1, `pts ${i}: ${pts} vs ${pk[i].ptsUs}`);
    assert.ok(Math.abs(dts - pk[i].dtsUs) <= tick + 1, `dts ${i}: ${dts} vs ${pk[i].dtsUs}`);
  });
  const dec = spawnSync(FFMPEG, ['-v', 'error', '-i', out, '-f', 'null', '-'], { encoding: 'utf8' });
  assert.equal(dec.stderr.trim(), '', 'decodes without errors');
  assert.equal(dec.status, 0);
}

test('H.264 with B-frames', NEEDS_FFMPEG, () => {
  const f = join(dir, 'h264.mp4');
  ff(...SRC, '-c:v', 'libx264', '-bf', '2', '-g', '24', '-pix_fmt', 'yuv420p', f);
  const { buf, pk } = loadMp4(f);
  const avcC = findBox(buf, ['moov', 'trak', 'mdia', 'minf', 'stbl', 'stsd', 'avc1', 'avcC']);
  const hex = (b) => Buffer.from(b).toString('hex').toUpperCase();
  const codec = `avc1.${hex(avcC.subarray(1, 4))}`;
  const m = new Fmp4Muxer({ codec, description: new Uint8Array(avcC), width: 320, height: 240 });
  assert.equal(m.mimeType, `video/mp4; codecs="${codec}"`);
  roundTrip('h264', m, pk, { codec: 'h264' });
});

test('HEVC 10-bit HDR10 with colr', NEEDS_FFMPEG, () => {
  const f = join(dir, 'hevc.mp4');
  ff(...SRC, '-c:v', 'libx265', '-pix_fmt', 'yuv420p10le', '-tag:v', 'hvc1', '-x265-params',
    'colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:bframes=3:keyint=24:min-keyint=24:log-level=error', f);
  const { buf, pk } = loadMp4(f);
  const hvcC = findBox(buf, ['moov', 'trak', 'mdia', 'minf', 'stbl', 'stsd', 'hvc1', 'hvcC']);
  const m = new Fmp4Muxer({
    codec: 'hvc1.2.4.L93.B0', description: new Uint8Array(hvcC), width: 320, height: 240,
    color: { primaries: 9, transfer: 16, matrix: 9, fullRange: false },
  });
  roundTrip('hevc', m, pk, { codec: 'hevc', transfer: 'smpte2084' });
});

test('AV1', NEEDS_FFMPEG, () => {
  const f = join(dir, 'av1.mp4');
  ff(...SRC, '-c:v', 'libsvtav1', '-g', '24', '-pix_fmt', 'yuv420p', f);
  const { buf, pk } = loadMp4(f);
  const av1C = findBox(buf, ['moov', 'trak', 'mdia', 'minf', 'stbl', 'stsd', 'av01', 'av1C']);
  const m = new Fmp4Muxer({ codec: 'av01.0.04M.08', description: new Uint8Array(av1C), width: 320, height: 240 });
  roundTrip('av1', m, pk, { codec: 'av1' });
});

test('VP9 without description', NEEDS_FFMPEG, () => {
  const f = join(dir, 'vp9.ivf');
  ff(...SRC, '-c:v', 'libvpx-vp9', '-g', '24', '-pix_fmt', 'yuv420p', '-f', 'ivf', f);
  const { pk } = loadIvf(f);
  const m = new Fmp4Muxer({ codec: 'vp09.00.10.08', description: null, width: 320, height: 240 });
  roundTrip('vp9', m, pk, { codec: 'vp9' });
});

test('vpcC builder', () => {
  const b = buildVpcC('vp09.02.51.10');
  assert.deepEqual([...b], [
    0, 0, 0, 20, 0x76, 0x70, 0x63, 0x43, 1, 0, 0, 0,
    2, 51, (10 << 4) | (1 << 1), 1, 1, 1, 0, 0]);
  const c = buildVpcC('vp09.00.40.08', { primaries: 9, transfer: 16, matrix: 9, fullRange: true });
  assert.deepEqual([...c.subarray(12)], [0, 40, (8 << 4) | 2 | 1, 9, 16, 9, 0, 0]);
  const e = buildVpcC('vp09.00.31.10.03.09.16.09.01');
  assert.deepEqual([...e.subarray(12, 18)], [0, 31, (10 << 4) | (3 << 1) | 1, 9, 16, 9]);
});

test('negative dts shifts the whole stream by a constant', () => {
  const m = new Fmp4Muxer({ codec: 'vp09.00.10.08', width: 64, height: 64 });
  const mk = (i) => ({
    data: new Uint8Array([i, i, i]), dtsUs: -83333 + i * 41667, ptsUs: i * 41667,
    durationUs: 41667, key: i === 0,
  });
  const frag = m.fragment([mk(0), mk(1), mk(2)]);
  assert.equal(m.offsetUs, 83333);
  const buf = Buffer.from(frag);
  const tfdt = findBox(buf, ['moof', 'traf', 'tfdt']);
  assert.equal(tfdt[0], 1);
  assert.equal(Number(tfdt.readBigUInt64BE(4)), 0);
  // Second fragment keeps the same offset and continues the timeline.
  const next = Buffer.from(m.fragment([mk(3)]));
  const base = Number(findBox(next, ['moof', 'traf', 'tfdt']).readBigUInt64BE(4));
  assert.equal(base, Math.round(((-83333 + 3 * 41667 + 83333) * 90000) / 1e6));
  assert.equal(findBox(next, ['moof', 'mfhd']).readUInt32BE(4), 2);
  // trun: data offset points at the first mdat payload byte.
  const trun = findBox(buf, ['moof', 'traf', 'trun']);
  const moofSize = buf.readUInt32BE(0);
  assert.equal(trun.readInt32BE(8), moofSize + 8);
  assert.equal(trun.readUInt32BE(20), 0x02000000);
  assert.equal(trun.readUInt32BE(12 + 16 + 8), 0x01010000);
  // Composition offset of the first sample is pts - dts = 83333 us, in ticks.
  assert.equal(trun.readInt32BE(24), Math.round(83333 * 0.09));
});

// ---- Audio ----

const ASRC = (rate, ch) => ['-y', '-v', 'error', '-f', 'lavfi', '-i', `anoisesrc=r=${rate}:a=0.3:seed=7`, '-t', '2', '-ac', String(ch)];

function readAudioPackets(file) {
  const j = probe(file, '-select_streams', 'a:0', '-show_packets',
    '-show_entries', 'packet=pts,duration,pos,size', '-show_entries', 'stream=time_base');
  const [num, den] = j.streams[0].time_base.split('/').map(Number);
  const us = (v) => Math.round((v * 1e6 * num) / den);
  const buf = readFileSync(file);
  const pk = j.packets.map((p) => ({
    ptsUs: us(p.pts), durationUs: us(p.duration), key: true,
    data: new Uint8Array(buf.subarray(Number(p.pos), Number(p.pos) + Number(p.size))),
  }));
  return { buf, pk };
}

const chunks = (a, n) => Array.from({ length: Math.ceil(a.length / n) }, (_, i) => a.slice(i * n, (i + 1) * n));

function audioRoundTrip(name, muxer, pk, expect) {
  const parts = [muxer.init(), ...chunks(pk, Math.ceil(pk.length / 3)).map((g) => muxer.fragment(g))];
  assert.ok(parts.length >= 3, 'init plus at least two fragments');
  const out = join(dir, `${name}-out.mp4`);
  writeFileSync(out, Buffer.concat(parts));
  const j = probe(out, '-select_streams', 'a:0', '-show_streams', '-show_packets',
    '-show_entries', 'stream=codec_name,channels,sample_rate,time_base:packet=pts,dts,size');
  const s = j.streams[0];
  assert.equal(s.codec_name, expect.codec);
  assert.equal(s.channels, expect.channels);
  assert.equal(Number(s.sample_rate), expect.rate);
  assert.equal(j.packets.length, pk.length);
  const [num, den] = s.time_base.split('/').map(Number);
  const tick = (1e6 * num) / den;
  j.packets.forEach((p, i) => {
    const pts = (p.pts * 1e6 * num) / den - muxer.offsetUs;
    assert.ok(Math.abs(pts - pk[i].ptsUs) <= tick + 1, `pts ${i}: ${pts} vs ${pk[i].ptsUs}`);
    assert.equal(p.dts, p.pts);
    assert.equal(Number(p.size), pk[i].data.length);
  });
  const dec = spawnSync(FFMPEG, ['-v', 'error', '-i', out, '-f', 'null', '-'], { encoding: 'utf8' });
  assert.equal(dec.stderr.trim(), '', 'decodes without errors');
  assert.equal(dec.status, 0);
  return out;
}

const pcm = (file) => execFileSync(FFMPEG, ['-v', 'error', '-i', file, '-map', '0:a:0', '-f', 's32le', '-'],
  { maxBuffer: 1 << 28 });

test('AAC-LC stereo with priming', NEEDS_FFMPEG, () => {
  const f = join(dir, 'aac.mp4');
  ff(...ASRC(48000, 2), '-c:a', 'aac', f);
  const { buf, pk } = readAudioPackets(f);
  assert.ok(pk[0].ptsUs < 0, 'source starts with negative priming pts');
  const esds = findBox(buf, ['moov', 'trak', 'mdia', 'minf', 'stbl', 'stsd', 'mp4a', 'esds']);
  const asc = esds.subarray(esds.indexOf(Buffer.from([0x05, 0x80, 0x80, 0x80])) + 5, esds.indexOf(Buffer.from([0x06, 0x80, 0x80, 0x80])));
  const m = new Fmp4Muxer({
    codec: 'mp4a.40.2', description: new Uint8Array(asc), channels: 2, sampleRate: 48000,
  });
  assert.equal(m.mimeType, 'audio/mp4; codecs="mp4a.40.2"');
  assert.equal(m.timescale, 48000);
  audioRoundTrip('aac', m, pk, { codec: 'aac', channels: 2, rate: 48000 });
  assert.equal(m.offsetUs, -pk[0].ptsUs);
});

for (const ch of [2, 6]) {
  test(`Opus ${ch}ch`, NEEDS_FFMPEG, () => {
    // Packets from an mp4 (ffprobe pos points at raw payload there), OpusHead from the mkv CodecPrivate.
    const mkv = join(dir, `opus${ch}.mkv`);
    const f = join(dir, `opus${ch}.mp4`);
    ff(...ASRC(48000, ch), '-c:a', 'libopus', mkv);
    ff('-v', 'error', '-i', mkv, '-c', 'copy', f);
    const { pk } = readAudioPackets(f);
    const mb = readFileSync(mkv);
    const at = mb.indexOf('OpusHead');
    const fam = mb[at + 18];
    const head = new Uint8Array(mb.subarray(at, at + (fam ? 21 + mb[at + 9] : 19)));
    const m = new Fmp4Muxer({ codec: 'opus', description: head, channels: ch, sampleRate: 48000 });
    assert.equal(m.mimeType, 'audio/mp4; codecs="opus"');
    const out = audioRoundTrip(`opus${ch}`, m, pk, { codec: 'opus', channels: ch, rate: 48000 });
    const dops = findBox(readFileSync(out), ['moov', 'trak', 'mdia', 'minf', 'stbl', 'stsd', 'Opus', 'dOps']);
    assert.equal(dops[0], 0);
    assert.equal(dops.length, fam ? 11 + 2 + ch : 11);
  });
}

const FLAC_CASES = [
  { name: '2ch 48k 16-bit', ch: 2, rate: 48000, bits: 16, fmt: ['-sample_fmt', 's16'], field: 48000 },
  { name: '6ch 48k 24-bit', ch: 6, rate: 48000, bits: 24, fmt: ['-sample_fmt', 's32', '-bits_per_raw_sample', '24'], field: 48000 },
  { name: '2ch 96k 24-bit', ch: 2, rate: 96000, bits: 24, fmt: ['-sample_fmt', 's32', '-bits_per_raw_sample', '24'], field: 48000 },
  { name: '2ch 192k 24-bit', ch: 2, rate: 192000, bits: 24, fmt: ['-sample_fmt', 's32', '-bits_per_raw_sample', '24'], field: 48000 },
];
for (const c of FLAC_CASES) {
  test(`FLAC ${c.name}`, NEEDS_FFMPEG, () => {
    const f = join(dir, `flac-${c.ch}-${c.rate}.flac`);
    ff(...ASRC(c.rate, c.ch), '-c:a', 'flac', ...c.fmt, f);
    const { buf, pk } = readAudioPackets(f);
    assert.equal(buf.toString('latin1', 0, 4), 'fLaC');
    const info = new Uint8Array(buf.subarray(8, 42));
    const m = new Fmp4Muxer({ codec: 'flac', description: info, channels: c.ch, sampleRate: c.rate });
    assert.equal(m.mimeType, 'audio/mp4; codecs="flac"');
    const out = audioRoundTrip(`flac-${c.ch}-${c.rate}`, m, pk, { codec: 'flac', channels: c.ch, rate: c.rate });
    assert.equal(m.offsetUs, 0);
    const o = readFileSync(out);
    const entry = findBox(o, ['moov', 'trak', 'mdia', 'minf', 'stbl', 'stsd', 'fLaC']);
    assert.equal(entry.readUInt16BE(24), c.field, '16.16 sample rate integer part');
    assert.equal(entry.readUInt16BE(16), c.ch);
    assert.equal(entry.readUInt16BE(18), c.bits);
    assert.ok(pcm(f).equals(pcm(out)), 'PCM identical to the original');
  });
}
