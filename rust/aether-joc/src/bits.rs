//! MSB-first bit reader over a byte slice. Reads past the end return zeros and
//! set the `overrun` flag instead of panicking.

#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
    pub overrun: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, end: data.len() * 8, overrun: false }
    }

    /// Reader limited to `end_bit` total bits (clamped to the data length).
    pub fn with_len(data: &'a [u8], end_bit: usize) -> Self {
        Self { data, pos: 0, end: end_bit.min(data.len() * 8), overrun: false }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn end(&self) -> usize {
        self.end
    }

    pub fn seek(&mut self, bit: usize) {
        self.pos = bit;
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
        if self.pos > self.end {
            self.overrun = true;
        }
    }

    pub fn bit(&mut self) -> u32 {
        if self.pos >= self.end {
            self.overrun = true;
            self.pos += 1;
            return 0;
        }
        let b = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        b as u32
    }

    pub fn flag(&mut self) -> bool {
        self.bit() != 0
    }

    pub fn bits(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }

    /// Two's complement signed value of `n` bits.
    pub fn signed(&mut self, n: usize) -> i32 {
        let v = self.bits(n) as i32;
        if n > 0 && v & (1 << (n - 1)) != 0 {
            v - (1 << n)
        } else {
            v
        }
    }

    /// EMDF `variable_bits(n)` (ETSI TS 102 366 H.2.2).
    pub fn variable_bits(&mut self, n: usize) -> u32 {
        let mut value = 0u32;
        loop {
            value = value.wrapping_add(self.bits(n));
            if self.bit() == 0 || self.overrun {
                break;
            }
            value = value.wrapping_shl(n as u32).wrapping_add(1 << n);
            if value > (1 << 24) {
                break;
            }
        }
        value
    }

    /// OAMD `variable_bits_max(n, max_num_groups)` (ETSI TS 103 420 5.5.1).
    pub fn variable_bits_max(&mut self, n: usize, max_groups: u32) -> u32 {
        let mut value = self.bits(n);
        let mut more = self.flag();
        let mut group = 1u32;
        if max_groups > group {
            if more {
                value = (value << n) + (1 << n);
            }
            while more {
                value += self.bits(n);
                more = self.flag();
                if group >= max_groups {
                    break;
                }
                if more {
                    value = (value << n) + (1 << n);
                    group += 1;
                }
                if self.overrun {
                    break;
                }
            }
        }
        value
    }
}
