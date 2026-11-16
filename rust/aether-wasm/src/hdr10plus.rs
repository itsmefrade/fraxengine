//! HDR10+ (SMPTE ST 2094-40) dynamic metadata exports.
//!
//! `aether_hdr10plus_parse(ptr, len, format) -> i32`: `format` 0 is HEVC Annex B, 1..4 HEVC length
//! prefixed with that size, 16 is an AV1 temporal unit. Returns 1 and a JSON result when the
//! access unit carries HDR10+, 0 when it does not (malformed messages count as absent), -1 for a
//! bad `format`. The JSON holds the raw integers of every field (denominators as in ffprobe):
//! `{"v","windows","target","targetPeak":{"rows","cols","values"}|null,"masterPeak":...,
//! "geometry":[...],"params":[{"maxscl":[r,g,b],"avg","pct":[[percentage,value]..],"fbp",
//! "knee":[x,y]|null,"anchors":[..],"sat":n|null}],"raw":"<hex of the T.35 payload>"}`.
//!
//! `aether_hdr10plus_gpu_params(raw_ptr, raw_len, display_peak_nits: f32) -> i32` builds the 36 f32
//! parameter block (layout in `aether_dovi::hdr10plus::ToneParams::to_gpu`) from the `raw` bytes
//! and returns 36, or -1 when they do not parse. `aether_hdr10plus_apply(raw_ptr, raw_len, peak,
//! r, g, b) -> i32` maps one PQ pixel with the Rust reference curve (3 f32 result), for tests.
//! All f32 results are little endian.

use crate::{input, set_result};
use aether_dovi::{
    hdr10plus::{self, Hdr10Plus, PeakGrid, ToneParams},
    hevc::Framing,
};
use std::fmt::Write;

fn grid(g: &Option<PeakGrid>) -> String {
    match g {
        None => "null".into(),
        Some(g) => format!("{{\"rows\":{},\"cols\":{},\"values\":{:?}}}", g.rows, g.cols, g.values),
    }
}

fn json(h: &Hdr10Plus) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{{\"v\":{},\"windows\":{},\"target\":{},\"targetPeak\":{},\"masterPeak\":{},\"geometry\":[",
        h.application_version,
        h.num_windows,
        h.targeted_system_display_maximum_luminance,
        grid(&h.targeted_actual_peak),
        grid(&h.mastering_actual_peak),
    );
    for (i, g) in h.windows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "[{},{},{},{},{},{},{},{},{},{},{}]",
            g.upper_left_x,
            g.upper_left_y,
            g.lower_right_x,
            g.lower_right_y,
            g.center_x,
            g.center_y,
            g.rotation_angle,
            g.semimajor_internal,
            g.semimajor_external,
            g.semiminor_external,
            g.overlap_process_option
        );
    }
    s.push_str("],\"params\":[");
    for (i, p) in h.params.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let pct: Vec<String> = p.percentiles.iter().map(|x| format!("[{},{}]", x.percentage, x.value)).collect();
        let (knee, anchors) = match &p.tone_mapping {
            Some(t) => (format!("[{},{}]", t.knee_x, t.knee_y), format!("{:?}", t.anchors)),
            None => ("null".into(), "[]".into()),
        };
        let sat = p.color_saturation_weight.map_or("null".into(), |v| v.to_string());
        let _ = write!(
            s,
            "{{\"maxscl\":{:?},\"avg\":{},\"pct\":[{}],\"fbp\":{},\"knee\":{},\"anchors\":{},\"sat\":{}}}",
            p.maxscl,
            p.average_maxrgb,
            pct.join(","),
            p.fraction_bright_pixels,
            knee,
            anchors,
            sat
        );
    }
    s.push_str("],\"raw\":\"");
    for b in &h.raw {
        let _ = write!(s, "{b:02x}");
    }
    s.push_str("\"}");
    s
}

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_hdr10plus_parse(ptr: *const u8, len: usize, format: u32) -> i32 {
    let data = unsafe { input(ptr, len) };
    let found = match format {
        0 => hdr10plus::extract_hevc(data, Framing::AnnexB),
        1..=4 => hdr10plus::extract_hevc(data, Framing::LengthPrefixed(format as u8)),
        16 => hdr10plus::extract_av1(data),
        _ => return -1,
    };
    match found {
        Some(h) => {
            set_result(json(&h).into_bytes());
            1
        }
        None => 0,
    }
}

fn params_from(raw: &[u8], peak: f32) -> Option<ToneParams> {
    hdr10plus::parse_t35(raw).ok().map(|h| ToneParams::new(&h, peak))
}

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_hdr10plus_gpu_params(ptr: *const u8, len: usize, display_peak_nits: f32) -> i32 {
    match params_from(unsafe { input(ptr, len) }, display_peak_nits) {
        Some(p) => {
            set_result(p.to_gpu().iter().flat_map(|v| v.to_le_bytes()).collect());
            hdr10plus::GPU_PARAM_LEN as i32
        }
        None => -1,
    }
}

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_hdr10plus_apply(ptr: *const u8, len: usize, display_peak_nits: f32, r: f32, g: f32, b: f32) -> i32 {
    match params_from(unsafe { input(ptr, len) }, display_peak_nits) {
        Some(p) => {
            let o = hdr10plus::apply([r, g, b], &p);
            set_result(o.iter().flat_map(|v| v.to_le_bytes()).collect());
            3
        }
        None => -1,
    }
}
