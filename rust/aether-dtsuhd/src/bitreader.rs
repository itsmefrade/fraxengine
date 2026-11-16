//! MSB-first bit reader with the ACE primitive codes of ETSI TS 103 491 clause 8.2.
//!
//! The reader never panics. Reading past the end of the window yields zero bits and
//! records an overrun, which callers use to reject a corrupt frame.

#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Current position in bits from the start of `data`.
    pos: usize,
    /// Capacity in bits (may be smaller than `data.len() * 8`).
    total: usize,
    overrun: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            total: data.len().saturating_mul(8),
            overrun: false,
        }
    }

    /// Reader over the first `bits` bits of `data` (clamped to the slice).
    pub fn with_capacity(data: &'a [u8], bits: usize) -> Self {
        let max = data.len().saturating_mul(8);
        BitReader {
            data,
            pos: 0,
            total: bits.min(max),
            overrun: false,
        }
    }

    pub fn bits_used(&self) -> usize {
        self.pos
    }
    pub fn bits_total(&self) -> usize {
        self.total
    }
    pub fn bits_left(&self) -> i64 {
        self.total as i64 - self.pos as i64
    }
    pub fn overrun(&self) -> bool {
        self.overrun
    }
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }
    pub fn skip_bits(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
        if self.pos > self.total {
            self.overrun = true;
        }
    }
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
        if self.pos > self.total {
            self.overrun = true;
        }
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        if self.pos >= self.total {
            self.overrun = true;
            self.pos += 1;
            return 0;
        }
        let byte = self.data[self.pos >> 3];
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        bit as u32
    }

    pub fn read_bool(&mut self) -> bool {
        self.read_bit() != 0
    }

    /// Reads `n` bits (0..=32) as an unsigned integer.
    pub fn read_uint(&mut self, n: u32) -> u32 {
        let mut v: u64 = 0;
        for _ in 0..n.min(32) {
            v = (v << 1) | self.read_bit() as u64;
        }
        v as u32
    }

    pub fn read_int(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.read_uint(n) as i64;
        if v >= (1i64 << n) >> 1 {
            (v - (1i64 << n)) as i32
        } else {
            v as i32
        }
    }

    /// ReadUnary(alpha): zeros terminated by a one, truncated at alpha-1 zeros.
    pub fn read_unary(&mut self, alpha: u32) -> u32 {
        if alpha <= 1 {
            return 0;
        }
        let mut r = 0;
        while r + 1 < alpha {
            if self.read_bit() == 1 {
                break;
            }
            r += 1;
        }
        r
    }

    pub fn read_uniform(&mut self, alpha: u32) -> u32 {
        if alpha <= 1 {
            return 0;
        }
        let m = num_bits(alpha - 1);
        let gap = ((1u64 << m) - alpha as u64) as u32;
        let mut r = self.read_uint(m - 1);
        if r >= gap {
            r = (((r as u64) << 1) | self.read_bit() as u64) as u32;
            r = r.wrapping_sub(gap);
        }
        r
    }

    pub fn read_golomb(&mut self, alpha: u32, kparam: u32) -> u32 {
        if alpha <= 1 {
            return 0;
        }
        let kmax = num_bits(alpha - 1) - 1;
        if kparam >= kmax {
            return self.read_uniform(alpha);
        }
        let mut r = self.read_uint(kparam);
        r |= self.read_unary(1 + ((alpha - 1) >> kparam)) << kparam;
        r
    }

    pub fn read_golomb_limited(&mut self, alpha: u32, kparam: u32, limit: u32) -> u32 {
        if alpha <= 1 {
            return 0;
        }
        let mut limit = limit;
        if limit < (1u32 << kparam.min(31)) {
            limit = 1u32 << kparam.min(31);
        }
        if limit >= alpha {
            limit = alpha - 1;
        }
        let r = self.read_golomb(limit + 1, kparam);
        if r == limit {
            return self.read_uniform(alpha - limit) + limit;
        }
        r
    }

    /// ReadVLC(primary_table, escape_table).
    pub fn read_vlc(&mut self, primary: &[u32], escape: &[u32]) -> u32 {
        let alpha_of = |t: &[u32], i: usize| -> u64 { t[..i].iter().map(|&b| 1u64 << b).sum() };
        let p = self.read_unary(primary.len() as u32) as usize;
        let mut result = self.read_uint(primary[p]) as u64 + alpha_of(primary, p);
        let palpha = alpha_of(primary, primary.len());
        if result + escape.len() as u64 >= palpha && !escape.is_empty() {
            let e = (result + escape.len() as u64 - palpha) as usize;
            let e = e.min(escape.len() - 1);
            result = self.read_uint(escape[e]) as u64 + alpha_of(escape, e) + palpha;
        }
        result.min(u32::MAX as u64) as u32
    }

    pub fn read_limits_vlc(&mut self, table: &[u32]) -> u32 {
        let mut result: u64 = 0;
        for &res in table {
            let value = self.read_uint(res) as u64;
            result += value;
            if value + 1 < (1u64 << res) {
                break;
            }
        }
        result.min(u32::MAX as u64) as u32
    }

    pub fn read_limits_vlc_default(&mut self) -> u32 {
        self.read_limits_vlc(&[1, 1, 2, 2, 2, 4, 4, 16])
    }

    pub fn read_non_uniform_five_ten(&mut self) -> u32 {
        let mut r = self.read_uint(5);
        if r > 23 {
            r = ((r - 24) << 5) + self.read_uint(5);
        }
        if r == 279 {
            r += self.read_uint(22);
        }
        r
    }

    /// DTS-UHD header prefix code (clause 5.2.3.1): prefixes 0, 10, 110, 111 select a
    /// field width from `table`; the value is offset by the sum of `1 << table[j]` for
    /// the shorter alternatives when `add` is true.
    pub fn read_var_len_bit_fields(&mut self, table: &[u32; 4], add: bool) -> u32 {
        let idx = if self.read_bit() == 0 {
            0
        } else if self.read_bit() == 0 {
            1
        } else if self.read_bit() == 0 {
            2
        } else {
            3
        };
        let w = table[idx];
        if w == 0 {
            return 0;
        }
        let mut v: u64 = 0;
        if add {
            for j in 0..idx {
                v += 1u64 << table[j];
            }
        }
        v += self.read_uint(w) as u64;
        v.min(u32::MAX as u64) as u32
    }
}

pub fn num_bits(mut v: u32) -> u32 {
    let mut r = 0;
    while v != 0 {
        v >>= 1;
        r += 1;
    }
    r
}

/// Unsigned to signed map of clause 8.3.3 (NegativeRiceMapDecode).
pub fn negative_rice_map_decode(v: u32) -> i32 {
    if v & 1 == 1 {
        -(((v as i64 + 1) / 2) as i32)
    } else {
        (v / 2) as i32
    }
}

pub fn positive_rice_map_decode(v: u32) -> i32 {
    -negative_rice_map_decode(v)
}

pub fn modular(value: i32, low: i32, high: i32) -> i32 {
    let alpha = (high - low + 1).max(1);
    let mut r = value;
    // Closed form instead of the repeated subtraction of the specification.
    if r > high {
        r = low + (r - low).rem_euclid(alpha);
    }
    if r < low {
        r = low + (r - low).rem_euclid(alpha);
    }
    r
}

pub fn decode_center_map(alpha: u32, c: u32, y: u32) -> u32 {
    if alpha == 0 {
        return 0;
    }
    let c = c.min(alpha - 1);
    let y = y.min(alpha - 1);
    if c <= (alpha - 1) / 2 {
        if y > 2 * c {
            y
        } else if y % 2 == 0 {
            c - y / 2
        } else {
            c + (y + 1) / 2
        }
    } else {
        alpha - 1 - decode_center_map(alpha, alpha - 1 - c, y)
    }
}
