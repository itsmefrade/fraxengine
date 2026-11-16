// SPDX-License-Identifier: LGPL-3.0-only
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { FlacEncoder } from '../js/mse/flac-enc.js';

// FFMPEG / FFPROBE override the tools found on PATH; without them the tests that check against ffmpeg skip.
const FFMPEG = process.env.FFMPEG || 'ffmpeg';
const NEEDS_FFMPEG = { skip: spawnSync(FFMPEG, ['-version']).status === 0 ? false : 'ffmpeg/ffprobe not on PATH' };
const dir = mkdtempSync(join(tmpdir(), 'flac-enc-'));
process.on('exit', () => rmSync(dir, { recursive: true, force: true }));

function makeRng(seed) {
  let s = seed >>> 0;
  return () => (s = (Math.imul(s, 1664525) + 1013904223) >>> 0) / 4294967296;
}

// Sections: tone+noise, silence, clipping, constant, tone, full-scale white noise, tone+noise.
function makeSignal(channels, n, seed = 1) {
  const rnd = makeRng(seed);
  const planes = [];
  for (let c = 0; c < channels; c++) planes.push(new Float32Array(n));
  for (let i = 0; i < n; i++) {
    const base = Math.sin(i * 0.031) * 0.4 + Math.sin(i * 0.173) * 0.2;
    for (let c = 0; c < channels; c++) {
      const tone = base * (1 - 0.1 * c) + Math.sin(i * (0.011 + 0.007 * c)) * 0.15;
      const noise = (rnd() - 0.5) * 0.002;
      let v;
      if (i < 8000) v = tone + noise;
      else if (i < 20000) v = 0;
      else if (i < 24000) v = Math.sin(i * 0.05 + c) * 1.6;
      else if (i < 36000) v = 0.3;
      else if (i < 44000) v = tone;
      else if (i < 48000) v = (rnd() * 2 - 1) * (c === 0 ? 1.0 : 0.999);
      else v = tone + noise;
      planes[c][i] = v;
    }
  }
  return planes;
}

function quantize(x, bits) {
  const scale = 2 ** (bits - 1);
  return Math.max(-scale, Math.min(scale - 1, Math.round(x * scale)));
}

function expectedInterleaved(planes, bits, from = 0) {
  const n = planes[0].length - from;
  const out = bits === 16 ? new Int16Array(n * planes.length) : new Int32Array(n * planes.length);
  for (let i = 0; i < n; i++) {
    for (let c = 0; c < planes.length; c++) {
      const q = quantize(planes[c][from + i], bits);
      out[i * planes.length + c] = bits === 24 ? q * 256 : q;
    }
  }
  return out;
}

function writeFlac(name, enc, frames) {
  const info = enc.streamInfo();
  assert.equal(info.length, 34);
  const total = 4 + 4 + 34 + frames.reduce((a, f) => a + f.data.length, 0);
  const file = new Uint8Array(total);
  file.set([0x66, 0x4c, 0x61, 0x43, 0x80, 0, 0, 34], 0);
  file.set(info, 8);
  let off = 42;
  for (const f of frames) { file.set(f.data, off); off += f.data.length; }
  const path = join(dir, name);
  writeFileSync(path, file);
  return { path, bytes: total };
}

function decode(path, bits) {
  const r = spawnSync(FFMPEG, ['-v', 'error', '-err_detect', 'crccheck+bitstream', '-i', path,
    '-f', bits === 16 ? 's16le' : 's32le', '-'], { maxBuffer: 1 << 29 });
  assert.equal(r.stderr.toString(), '', 'ffmpeg reported errors');
  assert.equal(r.status, 0);
  const raw = new Uint8Array(r.stdout);
  return bits === 16 ? new Int16Array(raw.buffer, 0, raw.length >> 1) : new Int32Array(raw.buffer, 0, raw.length >> 2);
}

function encodeAll(enc, planes, chunk) {
  const frames = [];
  const n = planes[0].length;
  for (let off = 0; off < n; off += chunk) {
    const end = Math.min(n, off + chunk);
    frames.push(...enc.encode(planes.map((p) => p.subarray(off, end))));
  }
  frames.push(...enc.flush());
  return frames;
}

function assertSame(actual, expected) {
  assert.equal(actual.length, expected.length, 'decoded length');
  for (let i = 0; i < expected.length; i++) {
    if (actual[i] !== expected[i]) assert.fail(`mismatch at ${i}: got ${actual[i]} want ${expected[i]}`);
  }
}

const N = 60000;
const cases = [
  [1, 48000, 24], [1, 44100, 16], [2, 44100, 24], [2, 96000, 16], [2, 192000, 24],
  [6, 48000, 24], [8, 48000, 16], [8, 192000, 24],
];

// Default is the verbatim fast path; compress: true is the predictor + Rice path. Both must decode
// sample-exact (assertSame compares every decoded sample).
for (const compress of [false, true]) {
  for (const blockSize of [4096, 1152]) {
    for (const [channels, sampleRate, bits] of cases) {
      test(`round trip ${channels}ch ${sampleRate}Hz ${bits}bit block ${blockSize} compress ${compress}`, NEEDS_FFMPEG, () => {
        const planes = makeSignal(channels, N, channels * 7 + bits);
        const enc = new FlacEncoder({ channels, sampleRate, bitsPerSample: bits, blockSize, compress });
        const frames = encodeAll(enc, planes, 3000);
        assert.equal(frames.reduce((a, f) => a + f.samples, 0), N);
        assert.equal(frames.at(-1).samples, N % blockSize);
        const { path } = writeFlac(`rt-${channels}-${sampleRate}-${bits}-${blockSize}-${compress}.flac`, enc, frames);
        assertSame(decode(path, bits), expectedInterleaved(planes, bits));
      });
    }
  }
}

test('odd rates and tiny final block', NEEDS_FFMPEG, () => {
  for (const sampleRate of [8000, 22050, 88200, 12345, 655350]) {
    const planes = makeSignal(2, 4096 + 5, 3);
    const enc = new FlacEncoder({ channels: 2, sampleRate, bitsPerSample: 16 });
    const frames = encodeAll(enc, planes, 4096 + 5);
    const { path } = writeFlac(`odd-${sampleRate}.flac`, enc, frames);
    assertSame(decode(path, 16), expectedInterleaved(planes, 16));
  }
});

test('reset restarts frame numbering and the new stream decodes', NEEDS_FFMPEG, () => {
  const planes = makeSignal(2, N, 9);
  const enc = new FlacEncoder({ channels: 2, sampleRate: 48000, bitsPerSample: 24 });
  const first = enc.encode(planes.map((p) => p.subarray(0, 20000)));
  assert.equal(first[0].data[4], 0);
  assert.equal(first[1].data[4], 1);
  enc.reset();
  const from = 30000;
  const frames = [...enc.encode(planes.map((p) => p.subarray(from))), ...enc.flush()];
  assert.equal(frames[0].data[4], 0);
  assert.equal(frames[1].data[4], 1);
  const { path } = writeFlac('reset.flac', enc, frames);
  assertSame(decode(path, 24), expectedInterleaved(planes, 24, from));
});

test('frame numbers above 127 use multi-byte UTF-8', NEEDS_FFMPEG, () => {
  const planes = makeSignal(1, 16 * 200 + 3, 5);
  const enc = new FlacEncoder({ channels: 1, sampleRate: 44100, bitsPerSample: 16, blockSize: 16 });
  const frames = encodeAll(enc, planes, 777);
  const { path } = writeFlac('many.flac', enc, frames);
  assertSame(decode(path, 16), expectedInterleaved(planes, 16));
});

test('speed 8ch 48kHz 24bit', () => {
  const seconds = 20;
  const planes = makeSignal(8, 48000 * seconds, 11);
  const enc = new FlacEncoder({ channels: 8, sampleRate: 48000, bitsPerSample: 24 });
  encodeAll(enc, planes.map((p) => p.subarray(0, 48000)), 4096);
  enc.reset();
  const t0 = performance.now();
  const frames = encodeAll(enc, planes, 4096);
  const ms = performance.now() - t0;
  const bytes = frames.reduce((a, f) => a + f.data.length, 0);
  const raw = 48000 * seconds * 8 * 3;
  console.log(`speed: ${(seconds * 1000 / ms).toFixed(1)}x realtime, ratio ${(bytes / raw * 100).toFixed(1)}% of raw`);
  assert.ok(bytes > 0);
});
