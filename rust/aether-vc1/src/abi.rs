//! Raw C ABI for WebAssembly hosts (no wasm-bindgen).  All entry points are plain `extern "C"`
//! functions operating on an opaque handle; pointers passed from the host must come from
//! `vc1_alloc` (or any region of linear memory the host owns).
//!
//! Typical use from JS: `h = vc1_create(); vc1_set_extradata(...); loop { vc1_decode(...);
//! while (vc1_next_frame(h)) { read planes via vc1_frame_plane/stride } } vc1_flush(h); ...`
//! Plane pointers remain valid until the next call that decodes, flushes or resets.
#![allow(clippy::missing_safety_doc)]
use crate::decoder::{Decoder, Frame};

pub struct Handle {
    dec: Decoder,
    frame: Option<Frame>,
    last_error: i32,
}

pub const FLAG_INTERLACED: i32 = 1;
pub const FLAG_TFF: i32 = 2;
pub const FLAG_RFF: i32 = 4;
pub const FLAG_KEY: i32 = 8;
pub const FLAG_FIELD_CODED: i32 = 16;

fn err_code(e: crate::core::Error) -> i32 {
    match e {
        crate::core::Error::Invalid(_) => -1,
        crate::core::Error::Unsupported(_) => -2,
        crate::core::Error::NoSequenceHeader => -3,
    }
}

#[no_mangle]
pub extern "C" fn vc1_create() -> *mut Handle {
    Box::into_raw(Box::new(Handle { dec: Decoder::new(), frame: None, last_error: 0 }))
}

#[no_mangle]
pub unsafe extern "C" fn vc1_destroy(h: *mut Handle) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

#[no_mangle]
pub extern "C" fn vc1_alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[no_mangle]
pub unsafe extern "C" fn vc1_free(p: *mut u8, n: usize) {
    if !p.is_null() {
        drop(Vec::from_raw_parts(p, 0, n.max(1)));
    }
}

unsafe fn slice<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(p, n)
    }
}

/// Matroska CodecPrivate / ASF extra data (units with start codes). Returns 0 or a negative error.
#[no_mangle]
pub unsafe extern "C" fn vc1_set_extradata(h: *mut Handle, p: *const u8, n: usize) -> i32 {
    let h = &mut *h;
    match h.dec.set_extradata(slice(p, n)) {
        Ok(()) => 0,
        Err(e) => err_code(e),
    }
}

/// Decode one access unit (container packet or Annex G unit run). Returns 0 or a negative error.
#[no_mangle]
pub unsafe extern "C" fn vc1_decode(h: *mut Handle, p: *const u8, n: usize, pts_lo: u32, pts_hi: i32) -> i32 {
    let h = &mut *h;
    let pts = ((pts_hi as i64) << 32) | pts_lo as i64;
    match h.dec.decode(slice(p, n), pts) {
        Ok(()) => 0,
        Err(e) => {
            h.last_error = err_code(e);
            h.last_error
        }
    }
}

/// Feed a chunk of raw Annex G byte stream that contains whole access units. Returns the number
/// of access units decoded. Timestamps are assigned as `pts_lo + index`.
#[no_mangle]
pub unsafe extern "C" fn vc1_decode_annexg(h: *mut Handle, p: *const u8, n: usize, pts_lo: u32) -> i32 {
    let h = &mut *h;
    h.dec.decode_annexg(slice(p, n), |i| pts_lo as i64 + i as i64) as i32
}

#[no_mangle]
pub unsafe extern "C" fn vc1_flush(h: *mut Handle) {
    let h = &mut *h;
    h.dec.flush();
}

#[no_mangle]
pub unsafe extern "C" fn vc1_reset(h: *mut Handle) {
    let h = &mut *h;
    h.dec.reset();
    h.frame = None;
}

/// Pops the next decoded frame into the "current frame" slot. Returns 1 if there is one.
#[no_mangle]
pub unsafe extern "C" fn vc1_next_frame(h: *mut Handle) -> i32 {
    let h = &mut *h;
    h.frame = h.dec.take_frame();
    h.frame.is_some() as i32
}

/// Query the current frame: 0 width, 1 height, 2 flags, 3 repeat_pict, 4 pts_lo, 5 pts_hi,
/// 6 picture type (ASCII), 7 repeat_frames, 8 stride Y, 9 stride U, 10 stride V.
#[no_mangle]
pub unsafe extern "C" fn vc1_frame_info(h: *mut Handle, what: i32) -> i32 {
    let h = &*h;
    let f = match &h.frame {
        Some(f) => f,
        None => return 0,
    };
    match what {
        0 => f.width as i32,
        1 => f.height as i32,
        2 => {
            (f.interlaced as i32 * FLAG_INTERLACED)
                | (f.top_field_first as i32 * FLAG_TFF)
                | (f.repeat_first_field as i32 * FLAG_RFF)
                | (f.keyframe as i32 * FLAG_KEY)
                | (f.coded_as_fields as i32 * FLAG_FIELD_CODED)
        }
        3 => f.repeat_pict as i32,
        4 => f.pts as u32 as i32,
        5 => (f.pts >> 32) as i32,
        6 => f.pict_type as i32,
        7 => f.repeat_frames as i32,
        8..=10 => f.plane(what as usize - 8).1 as i32,
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn vc1_frame_plane(h: *mut Handle, plane: i32) -> *const u8 {
    let h = &*h;
    match &h.frame {
        Some(f) if (0..3).contains(&plane) => f.plane(plane as usize).0.as_ptr(),
        _ => std::ptr::null(),
    }
}

/// Stream info: 0 coded width, 1 coded height, 2 interlace flag, 3 fps num, 4 fps den,
/// 5 aspect num, 6 aspect den, 7 profile, 8 level; returns 0 when headers are not yet known.
#[no_mangle]
pub unsafe extern "C" fn vc1_stream_info(h: *mut Handle, what: i32) -> i32 {
    let h = &*h;
    match h.dec.info() {
        Some(i) => match what {
            0 => i.width as i32,
            1 => i.height as i32,
            2 => i.interlace as i32,
            3 => i.fps_num as i32,
            4 => i.fps_den as i32,
            5 => i.aspect_num as i32,
            6 => i.aspect_den as i32,
            7 => i.profile as i32,
            8 => i.level as i32,
            _ => 0,
        },
        None => 0,
    }
}

/// Count of macroblock layer errors since creation (corrupt data is concealed, not fatal).
#[no_mangle]
pub unsafe extern "C" fn vc1_error_count(h: *mut Handle) -> i32 {
    (*h).dec.errors as i32
}

/// Split an Annex G byte range into access units. Writes up to `max` (start, end) u32 pairs to
/// `out` and returns the pair count. The final pair is only complete if the host knows the
/// stream continues no further (the host should hold it back until more data arrives).
#[no_mangle]
pub unsafe extern "C" fn vc1_split(p: *const u8, n: usize, out: *mut u32, max: usize) -> i32 {
    let aus = crate::decoder::split_access_units(slice(p, n));
    let mut k = 0;
    for (a, b) in aus.iter().take(max) {
        *out.add(2 * k) = *a as u32;
        *out.add(2 * k + 1) = *b as u32;
        k += 1;
    }
    k as i32
}
