// SPDX-License-Identifier: LGPL-3.0-only
// Whether a seek target is media the element already holds. Such a seek is the element's own (currentTime), as in any MSE player:
// nothing is flushed, no request is made, the demuxer goes on where it is. Measured on an 85 Mbit/s link with 75 ms to the first
// byte, 4K HEVC at 39 Mbit/s: a step of 10 s forward or 5 s back took 0.9 to 9.4 s through a flush and a new download (the
// video SourceBuffer was emptied and read again from the keyframe, up to 10 s before the target) against 48 to 291 ms for a
// plain <video>.
// `ranges` are [start, end] pairs in seconds on the element's timeline (HTMLMediaElement.buffered, the tracks' intersection).
// The target has to lie inside one range, `marginS` before its end: at the very end the element would wait for the next append
// anyway, and a range that stopped growing (the link) is better served by a seek that starts over at the target.
// The range being played (it holds `fromT`) reaches `reachS` further: that much media is already downloaded and waits in the ring
// and the lanes, so appending it is quicker than any request. Without this a step of 10 s at 39 Mbit/s fell just outside the
// 48 MB the SourceBuffer holds ahead, and the flush threw away the GOP it needed (its keyframe up to 10 s back, no longer in the ring).
// Bounded, because appending is not free either: a target 10 s past the buffered end took 1.7 s to reach this way (4K, 37 Mbit/s),
// more than a new request at a keyframe near the target costs on a link that carries twice the bitrate.
export const SOFT_SEEK_MARGIN_S = 0.25;
export const SOFT_SEEK_REACH_MAX_S = 5;
export function holdsTarget(ranges, t, { marginS = SOFT_SEEK_MARGIN_S, ended = false, fromT = null, reachS = 0 } = {}) {
  if (!Number.isFinite(t)) return false;
  for (const [a, b] of ranges) {
    if (t < a) continue;
    if (t < b - (ended ? 0 : marginS)) return true;
    if (reachS > 0 && fromT != null && a <= fromT && fromT <= b && t < b + reachS) return true;
  }
  return false;
}
// Media seconds the demuxer has downloaded past what it handed on (demux stats `net`), with a fifth off for the estimate's error.
export function downloadedReachS(net) {
  return net && net.kind === 'stream' && net.aheadS > 0 ? Math.min(SOFT_SEEK_REACH_MAX_S, net.aheadS * 0.8) : 0;
}
export const elementRanges = el => { const r = []; for (let i = 0; i < el.buffered.length; i++) r.push([el.buffered.start(i), el.buffered.end(i)]); return r; };
