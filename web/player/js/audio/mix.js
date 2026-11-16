// SPDX-License-Identifier: LGPL-3.0-only
// Channel mapping and downmix by canonical layout labels (rust/README.md "Channel layouts").
//
// The output device layout is one of a few fixed Web Audio speaker orders (see OUT_LAYOUTS). A source is
// described by one label per plane. buildMixMatrix() folds every source label into the output layout with
// ITU-R BS.775 style coefficients: centre and surrounds at -3 dB into Lo/Ro, heights folded into fronts and
// rears at -3 dB, LFE dropped when the output has no LFE. Direct label matches are copied at unity.
// Pure functions, no DOM: unit-tested under node (web/player/test/audio.test.mjs).

const K = Math.SQRT1_2;   // -3 dB

export const OUT_LAYOUTS = {
  1: ['FC'],
  2: ['FL', 'FR'],
  6: ['FL', 'FR', 'FC', 'LFE', 'SL', 'SR'],
  8: ['FL', 'FR', 'FC', 'LFE', 'SL', 'SR', 'BL', 'BR'],
  12: ['FL', 'FR', 'FC', 'LFE', 'SL', 'SR', 'BL', 'BR', 'TFL', 'TFR', 'TBL', 'TBR'],
};
const SIZES = [12, 8, 6, 2, 1];

/** Largest supported output channel count that fits both the source and the device. */
export function snapChannels(srcChannels, maxChannelCount) {
  const n = Math.max(1, Math.min(srcChannels | 0, maxChannelCount | 0 || 2));
  return SIZES.find(s => s <= n) || 1;
}

/** Labels for a plane count when the decoder gave none (WebCodecs): FFmpeg/WAVE default orders. */
export function defaultLabels(n) {
  switch (n) {
    case 1: return ['FC'];
    case 2: return ['FL', 'FR'];
    case 3: return ['FL', 'FR', 'FC'];
    case 4: return ['FL', 'FR', 'BL', 'BR'];
    case 5: return ['FL', 'FR', 'FC', 'BL', 'BR'];
    case 6: return ['FL', 'FR', 'FC', 'LFE', 'BL', 'BR'];
    case 7: return ['FL', 'FR', 'FC', 'LFE', 'BC', 'SL', 'SR'];
    case 8: return ['FL', 'FR', 'FC', 'LFE', 'BL', 'BR', 'SL', 'SR'];
    case 12: return OUT_LAYOUTS[12].slice();
    default: return Array.from({ length: n }, (_, i) => `CH${i}`);
  }
}

// Fold tables: source label -> [[output label, gain], ...] per output class. Labels missing from a table
// fall back to the "2" table's spirit (dropped) and are reported in `unmapped`.
const FOLD = {
  // outputs with centre and surrounds
  full: {
    FLC: [['FL', 1]], FRC: [['FR', 1]], WL: [['FL', K]], WR: [['FR', K]],
    BC: [['BL', K], ['BR', K]],
    TFL: [['FL', K]], TFR: [['FR', K]], TC: [['FL', 0.5], ['FR', 0.5]],
    TBL: [['BL', K]], TBR: [['BR', K]], TSL: [['SL', K]], TSR: [['SR', K]],
    SDL: [['SL', 1]], SDR: [['SR', 1]], LFE2: [['LFE', 1]],
  },
};
// Per-target specialisations of the same idea.
function foldFor(outLabels) {
  const has = l => outLabels.includes(l);
  const t = {};
  const add = (src, list) => { t[src] = list.filter(([o]) => has(o)); };
  const full = has('FC') && has('SL');
  if (has('TFL')) {   // 7.1.4
    for (const [s, l] of Object.entries(FOLD.full)) add(s, l);
    add('TFL', [['TFL', 1]]); add('TFR', [['TFR', 1]]); add('TBL', [['TBL', 1]]); add('TBR', [['TBR', 1]]);
    add('TC', [['TFL', K], ['TFR', K]]); add('TSL', [['TFL', K], ['TBL', K]]); add('TSR', [['TFR', K], ['TBR', K]]);
  } else if (has('BL')) {   // 7.1
    for (const [s, l] of Object.entries(FOLD.full)) add(s, l);
  } else if (full) {   // 5.1
    for (const [s, l] of Object.entries(FOLD.full)) add(s, l);
    add('BC', [['SL', K], ['SR', K]]); add('TBL', [['SL', K]]); add('TBR', [['SR', K]]);
    add('BL', [['SL', K]]); add('BR', [['SR', K]]);   // only reached when the source also has SL/SR
  } else if (has('FR')) {   // Lo/Ro
    add('FC', [['FL', K], ['FR', K]]);
    add('FLC', [['FL', 1]]); add('FRC', [['FR', 1]]); add('WL', [['FL', K]]); add('WR', [['FR', K]]);
    add('SL', [['FL', K]]); add('SR', [['FR', K]]); add('BL', [['FL', K]]); add('BR', [['FR', K]]);
    add('SDL', [['FL', K]]); add('SDR', [['FR', K]]); add('BC', [['FL', 0.5], ['FR', 0.5]]);
    add('TFL', [['FL', K]]); add('TFR', [['FR', K]]); add('TC', [['FL', 0.5], ['FR', 0.5]]);
    add('TBL', [['FL', 0.5]]); add('TBR', [['FR', 0.5]]); add('TSL', [['FL', 0.5]]); add('TSR', [['FR', 0.5]]);
  } else {   // mono
    add('FL', [['FC', K]]); add('FR', [['FC', K]]);
    for (const l of ['SL', 'SR', 'BL', 'BR', 'FLC', 'FRC', 'BC', 'SDL', 'SDR', 'WL', 'WR']) add(l, [['FC', 0.5]]);
    for (const l of ['TFL', 'TFR', 'TC', 'TBL', 'TBR', 'TSL', 'TSR']) add(l, [['FC', 0.35]]);
  }
  return t;
}

/**
 * @param {string[]} srcLabels one canonical label per source plane
 * @param {string[]} outLabels output speaker order (OUT_LAYOUTS[n])
 * @returns {{gains: Float32Array[], identity: boolean, name: string, normGain: number, peakGain: number,
 *            lfeDropped: boolean, unmapped: string[], srcEff: string[]}}
 *   gains[o][s]: output plane o gets gains[o][s] * source plane s.
 */
export function buildMixMatrix(srcLabels, outLabels) {
  // Positional alias: a source with rear (BL BR) but no side surrounds (WAVE 5.1 / 5.0 / 4.0) uses them as the
  // surround pair, so they play on SL SR of a 5.1 / 7.1 / 7.1.4 output.
  const hasL = l => srcLabels.includes(l);
  const rearAsSide = hasL('BL') && hasL('BR') && !hasL('SL') && !hasL('SR') && outLabels.includes('SL');
  const srcEff = srcLabels.map(l => (rearAsSide && l === 'BL' ? 'SL' : rearAsSide && l === 'BR' ? 'SR' : l));
  const fold = foldFor(outLabels);
  const gains = outLabels.map(() => new Float32Array(srcLabels.length));
  const unmapped = [];
  let lfeDropped = false;
  let identity = srcLabels.length === outLabels.length;
  srcEff.forEach((l, s) => {
    const o = outLabels.indexOf(l);
    if (o >= 0) { gains[o][s] = 1; if (srcLabels[s] !== outLabels[o] || o !== s) identity = false; return; }
    identity = false;
    const list = fold[l];
    if (list && list.length) { for (const [ol, g] of list) gains[outLabels.indexOf(ol)][s] += g; return; }
    if (l === 'LFE' || l === 'LFE2') { lfeDropped = true; return; }
    unmapped.push(l);
  });
  let peak = 0;
  for (const row of gains) peak = Math.max(peak, row.reduce((a, b) => a + Math.abs(b), 0));
  // Stereo and mono folds are normalised so a full-scale source cannot clip (what FFmpeg does by default);
  // multichannel folds (7.1.4 -> 7.1) keep unity on the bed and report the peak gain instead.
  let normGain = 1;
  if (!identity && outLabels.length <= 2 && peak > 1) { normGain = 1 / peak; for (const row of gains) for (let i = 0; i < row.length; i++) row[i] *= normGain; }
  const sn = srcLabels.length, on = outLabels.length;
  let name;
  if (identity) name = 'direct';
  else if (on === 2 && sn > 2) name = `ITU-R BS.775 Lo/Ro ${sn}ch->2.0 (centre/surround -3 dB, norm ${normGain.toFixed(3)}${lfeDropped ? ', LFE dropped' : ''})`;
  else if (on === 1) name = `mono sum ${sn}ch->1.0 (norm ${normGain.toFixed(3)})`;
  else if (sn > on) name = `ITU fold ${layoutName(srcLabels)}->${layoutName(outLabels)} (heights/rears -3 dB into ${on === 8 ? 'fronts and rears' : 'fronts and surrounds'}${lfeDropped ? ', LFE dropped' : ''})`;
  else name = `label remap ${layoutName(srcLabels)}->${layoutName(outLabels)}`;
  return { gains, identity, name, normGain, peakGain: identity ? 1 : peak * normGain, lfeDropped, unmapped, srcEff };
}

export function layoutName(labels) {
  const n = labels.length;
  const lfe = labels.includes('LFE') ? 1 : 0;
  const heights = labels.filter(l => /^T/.test(l)).length;
  if (n === 1) return '1.0';
  const base = n - lfe - heights;
  return heights ? `${base - 0}.${lfe}.${heights}` : `${base}.${lfe}`;
}

/** out planes = gains * in planes. `out` is reused when it has the right shape. */
export function applyMix(gains, inPlanes, frames, outPlanes) {
  for (let o = 0; o < gains.length; o++) {
    const dst = outPlanes[o], row = gains[o];
    dst.fill(0, 0, frames);
    for (let s = 0; s < row.length; s++) {
      const g = row[s];
      if (g === 0) continue;
      const src = inPlanes[s];
      if (g === 1) for (let i = 0; i < frames; i++) dst[i] += src[i];
      else for (let i = 0; i < frames; i++) dst[i] += g * src[i];
    }
  }
  return outPlanes;
}
