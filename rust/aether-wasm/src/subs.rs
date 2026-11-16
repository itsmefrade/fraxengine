//! Subtitle exports (`aether_subs_*`): one open track per module instance, like the audio decoder.
//!
//! Kinds: 0 PGS, 1 ASS/SSA, 2 SRT, 3 WebVTT. Variable results are written to the result buffer.
//!
//! Text results are UTF-8 JSON. `aether_subs_open` returns the track info
//! `{"playResX","playResY","wrapStyle","scaledBorder"}`; `aether_subs_push` returns an array of the
//! cues that are new (empty for a repeated packet), each
//! `{"s","e" (us),"layer","an","ml","mr","mv","bs","pos":[x,y]|null,"fad":[in,out]|null,
//! "spans":[{"t","fn","fs","b","i","u","st","c","oc","bc" ([r,g,b,a]),"bord","shad"}]}`
//! with lengths in PlayRes units.
//!
//! PGS results are little endian binary: `u32 count`, then per display set `i64 start_us,
//! i64 end_us (-1 = until the next one), u32 video_w, u32 video_h, u32 flags (bit 0 = clear),
//! u32 bitmaps`, then per bitmap `i32 x, i32 y, u32 w, u32 h, u32 forced` and `w*h*4` RGBA bytes.
//!
//! Statuses: >= 0 success (the count of cues or display sets in the result), -1 nothing open,
//! -2 unsupported kind, -3 unreadable input. Messages go to `aether_last_error`.

use crate::{input, set_error, set_result};
use aether_subs::{json, pgs, Display, PgsDecoder, TextFormat, TextTrack};
use std::cell::RefCell;

enum Open {
    Pgs(PgsDecoder),
    Text(TextTrack),
}

thread_local! { static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) }; }

fn text_format(kind: u32) -> Option<TextFormat> {
    match kind {
        1 => Some(TextFormat::Ass),
        2 => Some(TextFormat::Srt),
        3 => Some(TextFormat::WebVtt),
        _ => None,
    }
}

fn encode_displays(ds: &[Display]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend((ds.len() as u32).to_le_bytes());
    for d in ds {
        o.extend(d.start_us.to_le_bytes());
        o.extend(d.end_us.unwrap_or(-1).to_le_bytes());
        o.extend(d.video_width.to_le_bytes());
        o.extend(d.video_height.to_le_bytes());
        o.extend((d.clear as u32).to_le_bytes());
        o.extend((d.bitmaps.len() as u32).to_le_bytes());
        for b in &d.bitmaps {
            o.extend(b.x.to_le_bytes());
            o.extend(b.y.to_le_bytes());
            o.extend(b.width.to_le_bytes());
            o.extend(b.height.to_le_bytes());
            o.extend((b.forced as u32).to_le_bytes());
            o.extend(&b.rgba);
        }
    }
    o
}

/// Opens a track. `ptr`/`len` is the container codec private (the ASS script header; unused for
/// the other kinds). Result: track info JSON (`{"kind":"pgs"}` for PGS).
///
/// # Safety
/// `ptr` must reference `len` readable bytes (or be null with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_subs_open(kind: u32, ptr: *const u8, len: usize) -> i32 {
    let hdr = unsafe { input(ptr, len) };
    let (open, info) = if kind == 0 {
        (Open::Pgs(PgsDecoder::new()), "{\"kind\":\"pgs\"}".to_string())
    } else if let Some(f) = text_format(kind) {
        let t = TextTrack::new(f, hdr);
        let info = json::track_info(t.info());
        (Open::Text(t), info)
    } else {
        set_error(format!("unsupported subtitle kind {kind}"));
        return -2;
    };
    OPEN.with_borrow_mut(|o| *o = Some(open));
    set_result(info.into_bytes());
    0
}

/// One container packet shown from `pts_us` for `duration_us` (0 = unknown). Result as described
/// in the module docs; returns the number of new cues / completed display sets.
///
/// # Safety
/// `ptr` must reference `len` readable bytes (or be null with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_subs_push(pts_us: i64, duration_us: i64, ptr: *const u8, len: usize) -> i32 {
    let data = unsafe { input(ptr, len) };
    OPEN.with_borrow_mut(|o| match o {
        None => {
            set_error("no subtitle track open");
            -1
        }
        Some(Open::Pgs(d)) => {
            let mut ds = d.push(pts_us, data);
            if duration_us > 0 {
                for x in &mut ds {
                    x.end_us = Some(x.start_us + duration_us);
                }
            }
            set_result(encode_displays(&ds));
            ds.len() as i32
        }
        Some(Open::Text(t)) => match t.push_packet(pts_us, duration_us, data) {
            Some(c) => {
                set_result(format!("[{}]", json::cue(c)).into_bytes());
                1
            }
            None => {
                set_result(b"[]".to_vec());
                0
            }
        },
    })
}

/// Parses a whole sidecar file (`.sup` for kind 0, `.ass`/`.ssa`, `.srt`, `.vtt`) and replaces the
/// open track. Text result: `{"track":{...},"cues":[...]}`; PGS result: the binary display list
/// with every end time filled in. Returns the cue / display set count, or -3.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_subs_load_file(kind: u32, ptr: *const u8, len: usize) -> i32 {
    let data = unsafe { input(ptr, len) };
    if kind == 0 {
        return match pgs::parse_sup(data) {
            Ok(ds) => {
                set_result(encode_displays(&ds));
                OPEN.with_borrow_mut(|o| *o = Some(Open::Pgs(PgsDecoder::new())));
                ds.len() as i32
            }
            Err(e) => {
                set_error(e.to_string());
                -3
            }
        };
    }
    let Some(f) = text_format(kind) else {
        set_error(format!("unsupported subtitle kind {kind}"));
        return -2;
    };
    let t = TextTrack::parse_file(f, data);
    let cues: Vec<String> = t.cues.iter().map(json::cue).collect();
    set_result(format!("{{\"track\":{},\"cues\":[{}]}}", json::track_info(t.info()), cues.join(",")).into_bytes());
    let n = t.cues.len() as i32;
    OPEN.with_borrow_mut(|o| *o = Some(Open::Text(t)));
    n
}

/// Drops decoder state that depends on stream continuity (PGS epoch objects and palettes) after
/// a seek. Text tracks keep their de-duplication set so repeated packets stay ignored.
#[unsafe(no_mangle)]
pub extern "C" fn aether_subs_reset() -> i32 {
    OPEN.with_borrow_mut(|o| match o {
        None => -1,
        Some(Open::Pgs(d)) => {
            d.reset();
            0
        }
        Some(Open::Text(_)) => 0,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_subs_close() {
    OPEN.with_borrow_mut(|o| *o = None);
}
