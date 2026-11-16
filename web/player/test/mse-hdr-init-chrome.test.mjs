// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
Object.defineProperty(globalThis, 'navigator', { value: { userAgent: 'Mozilla/5.0 (Macintosh) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36' }, configurable: true });
const { appended, boxTypes, emptyHvcc, startWorker } = await import('./_mse-worker-harness.mjs');

test('Chrome: the init segment goes out at once and without HDR boxes, whatever the container says', async () => {
  await startWorker({ codec: 'hvc1.2.4.L153.B0', description: emptyHvcc(), width: 3840, height: 2160, containerColor: { primaries: 9, transfer: 16, matrix: 9, fullRange: false, mastering: [13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450, 10000000, 1], contentLight: [1000, 400] } });
  assert.equal(appended.length, 1);
  const types = boxTypes(appended[0]);
  for (const t of ['colr', 'mdcv', 'clli']) assert.equal(types.includes(t), false, t);
});
