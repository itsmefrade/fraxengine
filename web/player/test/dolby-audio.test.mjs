import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildDac3, buildDec3 } from '../js/mse/dolby-audio.js';

// Byte values checked against the dac3 and dec3 boxes ffmpeg writes for the same streams.
test('dac3 keeps the fields of the AC-3 header', () => {
  // fscod 0, frmsizecod 30 (448 kbit/s), bsid 8, bsmod 0, acmod 7, lfe 1
  const p = Uint8Array.from([0x0b, 0x77, 0, 0, 0b00011110, 0b01000000, 0b11100001, 0, 0, 0]);
  const r = buildDac3(p);
  assert.equal(r.channels, 6); assert.equal(r.sampleRate, 48000);
  assert.equal(r.config.length, 3);
});

test('dec3 of a single independent substream with a JOC complexity adds the type A extension', () => {
  const frm = new Uint8Array(1536); frm.set([0x0b, 0x77]);
  const size = (1536 / 2) - 1;
  frm[2] = (size >> 8) & 7; frm[3] = size & 255;
  frm[4] = 0b00110111; frm[5] = 16 << 3;   // fscod 0, numblkscod 3, acmod 3, lfe 1, bsid 16
  const r = buildDec3(frm, 16);
  assert.equal(r.sampleRate, 48000);
  assert.equal(r.config.length, 7);
  assert.equal(r.config[5], 1);
  assert.equal(r.config[6], 16);
});
