//! Raw C-ABI exports (no wasm-bindgen) so the decoder can be driven from a few
//! lines of JavaScript.
#![allow(static_mut_refs)]

use aether_ac3::{DecodedFrame, Decoder};

static mut DECODER: Option<Decoder> = None;
static mut FRAME: Option<DecodedFrame> = None;

/// Allocates `n` bytes in wasm memory for the host to write into.
#[no_mangle]
pub extern "C" fn ac3_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[no_mangle]
/// `ffmpeg_compat != 0` reproduces FFmpeg's overlap-buffer glitch (see
/// `Decoder::set_ffmpeg_bug_compat`); needed to match FFmpeg dumps exactly.
pub extern "C" fn ac3_init(ffmpeg_compat: i32) {
    unsafe {
        let mut d = Decoder::new();
        d.set_ffmpeg_bug_compat(ffmpeg_compat != 0);
        DECODER = Some(d);
        FRAME = Some(DecodedFrame::default());
    }
}

/// Decodes one access unit. Returns samples per channel, or a negative value
/// on error.
#[no_mangle]
pub extern "C" fn ac3_decode(ptr: *const u8, len: usize) -> i32 {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    unsafe {
        let (Some(dec), Some(frame)) = (DECODER.as_mut(), FRAME.as_mut()) else { return -1 };
        match dec.decode_into(data, frame) {
            Ok(()) => frame.frames as i32,
            Err(_) => -2,
        }
    }
}

/// Pointer to the planar f32 output of the last decoded frame.
#[no_mangle]
pub extern "C" fn ac3_output_ptr() -> *const f32 {
    unsafe { FRAME.as_ref().map_or(std::ptr::null(), |f| f.planar.as_ptr()) }
}

#[no_mangle]
pub extern "C" fn ac3_channels() -> i32 {
    unsafe { FRAME.as_ref().map_or(0, |f| f.channels() as i32) }
}

#[no_mangle]
pub extern "C" fn ac3_sample_rate() -> i32 {
    unsafe { FRAME.as_ref().map_or(0, |f| f.sample_rate as i32) }
}

/// Whether the last frame signalled Dolby Atmos (JOC).
#[no_mangle]
pub extern "C" fn ac3_is_joc() -> i32 {
    unsafe { FRAME.as_ref().map_or(0, |f| f.is_joc as i32) }
}

/// Length in bytes of the access unit (independent frame plus its dependent
/// frames) at the start of `ptr..ptr+len`, or 0 when there is none.
#[no_mangle]
pub extern "C" fn ac3_first_au_len(ptr: *const u8, len: usize) -> usize {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    let mut pos = 0usize;
    let mut started = false;
    while pos + 7 <= data.len() {
        let Ok(info) = aether_ac3::parse_frame_info(&data[pos..]) else { return 0 };
        if pos + info.frame_size > data.len() {
            return 0;
        }
        if started && info.frame_type != 1 {
            break;
        }
        started = true;
        pos += info.frame_size;
    }
    pos
}
