//! Native FLAC (`fLaC` + metadata blocks + frames). A frame ends where the next frame header starts,
//! confirmed by the header CRC-8 of that next header, its frame / sample number following on and the CRC-16
//! of the frame itself, so sync words inside the audio data do not cut a frame.

use super::{Fr, FrameRes};
use crate::{ByteSource, Error, read_exact_at};

pub(crate) struct Flac {
    pub rate: u32,
    pub channels: u32,
    pub bps: u32,
    pub min_bs: u32,
    pub max_bs: u32,
    /// Total samples per channel, 0 when unknown.
    pub total: u64,
    pub first_frame: u64,
    /// SEEKTABLE points `(sample, absolute offset)`, ascending, placeholders removed.
    pub seek: Vec<(u64, u64)>,
    info: [u8; 34],
}

impl Flac {
    /// `fLaC`, a STREAMINFO block header flagged last, the STREAMINFO body: the WebCodecs / Matroska description.
    pub fn codec_private(&self) -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend([0x80, 0, 0, 34]);
        v.extend(self.info);
        v
    }
}

pub(super) fn parse(src: &dyn ByteSource, base: u64) -> Result<Option<Flac>, Error> {
    let len = src.len();
    let mut pos = base + 4;
    let mut info = None;
    let mut seek_body: Vec<u8> = Vec::new();
    for _ in 0..4096 {
        if pos + 4 > len {
            return Ok(None);
        }
        let mut h = [0u8; 4];
        read_exact_at(src, pos, &mut h)?;
        let (last, ty) = (h[0] >> 7 == 1, h[0] & 0x7f);
        let size = (u64::from(h[1]) << 16) | (u64::from(h[2]) << 8) | u64::from(h[3]);
        let body = pos + 4;
        if body + size > len {
            return Ok(None);
        }
        match ty {
            0 if size == 34 => {
                let mut b = [0u8; 34];
                read_exact_at(src, body, &mut b)?;
                info = Some(b);
            }
            3 if size <= 1 << 20 => {
                seek_body = vec![0u8; size as usize];
                read_exact_at(src, body, &mut seek_body)?;
            }
            127 => return Ok(None),
            _ => {}
        }
        pos = body + size;
        if last {
            break;
        }
    }
    let Some(b) = info else { return Ok(None) };
    let rate = (u32::from(b[10]) << 12) | (u32::from(b[11]) << 4) | u32::from(b[12] >> 4);
    let channels = u32::from((b[12] >> 1) & 7) + 1;
    let bps = (u32::from(b[12] & 1) << 4 | u32::from(b[13] >> 4)) + 1;
    let total = (u64::from(b[13] & 15) << 32) | u64::from(u32::from_be_bytes(b[14..18].try_into().unwrap()));
    if rate == 0 || pos >= len {
        return Ok(None);
    }
    let mut seek: Vec<(u64, u64)> = seek_body
        .chunks_exact(18)
        .map(|e| (u64::from_be_bytes(e[..8].try_into().unwrap()), u64::from_be_bytes(e[8..16].try_into().unwrap())))
        .filter(|&(s, _)| s != u64::MAX)
        .map(|(s, o)| (s, pos + o))
        .filter(|&(_, o)| o < len)
        .collect();
    seek.sort_unstable();
    seek.dedup_by_key(|e| e.0);
    Ok(Some(Flac {
        rate,
        channels,
        bps,
        min_bs: u32::from(u16::from_be_bytes([b[0], b[1]])),
        max_bs: u32::from(u16::from_be_bytes([b[2], b[3]])),
        total,
        first_frame: pos,
        seek,
        info: b,
    }))
}

const fn crc8_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn crc16_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC8: [u8; 256] = crc8_table();
static CRC16: [u16; 256] = crc16_table();

fn crc8(b: &[u8]) -> u8 {
    b.iter().fold(0, |c, &x| CRC8[usize::from(c ^ x)])
}

fn crc16(b: &[u8]) -> u16 {
    b.iter().fold(0, |c, &x| (c << 8) ^ CRC16[usize::from((c >> 8) as u8 ^ x)])
}

struct Hdr {
    len: usize,
    variable: bool,
    blocksize: u32,
    /// Frame number (fixed block size) or sample number (variable).
    number: u64,
}

fn utf8_number(b: &[u8]) -> Option<(u64, usize)> {
    let f = *b.first()?;
    let n = match f {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        0xf8..=0xfb => 5,
        0xfc..=0xfd => 6,
        0xfe => 7,
        _ => return None,
    };
    let mut v = match n {
        1 => u64::from(f),
        7 => 0,
        _ => u64::from(f & (0x7f >> n)),
    };
    for i in 1..n {
        let c = *b.get(i)?;
        if c & 0xc0 != 0x80 {
            return None;
        }
        v = (v << 6) | u64::from(c & 0x3f);
    }
    Some((v, n))
}

/// Frame header at `b[0]`, checked against the stream and its CRC-8.
fn header(b: &[u8], fl: &Flac) -> Option<Hdr> {
    if b.len() < 6 || b[0] != 0xff || b[1] & 0xfe != 0xf8 || b[3] & 1 != 0 {
        return None;
    }
    let variable = b[1] & 1 == 1;
    let (bs_code, sr_code) = (b[2] >> 4, b[2] & 15);
    let (ch, ss) = (b[3] >> 4, (b[3] >> 1) & 7);
    if bs_code == 0 || sr_code == 15 || ch > 10 || ss == 3 {
        return None;
    }
    let channels = if ch < 8 { u32::from(ch) + 1 } else { 2 };
    if channels != fl.channels {
        return None;
    }
    if ss != 0 && [0, 8, 12, 0, 16, 20, 24, 32][usize::from(ss)] != fl.bps {
        return None;
    }
    let (number, n) = utf8_number(&b[4..])?;
    let mut at = 4 + n;
    let blocksize = match bs_code {
        1 => 192,
        2..=5 => 576 << (bs_code - 2),
        6 => {
            at += 1;
            u32::from(*b.get(at - 1)?) + 1
        }
        7 => {
            at += 2;
            u32::from(u16::from_be_bytes([*b.get(at - 2)?, *b.get(at - 1)?])) + 1
        }
        _ => 256 << (bs_code - 8),
    };
    let rate = match sr_code {
        0 => fl.rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => {
            at += 1;
            u32::from(*b.get(at - 1)?) * 1000
        }
        13 => {
            at += 2;
            u32::from(u16::from_be_bytes([*b.get(at - 2)?, *b.get(at - 1)?]))
        }
        _ => {
            at += 2;
            u32::from(u16::from_be_bytes([*b.get(at - 2)?, *b.get(at - 1)?])) * 10
        }
    };
    if rate != fl.rate {
        return None;
    }
    if *b.get(at)? != crc8(&b[..at]) {
        return None;
    }
    Some(Hdr { len: at + 1, variable, blocksize, number })
}

pub(super) fn frame(fl: &Flac, b: &[u8], eof: bool) -> FrameRes {
    if b.len() < 16 && !eof {
        return FrameRes::More(16);
    }
    let Some(h) = header(b, fl) else { return FrameRes::Bad };
    let expected = if h.variable { h.number + u64::from(h.blocksize) } else { h.number + 1 };
    let pts = if h.variable {
        h.number as i64
    } else {
        let bs = if fl.min_bs == fl.max_bs { fl.max_bs } else { h.blocksize };
        (h.number * u64::from(bs)) as i64
    };
    let mut from = h.len + 2;
    while from + 1 < b.len() {
        let Some(q) = (from..b.len() - 1).find(|&i| b[i] == 0xff && b[i + 1] & 0xfe == 0xf8) else { break };
        from = q + 1;
        if b.len() < q + 16 && !eof {
            return FrameRes::More(q + 16);
        }
        let Some(n) = header(&b[q..], fl) else { continue };
        if n.variable != h.variable || n.number != expected {
            continue;
        }
        if crc16(&b[..q]) == 0 {
            return FrameRes::Frame(Fr { len: q, dur: i64::from(h.blocksize), key: true, pts: Some(pts) });
        }
    }
    if !eof {
        return if b.len() >= super::MAX_FRAME { FrameRes::Bad } else { FrameRes::More(b.len() + 64 * 1024) };
    }
    // Last frame: it runs to the end of the data, minus an ID3v1 tag.
    let mut end = b.len();
    if end >= 128 && &b[end - 128..end - 125] == b"TAG" {
        end -= 128;
    }
    if end > h.len + 2 && crc16(&b[..end]) == 0 {
        FrameRes::Frame(Fr { len: end, dur: i64::from(h.blocksize), key: true, pts: Some(pts) })
    } else {
        FrameRes::Bad
    }
}
