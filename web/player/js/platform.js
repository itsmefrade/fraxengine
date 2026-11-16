// SPDX-License-Identifier: LGPL-3.0-only
// What differs between the browsers the player runs in, kept in one place. Everything here was measured on Safari 27.2.

// Chromium also says AppleWebKit in its user agent. The difference matters because some WebKit limits cannot be detected: WebCodecs
// accepts HEVC Main10 there and hands out 8-bit frames, and isConfigSupported says yes all the same.
export const isWebKit = typeof navigator !== 'undefined' && /AppleWebKit/.test(navigator.userAgent) && !/(Chrome|Chromium|Edg|OPR|Android)\//.test(navigator.userAgent);

// An iPhone has no MediaSource, only ManagedMediaSource: the same interface, where the system may also drop buffered data when it
// runs short of memory. Both exist in a worker there. iPad and Mac Safari keep the classic one.
export const MSE = globalThis.MediaSource ?? globalThis.ManagedMediaSource ?? null;
export const mseInWorker = !!MSE?.canConstructInDedicatedWorker;

// A phone or tablet running WebKit (an iPad asks for desktop sites as a Mac with a touch screen). The player takes the system's own
// decoder for everything it accepts there: 4K software HEVC in WASM is not something a phone holds at speed or in memory.
export const isHandheld = isWebKit && typeof navigator !== 'undefined' && (/iPhone|iPad|iPod/.test(navigator.userAgent) || (/Macintosh/.test(navigator.userAgent) && navigator.maxTouchPoints > 1));

// WebKit's isTypeSupported asks AVFoundation, which knows the sample entry names and is case sensitive: "flac" and "opus" are refused, "fLaC" and "Opus" accepted.
const SPELLING = { flac: 'fLaC', opus: 'Opus' };
export function mseMime(kind, codec) {
  const plain = `${kind}/mp4; codecs="${codec}"`;
  const alt = SPELLING[codec];
  if (!alt || !MSE || MSE.isTypeSupported(plain)) return plain;
  const other = `${kind}/mp4; codecs="${alt}"`;
  return MSE.isTypeSupported(other) ? other : plain;
}
export const mseAudioSupported = codec => !!MSE && MSE.isTypeSupported(mseMime('audio', codec));

// Dolby Vision the browser's own video pipeline can take. A Profile 7 MEL has no picture in its enhancement layer, so it goes in as
// single layer Profile 8.1 (the demux worker drops the layer and rewrites the RPU). FEL cannot: its enhancement layer is the picture.
export function nativeDolbyVision(t) {
  const dv = t.dolbyVision, c = t.dolbyVisionCodecString;
  if (!isWebKit || !dv || !c || t.enhancementLayer || !/^dv(h1|he)\./.test(c) || !MSE) return null;
  const convert = dv.profile === 7 && dv.elKind === 'mel' && dv.blPresent;
  if (!convert && (dv.elPresent || (dv.profile !== 5 && dv.profile !== 8))) return null;
  const codec = convert ? `dvh1.08.${String(dv.level).padStart(2, '0')}` : c;
  if (!MSE.isTypeSupported(`video/mp4; codecs="${codec}"`)) return null;
  return { codec, convert, record: convert ? { profile: 8, level: dv.level, rpuPresent: true, elPresent: false, blPresent: true, blCompat: 1 } : dv };
}

// Dolby Vision the system pipeline refuses (FEL, or a profile this device does not list) still has a picture it can play: the base
// layer of Profile 7 and of a cross compatible Profile 8 is plain HDR10, HLG or SDR HEVC. Profile 5 has no such layer.
export function dolbyVisionBaseLayer(t) {
  const dv = t.dolbyVision;
  if (!dv || !dv.blPresent || t.enhancementLayer) return false;
  return dv.profile === 7 || (dv.profile === 8 && [1, 2, 4, 6].includes(dv.blCompat));
}
