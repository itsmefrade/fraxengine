//! C ABI exports for wasm32 (no wasm-bindgen): decode packets and run the speed workload.
#![cfg(target_arch = "wasm32")]

use crate::Decoder;

#[no_mangle]
pub extern "C" fn dtsuhd_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `p` must come from `dtsuhd_alloc(n)` with the same `n`.
#[no_mangle]
pub unsafe extern "C" fn dtsuhd_free(p: *mut u8, n: usize) {
    drop(Vec::from_raw_parts(p, 0, n.max(1)));
}

#[no_mangle]
pub extern "C" fn dtsuhd_decoder_new() -> *mut Decoder {
    Box::into_raw(Box::new(Decoder::new()))
}

/// # Safety
/// `d` must come from `dtsuhd_decoder_new`.
#[no_mangle]
pub unsafe extern "C" fn dtsuhd_decoder_free(d: *mut Decoder) {
    drop(Box::from_raw(d));
}

/// Decodes one frame. Writes `channels * frame_len` planar f32 samples to `out` (capacity
/// 16 * 1024 floats) and returns the channel count, or a negative value on error.
///
/// # Safety
/// Pointers must be valid for the given lengths.
#[no_mangle]
pub unsafe extern "C" fn dtsuhd_decode(
    d: *mut Decoder,
    data: *const u8,
    len: usize,
    out: *mut f32,
) -> i32 {
    let dec = &mut *d;
    let data = std::slice::from_raw_parts(data, len);
    match dec.decode_frame(data) {
        Ok(f) => {
            let mut k = 0usize;
            for ch in f.pcm.iter().take(16) {
                for &v in ch.iter().take(1024) {
                    *out.add(k) = v;
                    k += 1;
                }
            }
            f.pcm.len().min(16) as i32
        }
        Err(_) => -1,
    }
}

/// Synthetic full load workload, returns the output checksum.
#[no_mangle]
pub extern "C" fn dtsuhd_bench_synthetic(frames: u32, seed: u32) -> f64 {
    let (ok, sum) = crate::bench::synthetic_stream_load(frames as usize, seed as u64);
    sum + ok as f64
}
