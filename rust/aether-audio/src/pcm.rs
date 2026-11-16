//! LPCM decoder: Matroska (`A_PCM/INT/LIT`, `A_PCM/INT/BIG`, `A_PCM/FLOAT/IEEE`), MP4 (`sowt`, `twos`,
//! `in24`, `in32`, `fl32`, `fl64`, `raw `, `ipcm`, `fpcm`, `lpcm`) and Blu-ray HDMV LPCM (the 4 byte
//! header per packet, big endian, channel assignment codes). Output is planar f32 with the
//! canonical labels of [`crate::layout`]; integer samples are divided by 2^(bits-1), which is
//! exact for 8, 16 and 24 bit input (and what FFmpeg's `-f f32le` produces for 32 bit).

use crate::decoder::{AudioDecoder, DecodeError, PcmBlock, Planar};
use crate::layout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcmFormat {
    /// Signed integer, little endian (`sowt`, `A_PCM/INT/LIT`; 8 bit is signed here).
    IntLe,
    /// Signed integer, big endian (`twos`, `in24`, `in32`, `A_PCM/INT/BIG`).
    IntBe,
    FloatLe,
    FloatBe,
    /// Unsigned 8 bit (`raw `, Matroska 8 bit integers).
    UintLe,
    /// Blu-ray HDMV LPCM: `bits`, `channels` and `sample_rate` come from each packet's header.
    BluRay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmParams {
    pub format: PcmFormat,
    pub bits: u8,
    pub channels: u8,
    pub sample_rate: u32,
    /// WAVE channel mask; 0 selects [`layout::default_for_channels`].
    pub layout_mask: u32,
}

/// HDMV LPCM channel assignment code to labels in stream order, `None` for reserved codes.
/// Codes 1..=12 follow the BD-ROM table (the same channel counts as FFmpeg's `pcm-bluray`):
/// 1 mono, 3 stereo, 4 L R C, 5 L R S, 6 L R C S, 7 L R Ls Rs, 8 L R C Ls Rs, 9 5.1, 10 7.0, 11 7.1.
/// (Code 2 is reserved and 12 does not exist; the "3/4+LFE" 7.1 layout is code 11.)
pub fn bluray_layout(code: u8) -> Option<&'static [&'static str]> {
    Some(match code {
        1 => &["FC"],
        3 => &["FL", "FR"],
        4 => &["FL", "FR", "FC"],
        5 => &["FL", "FR", "BC"],
        6 => &["FL", "FR", "FC", "BC"],
        7 => &["FL", "FR", "SL", "SR"],
        8 => &["FL", "FR", "FC", "SL", "SR"],
        9 => &["FL", "FR", "FC", "LFE", "SL", "SR"],
        10 => &["FL", "FR", "FC", "SL", "SR", "BL", "BR"],
        11 => &["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR"],
        _ => return None,
    })
}

/// A parsed 4 byte HDMV LPCM packet header.
#[derive(Debug, PartialEq, Eq)]
pub struct BluRayHeader {
    pub payload_size: usize,
    pub layout: &'static [&'static str],
    pub sample_rate: u32,
    pub bits: u8,
}

pub fn parse_bluray_header(data: &[u8]) -> Result<BluRayHeader, String> {
    if data.len() < 4 {
        return Err("HDMV LPCM packet shorter than its 4 byte header".into());
    }
    let size = u16::from_be_bytes([data[0], data[1]]) as usize;
    let code = data[2] >> 4;
    let layout = bluray_layout(code).ok_or_else(|| format!("HDMV LPCM reserved channel assignment {code}"))?;
    let sample_rate = match data[2] & 0x0f {
        1 => 48_000,
        4 => 96_000,
        5 => 192_000,
        r => return Err(format!("HDMV LPCM reserved sampling frequency {r}")),
    };
    let bits = match data[3] >> 6 {
        1 => 16,
        2 => 20,
        3 => 24,
        b => return Err(format!("HDMV LPCM reserved bits per sample {b}")),
    };
    Ok(BluRayHeader { payload_size: size, layout, sample_rate, bits })
}

pub struct PcmDecoder {
    params: PcmParams,
    labels: Vec<&'static str>,
    /// Bytes of an incomplete trailing frame, completed by the next packet.
    carry: Vec<u8>,
    ready: Option<PcmBlock>,
}

impl PcmDecoder {
    pub fn new(params: PcmParams) -> Result<Self, DecodeError> {
        let err = |m: String| DecodeError::Pcm(m);
        if params.format == PcmFormat::BluRay {
            return Ok(Self { params, labels: vec![], carry: vec![], ready: None });
        }
        if params.sample_rate == 0 {
            return Err(err("sample rate is 0".into()));
        }
        let ok = match params.format {
            PcmFormat::IntLe | PcmFormat::IntBe => matches!(params.bits, 8 | 16 | 24 | 32),
            PcmFormat::FloatLe | PcmFormat::FloatBe => matches!(params.bits, 32 | 64),
            PcmFormat::UintLe => params.bits == 8,
            PcmFormat::BluRay => true,
        };
        if !ok {
            return Err(err(format!("unsupported sample size {} bit for {:?}", params.bits, params.format)));
        }
        let labels = if params.layout_mask != 0 {
            layout::labels_from_mask(params.layout_mask)
        } else {
            layout::default_for_channels(usize::from(params.channels))
                .ok_or_else(|| err(format!("no default layout for {} channels, give a channel mask", params.channels)))?
        };
        if labels.len() != usize::from(params.channels) {
            return Err(err(format!("channel mask names {} channels, stream has {}", labels.len(), params.channels)));
        }
        Ok(Self { params, labels, carry: vec![], ready: None })
    }

    fn convert(&self, data: &[u8], bits: u8, labels: &[&'static str], sample_rate: u32, bluray: bool) -> PcmBlock {
        let channels = labels.len();
        let width = usize::from(bits.div_ceil(8));
        let frames = data.len() / (channels * width);
        let format = self.params.format;
        let mut planes = vec![Vec::with_capacity(frames); channels];
        for f in 0..frames {
            for (c, plane) in planes.iter_mut().enumerate() {
                let at = (f * channels + c) * width;
                let b = &data[at..at + width];
                plane.push(if bluray { bluray_sample(b, bits) } else { sample(b, format, bits) });
            }
        }
        let lossless = matches!(format, PcmFormat::FloatLe | PcmFormat::FloatBe) && bits == 32 || bits <= 24 && !matches!(format, PcmFormat::FloatLe | PcmFormat::FloatBe);
        PcmBlock { sample_rate, frames, labels: labels.iter().map(|s| (*s).to_owned()).collect(), samples: Planar::F32(planes), lossless, joc: false, joc_objects: 0, joc_rendered: false }
    }
}

fn bluray_sample(b: &[u8], bits: u8) -> f32 {
    match bits {
        16 => i16::from_be_bytes([b[0], b[1]]) as f32 / 32_768.0,
        _ => (i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8) as f32 / 8_388_608.0,
    }
}

fn sample(b: &[u8], format: PcmFormat, bits: u8) -> f32 {
    let be = matches!(format, PcmFormat::IntBe | PcmFormat::FloatBe);
    let mut v = [0u8; 8];
    v[..b.len()].copy_from_slice(b);
    if be {
        v[..b.len()].reverse();
    }
    match (format, bits) {
        (PcmFormat::UintLe, _) => (i32::from(v[0]) - 128) as f32 / 128.0,
        (PcmFormat::FloatLe | PcmFormat::FloatBe, 32) => f32::from_le_bytes([v[0], v[1], v[2], v[3]]),
        (PcmFormat::FloatLe | PcmFormat::FloatBe, _) => f64::from_le_bytes(v) as f32,
        (_, 8) => i32::from(v[0] as i8) as f32 / 128.0,
        (_, 16) => i32::from(i16::from_le_bytes([v[0], v[1]])) as f32 / 32_768.0,
        (_, 24) => (i32::from_le_bytes([0, v[0], v[1], v[2]]) >> 8) as f32 / 8_388_608.0,
        _ => i32::from_le_bytes([v[0], v[1], v[2], v[3]]) as f32 / 2_147_483_648.0,
    }
}

impl AudioDecoder for PcmDecoder {
    fn push_packet(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        if self.ready.is_some() {
            return Err(DecodeError::Pcm("pull the previous block before pushing".into()));
        }
        if self.params.format == PcmFormat::BluRay {
            let h = parse_bluray_header(data).map_err(DecodeError::Pcm)?;
            if h.bits == 20 {
                return Err(DecodeError::Pcm("HDMV LPCM 20 bit is not supported".into()));
            }
            let body = &data[4..];
            let body = &body[..body.len().min(h.payload_size)];
            let block = self.convert(body, h.bits, h.layout, h.sample_rate, true);
            if block.frames > 0 {
                self.ready = Some(block);
            }
            return Ok(());
        }
        let width = usize::from(self.params.bits.div_ceil(8));
        let frame_bytes = width * usize::from(self.params.channels);
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(data);
        let whole = buf.len() / frame_bytes * frame_bytes;
        self.carry = buf.split_off(whole);
        if whole > 0 {
            self.ready = Some(self.convert(&buf, self.params.bits, &self.labels, self.params.sample_rate, false));
        }
        Ok(())
    }

    fn pull(&mut self) -> Result<Option<PcmBlock>, DecodeError> {
        Ok(self.ready.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(format: PcmFormat, bits: u8, channels: u8) -> PcmParams {
        PcmParams { format, bits, channels, sample_rate: 48_000, layout_mask: 0 }
    }

    fn one(mut d: PcmDecoder, data: &[u8]) -> PcmBlock {
        d.push_packet(data).unwrap();
        d.pull().unwrap().unwrap()
    }

    #[test]
    fn s16le_is_exact() {
        let d = PcmDecoder::new(params(PcmFormat::IntLe, 16, 2)).unwrap();
        let b = one(d, &[0x00, 0x80, 0xff, 0x7f, 0x00, 0x40, 0x00, 0x00]);
        assert_eq!(b.labels, ["FL", "FR"]);
        assert_eq!(b.frames, 2);
        assert!(b.lossless);
        let Planar::F32(p) = b.samples else { panic!() };
        assert_eq!(p[0], [-1.0, 0.5]);
        assert_eq!(p[1], [32_767.0 / 32_768.0, 0.0]);
    }

    #[test]
    fn s24be_and_s32be() {
        let d = PcmDecoder::new(params(PcmFormat::IntBe, 24, 1)).unwrap();
        let b = one(d, &[0x80, 0x00, 0x00, 0x40, 0x00, 0x00]);
        assert_eq!(b.to_f32(), [[-1.0, 0.5]]);
        let d = PcmDecoder::new(params(PcmFormat::IntBe, 32, 1)).unwrap();
        let b = one(d, &[0x80, 0, 0, 0, 0x40, 0, 0, 0]);
        assert_eq!(b.to_f32(), [[-1.0, 0.5]]);
        assert!(!b.lossless);
    }

    #[test]
    fn float_and_unsigned() {
        let d = PcmDecoder::new(params(PcmFormat::FloatLe, 32, 1)).unwrap();
        let b = one(d, &0.25f32.to_le_bytes());
        assert_eq!(b.to_f32(), [[0.25]]);
        let d = PcmDecoder::new(params(PcmFormat::FloatBe, 64, 1)).unwrap();
        let b = one(d, &0.125f64.to_be_bytes());
        assert_eq!(b.to_f32(), [[0.125]]);
        let d = PcmDecoder::new(params(PcmFormat::UintLe, 8, 1)).unwrap();
        let b = one(d, &[0, 128, 255]);
        assert_eq!(b.to_f32(), [[-1.0, 0.0, 127.0 / 128.0]]);
    }

    #[test]
    fn partial_frames_are_carried() {
        let mut d = PcmDecoder::new(params(PcmFormat::IntLe, 16, 2)).unwrap();
        d.push_packet(&[1, 0, 2]).unwrap();
        assert_eq!(d.pull().unwrap(), None);
        d.push_packet(&[0, 3, 0, 4, 0]).unwrap();
        let b = d.pull().unwrap().unwrap();
        assert_eq!(b.frames, 2);
    }

    #[test]
    fn layouts_from_mask_and_defaults() {
        let d = PcmDecoder::new(params(PcmFormat::IntLe, 16, 8)).unwrap();
        assert_eq!(d.labels, ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]);
        let mut p = params(PcmFormat::IntLe, 16, 6);
        p.layout_mask = 0x60f;
        assert_eq!(PcmDecoder::new(p).unwrap().labels, ["FL", "FR", "FC", "LFE", "SL", "SR"]);
        p.channels = 5;
        assert!(PcmDecoder::new(p).is_err());
        assert!(PcmDecoder::new(params(PcmFormat::IntLe, 16, 12)).is_err());
        assert!(PcmDecoder::new(params(PcmFormat::IntLe, 20, 2)).is_err());
    }

    #[test]
    fn bluray_71_24bit() {
        let mut d = PcmDecoder::new(params(PcmFormat::BluRay, 0, 0)).unwrap();
        let mut pkt = vec![0, 48, 0xb1, 0xc0]; // 48 payload bytes, code 11 (7.1), 48 kHz, 24 bit
        for c in 0..8i32 {
            for f in 0..2i32 {
                let v = (c * 1000 + f) * if f == 1 { -1 } else { 1 };
                pkt.extend(&v.to_be_bytes()[1..]);
            }
        }
        // frames are interleaved, so re-pack frame-major
        let mut pkt2 = pkt[..4].to_vec();
        for f in 0..2usize {
            for c in 0..8usize {
                pkt2.extend(&pkt[4 + (c * 2 + f) * 3..4 + (c * 2 + f) * 3 + 3]);
            }
        }
        d.push_packet(&pkt2).unwrap();
        let b = d.pull().unwrap().unwrap();
        assert_eq!((b.frames, b.sample_rate), (2, 48_000));
        assert_eq!(b.labels, ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR"]);
        let p = b.to_f32();
        assert_eq!(p[7][0], 7000.0 / 8_388_608.0);
        assert_eq!(p[3][1], -3001.0 / 8_388_608.0);
    }

    #[test]
    fn bluray_16bit_stereo_and_errors() {
        let mut d = PcmDecoder::new(params(PcmFormat::BluRay, 0, 0)).unwrap();
        d.push_packet(&[0, 4, 0x31, 0x40, 0x80, 0x00, 0x40, 0x00]).unwrap();
        let b = d.pull().unwrap().unwrap();
        assert_eq!(b.labels, ["FL", "FR"]);
        assert_eq!(b.to_f32(), [[-1.0], [0.5]]);
        assert!(d.push_packet(&[0, 4, 0x21, 0x40, 0, 0, 0, 0]).is_err(), "reserved channel code");
        assert!(d.push_packet(&[0, 4]).is_err());
        assert!(d.push_packet(&[0, 4, 0x33, 0x40, 0, 0, 0, 0]).is_err(), "reserved rate");
        for code in [1u8, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
            assert!(bluray_layout(code).is_some());
        }
        assert!(bluray_layout(12).is_none());
    }
}
