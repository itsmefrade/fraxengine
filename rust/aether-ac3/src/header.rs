//! Synchronisation information and frame splitting (FFmpeg `ac3_parser.c`).

use crate::bits::BitReader;
use crate::error::Error;
use crate::tables::*;

const AC3_HEADER_SIZE: usize = 7;

/// Parsed `syncinfo()` plus the leading `bsi()` fields that determine the
/// frame geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameInfo {
    pub bitstream_id: u8,
    pub bitstream_mode: u8,
    /// Audio coding mode (`acmod`).
    pub channel_mode: u8,
    pub lfe_on: bool,
    pub sample_rate: u32,
    pub bit_rate: u32,
    /// Coded channels including LFE.
    pub channels: u8,
    /// Frame size in bytes.
    pub frame_size: usize,
    /// Audio blocks per frame (6 for AC-3).
    pub num_blocks: u8,
    /// 0 independent, 1 dependent, 2 AC-3 convert (plain AC-3 reports 2).
    pub frame_type: u8,
    pub substreamid: u8,
    pub is_eac3: bool,
    pub(crate) sr_code: u8,
    pub(crate) sr_shift: u8,
    pub(crate) center_mix_level: u8,
    pub(crate) surround_mix_level: u8,
}

const EAC3_BLOCKS: [u8; 4] = [1, 2, 3, 6];
const CENTER_LEVELS: [u8; 4] = [4, 5, 6, 5];
const SURROUND_LEVELS: [u8; 4] = [4, 6, 7, 6];

/// Locates the sync word (0x0B77) within the first bytes, as FFmpeg does.
/// Returns the byte offset of the frame start.
pub fn find_syncword(buf: &[u8]) -> Option<usize> {
    let mut i = 1;
    while i < buf.len() {
        if buf[i] == 0x77 || buf[i] == 0x0B {
            if buf[i] ^ buf[i - 1] == 0x77 ^ 0x0B {
                return Some(i - 1);
            } else if i + 1 < buf.len() && buf[i] ^ buf[i + 1] == 0x77 ^ 0x0B {
                return Some(i);
            }
        }
        i += 2;
    }
    None
}

pub(crate) fn parse_header(br: &mut BitReader<'_>) -> Result<FrameInfo, Error> {
    let mut h = FrameInfo::default();
    if br.get_bits(16) != 0x0B77 {
        return Err(Error::Sync);
    }
    // Read ahead to bsid to tell AC-3 from E-AC-3.
    h.bitstream_id = (br.peek_bits(29) & 0x1F) as u8;
    if h.bitstream_id > 16 {
        return Err(Error::BitstreamId);
    }
    h.num_blocks = 6;
    h.center_mix_level = 5;
    h.surround_mix_level = 6;

    if h.bitstream_id <= 10 {
        // Normal AC-3.
        br.skip(16); // crc1
        h.sr_code = br.get_bits(2) as u8;
        if h.sr_code == 3 {
            return Err(Error::SampleRate);
        }
        let frame_size_code = br.get_bits(6) as usize;
        if frame_size_code > 37 {
            return Err(Error::FrameSize);
        }
        let ac3_bit_rate_code = frame_size_code >> 1;
        br.skip(5); // bsid
        h.bitstream_mode = br.get_bits(3) as u8;
        h.channel_mode = br.get_bits(3) as u8;
        if h.channel_mode == CHMODE_STEREO {
            br.skip(2); // dsurmod
        } else {
            if (h.channel_mode & 1) != 0 && h.channel_mode != CHMODE_MONO {
                h.center_mix_level = CENTER_LEVELS[br.get_bits(2) as usize];
            }
            if (h.channel_mode & 4) != 0 {
                h.surround_mix_level = SURROUND_LEVELS[br.get_bits(2) as usize];
            }
        }
        h.lfe_on = br.get_bit() != 0;
        h.sr_shift = h.bitstream_id.max(8) - 8;
        h.sample_rate = SAMPLE_RATE_TAB[h.sr_code as usize] >> h.sr_shift;
        h.bit_rate = (BITRATE_TAB[ac3_bit_rate_code] as u32 * 1000) >> h.sr_shift;
        h.channels = CHANNELS_TAB[h.channel_mode as usize] + h.lfe_on as u8;
        h.frame_size = FRAME_SIZE_TAB[frame_size_code][h.sr_code as usize] as usize * 2;
        h.frame_type = FRAME_TYPE_AC3_CONVERT;
        h.substreamid = 0;
        h.is_eac3 = false;
    } else {
        // Enhanced AC-3.
        h.is_eac3 = true;
        h.frame_type = br.get_bits(2) as u8;
        if h.frame_type == FRAME_TYPE_RESERVED {
            return Err(Error::FrameType);
        }
        h.substreamid = br.get_bits(3) as u8;
        h.frame_size = (br.get_bits(11) as usize + 1) << 1;
        if h.frame_size < AC3_HEADER_SIZE {
            return Err(Error::FrameSize);
        }
        h.sr_code = br.get_bits(2) as u8;
        if h.sr_code == 3 {
            let sr_code2 = br.get_bits(2) as usize;
            if sr_code2 == 3 {
                return Err(Error::SampleRate);
            }
            h.sample_rate = SAMPLE_RATE_TAB[sr_code2] / 2;
            h.sr_shift = 1;
        } else {
            h.num_blocks = EAC3_BLOCKS[br.get_bits(2) as usize];
            h.sample_rate = SAMPLE_RATE_TAB[h.sr_code as usize];
            h.sr_shift = 0;
        }
        h.channel_mode = br.get_bits(3) as u8;
        h.lfe_on = br.get_bit() != 0;
        h.bit_rate = (8u64 * h.frame_size as u64 * h.sample_rate as u64
            / (h.num_blocks as u64 * 256)) as u32;
        h.channels = CHANNELS_TAB[h.channel_mode as usize] + h.lfe_on as u8;
    }
    Ok(h)
}

/// Parses the synchronisation header of the frame starting at `data[0]`.
pub fn parse_frame_info(data: &[u8]) -> Result<FrameInfo, Error> {
    let mut padded = [0u8; 16];
    let n = data.len().min(8);
    padded[..n].copy_from_slice(&data[..n]);
    let mut br = BitReader::new(&padded, n * 8);
    parse_header(&mut br)
}

/// CRC-16 (polynomial 0x8005, init 0) over `data`; zero for an intact frame
/// when run over everything after the sync word.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc = (crc << 8) ^ CRC16_TAB[((crc >> 8) as u8 ^ b) as usize];
    }
    crc
}

/// Returns true when the frame's CRC check passes. `frame` must hold the whole
/// frame (at least `frame_size` bytes).
pub fn frame_crc_ok(frame: &[u8], frame_size: usize) -> bool {
    frame.len() >= frame_size && frame_size > 2 && crc16(&frame[2..frame_size]) == 0
}

/// Splits a raw elementary stream into access units: each unit is an
/// independent frame followed by its dependent frames, which is what one
/// container sample (and one [`Decoder::decode`](crate::Decoder::decode)
/// call) holds. Garbage between frames is skipped.
pub fn access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut pos = 0usize;
    let mut unit_start: Option<usize> = None;
    while pos + AC3_HEADER_SIZE <= stream.len() {
        if stream[pos] != 0x0B || stream[pos + 1] != 0x77 {
            pos += 1;
            continue;
        }
        let info = match parse_frame_info(&stream[pos..]) {
            Ok(i) => i,
            Err(_) => {
                pos += 1;
                continue;
            }
        };
        if pos + info.frame_size > stream.len() {
            break;
        }
        if info.frame_type != FRAME_TYPE_DEPENDENT {
            if let Some(s) = unit_start {
                units.push(&stream[s..pos]);
            }
            unit_start = Some(pos);
        } else if unit_start.is_none() {
            unit_start = Some(pos);
        }
        pos += info.frame_size;
    }
    if let Some(s) = unit_start {
        units.push(&stream[s..pos.min(stream.len())]);
    }
    units
}
