//! Bit-exact software HEVC/Main10 raw-plane decoder (single-threaded unless built with the `threads`
//! feature): a speed-optimised fork of the pinned
//! `rust_h265` (vendored in `vendor/rust_h265`, see `NOTICE`) with the session API of the project's earlier `hevc-rust` prototype.
//! Not the complete Aether port and not a native Dolby output implementation.
//!
//! What differs from the experiment's decoder, all required to leave every output sample unchanged:
//! - Reference output (`session::VideoSession::new_ref`, `aether_hevc_session_*_ref`): pictures are read in place
//!   from the decoded picture buffer, so a plane is copied once (by the host) instead of three times.
//! - 128-bit SIMD kernels (`simd128` on WebAssembly, NEON on AArch64, portable code elsewhere or with the
//!   `scalar` feature) for motion compensation, intra prediction, residual addition, inverse DCT and luma
//!   deblocking of 16-bit samples; a streamlined residual-coding parser with batched bypass decoding.
//! - Default-weight explicit weighted prediction takes the default prediction path; merge candidates are
//!   built only up to the selected index; table fills and intra reference construction avoid per-sample work.
pub mod plan;
pub mod session;
#[cfg(target_arch = "wasm32")]
mod session_abi;
#[cfg(all(target_arch = "wasm32", feature = "recon-stream"))]
mod recon_abi;
use rust_h265::{DecodeLimits, Decoder, Frame, PixelData, parse_annex_b};

/// With `threads`, all host workers allocate from std's allocator, whose lock is a no-op in the
/// prebuilt single-thread std: serialise it here.
#[cfg(all(target_arch = "wasm32", feature = "threads"))]
mod locked_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Locked;
    static BUSY: AtomicBool = AtomicBool::new(false);
    fn with<R>(f: impl FnOnce() -> R) -> R {
        while BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        let r = f();
        BUSY.store(false, Ordering::Release);
        r
    }
    unsafe impl GlobalAlloc for Locked {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            with(|| unsafe { System.alloc(l) })
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            with(|| unsafe { System.alloc_zeroed(l) })
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            with(|| unsafe { System.dealloc(p, l) })
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            with(|| unsafe { System.realloc(p, l, n) })
        }
    }
    #[global_allocator]
    static A: Locked = Locked;
}

pub const MAX_INPUT: usize = 8 * 1024 * 1024;
pub const MAX_FRAMES: usize = 3;

pub fn decode(bytes: &[u8], max_frames: usize) -> Result<Vec<Frame>, String> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT {
        return Err("input must contain 1..8MiB Annex B bytes".into());
    }
    if !(1..=MAX_FRAMES).contains(&max_frames) {
        return Err("prototype returns at most three frames".into());
    }
    let limits = DecodeLimits {
        max_picture_pixels: 3840 * 2160,
        max_picture_memory_bytes: 256 * 1024 * 1024,
    };
    let mut decoder = Decoder::with_limits(limits)
        .with_display_order()
        .with_threads(1);
    let mut frames = Vec::new();
    for nal in parse_annex_b(bytes) {
        decoder
            .decode_nal(&nal)
            .map_err(|e| format!("HEVC decode: {e:?}"))?;
        while let Some(frame) = decoder.next_output() {
            frames.push(frame);
            if frames.len() == max_frames {
                return Ok(frames);
            }
        }
    }
    while let Some(frame) = decoder.flush() {
        frames.push(frame);
        if frames.len() == max_frames {
            break;
        }
    }
    if frames.is_empty() {
        return Err("no decoded pictures".into());
    }
    Ok(frames)
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}
#[allow(dead_code)]
fn samples(plane: &PixelData) -> Vec<u16> {
    match plane {
        PixelData::U8(v) => v.iter().map(|&x| x as u16).collect(),
        PixelData::U16(v) => v.clone(),
    }
}

/// Binary ABI: byteLength, status, frameCount, followed by eight u32 fields and U16 planes per frame.
fn append_plane(out: &mut Vec<u8>, plane: &PixelData) {
    match plane {
        PixelData::U8(values) => {
            for &value in values {
                out.extend_from_slice(&u16::from(value).to_le_bytes());
            }
        }
        PixelData::U16(values) => {
            #[cfg(target_endian = "little")]
            {
                // SAFETY: u16 has exactly two initialized bytes and no padding.
                // The immutable plane stays alive for this copy; out is a separate
                // owned allocation. Byte alignment is one and len fits the slice.
                let bytes = unsafe {
                    std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), values.len() * 2)
                };
                out.extend_from_slice(bytes);
            }
            #[cfg(target_endian = "big")]
            for &value in values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
}
fn encode_result(result: Result<Vec<Frame>, String>) -> Box<[u8]> {
    let capacity = match &result {
        Ok(frames) => {
            12 + frames
                .iter()
                .map(|f| 32 + 2 * (f.y.len() + f.u.len() + f.v.len()))
                .sum::<usize>()
        }
        Err(message) => 12 + message.len(),
    };
    let mut out = Vec::with_capacity(capacity);
    push_u32(&mut out, capacity as u32);
    match result {
        Ok(frames) => {
            push_u32(&mut out, 0);
            push_u32(&mut out, frames.len() as u32);
            for frame in frames {
                for value in [
                    frame.width,
                    frame.height,
                    frame.bit_depth as u32,
                    frame.chroma_format_idc,
                    frame.pic_order_cnt as u32,
                    frame.y.len() as u32,
                    frame.u.len() as u32,
                    frame.v.len() as u32,
                ] {
                    push_u32(&mut out, value);
                }
                append_plane(&mut out, &frame.y);
                append_plane(&mut out, &frame.u);
                append_plane(&mut out, &frame.v);
            }
        }
        Err(message) => {
            push_u32(&mut out, 1);
            push_u32(&mut out, 0);
            out.extend(message.as_bytes());
        }
    }
    debug_assert_eq!(out.len(), capacity);
    out.into_boxed_slice()
}

/// Caller frees this allocation with its exact length; only 1..8MiB inputs are accepted.
#[unsafe(no_mangle)]
pub extern "C" fn aether_video_alloc(len: usize) -> *mut u8 {
    if len == 0 || len > MAX_INPUT {
        return std::ptr::null_mut();
    }
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// # Safety
/// ptr and len must describe a live allocation returned by this module, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_video_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }
    }
}

/// # Safety
/// ptr must reference len readable bytes allocated in this module. Result starts with its u32 length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_video_decode(
    ptr: *const u8,
    len: usize,
    max_frames: usize,
) -> *mut u8 {
    let result = if ptr.is_null() || len == 0 || len > MAX_INPUT {
        Err("invalid input allocation".into())
    } else {
        decode(unsafe { std::slice::from_raw_parts(ptr, len) }, max_frames)
    };
    Box::into_raw(encode_result(result)).cast()
}


