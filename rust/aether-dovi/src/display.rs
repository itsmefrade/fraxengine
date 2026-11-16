//! Dolby Vision display metadata of one RPU as compact JSON (camelCase, integers exactly as
//! coded). Parsing goes through `semantic::parse_rpu_checked`, so the CRC and `rpu_guard`
//! preflight run before libdovi sees a byte.
//!
//! Layout (every key is always present, absent blocks are `null` or `[]`):
//! `{"profile", "compressed", "usePrevMapping", "dmId":{"affected","current"}, "sceneRefresh",
//!  "yccToRgb":[9 i16, row major, /8192], "yccToRgbOffset":[3 u32, /2^28], "rgbToLms":[9 i16, row major, /16384],
//!  "signal":{"eotf","eotfParam":[3],"bitDepth","colorSpace","chromaFormat","fullRange"},
//!  "source":{"minPq","maxPq","diagonal"},
//!  "l1":{"minPq","avgPq","maxPq"}, "l2":[{"targetMaxPq","slope","offset","power","chromaWeight","saturationGain","msWeight"}],
//!  "l3":{"minPqOffset","avgPqOffset","maxPqOffset"}, "l4":{"anchorPq","anchorPower"},
//!  "l5":{"left","right","top","bottom"}, "l6":{"maxMasteringLuminance","minMasteringLuminance","maxCll","maxFall"},
//!  "l8":[{"length" (coded bytes: fields beyond it are libdovi defaults),"targetDisplayIndex","slope","offset","power","chromaWeight","saturationGain","msWeight","targetMidContrast","clipTrim","saturationVector":[6],"hueVector":[6]}],
//!  "l9":{"length","primaryIndex","primaries":[rx,ry,gx,gy,bx,by,wx,wy]}, "l10":[{"length","targetDisplayIndex","targetMaxPq","targetMinPq","primaryIndex","primaries":[8]}],
//!  "l11":{"contentType","whitepoint","referenceMode"}, "cm":[29,40 present versions]}`
//! L1 to L6 live in the CM v2.9 section of an RPU and L3, L8 to L11 in the CM v4.0 section; the output merges both.
//! A `compressed` RPU (DM data repeated by id) carries no matrices or signal fields: they are zero and the caller keeps the
//! previous full RPU's values (`web/engine/dovi.mjs` does).

use crate::semantic::{SemanticError, parse_rpu_checked};
use dolby_vision::rpu::extension_metadata::{DmData, WithExtMetadataBlocks, blocks::ExtMetadataBlock};
use std::fmt::Write;

fn arr<T: std::fmt::Display>(v: &[T]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{x}");
    }
    s.push(']');
    s
}

pub fn display_json(nal: &[u8]) -> Result<String, SemanticError> {
    let rpu = parse_rpu_checked(nal)?;
    let dm = rpu.vdr_dm_data.as_ref();
    let d = dm.cloned().unwrap_or_default();
    let mut blocks: Vec<&ExtMetadataBlock> = Vec::new();
    let mut cm = Vec::new();
    for (version, section) in [(29, d.cmv29_metadata.as_ref()), (40, d.cmv40_metadata.as_ref())] {
        match section {
            Some(DmData::V29(m)) => {
                cm.push(version);
                blocks.extend(m.blocks_ref());
            }
            Some(DmData::V40(m)) => {
                cm.push(version);
                blocks.extend(m.blocks_ref());
            }
            None => {}
        }
    }
    let mut o = String::with_capacity(1024);
    let _ = write!(
        o,
        "{{\"profile\":{},\"hasDm\":{},\"compressed\":{},\"usePrevMapping\":{},\"dmId\":{{\"affected\":{},\"current\":{}}},\"sceneRefresh\":{},",
        rpu.dovi_profile,
        dm.is_some(),
        d.compressed,
        rpu.header.use_prev_vdr_rpu_flag,
        d.affected_dm_metadata_id,
        d.current_dm_metadata_id,
        d.scene_refresh_flag
    );
    let _ = write!(
        o,
        "\"yccToRgb\":{},\"yccToRgbOffset\":{},\"rgbToLms\":{},",
        arr(&[
            d.ycc_to_rgb_coef0, d.ycc_to_rgb_coef1, d.ycc_to_rgb_coef2, d.ycc_to_rgb_coef3, d.ycc_to_rgb_coef4,
            d.ycc_to_rgb_coef5, d.ycc_to_rgb_coef6, d.ycc_to_rgb_coef7, d.ycc_to_rgb_coef8
        ]),
        arr(&[d.ycc_to_rgb_offset0, d.ycc_to_rgb_offset1, d.ycc_to_rgb_offset2]),
        arr(&[
            d.rgb_to_lms_coef0, d.rgb_to_lms_coef1, d.rgb_to_lms_coef2, d.rgb_to_lms_coef3, d.rgb_to_lms_coef4,
            d.rgb_to_lms_coef5, d.rgb_to_lms_coef6, d.rgb_to_lms_coef7, d.rgb_to_lms_coef8
        ])
    );
    let _ = write!(
        o,
        "\"signal\":{{\"eotf\":{},\"eotfParam\":{},\"bitDepth\":{},\"colorSpace\":{},\"chromaFormat\":{},\"fullRange\":{}}},\"source\":{{\"minPq\":{},\"maxPq\":{},\"diagonal\":{}}},",
        d.signal_eotf,
        arr(&[u32::from(d.signal_eotf_param0), u32::from(d.signal_eotf_param1), d.signal_eotf_param2]),
        d.signal_bit_depth,
        d.signal_color_space,
        d.signal_chroma_format,
        d.signal_full_range_flag,
        d.source_min_pq,
        d.source_max_pq,
        d.source_diagonal
    );
    let (mut l1, mut l3, mut l4, mut l5, mut l6, mut l9, mut l11) =
        ("null".to_string(), "null".to_string(), "null".to_string(), "null".to_string(), "null".to_string(), "null".to_string(), "null".to_string());
    let (mut l2, mut l8, mut l10) = (Vec::new(), Vec::new(), Vec::new());
    for b in blocks {
        match b {
            ExtMetadataBlock::Level1(x) => l1 = format!("{{\"minPq\":{},\"avgPq\":{},\"maxPq\":{}}}", x.min_pq, x.avg_pq, x.max_pq),
            ExtMetadataBlock::Level2(x) => l2.push(format!(
                "{{\"targetMaxPq\":{},\"slope\":{},\"offset\":{},\"power\":{},\"chromaWeight\":{},\"saturationGain\":{},\"msWeight\":{}}}",
                x.target_max_pq, x.trim_slope, x.trim_offset, x.trim_power, x.trim_chroma_weight, x.trim_saturation_gain, x.ms_weight
            )),
            ExtMetadataBlock::Level3(x) => {
                l3 = format!("{{\"minPqOffset\":{},\"avgPqOffset\":{},\"maxPqOffset\":{}}}", x.min_pq_offset, x.avg_pq_offset, x.max_pq_offset)
            }
            ExtMetadataBlock::Level4(x) => l4 = format!("{{\"anchorPq\":{},\"anchorPower\":{}}}", x.anchor_pq, x.anchor_power),
            ExtMetadataBlock::Level5(x) => {
                l5 = format!(
                    "{{\"left\":{},\"right\":{},\"top\":{},\"bottom\":{}}}",
                    x.active_area_left_offset, x.active_area_right_offset, x.active_area_top_offset, x.active_area_bottom_offset
                )
            }
            ExtMetadataBlock::Level6(x) => {
                l6 = format!(
                    "{{\"maxMasteringLuminance\":{},\"minMasteringLuminance\":{},\"maxCll\":{},\"maxFall\":{}}}",
                    x.max_display_mastering_luminance, x.min_display_mastering_luminance, x.max_content_light_level, x.max_frame_average_light_level
                )
            }
            ExtMetadataBlock::Level8(x) => l8.push(format!(
                "{{\"length\":{},\"targetDisplayIndex\":{},\"slope\":{},\"offset\":{},\"power\":{},\"chromaWeight\":{},\"saturationGain\":{},\"msWeight\":{},\"targetMidContrast\":{},\"clipTrim\":{},\"saturationVector\":{},\"hueVector\":{}}}",
                x.length, x.target_display_index, x.trim_slope, x.trim_offset, x.trim_power, x.trim_chroma_weight, x.trim_saturation_gain, x.ms_weight,
                x.target_mid_contrast, x.clip_trim,
                arr(&[x.saturation_vector_field0, x.saturation_vector_field1, x.saturation_vector_field2, x.saturation_vector_field3, x.saturation_vector_field4, x.saturation_vector_field5]),
                arr(&[x.hue_vector_field0, x.hue_vector_field1, x.hue_vector_field2, x.hue_vector_field3, x.hue_vector_field4, x.hue_vector_field5])
            )),
            ExtMetadataBlock::Level9(x) => {
                l9 = format!(
                    "{{\"length\":{},\"primaryIndex\":{},\"primaries\":{}}}",
                    x.length,
                    x.source_primary_index,
                    arr(&[
                        x.source_primary_red_x, x.source_primary_red_y, x.source_primary_green_x, x.source_primary_green_y,
                        x.source_primary_blue_x, x.source_primary_blue_y, x.source_primary_white_x, x.source_primary_white_y
                    ])
                )
            }
            ExtMetadataBlock::Level10(x) => l10.push(format!(
                "{{\"length\":{},\"targetDisplayIndex\":{},\"targetMaxPq\":{},\"targetMinPq\":{},\"primaryIndex\":{},\"primaries\":{}}}",
                x.length, x.target_display_index, x.target_max_pq, x.target_min_pq, x.target_primary_index,
                arr(&[
                    x.target_primary_red_x, x.target_primary_red_y, x.target_primary_green_x, x.target_primary_green_y,
                    x.target_primary_blue_x, x.target_primary_blue_y, x.target_primary_white_x, x.target_primary_white_y
                ])
            )),
            ExtMetadataBlock::Level11(x) => {
                l11 = format!("{{\"contentType\":{},\"whitepoint\":{},\"referenceMode\":{}}}", x.content_type, x.whitepoint, x.reference_mode_flag)
            }
            _ => {}
        }
    }
    let _ = write!(
        o,
        "\"l1\":{l1},\"l2\":[{}],\"l3\":{l3},\"l4\":{l4},\"l5\":{l5},\"l6\":{l6},\"l8\":[{}],\"l9\":{l9},\"l10\":[{}],\"l11\":{l11},\"cm\":{}}}",
        l2.join(","),
        l8.join(","),
        l10.join(","),
        arr(&cm)
    );
    Ok(o)
}
