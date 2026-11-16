// SPDX-License-Identifier: LGPL-3.0-only
// Seconds of media to have downloaded before a start or after a seek. A slow or unmeasured link keeps `base`; a link comfortably above
// the bitrate needs little, and if it later falls short the rebuffer path builds a doubled cushion. The bitrate is the larger of the
// file's average and what the demuxer has seen so far, so an expensive opening scene counts. `net.aheadS` leaves out the packets in
// flight to the video worker (12 at 24 fps: another half second), so 0.6 here is about 1 s of media really buffered.
export const NET_FAST_S = 0.6, NET_FAST_MARGIN = 1.3;       // a third of a second gained every second of playback
export const NET_DOUBLE_S = 0.25, NET_DOUBLE_MARGIN = 2;   // the decode preroll alone
export function netCushionS(base, net, bitrateMbps) {
  const rate = Math.max((net?.bytesPerMediaS || 0) * 8, (bitrateMbps || 0) * 1e6);
  if (!net?.linkBps || !rate) return base;
  const ratio = net.linkBps / rate;
  return ratio >= NET_DOUBLE_MARGIN ? NET_DOUBLE_S : ratio >= NET_FAST_MARGIN ? Math.min(base, NET_FAST_S) : base;
}
