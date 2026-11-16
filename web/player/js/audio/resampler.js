// SPDX-License-Identifier: LGPL-3.0-only
// Streaming polyphase windowed-sinc resampler (Kaiser window, exact rational ratio).
//
// out[k] is the input at time k * inRate / outRate. With g = gcd(inRate, outRate), L = outRate / g phases and
// M = inRate / g input samples per L outputs, every output uses one row of a precomputed (L x taps) table, so the
// per-sample cost is `taps` multiply-adds regardless of the ratio. The cutoff is min(inRate, outRate) / 2 scaled by
// 0.94 (transition band below the new Nyquist), taps are 2 * 16 * max(1, inRate / outRate) for about 100 dB stopband.
// Planar Float32Array in, planar out; state (history and phase) carries across process() calls.

function gcd(a, b) { while (b) { [a, b] = [b, a % b]; } return a; }

function bessel0(x) {
  let sum = 1, term = 1;
  for (let k = 1; k < 40; k++) { term *= (x / (2 * k)) ** 2; sum += term; if (term < 1e-12 * sum) break; }
  return sum;
}

export class SincResampler {
  constructor(inRate, outRate, channels, { halfTaps = 16, beta = 9.5, cutoff = 0.94 } = {}) {
    this.inRate = inRate; this.outRate = outRate; this.channels = channels;
    const g = gcd(inRate, outRate);
    this.L = outRate / g; this.M = inRate / g;
    this.passthrough = inRate === outRate;
    const down = Math.max(1, inRate / outRate);
    this.half = Math.ceil(halfTaps * down);          // input samples on each side of the centre
    this.taps = 2 * this.half;
    const fc = Math.min(1, outRate / inRate) * cutoff;   // cutoff relative to the input Nyquist
    const table = new Float32Array(this.L * this.taps);
    const inv = 1 / bessel0(beta);
    for (let p = 0; p < this.L; p++) {
      const frac = p / this.L;
      let sum = 0;
      for (let j = 0; j < this.taps; j++) {
        const x = (j - (this.half - 1)) - frac;       // distance of tap j from the output instant, in input samples
        const w = bessel0(beta * Math.sqrt(Math.max(0, 1 - (x / this.half) ** 2))) * inv;
        const sinc = x === 0 ? fc : Math.sin(Math.PI * fc * x) / (Math.PI * x);
        const v = sinc * w;
        table[p * this.taps + j] = v; sum += v;
      }
      for (let j = 0; j < this.taps; j++) table[p * this.taps + j] /= sum;   // unity DC gain per phase
    }
    this.table = table;
    this.reset();
  }

  reset() {
    // hist[c] always starts at the window start of the next output, so the centre sample of that output sits at
    // index half-1 of (hist + new input). The first output is input sample 0 with half-1 zeros in front.
    this.hist = Array.from({ length: this.channels }, () => new Float32Array(this.half - 1));
    this.phase = 0;   // fractional position of the next output, in 1/L input samples
  }

  /** Output frames available for `n` more input frames (upper bound). */
  maxOut(n) { return Math.ceil((n * this.L) / this.M) + 2; }

  /** @param {Float32Array[]} planes @param {number} n frames @returns {{planes: Float32Array[], frames: number}} */
  process(planes, n) {
    if (this.passthrough) return { planes: planes.map(p => p.subarray(0, n)), frames: n };
    const { L, M, taps, half, table } = this;
    const out = [];
    let produced = 0;
    for (let c = 0; c < this.channels; c++) {
      const prev = this.hist[c];
      const buf = new Float32Array(prev.length + n);
      buf.set(prev, 0); buf.set(planes[c].subarray(0, n), prev.length);
      const o = new Float32Array(this.maxOut(n));
      let pos = half - 1, phase = this.phase, k = 0;
      // window for output at integer index pos covers buf[pos - (half-1) .. pos + half]
      const last = buf.length - half - 1;
      while (pos <= last) {
        const row = phase * taps, start = pos - (half - 1);
        let acc = 0;
        for (let j = 0; j < taps; j++) acc += buf[start + j] * table[row + j];
        o[k++] = acc;
        phase += M;
        if (phase >= L) { const q = Math.floor(phase / L); pos += q; phase -= q * L; }
      }
      // keep everything the next window needs
      this.hist[c] = buf.slice(pos - (half - 1));
      if (c === 0) { this.phase = phase; produced = k; }
      out.push(o.subarray(0, k));
    }
    return { planes: out, frames: produced };
  }
}
