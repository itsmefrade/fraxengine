//! VC-1 Advanced Profile video decoder session (aether-vc1). Thin `aether_vc1_*` exports over the crate's own C ABI
//! (`aether_vc1::abi`), so one name space stays in this module. A handle is one decoder; create one per worker.
//!
//! Typical call order: `create`, `set_extradata` (Matroska CodecPrivate), then per packet `decode` and a
//! `next_frame` loop reading `frame_info` / `frame_plane` (pointers stay valid until the next decode, flush or reset),
//! `reset` after a seek, `flush` at the end of the stream. Planes are 8 bit 4:2:0 in display order.

use aether_vc1::abi::{self, Handle};

/// Decoder handle (pointer in linear memory), 0 when allocation failed.
#[unsafe(no_mangle)]
pub extern "C" fn aether_vc1_create() -> *mut Handle {
    abi::vc1_create()
}

/// # Safety
/// `h` must come from `aether_vc1_create` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_destroy(h: *mut Handle) {
    unsafe { abi::vc1_destroy(h) }
}

/// Scratch allocation for packet bytes (same allocator as `aether_alloc`).
#[unsafe(no_mangle)]
pub extern "C" fn aether_vc1_alloc(n: usize) -> *mut u8 {
    abi::vc1_alloc(n)
}

/// # Safety
/// `p` and `n` must be a pair from `aether_vc1_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_free(p: *mut u8, n: usize) {
    unsafe { abi::vc1_free(p, n) }
}

/// Matroska CodecPrivate / ASF extra data. 0 or a negative error (-1 invalid, -2 unsupported profile, -3 no sequence header).
///
/// # Safety
/// `h` is a live handle, `p` references `n` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_set_extradata(h: *mut Handle, p: *const u8, n: usize) -> i32 {
    unsafe { abi::vc1_set_extradata(h, p, n) }
}

/// Decodes one access unit (a container packet). 0 or a negative error; the pts comes back with the frame.
///
/// # Safety
/// `h` is a live handle, `p` references `n` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_decode(h: *mut Handle, p: *const u8, n: usize, pts_lo: u32, pts_hi: i32) -> i32 {
    unsafe { abi::vc1_decode(h, p, n, pts_lo, pts_hi) }
}

/// Drains the delayed reference picture at the end of a stream (or before a seek that keeps the headers).
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_flush(h: *mut Handle) {
    unsafe { abi::vc1_flush(h) }
}

/// Forgets references and queued frames (seek). Sequence headers are kept.
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_reset(h: *mut Handle) {
    unsafe { abi::vc1_reset(h) }
}

/// Pops the next display-order frame into the current slot: 1 if there is one.
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_next_frame(h: *mut Handle) -> i32 {
    unsafe { abi::vc1_next_frame(h) }
}

/// Current frame: 0 width, 1 height, 2 flags (1 interlaced, 2 top field first, 4 repeat first field, 8 key, 16 field coded),
/// 3 repeat_pict, 4 pts_lo, 5 pts_hi, 6 picture type (ASCII), 7 repeat_frames, 8 / 9 / 10 stride of Y / U / V.
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_frame_info(h: *mut Handle, what: i32) -> i32 {
    unsafe { abi::vc1_frame_info(h, what) }
}

/// Plane pointer (0 Y, 1 U, 2 V) of the current frame, valid until the next decode, flush or reset.
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_frame_plane(h: *mut Handle, plane: i32) -> *const u8 {
    unsafe { abi::vc1_frame_plane(h, plane) }
}

/// Stream info once the headers were seen: 0 coded width, 1 height, 2 interlace flag, 3 / 4 fps num / den, 5 / 6 aspect num / den, 7 profile, 8 level.
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_stream_info(h: *mut Handle, what: i32) -> i32 {
    unsafe { abi::vc1_stream_info(h, what) }
}

/// Macroblock layer errors since creation (corrupt data is concealed, not fatal).
///
/// # Safety
/// `h` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_vc1_error_count(h: *mut Handle) -> i32 {
    unsafe { abi::vc1_error_count(h) }
}
