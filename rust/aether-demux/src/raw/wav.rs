//! WAV (RIFF), RF64 / BW64 and Sony Wave64 containers holding PCM. Packets are 4096 sample frames, the
//! size libavformat's WAV demuxer reads, so the list matches `ffprobe -show_packets`.

use super::ScanOut;
use crate::{ByteSource, Error, PacketRef, read_exact_at};

pub(super) const FRAMES: u64 = 4096;

const W64_RIFF: [u8; 16] = [0x72, 0x69, 0x66, 0x66, 0x2e, 0x91, 0xcf, 0x11, 0xa5, 0xd6, 0x28, 0xdb, 0x04, 0xc1, 0x00, 0x00];
const W64_WAVE: [u8; 16] = [0x77, 0x61, 0x76, 0x65, 0xf3, 0xac, 0xd3, 0x11, 0x8c, 0xd1, 0x00, 0xc0, 0x4f, 0x8e, 0xdb, 0x8a];
const W64_FMT: [u8; 16] = [0x66, 0x6d, 0x74, 0x20, 0xf3, 0xac, 0xd3, 0x11, 0x8c, 0xd1, 0x00, 0xc0, 0x4f, 0x8e, 0xdb, 0x8a];
const W64_DATA: [u8; 16] = [0x64, 0x61, 0x74, 0x61, 0xf3, 0xac, 0xd3, 0x11, 0x8c, 0xd1, 0x00, 0xc0, 0x4f, 0x8e, 0xdb, 0x8a];

pub(super) struct Wav {
    /// The `fmt ` chunk body (WAVEFORMATEX or WAVEFORMATEXTENSIBLE).
    pub fmt: Vec<u8>,
    /// Format tag, the sub format for WAVE_FORMAT_EXTENSIBLE.
    pub tag: u16,
    pub channels: u32,
    pub rate: u32,
    pub block_align: u32,
    pub bits: u32,
    pub data_start: u64,
    pub data_end: u64,
}

impl Wav {
    /// Name of the sample format in the `pcm-s24le` spelling, `None` for formats the PCM decoder lacks.
    pub fn codec_string(&self) -> Option<String> {
        if self.bits == 0 || self.bits % 8 != 0 || self.channels == 0 || self.block_align != self.channels * self.bits / 8 {
            return None;
        }
        match (self.tag, self.bits) {
            (1, 8) => Some("pcm-u8le".into()),
            (1, 16 | 24 | 32) => Some(format!("pcm-s{}le", self.bits)),
            (3, 32 | 64) => Some(format!("pcm-f{}le", self.bits)),
            _ => None,
        }
    }

    pub fn scan(&self, data_start: u64, limit: u64, from: u64, stop_at: u64, budget: u64) -> ScanOut {
        let ba = u64::from(self.block_align);
        let end_data = data_start + (self.data_end.min(limit).saturating_sub(data_start) / ba) * ba;
        let mut packets = Vec::new();
        let mut pos = from;
        while pos < end_data && pos < stop_at && pos - from < budget {
            let len = (ba * FRAMES).min(end_data - pos);
            if pos + len > stop_at && stop_at < end_data {
                pos = stop_at;
                break;
            }
            let pts = ((pos - data_start) / ba) as i64;
            packets.push(PacketRef { track: 1, pts, dts: pts, duration: (len / ba) as i64, keyframe: true, presentation: true, offset: pos, size: len as u32, inner: 0 });
            pos += len;
        }
        ScanOut { packets, end: pos, pts_end: ((pos - data_start) / ba) as i64 }
    }
}

fn parse_fmt(body: &[u8]) -> Option<(u16, u32, u32, u32, u32)> {
    if body.len() < 16 {
        return None;
    }
    let u16le = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let mut tag = u16le(0);
    if tag == 0xfffe && body.len() >= 40 {
        tag = u16le(24);
    }
    let channels = u32::from(u16le(2));
    let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
    let block_align = u32::from(u16le(12));
    let bits = u32::from(u16le(14));
    Some((tag, channels, rate, block_align, bits))
}

/// Reads the chunk list of a WAV-family file (`head` holds its first bytes). `None` when it is none.
pub(super) fn parse(src: &dyn ByteSource, head: &[u8]) -> Result<Option<Wav>, Error> {
    let len = src.len();
    let riff = head.len() >= 12 && matches!(&head[..4], b"RIFF" | b"RF64" | b"BW64") && &head[8..12] == b"WAVE";
    let w64 = head.len() >= 40 && head[..16] == W64_RIFF && head[24..40] == W64_WAVE;
    if !riff && !w64 {
        return Ok(None);
    }
    let mut pos: u64 = if riff { 12 } else { 40 };
    let mut ds64_data: Option<u64> = None;
    let mut fmt: Option<Vec<u8>> = None;
    for _ in 0..512 {
        let (id_ok, size, header): (ChunkId, u64, u64) = if riff {
            if pos + 8 > len {
                return Ok(None);
            }
            let mut h = [0u8; 8];
            read_exact_at(src, pos, &mut h)?;
            let id = match &h[..4] {
                b"ds64" => ChunkId::Ds64,
                b"fmt " => ChunkId::Fmt,
                b"data" => ChunkId::Data,
                _ => ChunkId::Other,
            };
            (id, u64::from(u32::from_le_bytes(h[4..8].try_into().unwrap())), 8)
        } else {
            if pos + 24 > len {
                return Ok(None);
            }
            let mut h = [0u8; 24];
            read_exact_at(src, pos, &mut h)?;
            let id = if h[..16] == W64_FMT {
                ChunkId::Fmt
            } else if h[..16] == W64_DATA {
                ChunkId::Data
            } else {
                ChunkId::Other
            };
            (id, u64::from_le_bytes(h[16..24].try_into().unwrap()).saturating_sub(24), 24)
        };
        let body = pos + header;
        match id_ok {
            ChunkId::Ds64 => {
                let mut b = [0u8; 16];
                if size >= 16 && body + 16 <= len {
                    read_exact_at(src, body, &mut b)?;
                    ds64_data = Some(u64::from_le_bytes(b[8..16].try_into().unwrap()));
                }
            }
            ChunkId::Fmt => {
                let n = size.min(256).min(len.saturating_sub(body)) as usize;
                let mut b = vec![0u8; n];
                read_exact_at(src, body, &mut b)?;
                fmt = Some(b);
            }
            ChunkId::Data => {
                let Some(fmt) = fmt else { return Ok(None) };
                let Some((tag, channels, rate, mut block_align, bits)) = parse_fmt(&fmt) else { return Ok(None) };
                if block_align == 0 {
                    block_align = channels * bits / 8;
                }
                if channels == 0 || rate == 0 || block_align == 0 {
                    return Ok(None);
                }
                let mut size = if riff && size == 0xffff_ffff { ds64_data.unwrap_or(0) } else { size };
                let avail = len.saturating_sub(body);
                // A streamed file declares no usable size: the data runs to the end.
                if size == 0 || size > avail {
                    size = avail;
                }
                let size = size / u64::from(block_align) * u64::from(block_align);
                if size == 0 {
                    return Ok(None);
                }
                return Ok(Some(Wav { fmt, tag, channels, rate, block_align, bits, data_start: body, data_end: body + size }));
            }
            ChunkId::Other => {}
        }
        let padded = if riff { size + (size & 1) } else { (size + 24 + 7) / 8 * 8 - 24 };
        pos = match body.checked_add(padded) {
            Some(p) => p,
            None => return Ok(None),
        };
    }
    Ok(None)
}

enum ChunkId {
    Ds64,
    Fmt,
    Data,
    Other,
}
