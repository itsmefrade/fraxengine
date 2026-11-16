//! Metadata chunk 0x01 (ETSI TS 103 491 clause 7): frame level audio presentation
//! description. Channel mask based objects and the full channel based mix used by
//! DTS-UHD Profile 2 are parsed completely. 3D object, Ambisonic and rendering
//! exception metadata are recognised and reported but not decoded further.

use crate::bitreader::BitReader;
use crate::ftoc::{crc16, ParseError};
use crate::layout::{num_speakers_in_layout, CH_MASK_INDEX_TABLE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepType {
    ChMask,
    Mtrx2dChMask,
    Mtrx3dChMask,
    Binaural,
    Ambisonic,
    AudioTracks,
    Object3dSingleSrcPerWf,
    MonoObject3dMultiSrc,
}

impl RepType {
    fn from_index(i: u32) -> RepType {
        match i {
            0 => RepType::ChMask,
            1 => RepType::Mtrx2dChMask,
            2 => RepType::Mtrx3dChMask,
            3 => RepType::Binaural,
            4 => RepType::Ambisonic,
            5 => RepType::AudioTracks,
            6 => RepType::Object3dSingleSrcPerWf,
            _ => RepType::MonoObject3dMultiSrc,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LongTermLoudness {
    pub loudness_db: f32,
    pub measurement_type: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectMd {
    /// Object identifier; 256 denotes the full channel mask based mix.
    pub id: u32,
    pub is_group: bool,
    pub active: bool,
    pub static_flag: bool,
    pub rep_type: Option<RepType>,
    pub importance: u32,
    pub audio_chunk_index: u32,
    pub navi_within_chunk: u32,
    pub ch_layout_index: Option<u32>,
    pub ch_mask: Option<u32>,
    pub num_waveforms: Option<usize>,
    pub rendering_exception_present: bool,
    pub gain_q15: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Md01Chunk {
    pub audio_presentation_index: u32,
    pub object_ids: Vec<u32>,
    pub objects: Vec<ObjectMd>,
    pub out_scales_db: [Option<i32>; 4],
    pub static_md_present: bool,
    pub loudness: Option<LongTermLoudness>,
    pub crc_ok: Option<bool>,
    /// False when the object metadata used a representation that is not decoded here.
    pub objects_complete: bool,
    pub incomplete_reason: Option<&'static str>,
    pub bits_used: usize,
    pub size_bytes: usize,
}

fn skip_custom_drc(br: &mut BitReader<'_>) {
    let idx = br.read_uint(4);
    if idx == 15 {
        br.read_uint(15);
    }
}

fn parse_static_ld_fullmix(br: &mut BitReader<'_>) -> LongTermLoudness {
    // Full channel based mix: nominal description, one loudness set (clause 7.7.6)
    let table_idx = br.read_uint(6) as usize;
    let mtype = br.read_uint(2);
    // DRC compression types: low, medium, high (clause 7.7.6.7 to 7.7.6.16)
    for _ in 0..3 {
        if br.read_bool() {
            skip_custom_drc(br);
        }
        if br.read_bool() {
            // fast attack, slow attack, fast release, slow release, attack/release thresholds
            for _ in 0..6 {
                br.read_uint(6);
            }
        }
    }
    LongTermLoudness {
        loudness_db: LONG_TERM_LOUDNESS[table_idx.min(63)],
        measurement_type: mtype,
    }
}

const LONG_TERM_LOUDNESS: [f32; 64] = [
    -40.0, -39.0, -38.0, -37.0, -36.0, -35.0, -34.0, -33.0, -32.0, -31.0, -30.0, -29.5, -29.0,
    -28.5, -28.0, -27.5, -27.0, -26.75, -26.5, -26.25, -26.0, -25.75, -25.5, -25.25, -25.0, -24.75,
    -24.5, -24.25, -24.0, -23.75, -23.5, -23.25, -23.0, -22.75, -22.5, -22.25, -22.0, -21.75,
    -21.5, -21.25, -21.0, -20.5, -20.0, -19.5, -19.0, -18.0, -17.0, -16.0, -15.0, -14.0, -13.0,
    -12.0, -11.0, -10.0, -9.0, -8.0, -7.0, -6.0, -5.0, -4.0, -3.0, -2.0, -1.0, 0.0,
];

/// Parses one metadata chunk of `size` bytes at `data` (the chunk starts with its ID byte).
pub fn parse_md_chunk(
    data: &[u8],
    size: usize,
    crc_flag: bool,
    is_sync: bool,
    full_channel_based_mix: bool,
    frame_duration: u32,
    selectable: bool,
    interact_limits_present: bool,
) -> Result<Option<Md01Chunk>, ParseError> {
    if size == 0 || data.len() < size {
        return Err(ParseError::Truncated);
    }
    let data = &data[..size];
    let mut br = BitReader::new(data);
    let id = br.read_uint(8);
    if id != 0x01 {
        return Ok(None);
    }
    let mut chunk = Md01Chunk {
        size_bytes: size,
        objects_complete: true,
        ..Default::default()
    };
    if crc_flag {
        if size < 3 {
            return Err(ParseError::Invalid("metadata chunk too small for CRC"));
        }
        let calc = crc16(&data[..size - 2]);
        let got = ((data[size - 2] as u16) << 8) | data[size - 1] as u16;
        chunk.crc_ok = Some(calc == got);
        if calc != got {
            return Err(ParseError::BadCrc);
        }
    }
    chunk.audio_presentation_index = br.read_var_len_bit_fields(&[0, 2, 4, 4], true);
    // object id list (clause 6.3.4)
    if full_channel_based_mix {
        chunk.object_ids.push(256);
    } else {
        let n = br.read_var_len_bit_fields(&[3, 4, 6, 8], true) as usize + 1;
        for _ in 0..n.min(256) {
            let bits = if br.read_bool() { 8 } else { 4 };
            chunk.object_ids.push(br.read_uint(bits));
        }
    }
    let mut mfd_present = false;
    if selectable {
        if is_sync {
            // presentation scaling: four flags, each followed by a 5 bit gain index
            for (i, slot) in chunk.out_scales_db.iter_mut().enumerate() {
                let _ = i;
                if br.read_bool() {
                    *slot = Some(-25 + br.read_uint(5) as i32);
                }
            }
            mfd_present = br.read_bool();
        }
        if mfd_present {
            chunk.static_md_present = true;
            if full_channel_based_mix {
                chunk.loudness = Some(parse_static_ld_fullmix(&mut br));
            } else {
                // Distributed static metadata: packet parameters and payload bytes
                chunk.objects_complete = false;
                chunk.incomplete_reason =
                    Some("distributed static loudness and dynamics packets are not decoded");
                chunk.bits_used = br.bits_used();
                return Ok(Some(chunk));
            }
        }
    }
    // object metadata (clause 7.8)
    if is_sync && !full_channel_based_mix {
        // mixing studio parameters
        if br.read_bool() {
            br.read_uint(4);
            br.read_uint(4);
            br.read_uint(3);
        }
    }
    let ids = chunk.object_ids.clone();
    for &oid in ids.iter() {
        let mut o = ObjectMd {
            id: oid,
            is_group: (224..256).contains(&oid),
            active: true,
            static_flag: false,
            rep_type: None,
            importance: 7,
            audio_chunk_index: 0,
            navi_within_chunk: 0,
            ch_layout_index: None,
            ch_mask: None,
            num_waveforms: None,
            rendering_exception_present: false,
            gain_q15: None,
        };
        if oid < 224 {
            if !br.read_bool() {
                // metadata for one renderer type only
                let _required = br.read_uint(1);
                let skip = br.read_var_len_bit_fields(&[8, 10, 12, 14], true) as usize + 1;
                br.skip_bits(skip);
                chunk.objects_complete = false;
                chunk.incomplete_reason = Some("renderer specific object metadata skipped");
                chunk.objects.push(o);
                continue;
            }
        }
        if !is_sync {
            // Non-sync object updates need the previous state of the object list.
            chunk.objects_complete = false;
            chunk.incomplete_reason = Some("non-sync object metadata updates are not decoded");
            chunk.objects.push(o);
            break;
        }
        o.static_flag = if oid == 256 { true } else { br.read_bool() };
        if o.is_group {
            chunk.objects_complete = false;
            chunk.incomplete_reason = Some("object group definitions are not decoded");
            chunk.objects.push(o);
            break;
        }
        o.active = if oid == 256 { true } else { br.read_bool() };
        let rep = RepType::from_index(br.read_uint(3));
        o.rep_type = Some(rep);
        if oid != 256 {
            o.importance = br.read_uint(3);
            if br.read_bool() {
                let w = if br.read_bool() { 3 } else { 5 };
                br.read_uint(w);
            }
            o.audio_chunk_index = br.read_var_len_bit_fields(&[1, 4, 4, 8], true);
            o.navi_within_chunk = br.read_var_len_bit_fields(&[3, 3, 4, 8], true);
            if br.read_bool() {
                // per object long term loudness: 6 + 1 + 1 bits
                br.read_uint(8);
            }
            // interactivity
            let interactive = br.read_bool();
            if interactive && interact_limits_present && br.read_bool() {
                br.read_uint(2);
                br.read_uint(3);
                if matches!(
                    rep,
                    RepType::Object3dSingleSrcPerWf | RepType::MonoObject3dMultiSrc
                ) {
                    br.read_uint(3);
                    br.read_uint(3);
                }
            }
        }
        match rep {
            RepType::ChMask | RepType::Mtrx2dChMask | RepType::Mtrx3dChMask | RepType::Binaural => {
                let layout = if rep == RepType::Binaural {
                    1
                } else {
                    br.read_uint(4)
                };
                o.ch_layout_index = Some(layout);
                let mask = match layout {
                    14 => br.read_uint(16),
                    15 => br.read_uint(32),
                    i => CH_MASK_INDEX_TABLE[(i as usize).min(13)],
                };
                o.ch_mask = Some(mask);
                o.num_waveforms = Some(num_speakers_in_layout(mask));
                o.rendering_exception_present = if layout < 2 { false } else { br.read_bool() };
                if o.rendering_exception_present {
                    chunk.objects_complete = false;
                    chunk.incomplete_reason = Some("rendering exception matrices are not decoded");
                    chunk.objects.push(o);
                    break;
                }
                // per sampling point metadata: object gain (clause 7.8.11.21)
                let points = (frame_duration >> 9).max(1);
                let mut param_present = false;
                for p in 0..points {
                    let update = if p == 0 {
                        true
                    } else if oid == 256 {
                        false
                    } else {
                        br.read_bool()
                    };
                    if update {
                        if p == 0 {
                            // UpdateCode with a predefined sync frame value: presence flag, update flag, 6 bit index
                            param_present = br.read_bool();
                            if param_present && br.read_bool() {
                                let g = br.read_uint(6) as usize;
                                o.gain_q15 = SCALE_FACTORS.get(g).copied();
                            }
                        } else if param_present && br.read_bool() {
                            let g = br.read_uint(6) as usize;
                            o.gain_q15 = SCALE_FACTORS.get(g).copied();
                        }
                    }
                }
                if oid != 256 && br.read_bool() {
                    let n = br.read_var_len_bit_fields(&[7, 9, 11, 13], true) as usize + 1;
                    br.skip_bits(n);
                }
            }
            _ => {
                chunk.objects_complete = false;
                chunk.incomplete_reason =
                    Some("3D, Ambisonic and audio track object metadata are not decoded");
                chunk.objects.push(o);
                break;
            }
        }
        chunk.objects.push(o);
    }
    chunk.bits_used = br.bits_used();
    if br.overrun() {
        return Err(ParseError::Truncated);
    }
    Ok(Some(chunk))
}

const SCALE_FACTORS: [u32; 61] = [
    0, 41, 52, 65, 82, 104, 130, 164, 207, 260, 328, 413, 519, 654, 823, 1036, 1163, 1305, 1464,
    1642, 1843, 2068, 2320, 2603, 2920, 3277, 3677, 4125, 4629, 5193, 5827, 6172, 6538, 6925, 7336,
    7771, 8231, 8719, 9235, 9783, 10362, 10976, 11627, 12315, 13045, 13818, 14637, 15504, 16423,
    17396, 18427, 19519, 20675, 21900, 23170, 24573, 26029, 27571, 29205, 30935, 32768,
];
