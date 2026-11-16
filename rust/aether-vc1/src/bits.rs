//! MSB-first bit reader with FFmpeg `GetBitContext` semantics: reads past the end return zeros
//! and `bits_left` goes negative, so malformed input can never panic.

#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // bit position
    size: usize, // bits
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, size: data.len() * 8 }
    }
    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }
    #[inline]
    pub fn bits_left(&self) -> isize {
        self.size as isize - self.pos as isize
    }
    /// Peek up to 32 bits (zero padded past the end).
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let sh = (self.pos & 7) as u32;
        let v: u64 = if byte + 8 <= self.data.len() {
            u64::from_be_bytes([
                self.data[byte],
                self.data[byte + 1],
                self.data[byte + 2],
                self.data[byte + 3],
                self.data[byte + 4],
                self.data[byte + 5],
                self.data[byte + 6],
                self.data[byte + 7],
            ])
        } else {
            let mut v = 0u64;
            for i in 0..8 {
                v = (v << 8) | self.data.get(byte + i).copied().unwrap_or(0) as u64;
            }
            v
        };
        ((v << sh) >> (64 - n)) as u32
    }
    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }
    #[inline]
    pub fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }
    #[inline]
    pub fn get1(&mut self) -> u32 {
        let byte = self.pos >> 3;
        let b = self.data.get(byte).copied().unwrap_or(0);
        let v = (b >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        v as u32
    }
    #[inline]
    pub fn flag(&mut self) -> bool {
        self.get1() != 0
    }
    /// FFmpeg `get_unary(gb, stop, len)`: count of `!stop` bits before a `stop` bit, up to `len`.
    pub fn unary(&mut self, stop: u32, len: u32) -> u32 {
        let mut i = 0;
        while i < len && self.get1() != stop {
            i += 1;
        }
        i
    }
    /// 0, 10, 11 -> 0, 1, 2
    pub fn decode012(&mut self) -> u32 {
        if self.get1() == 0 {
            0
        } else {
            1 + self.get1()
        }
    }
    /// 1, 01, 00 -> 0, 1, 2
    pub fn decode210(&mut self) -> u32 {
        if self.get1() == 1 {
            0
        } else {
            2 - self.get1()
        }
    }
    #[inline]
    pub fn get_vlc(&mut self, v: &Vlc) -> i32 {
        v.decode(self)
    }
    pub fn align_byte(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

#[derive(Clone, Copy, Default)]
struct Entry {
    sym: u32,
    len: i8,     // >0 leaf with `len` remaining bits; 0 invalid; <0 sub table
    sub_bits: u8,
}

/// Multi level VLC table. Symbols are the indices of the code list passed to `build`.
pub struct Vlc {
    bits: u32,
    t: Vec<Entry>,
}

impl Vlc {
    /// `codes[i]` is (code, length) for symbol `i`; entries with length 0 are absent.
    pub fn build(bits: u32, codes: &[(u32, u8)]) -> Vlc {
        let list: Vec<(u32, u8, u32)> = codes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.1 > 0)
            .map(|(i, c)| (c.0, c.1, i as u32))
            .collect();
        let mut t = vec![Entry::default(); 1 << bits];
        Self::fill(&mut t, 0, bits, &list, 0);
        Vlc { bits, t }
    }

    // `list` holds (code, total_len, sym); `consumed` bits are already matched by the path.
    fn fill(t: &mut Vec<Entry>, base: usize, nbits: u32, list: &[(u32, u8, u32)], consumed: u32) {
        // group long codes by their next `nbits` bits
        let mut groups: std::collections::BTreeMap<u32, Vec<(u32, u8, u32)>> = Default::default();
        for &(code, len, sym) in list {
            let rem = len as u32 - consumed;
            if rem <= nbits {
                let low = code & ((1u32 << rem) - 1);
                let first = (low << (nbits - rem)) as usize;
                let cnt = 1usize << (nbits - rem);
                for j in 0..cnt {
                    t[base + first + j] = Entry { sym, len: rem as i8, sub_bits: 0 };
                }
            } else {
                let top = (code >> (rem - nbits)) & ((1u32 << nbits) - 1);
                groups.entry(top).or_default().push((code, len, sym));
            }
        }
        for (top, g) in groups {
            let maxrem = g.iter().map(|c| c.1 as u32 - consumed - nbits).max().unwrap();
            let sub = maxrem.min(8);
            let idx = t.len();
            t.resize(idx + (1 << sub), Entry::default());
            t[base + top as usize] = Entry { sym: idx as u32, len: -(nbits as i8), sub_bits: sub as u8 };
            Self::fill(t, idx, sub, &g, consumed + nbits);
        }
    }

    #[inline]
    pub fn decode(&self, br: &mut BitReader) -> i32 {
        let avail = br.peek(32);
        let mut sh = 32 - self.bits;
        let mut e = self.t[(avail >> sh) as usize];
        let mut used = 0u32;
        let mut nb = self.bits;
        while e.len < 0 {
            used += nb;
            nb = e.sub_bits as u32;
            sh -= nb;
            let idx = ((avail >> sh) & ((1u32 << nb) - 1)) as usize;
            e = self.t[e.sym as usize + idx];
        }
        if e.len == 0 {
            // invalid code: consume nothing
            return -1;
        }
        br.skip(used as usize + e.len as usize);
        e.sym as i32
    }
}
