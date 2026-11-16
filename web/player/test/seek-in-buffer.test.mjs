// SPDX-License-Identifier: LGPL-3.0-only
// A seek whose target the element already holds stays in the element (js/seek-policy.js): which targets count as held.
import test from 'node:test';
import assert from 'node:assert/strict';
import { holdsTarget, elementRanges, downloadedReachS, SOFT_SEEK_MARGIN_S, SOFT_SEEK_REACH_MAX_S } from '../js/seek-policy.js';

test('a target inside a buffered range is held, one outside every range is not', () => {
  const ranges = [[29.3, 61.4]];
  assert.equal(holdsTarget(ranges, 40), true);
  assert.equal(holdsTarget(ranges, 29.3), true);          // the range starts at a keyframe: its first instant is decodable
  assert.equal(holdsTarget(ranges, 29.29), false);
  assert.equal(holdsTarget(ranges, 70), false);
  assert.equal(holdsTarget([], 40), false);
});

test('the end of a growing range is not held: the element would wait there for the next append', () => {
  const ranges = [[10, 20]];
  assert.equal(holdsTarget(ranges, 20 - SOFT_SEEK_MARGIN_S - 0.01), true);
  assert.equal(holdsTarget(ranges, 20 - SOFT_SEEK_MARGIN_S + 0.01), false);
  assert.equal(holdsTarget(ranges, 20), false);
});

test('at the end of the stream nothing more is appended, so the range counts up to its end', () => {
  assert.equal(holdsTarget([[100, 120]], 119.9, { ended: true }), true);
  assert.equal(holdsTarget([[100, 120]], 120, { ended: true }), false);
});

test('a hole between two ranges is not held (Chrome evicts on its own above the quota)', () => {
  const ranges = [[0, 12], [30, 55]];
  assert.equal(holdsTarget(ranges, 11), true);
  assert.equal(holdsTarget(ranges, 20), false);
  assert.equal(holdsTarget(ranges, 31), true);
});

test('a target that is not a time is never held', () => {
  assert.equal(holdsTarget([[0, 10]], NaN), false);
  assert.equal(holdsTarget([[0, 10]], Infinity), false);
});

test('elementRanges reads HTMLMediaElement.buffered', () => {
  const el = { buffered: { length: 2, start: i => [1, 8][i], end: i => [5, 9][i] } };
  assert.deepEqual(elementRanges(el), [[1, 5], [8, 9]]);
});

test('the range being played reaches as far as the media already downloaded behind it', () => {
  const ranges = [[0, 12], [29.3, 39.4]];
  // 10 s ahead of 29.5 s is 0.1 s past the range: held when 8 s more are in the ring, not when nothing is
  assert.equal(holdsTarget(ranges, 39.5, { fromT: 29.5, reachS: 8 }), true);
  assert.equal(holdsTarget(ranges, 39.5, { fromT: 29.5, reachS: 0 }), false);
  assert.equal(holdsTarget(ranges, 47.5, { fromT: 29.5, reachS: 8 }), false);
  // inside the margin at the end of the range it is held as well, the next append is already on its way
  assert.equal(holdsTarget(ranges, 39.3, { fromT: 29.5, reachS: 8 }), true);
  // only the range the element is in grows: the reach does not extend an old island
  assert.equal(holdsTarget(ranges, 14, { fromT: 29.5, reachS: 8 }), false);
  // and never backwards before a range
  assert.equal(holdsTarget(ranges, 29, { fromT: 29.5, reachS: 8 }), false);
});

test('the reach is what the continuous download holds, less a fifth, and bounded', () => {
  assert.equal(downloadedReachS(null), 0);
  assert.equal(downloadedReachS({ kind: 'blocks', aheadS: 5 }), 0);
  assert.equal(downloadedReachS({ kind: 'stream', aheadS: null }), 0);
  assert.equal(downloadedReachS({ kind: 'stream', aheadS: 5 }), 4);
  assert.equal(downloadedReachS({ kind: 'stream', aheadS: 60 }), SOFT_SEEK_REACH_MAX_S);
});
