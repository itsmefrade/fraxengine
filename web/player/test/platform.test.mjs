// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import { dolbyVisionBaseLayer, isHandheld, MSE } from '../js/platform.js';

const dv = (profile, blCompat, extra = {}) => ({ dolbyVision: { profile, blCompat, blPresent: true, ...extra } });

test('the base layer of profile 7 and of a cross compatible profile 8 is playable on its own', () => {
  assert.equal(dolbyVisionBaseLayer(dv(7, 6, { elPresent: true })), true);
  for (const c of [1, 2, 4]) assert.equal(dolbyVisionBaseLayer(dv(8, c)), true);
});

test('profile 5, profile 8 without a compatible base, an enhancement layer track and plain HEVC have none', () => {
  assert.equal(dolbyVisionBaseLayer(dv(5, 0)), false);
  assert.equal(dolbyVisionBaseLayer(dv(8, 0)), false);
  assert.equal(dolbyVisionBaseLayer({ ...dv(7, 6), enhancementLayer: true }), false);
  assert.equal(dolbyVisionBaseLayer({}), false);
});

test('outside a browser there is no media source and no handheld', () => {
  assert.equal(MSE, null);
  assert.equal(isHandheld, false);
});
