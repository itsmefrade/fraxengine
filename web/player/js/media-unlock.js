// SPDX-License-Identifier: LGPL-3.0-only
// Safari plays sound only from a media element or AudioContext that a user gesture has touched, and the player's own element and
// context are made seconds after the click (after the header read). So the first gesture on the page makes one of each here; they
// stay unlocked for the session and the next item (autoplay of an episode, no click) reuses them.
import { isWebKit } from './platform.js';

let el = null, ctx = null, armed = false;

function touch() {
  try {
    if (!el) { el = document.createElement('video'); el.playsInline = true; el.load(); el.play()?.catch(() => {}); }
    if (!ctx) ctx = new AudioContext({ sampleRate: 48000, latencyHint: 'playback' });
    if (ctx.state === 'suspended') ctx.resume().catch(() => {});
  } catch { /* the next gesture tries again */ }
}

export function armMediaUnlock() {
  if (!isWebKit || armed) return;
  armed = true;
  for (const ev of ['pointerdown', 'keydown', 'touchend']) document.addEventListener(ev, touch, { capture: true, passive: true });
}

export const unlockedElement = () => el;
export function takeAudioContext() { const c = ctx; ctx = null; return c; }
// A context that goes back stays unlocked for the next playback; closed, the next one would need a new gesture.
export function returnAudioContext(c) { if (ctx || c.state === 'closed') { c.close?.().catch?.(() => {}); return; } ctx = c; c.suspend().catch(() => {}); }
