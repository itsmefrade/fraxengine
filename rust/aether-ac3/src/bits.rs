//! MSB-first bit reader with zero padding past the end of the data.

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    /// `data` should carry at least 8 bytes of zero padding after the `bits`
    /// valid bits so the fast path never leaves the slice.
    pub fn new(data: &'a [u8], bits: usize) -> Self {
        BitReader {
            data,
            pos: 0,
            end: bits,
        }
    }

    #[inline(always)]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// True when more bits were consumed than the frame holds.
    #[inline(always)]
    pub fn overread(&self) -> bool {
        self.pos > self.end
    }

    #[inline(always)]
    fn window(&self) -> u64 {
        let idx = self.pos >> 3;
        if idx + 8 <= self.data.len() {
            u64::from_be_bytes([
                self.data[idx],
                self.data[idx + 1],
                self.data[idx + 2],
                self.data[idx + 3],
                self.data[idx + 4],
                self.data[idx + 5],
                self.data[idx + 6],
                self.data[idx + 7],
            ])
        } else {
            let mut w = 0u64;
            for i in 0..8 {
                w = (w << 8) | self.data.get(idx + i).copied().unwrap_or(0) as u64;
            }
            w
        }
    }

    /// Reads `n` (0..=32) bits.
    #[inline(always)]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let w = self.window() << (self.pos & 7);
        self.pos += n as usize;
        (w >> (64 - n)) as u32
    }

    #[inline(always)]
    pub fn get_bit(&mut self) -> u32 {
        self.get_bits(1)
    }

    /// Reads `n` (0..=32) bits as a sign-extended value.
    #[inline(always)]
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get_bits(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    #[inline(always)]
    pub fn peek_bits(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let w = self.window() << (self.pos & 7);
        (w >> (64 - n)) as u32
    }

    #[inline(always)]
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}
