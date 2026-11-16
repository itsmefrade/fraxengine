//! Access unit and audio frame splitting for elementary streams carried in PES payloads.
//! The cuts follow libavcodec's parsers so that packet boundaries match `ffprobe -show_packets`.

use crate::util::Bits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    H264,
    Hevc,
    Vc1,
    Mpeg2,
    Ac3,
    TrueHd,
    Dts,
    Aac,
    /// One PES is one packet (PGS, HDMV LPCM, text subtitles).
    Plain,
}

impl Kind {
    pub fn per_pes(self) -> bool {
        self == Kind::Plain
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Frame {
    /// Bytes dropped before the frame.
    pub skip: usize,
    pub len: usize,
    pub key: bool,
    /// Duration in 90 kHz ticks when the header states it (audio).
    pub ticks: Option<i64>,
}

#[derive(Default)]
pub(super) struct Framer {
    kind: Option<Kind>,
    scan: usize,
    nals: u32,
    seen_slice: bool,
    key: bool,
    seen_frame: bool,
    recovery0: bool,
    first_start: Option<usize>,
    /// MPEG-2: picture_structure of the last picture (3 = frame) and whether a first field is open.
    open_field: bool,
    pic_ext_pending: bool,
    /// TrueHD: sample rate and samples per access unit from the last major sync.
    pub thd: Option<(u32, u32)>,
}

pub(super) fn ticks(samples: u32, rate: u32) -> Option<i64> {
    (rate > 0).then(|| (i64::from(samples) * 90_000) / i64::from(rate))
}

impl Framer {
    pub fn new(kind: Kind) -> Self {
        Self { kind: Some(kind), ..Default::default() }
    }

    fn reset_au(&mut self) {
        self.scan = 0;
        self.nals = 0;
        self.seen_slice = false;
        self.key = false;
        self.seen_frame = false;
        self.recovery0 = false;
        self.first_start = None;
        self.open_field = false;
        self.pic_ext_pending = false;
    }

    pub fn next(&mut self, buf: &[u8], flush: bool) -> Option<Frame> {
        let kind = self.kind?;
        match kind {
            Kind::H264 | Kind::Hevc | Kind::Vc1 | Kind::Mpeg2 => self.next_video(kind, buf, flush),
            Kind::Ac3 => ac3_next(buf, flush),
            Kind::TrueHd => self.truehd_next(buf, flush),
            Kind::Dts => dts_next(buf, flush),
            Kind::Aac => aac_next(buf, flush),
            Kind::Plain => (!buf.is_empty()).then(|| Frame { skip: 0, len: buf.len(), key: true, ticks: None }),
        }
    }

    fn next_video(&mut self, kind: Kind, buf: &[u8], flush: bool) -> Option<Frame> {
        loop {
            // Next start code at or after `scan`.
            let from = self.scan;
            let Some(p) = find_start(buf, from) else {
                self.scan = buf.len().saturating_sub(2).max(from);
                break;
            };
            // Header byte(s) needed to classify.
            let need = match kind {
                Kind::H264 => 5,
                Kind::Hevc => 6,
                _ => 5,
            };
            if buf.len() < p + need && !flush {
                self.scan = p;
                return None;
            }
            let nal = &buf[(p + 3).min(buf.len())..];
            if nal.is_empty() {
                break;
            }
            let first = self.first_start.is_none();
            let boundary = self.classify(kind, nal, first);
            if first {
                self.first_start = Some(p);
            } else if boundary {
                let start = self.first_start.unwrap();
                // H.264/HEVC count the zero_byte of a four byte start code with the unit it introduces.
                let zero_byte = matches!(kind, Kind::H264 | Kind::Hevc);
                let end = if zero_byte && buf[p - 1] == 0 && p > start { p - 1 } else { p };
                let lead = if zero_byte && start > 0 && buf[start - 1] == 0 { start - 1 } else { start };
                let frame = Frame { skip: lead, len: end - lead, key: self.key, ticks: None };
                self.reset_au();
                return Some(frame);
            }
            self.nals += 1;
            self.scan = p + 3;
        }
        if flush && self.first_start.is_some() {
            let start = self.first_start.unwrap();
            let zero_byte = matches!(kind, Kind::H264 | Kind::Hevc);
            let lead = if zero_byte && start > 0 && buf[start - 1] == 0 { start - 1 } else { start };
            let frame = Frame { skip: lead, len: buf.len() - lead, key: self.key, ticks: None };
            self.reset_au();
            return Some(frame);
        }
        None
    }

    /// Updates AU state for the NAL / start code at `nal` and says whether it starts a new access unit.
    fn classify(&mut self, kind: Kind, nal: &[u8], first: bool) -> bool {
        match kind {
            Kind::H264 => {
                let t = nal[0] & 31;
                let mut boundary = false;
                match t {
                    9 => boundary = !first,
                    6..=8 | 14..=18 => boundary = self.seen_slice,
                    1 | 5 => {
                        let first_mb_zero = nal.get(1).is_some_and(|b| b & 0x80 != 0);
                        boundary = self.seen_slice && first_mb_zero;
                    }
                    _ => {}
                }
                if boundary {
                    return true;
                }
                match t {
                    5 => {
                        self.seen_slice = true;
                        self.key = true;
                    }
                    1 | 2 => {
                        if !self.seen_slice && self.recovery0 {
                            // Open GOP: an I slice right after a recovery point SEI with count zero starts a clean decode.
                            let rbsp = crate::util::unescape(&nal[1..nal.len().min(16)]);
                            let mut r = Bits::new(&rbsp);
                            let _first_mb = r.ue();
                            if matches!(r.ue() % 5, 2 | 4) {
                                self.key = true;
                            }
                        }
                        self.seen_slice = true;
                    }
                    6 => {
                        if sei_recovery_count(&nal[1..]) == Some(0) {
                            self.recovery0 = true;
                        }
                    }
                    _ => {}
                }
                false
            }
            Kind::Hevc => {
                let t = (nal[0] >> 1) & 63;
                let boundary = match t {
                    35 => !first,
                    32..=34 | 39 | 41..=44 | 48..=55 => self.seen_slice,
                    0..=31 => self.seen_slice && nal.get(2).is_some_and(|b| b & 0x80 != 0),
                    _ => false,
                };
                if boundary {
                    return true;
                }
                if t < 32 {
                    self.seen_slice = true;
                    if (16..=23).contains(&t) {
                        self.key = true;
                    }
                }
                false
            }
            Kind::Vc1 => {
                let t = nal[0];
                match t {
                    0x0f | 0x0e => {
                        if self.seen_frame {
                            return true;
                        }
                        self.key = true;
                    }
                    0x0d => {
                        if self.seen_frame {
                            return true;
                        }
                        self.seen_frame = true;
                    }
                    _ => {}
                }
                false
            }
            Kind::Mpeg2 => {
                let t = nal[0];
                match t {
                    0xb3 | 0xb8 => {
                        if self.seen_frame && !self.open_field {
                            return true;
                        }
                        false
                    }
                    0x00 => {
                        if self.seen_frame && !self.open_field {
                            return true;
                        }
                        // picture_coding_type: 3 bits after the 10 bit temporal reference.
                        let pct = (u32::from(*nal.get(2).unwrap_or(&0)) >> 3) & 7;
                        if pct == 1 {
                            self.key = true;
                        }
                        self.seen_frame = true;
                        self.pic_ext_pending = true;
                        false
                    }
                    0xb5 => {
                        // Picture coding extension (id 8) carries picture_structure.
                        if nal.get(1).is_some_and(|b| b >> 4 == 8) && self.pic_ext_pending {
                            let structure = nal.get(3).map_or(3, |b| b & 3);
                            self.pic_ext_pending = false;
                            self.open_field = matches!(structure, 1 | 2) && !self.open_field;
                        }
                        false
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn truehd_next(&mut self, buf: &[u8], flush: bool) -> Option<Frame> {
        let mut i = 0;
        while i + 8 <= buf.len() {
            let len = ((usize::from(buf[i] & 0x0f) << 8) | usize::from(buf[i + 1])) * 2;
            if len >= 8 {
                if i + len > buf.len() {
                    if flush {
                        return None;
                    }
                    return None;
                }
                let major = buf[i + 4..i + 8] == [0xf8, 0x72, 0x6f, 0xba];
                if major && i + 9 <= buf.len() {
                    let ratebits = u32::from(buf[i + 8] >> 4);
                    let rate = if ratebits == 0xf { 0 } else { (if ratebits & 8 != 0 { 44_100 } else { 48_000 }) << (ratebits & 7) };
                    self.thd = Some((rate, 40u32 << (ratebits & 7)));
                }
                let t = self.thd.and_then(|(rate, samples)| ticks(samples, rate));
                return Some(Frame { skip: i, len, key: major, ticks: t });
            }
            i += 1;
        }
        None
    }
}

fn find_start(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i + 2] > 1 {
            i += 3;
        } else if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// `recovery_frame_cnt` of the first recovery point SEI message (payload type 6).
pub(super) fn sei_recovery_count(sei: &[u8]) -> Option<u32> {
    let mut i = 0;
    while i + 2 < sei.len() {
        let mut ty = 0usize;
        while *sei.get(i)? == 0xff {
            ty += 255;
            i += 1;
        }
        ty += usize::from(*sei.get(i)?);
        i += 1;
        let mut size = 0usize;
        while *sei.get(i)? == 0xff {
            size += 255;
            i += 1;
        }
        size += usize::from(*sei.get(i)?);
        i += 1;
        if ty == 6 {
            let rbsp = crate::util::unescape(sei.get(i..(i + size.max(1) + 2).min(sei.len()))?);
            return Some(Bits::new(&rbsp).ue());
        }
        i += size;
    }
    None
}

// ---------------------------------------------------------------- AC-3 / E-AC-3

const AC3_SIZES: [[u16; 38]; 3] = [
    [
        64, 64, 80, 80, 96, 96, 112, 112, 128, 128, 160, 160, 192, 192, 224, 224, 256, 256, 320, 320, 384, 384, 448, 448, 512, 512, 640, 640, 768, 768, 896,
        896, 1024, 1024, 1152, 1152, 1280, 1280,
    ],
    [
        69, 70, 87, 88, 104, 105, 121, 122, 139, 140, 174, 175, 208, 209, 243, 244, 278, 279, 348, 349, 417, 418, 487, 488, 557, 558, 696, 697, 835, 836,
        975, 976, 1114, 1115, 1253, 1254, 1393, 1394,
    ],
    [
        96, 96, 120, 120, 144, 144, 168, 168, 192, 192, 240, 240, 288, 288, 336, 336, 384, 384, 480, 480, 576, 576, 672, 672, 768, 768, 960, 960, 1152,
        1152, 1344, 1344, 1536, 1536, 1728, 1728, 1920, 1920,
    ],
];

#[derive(Clone, Copy, Debug)]
pub(crate) struct Ac3Header {
    pub eac3: bool,
    /// E-AC-3 stream type: 0 independent, 1 dependent, 2 AC-3 convert.
    pub strmtyp: u8,
    pub size: usize,
    pub samples: u32,
    pub rate: u32,
    pub channels: u32,
    /// Dependent substream channel map (bit set), when present.
    pub chanmap: Option<u16>,
}

pub(crate) fn ac3_header(b: &[u8]) -> Option<Ac3Header> {
    if b.len() < 8 || b[0] != 0x0b || b[1] != 0x77 {
        return None;
    }
    let bsid = b[5] >> 3;
    const CH: [u32; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
    if bsid <= 10 {
        let fscod = usize::from(b[4] >> 6);
        let frmsizecod = usize::from(b[4] & 0x3f);
        if fscod == 3 || frmsizecod > 37 {
            return None;
        }
        let acmod = b[6] >> 5;
        let mut r = Bits::new(&b[6..]);
        r.skip(3);
        if acmod & 1 != 0 && acmod != 1 {
            r.skip(2);
        }
        if acmod & 4 != 0 {
            r.skip(2);
        }
        if acmod == 2 {
            r.skip(2);
        }
        let lfe = r.bit() == 1;
        let rate = [48_000, 44_100, 32_000][fscod];
        Some(Ac3Header {
            eac3: false,
            strmtyp: 0,
            size: usize::from(AC3_SIZES[fscod][frmsizecod]) * 2,
            samples: 1536,
            rate,
            channels: CH[usize::from(acmod)] + u32::from(lfe),
            chanmap: None,
        })
    } else if bsid <= 16 {
        let mut r = Bits::new(&b[2..]);
        let strmtyp = r.bits(2) as u8;
        let _substream = r.bits(3);
        let frmsiz = r.bits(11) as usize;
        let fscod = r.bits(2) as usize;
        let (rate, blocks) = if fscod == 3 {
            let f2 = r.bits(2) as usize;
            if f2 == 3 {
                return None;
            }
            ([24_000, 22_050, 16_000][f2], 6)
        } else {
            ([48_000, 44_100, 32_000][fscod], [1, 2, 3, 6][r.bits(2) as usize])
        };
        let acmod = r.bits(3) as u8;
        let lfe = r.bit() == 1;
        let _bsid = r.bits(5);
        let mut chanmap = None;
        if strmtyp == 1 {
            r.skip(5); // dialnorm
            if r.bit() == 1 {
                r.skip(8);
            }
            if acmod == 0 {
                r.skip(5);
                if r.bit() == 1 {
                    r.skip(8);
                }
            }
            if r.bit() == 1 {
                chanmap = Some(r.bits(16) as u16);
            }
        }
        Some(Ac3Header {
            eac3: true,
            strmtyp,
            size: (frmsiz + 1) * 2,
            samples: 256 * blocks,
            rate,
            channels: CH[usize::from(acmod)] + u32::from(lfe),
            chanmap,
        })
    } else {
        None
    }
}

fn ac3_next(buf: &[u8], flush: bool) -> Option<Frame> {
    let mut i = 0;
    let first = loop {
        if i + 8 > buf.len() {
            return None;
        }
        if let Some(h) = ac3_header(&buf[i..]) {
            break h;
        }
        i += 1;
    };
    if i + first.size > buf.len() {
        return None;
    }
    let mut end = i + first.size;
    // Dependent E-AC-3 substreams belong to the access unit of the independent frame before them.
    if first.eac3 && first.strmtyp != 1 || !first.eac3 {
        loop {
            if end + 8 > buf.len() {
                if !flush {
                    return None;
                }
                break;
            }
            match ac3_header(&buf[end..]) {
                Some(h) if h.eac3 && h.strmtyp == 1 => {
                    if end + h.size > buf.len() {
                        return None;
                    }
                    end += h.size;
                }
                _ => break,
            }
        }
    }
    Some(Frame { skip: i, len: end - i, key: true, ticks: ticks(first.samples, first.rate) })
}

// ---------------------------------------------------------------- DTS

const DTS_RATES: [u32; 16] = [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0];
const DTS_CH: [u32; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];

#[derive(Clone, Copy, Debug)]
pub(crate) struct DtsCore {
    pub size: usize,
    pub samples: u32,
    pub rate: u32,
    pub channels: u32,
}

pub(crate) fn dts_core(b: &[u8]) -> Option<DtsCore> {
    if b.len() < 12 || b[..4] != [0x7f, 0xfe, 0x80, 0x01] {
        return None;
    }
    let mut r = Bits::new(&b[4..]);
    r.skip(1 + 5 + 1);
    let nblks = r.bits(7);
    let fsize = r.bits(14) as usize;
    let amode = r.bits(6) as usize;
    let sfreq = r.bits(4) as usize;
    r.skip(5 + 1 + 1 + 1 + 1 + 1 + 3 + 1 + 1);
    let lff = r.bits(2);
    if fsize < 95 || nblks < 5 {
        return None;
    }
    Some(DtsCore {
        size: fsize + 1,
        samples: (nblks + 1) * 32,
        rate: DTS_RATES[sfreq],
        channels: DTS_CH.get(amode).copied().unwrap_or(0) + u32::from(lff != 0),
    })
}

pub(crate) const EXSS_SYNC: [u8; 4] = [0x64, 0x58, 0x20, 0x25];

/// Size of one extension substream starting at `b`, when its header is complete.
pub(crate) fn exss_size(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || b[..4] != EXSS_SYNC {
        return None;
    }
    let mut r = Bits::new(&b[4..]);
    r.skip(8 + 2);
    let wide = r.bit() == 1;
    let (hb, fb) = if wide { (12, 20) } else { (8, 16) };
    let _header = r.bits(hb);
    let size = r.bits(fb) as usize + 1;
    (!r.overrun && size >= 9).then_some(size)
}

fn dts_next(buf: &[u8], flush: bool) -> Option<Frame> {
    let mut i = 0;
    loop {
        if i + 12 > buf.len() {
            return None;
        }
        if buf[i..i + 4] == [0x7f, 0xfe, 0x80, 0x01] || buf[i..i + 4] == EXSS_SYNC {
            break;
        }
        i += 1;
    }
    if buf[i..i + 4] == EXSS_SYNC {
        let size = exss_size(&buf[i..])?;
        if i + size > buf.len() {
            return None;
        }
        return Some(Frame { skip: i, len: size, key: true, ticks: None });
    }
    let core = dts_core(&buf[i..])?;
    let mut end = i + core.size;
    if end > buf.len() {
        return None;
    }
    loop {
        if end + 10 > buf.len() {
            if !flush && end + 4 > buf.len() {
                return None;
            }
            if end + 4 > buf.len() {
                break;
            }
        }
        if buf.len() >= end + 4 && buf[end..end + 4] == EXSS_SYNC {
            let Some(size) = exss_size(&buf[end..]) else {
                if flush {
                    break;
                }
                return None;
            };
            if end + size > buf.len() {
                return None;
            }
            end += size;
        } else {
            break;
        }
    }
    Some(Frame { skip: i, len: end - i, key: true, ticks: ticks(core.samples, core.rate) })
}

// ---------------------------------------------------------------- AAC ADTS

pub(crate) fn adts_header(b: &[u8]) -> Option<(usize, u32, u32, u32)> {
    if b.len() < 7 || b[0] != 0xff || b[1] & 0xf6 != 0xf0 {
        return None;
    }
    let profile = u32::from(b[2] >> 6) + 1;
    let sf = usize::from((b[2] >> 2) & 15);
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let rate = *RATES.get(sf)?;
    let ch = (u32::from(b[2] & 1) << 2) | u32::from(b[3] >> 6);
    let len = (usize::from(b[3] & 3) << 11) | (usize::from(b[4]) << 3) | usize::from(b[5] >> 5);
    let blocks = u32::from(b[6] & 3) + 1;
    (len >= 7).then_some((len, rate, ch, profile * 1000 + blocks))
}

fn aac_next(buf: &[u8], _flush: bool) -> Option<Frame> {
    let mut i = 0;
    while i + 7 <= buf.len() {
        if let Some((len, rate, _ch, pb)) = adts_header(&buf[i..]) {
            if i + len > buf.len() {
                return None;
            }
            return Some(Frame { skip: i, len, key: true, ticks: ticks(1024 * (pb % 1000), rate) });
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(t: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, t];
        v.extend(body);
        v
    }

    #[test]
    fn splits_h264_on_aud_and_first_mb() {
        let mut es = Vec::new();
        es.extend(nal(0x09, &[0x10]));
        es.extend(nal(0x65, &[0x88, 1, 2, 3]));
        es.extend(nal(0x09, &[0x30]));
        es.extend(nal(0x41, &[0x9a, 4, 5]));
        let mut f = Framer::new(Kind::H264);
        let a = f.next(&es, false).unwrap();
        assert_eq!((a.skip, a.len, a.key), (0, 15, true));
        let rest = &es[15..];
        assert!(f.next(rest, false).is_none());
        let b = f.next(rest, true).unwrap();
        assert_eq!((b.len, b.key), (rest.len(), false));
    }

    #[test]
    fn groups_dependent_eac3_substreams_with_the_independent_frame() {
        // Independent AC-3 frame of 128 bytes (fscod 0, frmsizecod 4 = 80 words is 160 bytes, use 96 words = 192).
        let mut ac3 = vec![0u8; 192];
        ac3[..8].copy_from_slice(&[0x0b, 0x77, 0, 0, 0x04, 0x30, 0xe1, 0xff]);
        let mut dep = vec![0u8; 64];
        // strmtyp 1, substreamid 0, frmsiz 31 (64 bytes), fscod 0, numblkscod 3, acmod 7, lfe 1, bsid 16
        dep[..2].copy_from_slice(&[0x0b, 0x77]);
        dep[2] = 0b0100_0000;
        dep[3] = 31;
        dep[4] = 0b0011_1111;
        dep[5] = 0b1000_0000;
        let mut es = ac3.clone();
        es.extend(&dep);
        es.extend(&ac3);
        let mut f = Framer::new(Kind::Ac3);
        let a = f.next(&es, false).unwrap();
        assert_eq!(a.len, 192 + 64);
        assert_eq!(a.ticks, Some(2880));
    }
}
