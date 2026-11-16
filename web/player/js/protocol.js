// SPDX-License-Identifier: LGPL-3.0-only
// Shared constants and helpers for the main thread and all workers.
// Message epochs: every cross-thread message that carries media state is tagged with the session
// epoch. Seek and stop bump the epoch; a receiver drops (and closes) anything from an older epoch.

// Int32 slots of the shared control block. Counters are frames, wrapping at 2^31 (about 12 h at 48 kHz;
// a seek allocates a fresh ring so the counters restart from zero).
export const CTL = {
  WRITE: 0,       // frames written by the audio worker
  READ: 1,        // frames consumed by the worklet (the master clock)
  PLAY: 2,        // 1 while the worklet should consume
  LATENCY_US: 3,  // output latency estimate, written by the main thread
  UNDERRUNS: 4,   // worklet quanta that had less data than needed while playing
  EOF: 5,         // audio worker has written the last frame
  DRAINED: 6,     // worklet (or the audio worker, in virtual mode) consumed everything after EOF
  VIRTUAL: 7,     // 1 while the audio worker consumes the ring on the wall clock (AudioContext not running)
  SIZE: 8,
};

export const VIDEO_INFLIGHT = 12;   // packets the demux worker may have outstanding at the video worker
export const AUDIO_INFLIGHT = 48;       // floor of the audio window, in packets
export const AUDIO_WINDOW_US = 1_000_000;   // the audio lane's window in media time: 1200 packets a second (TrueHD) and 31 (AC-3) get the same second
export const PENDING_BYTES_CAP = 48 * 1024 * 1024;

export function allocRing(sampleRate, channels, seconds = 4) {
  const cap = Math.ceil(sampleRate * seconds);
  return {
    sampleRate, channels, cap,
    ctl: new SharedArrayBuffer(CTL.SIZE * 4),
    data: new SharedArrayBuffer(cap * channels * 4),
  };
}

export function ringViews(ring) {
  const ctl = new Int32Array(ring.ctl);
  const planes = [];
  for (let c = 0; c < ring.channels; c++) planes.push(new Float32Array(ring.data, c * ring.cap * 4, ring.cap));
  return { ctl, planes, cap: ring.cap };
}

// Media time heard at the speaker, in microseconds.
export function audioClockUs(ctl, baseUs, sampleRate) {
  const read = Atomics.load(ctl, CTL.READ);
  const lat = Atomics.load(ctl, CTL.LATENCY_US);
  return baseUs + (read * 1e6) / sampleRate - lat;
}

export function percentile(values, p) {
  if (!values.length) return 0;
  const s = Array.from(values).sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(p * s.length))];
}

// Yield to the event loop without the 4 ms setTimeout clamp.
const yieldChannel = typeof MessageChannel !== 'undefined' ? new MessageChannel() : null;
let yieldResolvers = [];
if (yieldChannel) yieldChannel.port1.onmessage = () => { const r = yieldResolvers; yieldResolvers = []; for (const f of r) f(); };
export function yieldNow() {
  return new Promise(res => { yieldResolvers.push(res); yieldChannel.port2.postMessage(0); });
}
