// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
Object.defineProperty(globalThis, 'navigator', { value: { userAgent: 'Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.2 Safari/605.1.15' }, configurable: true });
const { appended, accessUnit, boxTypes, emptyHvcc, makeSei, makeSps, startWorker } = await import('./_mse-worker-harness.mjs');

test('Safari: an in-band-only track gets its init segment from the first keyframe, before any fragment', async () => {
  const w = await startWorker({ codec: 'hev1.2.4.L153.B0', description: emptyHvcc(), width: 3840, height: 2160 });
  assert.equal(appended.length, 0, 'the init segment waits for the keyframe');
  const data = accessUnit(makeSps(), makeSei([13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450, 10000000, 1], [1000, 400]), Uint8Array.of(0x26, 1, 0xaf));
  await w.send({ type: 'pkt', data: data.buffer.slice(0), ptsUs: 0, dtsUs: 0, durationUs: 41708, key: true });
  await w.send({ type: 'eof' });
  const types = boxTypes(appended[0]);
  for (const t of ['ftyp', 'colr', 'mdcv', 'clli']) assert.ok(types.includes(t), t);
  assert.deepEqual(boxTypes(appended[1]).slice(0, 1), ['moof']);
});
