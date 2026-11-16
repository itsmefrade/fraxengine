//! Frame table of contents and chunk navigation (ETSI TS 103 491 clause 6.4).

use crate::bitreader::BitReader;

pub const SYNC_WORD: u32 = 0x4041_1BF2;
pub const NONSYNC_WORD: u32 = 0x71C4_42E8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    BadSyncWord,
    BadCrc,
    Invalid(&'static str),
    Unsupported(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Truncated => write!(f, "truncated frame"),
            ParseError::BadSyncWord => write!(f, "bad sync word"),
            ParseError::BadCrc => write!(f, "CRC mismatch"),
            ParseError::Invalid(s) => write!(f, "invalid: {s}"),
            ParseError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}
impl std::error::Error for ParseError {}

/// CRC-16 (polynomial 0x1021, initial value 0xFFFF) of clause 6.3.8.
pub fn crc16(data: &[u8]) -> u16 {
    let mut r: u16 = 0xFFFF;
    for &b in data {
        r ^= (b as u16) << 8;
        for _ in 0..8 {
            r = if r & 0x8000 != 0 {
                (r << 1) ^ 0x1021
            } else {
                r << 1
            };
        }
    }
    r
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioChunkNav {
    pub index: u32,
    pub id: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MdChunkNav {
    pub size: u32,
    pub crc_flag: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PbrSmoothing {
    pub enabled: bool,
    pub cumulative_payload: u32,
    pub first_xll_sync_present: bool,
    pub buffer_kb: u32,
    pub init_delay_frames: u32,
    pub first_xll_sync_offset_dwords: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ftoc {
    pub is_sync: bool,
    pub ftoc_bytes: usize,
    pub full_channel_based_mix: bool,
    pub major_version: u32,
    pub minor_revision: u32,
    pub base_duration: u32,
    pub frame_duration: u32,
    pub clock_rate: u32,
    pub time_stamp: Option<u64>,
    pub sample_rate: u32,
    pub reserved_flag: bool,
    pub interact_obj_limits_present: bool,
    pub num_audio_presentations: u32,
    pub presentation_selectable: Vec<bool>,
    pub dep_presentation_mask: Vec<u32>,
    pub expl_obj_list_present_mask: Vec<u32>,
    pub md_chunks: Vec<MdChunkNav>,
    pub audio_chunks: Vec<AudioChunkNav>,
    pub pbr: PbrSmoothing,
    /// Total frame bytes: FTOC + metadata chunks + audio chunks.
    pub total_bytes: usize,
}

/// Stream level state that non-sync frames inherit from the last sync frame.
#[derive(Debug, Clone, Default)]
pub struct FtocState {
    pub full_channel_based_mix: bool,
    pub frame_duration: u32,
    pub clock_rate: u32,
    pub sample_rate: u32,
    pub base_duration: u32,
    pub major_version: u32,
    pub minor_revision: u32,
    pub reserved_flag: bool,
    pub interact_obj_limits_present: bool,
    pub num_audio_presentations: u32,
    pub presentation_selectable: Vec<bool>,
    pub dep_presentation_mask: Vec<u32>,
    pub expl_obj_list_present_mask: Vec<u32>,
    pub pbr_enabled: bool,
    pub audio_chunk_ids: Vec<(u32, u32)>, // (index, id) registered chunks
    pub have_sync: bool,
    pub time_stamp: Option<u64>,
}

impl FtocState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Parses the FTOC at the start of `data`. Updates `state` from sync frames.
pub fn parse_ftoc(data: &[u8], state: &mut FtocState) -> Result<Ftoc, ParseError> {
    if data.len() < 8 {
        return Err(ParseError::Truncated);
    }
    let mut br = BitReader::new(data);
    let word = br.read_uint(32);
    let is_sync = match word {
        SYNC_WORD => true,
        NONSYNC_WORD => false,
        _ => return Err(ParseError::BadSyncWord),
    };
    let ftoc_bytes = br.read_var_len_bit_fields(&[5, 8, 10, 12], true) as usize + 1;
    if ftoc_bytes > data.len() || ftoc_bytes < 6 {
        return Err(ParseError::Truncated);
    }
    let mut f = Ftoc {
        is_sync,
        ftoc_bytes,
        ..Default::default()
    };
    if !is_sync && !state.have_sync {
        return Err(ParseError::Invalid(
            "non-sync frame before the first sync frame",
        ));
    }
    if is_sync {
        state.full_channel_based_mix = br.read_bool();
    }
    f.full_channel_based_mix = state.full_channel_based_mix;
    // CRC: always for object streams; only in sync frames for full channel based mixes
    if !state.full_channel_based_mix || is_sync {
        if ftoc_bytes < 3 {
            return Err(ParseError::Invalid("FTOC too short for CRC"));
        }
        let calc = crc16(&data[..ftoc_bytes - 2]);
        let got = ((data[ftoc_bytes - 2] as u16) << 8) | data[ftoc_bytes - 1] as u16;
        if calc != got {
            return Err(ParseError::BadCrc);
        }
    }
    if is_sync {
        if !state.full_channel_based_mix {
            let short = br.read_bool();
            let bits = if short { 6 } else { 12 };
            let t = br.read_uint(bits);
            let half = bits >> 1;
            state.major_version = (t >> half) + 2;
            state.minor_revision = t & ((1 << half) - 1);
        } else {
            state.major_version = 2;
            state.minor_revision = 0;
        }
        let bd = br.read_uint(2);
        state.base_duration = match bd {
            0 => 512,
            1 => 480,
            2 => 384,
            _ => return Err(ParseError::Invalid("reserved base duration")),
        };
        let mult = br.read_uint(3) + 1;
        state.frame_duration = state.base_duration * mult;
        state.clock_rate = match br.read_uint(2) {
            0 => 32000,
            1 => 44100,
            2 => 48000,
            _ => return Err(ParseError::Invalid("reserved clock rate")),
        };
        // time stamp: presence flag, then 32 + 4 bits
        let present = br.read_bool();
        state.time_stamp = None;
        if present {
            let hi = br.read_uint(32) as u64;
            let lo = br.read_uint(4) as u64;
            state.time_stamp = Some((hi << 4) | lo);
        }
        let m = br.read_uint(2);
        state.sample_rate = state.clock_rate * (1 << m);
        if !state.full_channel_based_mix {
            state.reserved_flag = br.read_bool();
            state.interact_obj_limits_present = br.read_bool();
        } else {
            state.reserved_flag = false;
            state.interact_obj_limits_present = false;
        }
        // audio presentation parameters (clause 6.4.7 to 6.4.12)
        let n = if state.full_channel_based_mix {
            1
        } else {
            br.read_var_len_bit_fields(&[0, 2, 4, 5], true) + 1
        };
        state.num_audio_presentations = n;
        state.presentation_selectable = vec![false; n as usize];
        state.dep_presentation_mask = vec![0; n as usize];
        state.expl_obj_list_present_mask = vec![0; n as usize];
        for p in 0..n as usize {
            state.presentation_selectable[p] = if state.full_channel_based_mix {
                true
            } else {
                br.read_bool()
            };
            if state.presentation_selectable[p] {
                let mask = if p > 0 { br.read_uint(p as u32) } else { 0 };
                state.dep_presentation_mask[p] = mask;
                let mut flags = 0u32;
                for j in 0..p {
                    if (mask >> j) & 1 == 1 {
                        flags |= br.read_uint(1) << j;
                    }
                }
                state.expl_obj_list_present_mask[p] = flags;
                // explicit object lists: all present in sync frames
                for j in 0..p {
                    if (flags >> j) & 1 == 1 {
                        let _list = br.read_var_len_bit_fields(&[4, 8, 16, 32], true);
                    }
                }
            }
        }
        state.have_sync = true;
    } else {
        // Explicit object list updates in non-sync frames (full mixes carry none)
        if !state.full_channel_based_mix {
            for p in 0..state.num_audio_presentations as usize {
                if state.presentation_selectable[p] {
                    let flags = state.expl_obj_list_present_mask[p];
                    for j in 0..p {
                        if (flags >> j) & 1 == 1 && br.read_bool() {
                            let _ = br.read_var_len_bit_fields(&[4, 8, 16, 32], true);
                        }
                    }
                }
            }
        }
    }
    f.major_version = state.major_version;
    f.minor_revision = state.minor_revision;
    f.base_duration = state.base_duration;
    f.frame_duration = state.frame_duration;
    f.clock_rate = state.clock_rate;
    f.sample_rate = state.sample_rate;
    f.time_stamp = state.time_stamp;
    f.reserved_flag = state.reserved_flag;
    f.interact_obj_limits_present = state.interact_obj_limits_present;
    f.num_audio_presentations = state.num_audio_presentations;
    f.presentation_selectable = state.presentation_selectable.clone();
    f.dep_presentation_mask = state.dep_presentation_mask.clone();
    f.expl_obj_list_present_mask = state.expl_obj_list_present_mask.clone();

    // chunk navigation (clause 6.4.13)
    let full = state.full_channel_based_mix;
    let num_md = if full {
        if is_sync {
            1
        } else {
            0
        }
    } else {
        br.read_var_len_bit_fields(&[2, 4, 6, 8], true)
    };
    let mut payload: usize = 0;
    for _ in 0..num_md {
        let size = br.read_var_len_bit_fields(&[6, 9, 12, 15], true);
        let crc_flag = if full { false } else { br.read_bool() };
        payload += size as usize;
        f.md_chunks.push(MdChunkNav { size, crc_flag });
    }
    let num_ac = if full {
        1
    } else {
        br.read_var_len_bit_fields(&[2, 4, 6, 8], true)
    };
    if is_sync {
        state.audio_chunk_ids.clear();
    }
    for _ in 0..num_ac {
        let index = if full {
            0
        } else {
            br.read_var_len_bit_fields(&[2, 4, 6, 8], true)
        };
        let present_id = if is_sync {
            true
        } else if full {
            false
        } else {
            br.read_bool()
        };
        let id = if present_id {
            let id = br.read_var_len_bit_fields(&[2, 4, 6, 8], true);
            if let Some(e) = state.audio_chunk_ids.iter_mut().find(|e| e.0 == index) {
                e.1 = id;
            } else {
                state.audio_chunk_ids.push((index, id));
            }
            id
        } else {
            state
                .audio_chunk_ids
                .iter()
                .find(|e| e.0 == index)
                .map(|e| e.1)
                .ok_or(ParseError::Invalid("audio chunk id never announced"))?
        };
        let size = if id == 0 {
            0
        } else {
            br.read_var_len_bit_fields(&[9, 11, 13, 16], true)
        };
        payload += size as usize;
        f.audio_chunks.push(AudioChunkNav { index, id, size });
    }
    // peak bit rate smoothing parameters (clause 6.4.15)
    if is_sync {
        state.pbr_enabled = br.read_bool();
    }
    f.pbr.enabled = state.pbr_enabled;
    if state.pbr_enabled {
        let nb = 9 + 3 * br.read_uint(2);
        f.pbr.cumulative_payload = br.read_uint(nb) + 1;
        f.pbr.first_xll_sync_present = br.read_bool();
        if f.pbr.first_xll_sync_present {
            f.pbr.buffer_kb = (br.read_uint(2) + 1) << 6;
            f.pbr.init_delay_frames = br.read_uint(8);
            f.pbr.first_xll_sync_offset_dwords = br.read_uint(nb - 2);
        }
    }
    if br.overrun() {
        return Err(ParseError::Truncated);
    }
    f.total_bytes = ftoc_bytes + payload;
    Ok(f)
}
