//! Raw C ABI for the browser (no wasm-bindgen: the JS side keeps the instance and copies
//! bytes in and out of linear memory).
//!
//! Conventions
//! - Inputs are copied into memory from `aether_alloc` and released with `aether_free`.
//! - Calls that return variable-size data write it to a module-owned result buffer and
//!   return a status; read it with `aether_result_ptr` / `aether_result_len` before the
//!   next call. Status 0 is success, negative values are errors unless stated otherwise.
//! - 64-bit integers cross as `i64` / `u64` (BigInt in JS).
//! - Demux reads call back into the host through the `aether_host_read` import.

use std::cell::RefCell;

pub mod audio;
pub mod demux;
pub mod dovi;
pub mod hdr10plus;
pub mod output;
pub mod player;
pub mod subs;
pub mod vc1;

pub const MAX_ALLOC: usize = 128 * 1024 * 1024;

thread_local! {
    static RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Records the message of the most recent failing call (audio, demux, subtitle and output exports).
pub(crate) fn set_error(message: impl Into<String>) {
    LAST_ERROR.with_borrow_mut(|e| *e = message.into());
}

/// Copies the last error message (UTF-8, truncated to `cap`) into `ptr` and returns the full
/// message length in bytes, so a caller whose buffer was too small can retry. 0 means no error
/// has been recorded. With `ptr` null or `cap` 0 it only reports the length.
///
/// # Safety
/// `ptr` must reference `cap` writable bytes (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_last_error(ptr: *mut u8, cap: usize) -> usize {
    LAST_ERROR.with_borrow(|e| {
        let n = e.len().min(cap);
        if !ptr.is_null() && n > 0 {
            unsafe { std::ptr::copy_nonoverlapping(e.as_ptr(), ptr, n) };
        }
        e.len()
    })
}

/// Forgets the recorded error message.
#[unsafe(no_mangle)]
pub extern "C" fn aether_clear_error() {
    LAST_ERROR.with_borrow_mut(String::clear);
}

pub(crate) fn set_result(bytes: Vec<u8>) {
    RESULT.with_borrow_mut(|r| *r = bytes);
}

pub(crate) fn with_result<T>(f: impl FnOnce(&mut Vec<u8>) -> T) -> T {
    RESULT.with_borrow_mut(f)
}

/// # Safety
/// `ptr` must reference `len` readable bytes (or be null with `len == 0`).
pub(crate) unsafe fn input<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_alloc(len: usize) -> *mut u8 {
    if len == 0 || len > MAX_ALLOC {
        return std::ptr::null_mut();
    }
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// # Safety
/// `ptr` and `len` must come from one `aether_alloc` call and be freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 && len <= MAX_ALLOC {
        unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_result_ptr() -> *const u8 {
    RESULT.with_borrow(|r| r.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_result_len() -> usize {
    RESULT.with_borrow(Vec::len)
}

/// ABI revision; bump when an export changes meaning.
#[unsafe(no_mangle)]
pub extern "C" fn aether_abi_version() -> u32 {
    1
}
