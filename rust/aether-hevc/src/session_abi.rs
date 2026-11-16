//! Handle registry avoids exposing native decoder pointers to JavaScript. Byte
//! buffers follow the existing adapter allocation/free ownership contract.
use crate::{
    MAX_INPUT, encode_result,
    session::{SessionError, VideoSession},
};
use std::{cell::RefCell, collections::BTreeMap};
enum Managed {
    Plain(Box<VideoSession>),
}
struct Registry {
    next: u32,
    sessions: BTreeMap<u32, Managed>,
}
thread_local! { static REGISTRY: RefCell<Registry> = const { RefCell::new(Registry {next: 1,sessions: BTreeMap::new()}) }; }
fn failure(error: SessionError) -> Box<[u8]> {
    let code = match error {
        SessionError::Epoch => 2,
        SessionError::Finished => 3,
        SessionError::Poisoned => 4,
        SessionError::Backpressure => 5,
        SessionError::Nal => 6,
        SessionError::Decoder(_) => 7,
        SessionError::EpochMustAdvance => 8,
    };
    let mut bytes = encode_result(Err(format!("{error:?}")));
    bytes[4..8].copy_from_slice(&u32::to_le_bytes(code));
    bytes
}
fn access(
    handle: u32,
    operation: impl FnOnce(&mut VideoSession) -> Result<Vec<rust_h265::Frame>, SessionError>,
) -> *mut u8 {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        let result = match r.sessions.get_mut(&handle) {
            Some(Managed::Plain(s)) => match operation(s) {
                Ok(frames) => encode_result(Ok(frames)),
                Err(e) => failure(e),
            },
            None => encode_result(Err("unknown session handle".into())),
        };
        Box::into_raw(result).cast()
    })
}
fn new_session(session: VideoSession) -> u32 {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        if r.sessions.len() >= 2 || r.next == u32::MAX {
            return 0;
        }
        let id = r.next;
        r.next += 1;
        r.sessions.insert(id, Managed::Plain(Box::new(session)));
        id
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_video_session_new(epoch: u32) -> u32 {
    new_session(VideoSession::new(epoch))
}
/// Session whose pictures are exposed in place by `aether_video_session_next_ref`.
#[unsafe(no_mangle)]
pub extern "C" fn aether_hevc_session_new_ref(epoch: u32) -> u32 {
    new_session(VideoSession::new_ref(epoch))
}
/// Next picture of a reference session without copying its samples. The result is
/// `[len, status, count]` (as the other calls) and, when count is 1, 17 u32:
/// width, height, bitDepth, chromaFormat, poc, then for Y, U and V: byte pointer
/// into linear memory, row stride in samples, plane width, plane rows. The samples are
/// 16-bit and stay valid until the next call on this session; free the result with
/// `aether_video_free(ptr, len)` as usual.
#[unsafe(no_mangle)]
pub extern "C" fn aether_hevc_session_next_ref(handle: u32, epoch: u32) -> *mut u8 {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        let out = match r.sessions.get_mut(&handle) {
            Some(Managed::Plain(s)) => match s.pull_ref(epoch) {
                Ok(None) => encode_result(Ok(Vec::new())),
                Ok(Some(f)) => match (f.plane_u16(0), f.plane_u16(1), f.plane_u16(2)) {
                    (Some(y), Some(u), Some(v)) => {
                        let mut out = Vec::with_capacity(12 + 17 * 4);
                        for x in [(12 + 17 * 4) as u32, 0, 1] {
                            out.extend(x.to_le_bytes());
                        }
                        for x in [f.width, f.height, f.bit_depth as u32, f.chroma_format_idc, f.pic_order_cnt as u32] {
                            out.extend(x.to_le_bytes());
                        }
                        for p in [&y, &u, &v] {
                            for x in [p.data.as_ptr() as usize as u32, p.stride as u32, p.width as u32, p.rows as u32] {
                                out.extend(x.to_le_bytes());
                            }
                        }
                        out.into_boxed_slice()
                    }
                    _ => encode_result(Err("reference output needs 16-bit video".into())),
                },
                Err(e) => failure(e),
            },
            None => encode_result(Err("unknown session handle".into())),
        };
        Box::into_raw(out).cast()
    })
}
/// Session decoding on `threads` threads (pictures come from `aether_video_session_next`, copied).
/// Needs a `threads` build with at least `threads + 1` host workers in `aether_thread_main`.
#[cfg(feature = "threads")]
#[unsafe(no_mangle)]
pub extern "C" fn aether_hevc_session_new_threads(epoch: u32, threads: u32) -> u32 {
    new_session(VideoSession::with_threads(epoch, threads as usize))
}
/// Allocates a stack for a host worker; returns its top (16-byte aligned), the value the
/// worker's instance puts in `__stack_pointer` before calling `aether_thread_main`.
#[cfg(feature = "threads")]
#[unsafe(no_mangle)]
pub extern "C" fn aether_thread_stack(bytes: u32) -> u32 {
    let stack = Box::leak(vec![0u128; bytes as usize / 16].into_boxed_slice());
    let (lo, hi) = (stack.as_ptr() as usize, stack.as_ptr() as usize + stack.len() * 16);
    rust_h265::wasm_register_thread_stack(lo, hi);
    hi as u32
}
/// Body of host worker `index` (0..64): runs decoder tasks forever.
#[cfg(feature = "threads")]
#[unsafe(no_mangle)]
pub extern "C" fn aether_thread_main(index: u32) {
    rust_h265::wasm_thread_main(index as usize)
}
#[cfg(feature = "threads")]
#[unsafe(no_mangle)]
pub extern "C" fn aether_thread_idle() -> u32 {
    rust_h265::wasm_idle_workers()
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_video_session_close(handle: u32) -> u32 {
    REGISTRY.with(|r| u32::from(r.borrow_mut().sessions.remove(&handle).is_none()))
}
/// # Safety
/// ptr/len must name live readable module-owned input allocated by aether_video_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_video_session_push(
    handle: u32,
    ptr: *const u8,
    len: usize,
    epoch: u32,
) -> *mut u8 {
    if ptr.is_null() || len == 0 || len > MAX_INPUT {
        return Box::into_raw(failure(SessionError::Nal)).cast();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    access(handle, |s| s.push_nal(bytes, epoch).map(|()| Vec::new()))
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_video_session_next(handle: u32, epoch: u32) -> *mut u8 {
    access(handle, |s| {
        s.pull(epoch).map(|frame| frame.into_iter().collect())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_video_session_finish(handle: u32, epoch: u32) -> *mut u8 {
    access(handle, |s| s.finish(epoch).map(|()| Vec::new()))
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_video_session_reset(handle: u32, epoch: u32) -> *mut u8 {
    access(handle, |s| s.reset(epoch).map(|()| Vec::new()))
}


/// Plan for a task with a predecessor GOP as prefix (see `plan.rs`). Input: `[count u32]`, then per NAL `[head_len u32]`, then the
/// NAL heads back to back (parameter sets whole, slice NALs cut after 1024 bytes, others at least their two header bytes).
/// Result: `[len, status, count]`, then `[mode u32, skip u32]` and `count` bytes. Mode 0 means decode everything (no flags);
/// mode 1 means push only NALs whose flag byte is 1 and drop the first `skip` output pictures instead of the caller's count.
///
/// # Safety
/// ptr/len must name live readable module-owned input allocated by aether_video_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_hevc_plan(ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let rd = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    let parsed = (|| {
        if len < 4 {
            return None;
        }
        let n = rd(0);
        let mut off = 4 + 4 * n;
        let mut nals: Vec<&[u8]> = Vec::with_capacity(n);
        for i in 0..n {
            let l = rd(4 + 4 * i);
            nals.push(bytes.get(off..off + l)?);
            off += l;
        }
        Some(nals)
    })();
    let plan = parsed.as_ref().and_then(|nals| crate::plan::plan(nals));
    let count = parsed.as_ref().map_or(0, Vec::len);
    let mut out = Vec::new();
    let flags = plan.as_ref().map_or(0, |p| p.keep.len());
    for x in [(12 + 8 + flags) as u32, 0, count as u32, plan.is_some() as u32, plan.as_ref().map_or(0, |p| p.skip)] {
        out.extend(x.to_le_bytes());
    }
    if let Some(p) = &plan {
        out.extend(p.keep.iter().map(|&k| k as u8));
    }
    Box::into_raw(out.into_boxed_slice()).cast()
}
