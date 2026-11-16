//! Small shared helpers: bit reader over RBSP data and a zlib inflater.

/// MSB-first bit reader. Reads past the end return zeros and set `overrun`.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    pub overrun: bool,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, overrun: false }
    }
    pub fn bit(&mut self) -> u32 {
        let byte = self.pos >> 3;
        let v = match self.data.get(byte) {
            Some(b) => u32::from(b >> (7 - (self.pos & 7)) & 1),
            None => {
                self.overrun = true;
                0
            }
        };
        self.pos += 1;
        v
    }
    pub fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
        if self.pos > self.data.len() * 8 {
            self.overrun = true;
        }
    }
    pub fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            zeros += 1;
            if zeros > 32 || self.overrun {
                return 0;
            }
        }
        if zeros == 0 { 0 } else { ((1u64 << zeros) - 1 + u64::from(self.bits(zeros))) as u32 }
    }
    pub fn se(&mut self) -> i32 {
        let k = self.ue();
        if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) }
    }
}

/// Removes emulation prevention bytes (00 00 03 -> 00 00).
pub(crate) fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Decodes a zlib stream (RFC 1950/1951). The Adler-32 trailer is not verified.
/// `limit` bounds the output so hostile input cannot exhaust memory.
pub(crate) fn zlib_inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    if input.len() < 2 || input[0] & 0x0f != 8 {
        return Err("not a zlib stream");
    }
    let skip = if input[1] & 0x20 != 0 { 6 } else { 2 };
    inflate(input.get(skip..).ok_or("short zlib stream")?, limit)
}

struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offs[usize::from(l)])] = s as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Self { counts, symbols }
    }
}

struct Inflater<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl Inflater<'_> {
    fn bits(&mut self, n: u32) -> Result<u32, &'static str> {
        while self.bitcnt < n {
            let b = *self.data.get(self.pos).ok_or("truncated deflate stream")?;
            self.pos += 1;
            self.bitbuf |= u32::from(b) << self.bitcnt;
            self.bitcnt += 8;
        }
        let v = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(v)
    }
    fn decode(&mut self, h: &Huff) -> Result<u16, &'static str> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= self.bits(1)? as i32;
            let count = i32::from(h.counts[len]);
            if code - count < first {
                return Ok(h.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("bad huffman code")
    }
}

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Raw DEFLATE decoder.
pub(crate) fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut st = Inflater { data, pos: 0, bitbuf: 0, bitcnt: 0 };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = st.bits(1)?;
        match st.bits(2)? {
            0 => {
                st.bitbuf = 0;
                st.bitcnt = 0;
                let h = st.data.get(st.pos..st.pos + 4).ok_or("truncated stored block")?;
                let len = usize::from(u16::from_le_bytes([h[0], h[1]]));
                st.pos += 4;
                let body = st.data.get(st.pos..st.pos + len).ok_or("truncated stored block")?;
                if out.len() + len > limit {
                    return Err("inflated size exceeds limit");
                }
                out.extend_from_slice(body);
                st.pos += len;
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Huff::new(&l), Huff::new(&[5u8; 30]))
                } else {
                    let nlen = st.bits(5)? as usize + 257;
                    let ndist = st.bits(5)? as usize + 1;
                    let ncode = st.bits(4)? as usize + 4;
                    if nlen > 286 || ndist > 30 {
                        return Err("bad dynamic header");
                    }
                    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &o in ORDER.iter().take(ncode) {
                        cl[o] = st.bits(3)? as u8;
                    }
                    let ch = Huff::new(&cl);
                    let mut lengths = vec![0u8; nlen + ndist];
                    let mut i = 0;
                    while i < nlen + ndist {
                        let sym = st.decode(&ch)?;
                        if sym < 16 {
                            lengths[i] = sym as u8;
                            i += 1;
                        } else {
                            let (val, rep) = match sym {
                                16 => (*lengths.get(i.wrapping_sub(1)).ok_or("bad repeat")?, 3 + st.bits(2)?),
                                17 => (0, 3 + st.bits(3)?),
                                _ => (0, 11 + st.bits(7)?),
                            };
                            if i + rep as usize > nlen + ndist {
                                return Err("bad repeat");
                            }
                            for _ in 0..rep {
                                lengths[i] = val;
                                i += 1;
                            }
                        }
                    }
                    (Huff::new(&lengths[..nlen]), Huff::new(&lengths[nlen..]))
                };
                loop {
                    let sym = st.decode(&lit)?;
                    if sym < 256 {
                        if out.len() >= limit {
                            return Err("inflated size exceeds limit");
                        }
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let s = usize::from(sym - 257);
                        if s >= 29 {
                            return Err("bad length symbol");
                        }
                        let len = usize::from(LBASE[s]) + st.bits(u32::from(LEXT[s]))? as usize;
                        let d = usize::from(st.decode(&dist)?);
                        if d >= 30 {
                            return Err("bad distance symbol");
                        }
                        let dist_v = usize::from(DBASE[d]) + st.bits(u32::from(DEXT[d]))? as usize;
                        if dist_v > out.len() {
                            return Err("distance too far back");
                        }
                        if out.len() + len > limit {
                            return Err("inflated size exceeds limit");
                        }
                        for _ in 0..len {
                            out.push(out[out.len() - dist_v]);
                        }
                    }
                }
            }
            _ => return Err("bad deflate block type"),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflates_stored_and_fixed_blocks() {
        // zlib.compress(b"hello hello hello hello") at level 9.
        let z = [0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03, 0x08, 0xb1];
        assert_eq!(zlib_inflate(&z, 1000).unwrap(), b"hello hello hello hello");
        let stored = [0x78, 0x01, 0x01, 0x03, 0x00, 0xfc, 0xff, b'a', b'b', b'c'];
        assert_eq!(zlib_inflate(&stored, 10).unwrap(), b"abc");
        assert!(zlib_inflate(&z, 5).is_err());
    }

    #[test]
    fn exp_golomb() {
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!((b.ue(), b.ue(), b.ue(), b.se()), (0, 1, 2, 2));
    }
}
