//! Benchmark entry points for the reconstruction command stream on WebAssembly (feature `recon-stream`,
//! driven by `js/recon-bench.mjs`): one decoder per module, fed NAL by NAL from a buffer the host fills
//! through `aether_video_alloc`. Modes: 0 full decode (reference output, as the player's session), 1 full
//! decode + recording + packing, 2 entropy only (parse + recording + packing, no reconstruction), 3 entropy only
//! with the records dropped unpacked. Below them, the player's entropy-only session (`aether_recon_session_*`).
use rust_h265::{DecodeLimits, Decoder, parse_nal};
use std::cell::RefCell;

struct Bench {
    dec: Decoder,
    pack: bool,
    frames: u32,
    stream_bytes: f64,
    buf: Vec<u32>,
}
thread_local! { static BENCH: RefCell<Option<Bench>> = const { RefCell::new(None) }; }

#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_bench_new(mode: u32) {
    let dec = Decoder::with_limits(DecodeLimits {
        max_picture_pixels: 3840 * 2160,
        max_picture_memory_bytes: 256 * 1024 * 1024,
    })
    .with_display_order()
    .with_ref_output();
    let dec = match mode {
        1 => dec.with_recon_stream(false),
        2 | 3 => dec.with_recon_stream(true),
        _ => dec,
    };
    BENCH.with(|b| *b.borrow_mut() = Some(Bench { dec, pack: mode == 1 || mode == 2, frames: 0, stream_bytes: 0.0, buf: Vec::new() }));
}

fn drain(b: &mut Bench) {
    while let Some(f) = b.dec.next_output() {
        b.dec.recycle_frame(f);
        b.frames += 1;
    }
    let events = b.dec.take_recon_events();
    if b.pack {
        for ev in events {
            b.buf.clear();
            rust_h265::recon::encode_event(&ev, &mut b.buf);
            b.stream_bytes += b.buf.len() as f64 * 4.0;
        }
    }
}

/// # Safety
/// ptr must reference len readable bytes. Returns the frames output so far, or u32::MAX on a decode error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_recon_bench_push(ptr: *const u8, len: usize) -> u32 {
    let raw = unsafe { std::slice::from_raw_parts(ptr, len) };
    BENCH.with(|b| {
        let mut b = b.borrow_mut();
        let b = b.as_mut().expect("aether_recon_bench_new first");
        let Some(nal) = parse_nal(raw) else { return u32::MAX };
        if b.dec.decode_nal(&nal).is_err() {
            return u32::MAX;
        }
        drain(b);
        b.frames
    })
}

/// End of stream: returns the total frame count.
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_bench_finish() -> u32 {
    BENCH.with(|b| {
        let mut b = b.borrow_mut();
        let b = b.as_mut().expect("aether_recon_bench_new first");
        while let Some(f) = b.dec.flush() {
            b.dec.recycle_frame(f);
            b.frames += 1;
        }
        drain(b);
        b.frames
    })
}

/// Packed stream bytes produced so far.
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_bench_stream_bytes() -> f64 {
    BENCH.with(|b| b.borrow().as_ref().map_or(0.0, |b| b.stream_bytes))
}

// ---- Player session (web/player/js/recon-entropy-worker.js) ---------------------------------------------------
//
// One entropy-only decoder per module instance (the player runs one instance per layer and worker): the host pushes
// NAL units one by one, the packed events (FORMAT-STREAM.md, `[type, n, payload]` back to back, without the file
// magic) collect in an output buffer the host reads in place and then clears. A seek is a reset (a fresh decoder);
// a decode error poisons the session until the next reset.

struct Session {
    dec: Decoder,
    out: Vec<u32>,
    poisoned: bool,
}
thread_local! { static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) }; }

fn session_decoder(threads: usize) -> Decoder {
    Decoder::with_limits(DecodeLimits {
        max_picture_pixels: 3840 * 2160,
        max_picture_memory_bytes: 256 * 1024 * 1024,
    })
    .with_display_order()
    .with_ref_output()
    .with_threads(threads)
    .with_recon_stream(true)
}

// Output events are recorded when the decoder hands out a frame (`next_output` / `flush`), so frames are drained
// (and recycled: an entropy-only frame has no samples) before the events are taken.
fn session_drain(s: &mut Session, flush: bool) {
    loop {
        let f = if flush { s.dec.flush() } else { s.dec.next_output() };
        let Some(f) = f else { break };
        s.dec.recycle_frame(f);
    }
    for ev in s.dec.take_recon_events() {
        rust_h265::recon::encode_event(&ev, &mut s.out);
    }
}

/// Creates the session, or replaces it with a fresh decoder (seek to a random access point).
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_session_reset() {
    SESSION.with(|s| *s.borrow_mut() = Some(Session { dec: session_decoder(1), out: Vec::new(), poisoned: false }));
}

/// As `aether_recon_session_reset`, decoding the CTB rows of WPP slices on up to `threads` threads (pictures stay
/// sequential; the stream is the same as with one thread). Needs a `threads` build whose host workers run
/// `aether_thread_main` on stacks from `aether_thread_stack`; rows run inline when no worker is idle.
#[cfg(feature = "threads")]
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_session_reset_threads(threads: u32) {
    let dec = session_decoder(threads.max(1) as usize);
    SESSION.with(|s| *s.borrow_mut() = Some(Session { dec, out: Vec::new(), poisoned: false }));
}

/// # Safety
/// ptr must reference len readable bytes: one complete NAL unit (header included, no start code).
/// Returns 0, 1 for an unparsable NAL, 2 for a decode error, 3 for a poisoned or missing session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_recon_session_push(ptr: *const u8, len: usize) -> u32 {
    let raw = unsafe { std::slice::from_raw_parts(ptr, len) };
    SESSION.with(|s| {
        let mut s = s.borrow_mut();
        let Some(s) = s.as_mut() else { return 3 };
        if s.poisoned {
            return 3;
        }
        let Some(nal) = parse_nal(raw) else {
            s.poisoned = true;
            return 1;
        };
        if s.dec.decode_nal(&nal).is_err() {
            s.poisoned = true;
            return 2;
        }
        session_drain(s, false);
        0
    })
}

/// End of stream: finishes the picture in progress and outputs every picture left. Returns 0 or 3.
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_session_finish() -> u32 {
    SESSION.with(|s| {
        let mut s = s.borrow_mut();
        let Some(s) = s.as_mut() else { return 3 };
        if s.poisoned {
            return 3;
        }
        session_drain(s, true);
        0
    })
}

/// Byte address of the packed events collected since the last clear (valid until the next call into the module).
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_session_out_ptr() -> u32 {
    SESSION.with(|s| s.borrow().as_ref().map_or(0, |s| s.out.as_ptr() as usize as u32))
}

/// Number of u32 words at `aether_recon_session_out_ptr`.
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_session_out_words() -> u32 {
    SESSION.with(|s| s.borrow().as_ref().map_or(0, |s| s.out.len() as u32))
}

/// Drops the collected events (keeps the buffer's capacity).
#[unsafe(no_mangle)]
pub extern "C" fn aether_recon_session_out_clear() {
    SESSION.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.out.clear();
        }
    })
}
