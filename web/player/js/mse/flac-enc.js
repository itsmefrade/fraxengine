// SPDX-License-Identifier: LGPL-3.0-only
// Small dependency-free FLAC encoder (fixed blocksize, fixed predictors, partitioned Rice) for
// feeding decoded PCM to Media Source Extensions as FLAC in fragmented MP4.
//
// Default mode writes VERBATIM subframes: the stream only has to be lossless and valid, and the
// predictor search plus Rice coding cost about 9 to 11 ms per second of audio (8ch 48 kHz and
// 2ch 192 kHz, 24 bit, measured on node) against a straight byte copy. Pass compress: true for
// the fixed-predictor + Rice path (about 75% of raw size on test material).

const CRC8 = new Uint8Array(256);
const CRC16 = new Uint16Array(256);
for (let i = 0; i < 256; i++) {
  let c8 = i;
  let c16 = i << 8;
  for (let b = 0; b < 8; b++) {
    c8 = c8 & 0x80 ? ((c8 << 1) ^ 0x07) & 0xff : (c8 << 1) & 0xff;
    c16 = c16 & 0x8000 ? ((c16 << 1) ^ 0x8005) & 0xffff : (c16 << 1) & 0xffff;
  }
  CRC8[i] = c8;
  CRC16[i] = c16;
}

// Slicing-by-4 tables for CRC16 (Tk[i] = T0[i] advanced through k more zero bytes). The verbatim
// path checksums every output byte, so this is its largest remaining cost after the sample copy.
const CRC16_S = [CRC16, new Uint16Array(256), new Uint16Array(256), new Uint16Array(256)];
for (let k = 1; k < 4; k++) {
  for (let i = 0; i < 256; i++) {
    const p = CRC16_S[k - 1][i];
    CRC16_S[k][i] = ((p << 8) & 0xffff) ^ CRC16[p >> 8];
  }
}
const [C16_0, C16_1, C16_2, C16_3] = CRC16_S;

const MAX_PART_ORDER = 6;
const T_CONST = 0;
const T_VERBATIM = 1;
const T_FIXED = 2;
const POW2_INV = new Float64Array(32);
for (let k = 0; k < 32; k++) POW2_INV[k] = 1 / 2 ** k;

// Byte-oriented MSB-first bit writer. acc never holds more than 7 pending bits between calls.
class BitWriter {
  constructor(size) {
    this.buf = new Uint8Array(size);
    this.pos = 0;
    this.acc = 0;
    this.fill = 0;
  }
  reset() {
    this.pos = 0;
    this.acc = 0;
    this.fill = 0;
  }
  grow() {
    const nb = new Uint8Array(this.buf.length * 2);
    nb.set(this.buf);
    this.buf = nb;
  }
  // n in 0..32, v may be negative (two's complement low n bits are written).
  write(v, n) {
    if (n > 24) {
      this.write((v >>> 16) & ((1 << (n - 16)) - 1), n - 16);
      this.write(v & 0xffff, 16);
      return;
    }
    if (this.pos + 8 > this.buf.length) this.grow();
    this.acc = (this.acc << n) | (v & ((1 << n) - 1));
    this.fill += n;
    while (this.fill >= 8) {
      this.fill -= 8;
      this.buf[this.pos++] = (this.acc >>> this.fill) & 0xff;
    }
    this.acc &= (1 << this.fill) - 1;
  }
  align() {
    if (this.fill > 0) this.write(0, 8 - this.fill);
  }
}

function blockSizeCode(n) {
  switch (n) {
    case 192: return 1;
    case 576: return 2;
    case 1152: return 3;
    case 2304: return 4;
    case 4608: return 5;
    case 256: return 8;
    case 512: return 9;
    case 1024: return 10;
    case 2048: return 11;
    case 4096: return 12;
    case 8192: return 13;
    case 16384: return 14;
    case 32768: return 15;
    default: return n <= 256 ? 6 : 7;
  }
}

function sampleRateCode(r) {
  switch (r) {
    case 88200: return 1;
    case 176400: return 2;
    case 192000: return 3;
    case 8000: return 4;
    case 16000: return 5;
    case 22050: return 6;
    case 24000: return 7;
    case 32000: return 8;
    case 44100: return 9;
    case 48000: return 10;
    case 96000: return 11;
    default:
      if (r % 1000 === 0 && r / 1000 <= 255) return 12;
      if (r <= 65535) return 13;
      return 14;
  }
}

function newSlot(blockSize) {
  return {
    resid: new Int32Array(blockSize),
    params: new Int32Array(1 << MAX_PART_ORDER),
    type: T_VERBATIM, order: 0, po: 0, method: 0, bits: 0, bps: 0, n: 0, src: null,
  };
}

export class FlacEncoder {
  constructor({ channels, sampleRate, bitsPerSample = 24, blockSize = 4096, compress = false }) {
    if (!Number.isInteger(channels) || channels < 1 || channels > 8) throw new RangeError('channels must be 1..8');
    if (!Number.isInteger(sampleRate) || sampleRate < 1 || sampleRate > 655350) throw new RangeError('bad sampleRate');
    if (sampleRate > 65535 && sampleRate % 10 !== 0 && sampleRate % 1000 !== 0) throw new RangeError('unrepresentable sampleRate');
    if (bitsPerSample !== 16 && bitsPerSample !== 24) throw new RangeError('bitsPerSample must be 16 or 24');
    if (!Number.isInteger(blockSize) || blockSize < 16 || blockSize > 65535) throw new RangeError('blockSize must be 16..65535');
    this.compress = !!compress;
    this.channels = channels;
    this.sampleRate = sampleRate;
    this.bps = bitsPerSample;
    this.blockSize = blockSize;
    this.scale = 2 ** (bitsPerSample - 1);
    this.pending = [];
    for (let c = 0; c < channels; c++) this.pending.push(new Int32Array(blockSize));
    this.mid = new Int32Array(blockSize);
    this.side = new Int32Array(blockSize);
    this.filled = 0;
    this.frameNo = 0;
    this.slots = [];
    for (let i = 0; i < Math.max(channels, 4); i++) this.slots.push(newSlot(blockSize));
    this.sums = new Float64Array(2 << MAX_PART_ORDER);
    this.ks14 = new Int32Array(1 << MAX_PART_ORDER);
    this.ks30 = new Int32Array(1 << MAX_PART_ORDER);
    this.w = new BitWriter(64 + channels * (Math.ceil(blockSize * (bitsPerSample + 1) / 8) + 16));
  }

  streamInfo() {
    const b = new Uint8Array(34);
    const n = this.blockSize;
    b[0] = n >> 8; b[1] = n & 255; b[2] = n >> 8; b[3] = n & 255;
    const r = this.sampleRate;
    b[10] = r >> 12;
    b[11] = (r >> 4) & 255;
    b[12] = ((r & 15) << 4) | ((this.channels - 1) << 1) | ((this.bps - 1) >> 4);
    b[13] = ((this.bps - 1) & 15) << 4;
    return b;
  }

  encode(planes) {
    if (!planes || planes.length !== this.channels) throw new RangeError('plane count must equal channels');
    const len = planes[0].length;
    for (let c = 1; c < planes.length; c++) if (planes[c].length !== len) throw new RangeError('planes differ in length');
    const out = [];
    const scale = this.scale, hi = scale - 1, lo = -scale;
    let off = 0;
    while (off < len) {
      const take = Math.min(len - off, this.blockSize - this.filled);
      for (let c = 0; c < this.channels; c++) {
        const src = planes[c], dst = this.pending[c];
        for (let i = 0, j = this.filled, k = off; i < take; i++, j++, k++) {
          let s = Math.round(src[k] * scale);
          if (s > hi) s = hi;
          else if (s < lo) s = lo;
          else if (s !== s) s = 0;
          dst[j] = s;
        }
      }
      this.filled += take;
      off += take;
      if (this.filled === this.blockSize) {
        out.push({ data: this.encodeFrame(this.blockSize), samples: this.blockSize });
        this.filled = 0;
      }
    }
    return out;
  }

  flush() {
    if (this.filled === 0) return [];
    const n = this.filled;
    this.filled = 0;
    return [{ data: this.encodeFrame(n), samples: n }];
  }

  reset() {
    this.filled = 0;
    this.frameNo = 0;
  }

  // Fills slot with the cheapest representation of src[0..n) at the given bit depth.
  analyze(slot, src, n, bps) {
    slot.src = src; slot.n = n; slot.bps = bps;
    const first = src[0];
    let i = 1;
    while (i < n && src[i] === first) i++;
    if (i === n) {
      slot.type = T_CONST;
      slot.bits = 8 + bps;
      return;
    }
    slot.type = T_VERBATIM;
    slot.bits = 8 + n * bps;
    if (n < 8) return;

    // Pick the fixed order by summed absolute residual over the common range.
    let a0 = 0, a1 = 0, a2 = 0, a3 = 0, a4 = 0;
    let s1 = src[3], s2 = src[2], s3 = src[1], s4 = src[0];
    for (i = 4; i < n; i++) {
      const x = src[i];
      a0 += x < 0 ? -x : x;
      let e = x - s1; a1 += e < 0 ? -e : e;
      e = x - 2 * s1 + s2; a2 += e < 0 ? -e : e;
      e = x - 3 * s1 + 3 * s2 - s3; a3 += e < 0 ? -e : e;
      e = x - 4 * s1 + 6 * s2 - 4 * s3 + s4; a4 += e < 0 ? -e : e;
      s4 = s3; s3 = s2; s2 = s1; s1 = x;
    }
    let order = 0, best = a0;
    if (a1 < best) { best = a1; order = 1; }
    if (a2 < best) { best = a2; order = 2; }
    if (a3 < best) { best = a3; order = 3; }
    if (a4 < best) { best = a4; order = 4; }

    const r = slot.resid;
    switch (order) {
      case 0: for (i = 0; i < n; i++) r[i] = src[i]; break;
      case 1: for (i = 1; i < n; i++) r[i - 1] = src[i] - src[i - 1]; break;
      case 2: for (i = 2; i < n; i++) r[i - 2] = src[i] - 2 * src[i - 1] + src[i - 2]; break;
      case 3: for (i = 3; i < n; i++) r[i - 3] = src[i] - 3 * src[i - 1] + 3 * src[i - 2] - src[i - 3]; break;
      default:
        for (i = 4; i < n; i++) r[i - 4] = src[i] - 4 * src[i - 1] + 6 * src[i - 2] - 4 * src[i - 3] + src[i - 4];
    }

    let pmax = 0;
    while (pmax < MAX_PART_ORDER && n % (2 << pmax) === 0 && (n >> (pmax + 1)) > order) pmax++;
    const sums = this.sums;
    const finest = 1 << pmax;
    const P = n >> pmax;
    for (let j = 0; j < finest; j++) {
      const lo = j === 0 ? 0 : j * P - order;
      const hi = (j + 1) * P - order;
      let s = 0;
      for (let k = lo; k < hi; k++) {
        const v = r[k];
        s += ((v << 1) ^ (v >> 31)) >>> 0;
      }
      sums[finest + j] = s;
    }
    for (let p = pmax - 1; p >= 0; p--) {
      const base = 1 << p;
      for (let j = 0; j < base; j++) sums[base + j] = sums[2 * base + 2 * j] + sums[2 * base + 2 * j + 1];
    }

    let bestCost = Infinity, bestP = 0, bestMethod = 0;
    const ks14 = this.ks14, ks30 = this.ks30;
    for (let p = 0; p <= pmax; p++) {
      const parts = 1 << p;
      const psz = n >> p;
      let c14 = 0, c30 = 0;
      for (let j = 0; j < parts; j++) {
        const nj = j === 0 ? psz - order : psz;
        const S = sums[parts + j];
        const mean = S / nj;
        const kg = mean < 1 ? 0 : Math.floor(Math.log2(mean));
        const lo = kg > 0 ? kg - 1 : 0;
        const hi = kg + 1 < 30 ? kg + 1 : 30;
        let b30 = Infinity, k30 = 0, b14 = Infinity, k14 = 0;
        for (let k = lo; k <= hi; k++) {
          const c = nj * (k + 1) + S * POW2_INV[k];
          if (c < b30) { b30 = c; k30 = k; }
          if (k <= 14 && c < b14) { b14 = c; k14 = k; }
        }
        if (b14 === Infinity) { k14 = 14; b14 = nj * 15 + S * POW2_INV[14]; }
        ks14[j] = k14; ks30[j] = k30;
        c14 += 4 + b14; c30 += 5 + b30;
      }
      const method = c14 <= c30 ? 0 : 1;
      const c = method === 0 ? c14 : c30;
      if (c < bestCost) {
        bestCost = c; bestP = p; bestMethod = method;
        slot.params.set((method === 0 ? ks14 : ks30).subarray(0, parts));
      }
    }
    const total = Math.ceil(8 + order * bps + 6 + bestCost);
    if (total < slot.bits) {
      slot.type = T_FIXED;
      slot.bits = total;
      slot.order = order;
      slot.po = bestP;
      slot.method = bestMethod;
    }
  }

  writeSubframe(slot) {
    const w = this.w, bps = slot.bps, src = slot.src, n = slot.n;
    if (slot.type === T_CONST) {
      w.write(0, 8);
      w.write(src[0], bps);
      return;
    }
    if (slot.type === T_VERBATIM) {
      w.write(0x02, 8);
      for (let i = 0; i < n; i++) w.write(src[i], bps);
      return;
    }
    const order = slot.order;
    w.write((8 + order) << 1, 8);
    for (let i = 0; i < order; i++) w.write(src[i], bps);
    w.write(slot.method, 2);
    w.write(slot.po, 4);
    const parts = 1 << slot.po;
    const psz = n >> slot.po;
    const pbits = slot.method === 0 ? 4 : 5;
    const r = slot.resid;
    let idx = 0;
    for (let j = 0; j < parts; j++) {
      const k = slot.params[j];
      w.write(k, pbits);
      const cnt = j === 0 ? psz - order : psz;
      const mask = (1 << k) - 1;
      for (let m = 0; m < cnt; m++) {
        const v = r[idx++];
        const u = ((v << 1) ^ (v >> 31)) >>> 0;
        const q = u >>> k;
        const total = q + 1 + k;
        if (total <= 24) {
          w.write((1 << k) | (u & mask), total);
        } else {
          let z = q;
          while (z > 24) { w.write(0, 24); z -= 24; }
          w.write(1, z + 1);
          if (k > 0) w.write(u & mask, k);
        }
      }
    }
  }

  // Verbatim frame: header, then each channel as a 1-byte subframe header plus raw big-endian
  // samples, then CRC16. The frame is written straight into its final array (no BitWriter, no copy).
  encodeFrameVerbatim(n) {
    const ch = this.channels, bps = this.bps, pend = this.pending;
    const w = this.w;
    w.reset();
    this.writeHeader(ch - 1, n);
    const hl = w.pos;
    const bytes = bps >> 3;
    const out = new Uint8Array(hl + ch * (1 + n * bytes) + 2);
    out.set(w.buf.subarray(0, hl));
    let p = hl;
    for (let c = 0; c < ch; c++) {
      out[p++] = 0x02;
      const src = pend[c];
      if (bytes === 3) {
        for (let i = 0; i < n; i++) {
          const v = src[i];
          out[p] = v >> 16; out[p + 1] = v >> 8; out[p + 2] = v;
          p += 3;
        }
      } else {
        for (let i = 0; i < n; i++) {
          const v = src[i];
          out[p] = v >> 8; out[p + 1] = v;
          p += 2;
        }
      }
    }
    let crc = 0, i = 0;
    const end = p - (p & 3);
    for (; i < end; i += 4) {
      crc = C16_3[(crc >> 8) ^ out[i]] ^ C16_2[(crc & 255) ^ out[i + 1]] ^ C16_1[out[i + 2]] ^ C16_0[out[i + 3]];
    }
    for (; i < p; i++) crc = ((crc << 8) & 0xffff) ^ CRC16[(crc >> 8) ^ out[i]];
    out[p] = crc >> 8; out[p + 1] = crc & 255;
    return out;
  }

  writeHeader(assign, n) {
    const w = this.w, bps = this.bps;
    w.write(0xfff8, 16);
    const bsc = blockSizeCode(n), src = sampleRateCode(this.sampleRate);
    w.write(bsc, 4);
    w.write(src, 4);
    w.write(assign, 4);
    w.write(bps === 16 ? 4 : 6, 3);
    w.write(0, 1);
    const fn = this.frameNo++;
    if (fn < 0x80) {
      w.write(fn, 8);
    } else {
      const nb = fn < 0x800 ? 2 : fn < 0x10000 ? 3 : fn < 0x200000 ? 4 : fn < 0x4000000 ? 5 : 6;
      w.write(((0xff00 >> nb) & 0xff) | Math.floor(fn / 2 ** (6 * (nb - 1))), 8);
      for (let i = nb - 2; i >= 0; i--) w.write(0x80 | (Math.floor(fn / 2 ** (6 * i)) & 0x3f), 8);
    }
    if (bsc === 6) w.write(n - 1, 8);
    else if (bsc === 7) w.write(n - 1, 16);
    if (src === 12) w.write(this.sampleRate / 1000, 8);
    else if (src === 13) w.write(this.sampleRate, 16);
    else if (src === 14) w.write(this.sampleRate / 10, 16);
    let crc = 0;
    for (let i = 0; i < w.pos; i++) crc = CRC8[crc ^ w.buf[i]];
    w.write(crc, 8);
  }

  encodeFrame(n) {
    if (!this.compress) return this.encodeFrameVerbatim(n);
    const ch = this.channels, bps = this.bps, slots = this.slots, pend = this.pending;
    let assign = ch - 1;
    let sub0 = 0, sub1 = 1;
    if (ch === 2) {
      const L = pend[0], R = pend[1], mid = this.mid, side = this.side;
      for (let i = 0; i < n; i++) {
        const l = L[i], r = R[i];
        side[i] = l - r;
        mid[i] = (l + r) >> 1;
      }
      this.analyze(slots[0], L, n, bps);
      this.analyze(slots[1], R, n, bps);
      this.analyze(slots[2], mid, n, bps);
      this.analyze(slots[3], side, n, bps + 1);
      const cL = slots[0].bits, cR = slots[1].bits, cM = slots[2].bits, cS = slots[3].bits;
      let bestBits = cL + cR;
      if (cL + cS < bestBits) { bestBits = cL + cS; assign = 8; sub0 = 0; sub1 = 3; }
      if (cS + cR < bestBits) { bestBits = cS + cR; assign = 9; sub0 = 3; sub1 = 1; }
      if (cM + cS < bestBits) { assign = 10; sub0 = 2; sub1 = 3; }
    } else {
      for (let c = 0; c < ch; c++) this.analyze(slots[c], pend[c], n, bps);
    }

    const w = this.w;
    w.reset();
    this.writeHeader(assign, n);

    if (ch === 2) {
      this.writeSubframe(slots[sub0]);
      this.writeSubframe(slots[sub1]);
    } else {
      for (let c = 0; c < ch; c++) this.writeSubframe(slots[c]);
    }
    w.align();
    let c16 = 0;
    const buf = w.buf;
    for (let i = 0, e = w.pos; i < e; i++) c16 = ((c16 << 8) & 0xffff) ^ CRC16[(c16 >> 8) ^ buf[i]];
    w.write(c16, 16);
    return w.buf.slice(0, w.pos);
  }
}
