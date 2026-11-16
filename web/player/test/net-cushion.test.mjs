// SPDX-License-Identifier: LGPL-3.0-only
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { netCushionS, NET_FAST_S, NET_DOUBLE_S } from '../js/net-policy.js';

const net = (linkMbps, mediaMbps) => ({ linkBps: linkMbps * 1e6, bytesPerMediaS: mediaMbps * 1e6 / 8 });

test('a link comfortably above the bitrate starts on a small cushion', () => {
  assert.equal(netCushionS(4, net(90, 56), 56), NET_FAST_S);
  assert.equal(netCushionS(4, net(120, 56), 56), NET_DOUBLE_S);
});

test('a link close to or below the bitrate keeps the full cushion', () => {
  assert.equal(netCushionS(4, net(62, 56), 56), 4);
  assert.equal(netCushionS(3, net(45, 56), 56), 3);
});

test('an expensive opening scene counts as the bitrate', () => {
  assert.equal(netCushionS(4, net(90, 80), 56), 4);
});

test('an unmeasured link or an unknown bitrate keeps the full cushion', () => {
  assert.equal(netCushionS(4, { linkBps: null }, 56), 4);
  assert.equal(netCushionS(4, { linkBps: 9e7 }, null), 4);
  assert.equal(netCushionS(4, null, 56), 4);
});
