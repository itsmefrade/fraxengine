// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { Fmp4Muxer } from '../js/mse/fmp4.js';
import { hdrSignal, hdrSignalNeedsKeyframe, hevcSpsColour, hevcHdrSei } from '../js/mse/hdr-signal.js';
import { openDemuxer } from '../../engine/demuxer.mjs';
import { accessUnit, boxTypes, emptyHvcc, makeSei, makeSps } from './_mse-worker-harness.mjs';

const MASTERING = [13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450, 10000000, 1];
const fixture = name => fileURLToPath(new URL(`../fixtures/${name}`, import.meta.url));
const idr = Uint8Array.of(0x26, 0x01, 0xaf);

function entryBoxes(mux) {
  const b = mux.init();
  const types = boxTypes(b);
  const at = (t) => {
    for (let i = 0; i + 4 <= b.length; i++) if (String.fromCharCode(...b.subarray(i, i + 4)) === t) return b.subarray(i + 4, i - 4 + new DataView(b.buffer, b.byteOffset).getUint32(i - 4));
    return null;
  };
  return { types, at };
}

test('SPS colour: PQ limited range and HLG full range', () => {
  assert.deepEqual(hevcSpsColour(makeSps()), { primaries: 9, transfer: 16, matrix: 9, fullRange: false });
  assert.deepEqual(hevcSpsColour(makeSps({ transfer: 18, fullRange: 1 })), { primaries: 9, transfer: 18, matrix: 9, fullRange: true });
});

test('SPS colour: unspecified fields are null, the range flag stays', () => {
  assert.deepEqual(hevcSpsColour(makeSps({ primaries: 2, transfer: 2, matrix: 2, fullRange: 1 })), { primaries: null, transfer: null, matrix: null, fullRange: true });
});

test('SPS colour: an inter predicted short term set is walked with its real delta POCs', () => {
  const first = ({ ue, u }) => { ue(2); ue(1); ue(0); u(1, 1); ue(1); u(1, 1); ue(0); u(1, 1); };
  // The flag count of this set is NumDeltaPocs + 1 of the set before it (4), with one use_delta_flag read after a zero.
  const inter = ({ u, ue }) => { u(1, 1); u(0, 1); ue(0); u(1, 1); u(1, 1); u(0, 1); u(1, 1); u(1, 1); };
  assert.deepEqual(hevcSpsColour(makeSps({ rps: [first, inter] })), { primaries: 9, transfer: 16, matrix: 9, fullRange: false });
});

test('SEI: mastering display and content light in box layout', () => {
  assert.deepEqual(hevcHdrSei(makeSei(MASTERING, [1000, 400])), { mastering: MASTERING, contentLight: [1000, 400] });
});

test('hdrSignal: the container wins, the first keyframe fills what is left', () => {
  const au = accessUnit(makeSps({ transfer: 18 }), makeSei(MASTERING, [1000, 400]), idr);
  const track = { codec: 'hev1.2.4.L153.B0', description: emptyHvcc(), containerColor: { primaries: 9, transfer: 16, matrix: null, fullRange: null, mastering: null, contentLight: null } };
  assert.equal(hdrSignal(track).color, null);
  assert.deepEqual(hdrSignal(track, au), { color: { primaries: 9, transfer: 16, matrix: 9, fullRange: false }, mastering: MASTERING, contentLight: [1000, 400] });
});

test('hdrSignal: a colour volume that describes no display is dropped', () => {
  const track = { codec: 'hvc1.2.4.L153.B0', description: emptyHvcc(), containerColor: { primaries: 9, transfer: 16, matrix: 9, fullRange: false, mastering: new Array(10).fill(0), contentLight: null } };
  assert.equal(hdrSignal(track).mastering, null);
});

test('hdrSignalNeedsKeyframe: only HEVC, only what is missing', () => {
  const hevc = { codec: 'hvc1.2.4.L153.B0' };
  const pq = { primaries: 9, transfer: 16, matrix: 9, fullRange: false };
  assert.equal(hdrSignalNeedsKeyframe(hevc, { color: null, mastering: null, contentLight: null }), true);
  assert.equal(hdrSignalNeedsKeyframe(hevc, { color: pq, mastering: null, contentLight: null }), true);
  assert.equal(hdrSignalNeedsKeyframe(hevc, { color: pq, mastering: MASTERING, contentLight: [1, 1] }), false);
  assert.equal(hdrSignalNeedsKeyframe(hevc, { color: { ...pq, transfer: 18 }, mastering: null, contentLight: null }), false);
  assert.equal(hdrSignalNeedsKeyframe({ codec: 'av01.0.13M.10' }, { color: null, mastering: null, contentLight: null }), false);
});

test('sample entry: colr, mdcv and clli bytes, big endian, G B R white point', () => {
  const m = new Fmp4Muxer({ codec: 'hvc1.2.4.L153.B0', description: emptyHvcc(), width: 3840, height: 2160,
    color: { primaries: 9, transfer: 16, matrix: 9, fullRange: false }, mastering: MASTERING, contentLight: [1000, 400] });
  const { types, at } = entryBoxes(m);
  assert.deepEqual(Array.from(at('colr')), [0x6e, 0x63, 0x6c, 0x78, 0, 9, 0, 16, 0, 9, 0]);
  assert.deepEqual(Array.from(at('mdcv')).map(v => v.toString(16).padStart(2, '0')).join(''), '33c286c41d4c0bb884d03e803d1340420098968000000001');
  assert.deepEqual(Array.from(at('clli')), [0x03, 0xe8, 0x01, 0x90]);
  assert.ok(types.indexOf('colr') > types.indexOf('hvcC') && types.indexOf('clli') < types.indexOf('pasp'));
});

test('sample entry: without HDR values it has no HDR boxes (Chrome path)', () => {
  const { types } = entryBoxes(new Fmp4Muxer({ codec: 'hvc1.2.4.L153.B0', description: emptyHvcc(), width: 1920, height: 1080 }));
  for (const t of ['colr', 'mdcv', 'clli']) assert.equal(types.includes(t), false, t);
});

const REAL = { skip: existsSync(fixture('hdr10plus.mkv')) ? false : 'fixtures not present' };

test('real Matroska: Colour reaches the track object', REAL, async () => {
  const d = await openDemuxer({ path: fixture('hdr10plus.mkv') });
  const t = d.tracks().find(x => x.kind === 'video');
  assert.equal(t.containerColor.primaries, 9);
  assert.equal(t.containerColor.transfer, 16);
  assert.equal(t.color, undefined);
});

test('real MP4: colr reaches the track object, HLG full range', { skip: existsSync(fixture('zz-n13-hlgplain.mp4')) ? false : 'fixture not present' }, async () => {
  const d = await openDemuxer({ path: fixture('zz-n13-hlgplain.mp4') });
  const t = d.tracks().find(x => x.kind === 'video');
  assert.deepEqual(hdrSignal(t).color, { primaries: 9, transfer: 18, matrix: 9, fullRange: true });
});
