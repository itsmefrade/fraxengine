//! Raw-ABI exports for `wasm32-unknown-unknown` (no wasm-bindgen). One decoder
//! instance lives in the module; the host copies access units into memory
//! obtained from `joc_alloc` and reads results through the pointer getters.
//!
//! Metadata records are `META_STRIDE` f64 values:
//! `[object, oamd_index, start_sample, bed_start_sample, ramp_duration,
//!   active, gain, x, y, z, snap, bed_label (-1 none, else speaker index),
//!   size_w, size_d, size_h]`.

#![allow(static_mut_refs)]

use crate::{ObjectDecoder, ObjectFrame};
use aether_ac3::{DecodedFrame, Decoder};

pub const META_STRIDE: usize = 15;

struct State {
    ac3: Decoder,
    joc: ObjectDecoder,
    bed: DecodedFrame,
    out: ObjectFrame,
    meta: Vec<f64>,
    units: Vec<u32>,
}

static mut STATE: Option<State> = None;

fn st() -> &'static mut State {
    unsafe {
        if STATE.is_none() {
            STATE = Some(State {
                ac3: Decoder::new(),
                joc: ObjectDecoder::new(),
                bed: DecodedFrame::default(),
                out: ObjectFrame::default(),
                meta: Vec::new(),
                units: Vec::new(),
            });
        }
        STATE.as_mut().unwrap()
    }
}

#[no_mangle]
pub extern "C" fn joc_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn joc_init() {
    let s = st();
    s.ac3 = Decoder::new();
    s.joc = ObjectDecoder::new();
}

/// Splits `len` bytes at `ptr` into access units; returns the count. The
/// `(offset, length)` pairs are readable through `joc_units_ptr`.
#[no_mangle]
pub extern "C" fn joc_split(ptr: *const u8, len: usize) -> usize {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    let s = st();
    s.units.clear();
    for au in aether_ac3::access_units(data) {
        s.units.push((au.as_ptr() as usize - ptr as usize) as u32);
        s.units.push(au.len() as u32);
    }
    s.units.len() / 2
}

#[no_mangle]
pub extern "C" fn joc_units_ptr() -> *const u32 {
    st().units.as_ptr()
}

/// Decodes one access unit. `with_objects == 0` decodes the bed only.
/// Returns the number of objects (0 for the bed only or a stream without
/// JOC), or a negative error code.
#[no_mangle]
pub extern "C" fn joc_decode(ptr: *const u8, len: usize, with_objects: i32) -> i32 {
    let au = unsafe { std::slice::from_raw_parts(ptr, len) };
    let s = st();
    if s.ac3.decode_into(au, &mut s.bed).is_err() {
        return -1;
    }
    if with_objects == 0 || s.bed.frames == 0 {
        s.out.num_objects = 0;
        return 0;
    }
    match s.joc.process(au, &s.bed) {
        Ok(f) => {
            s.out = f;
            s.meta.clear();
            for m in &s.out.metadata {
                s.meta.extend_from_slice(&[
                    m.object as f64,
                    m.oamd_index as f64,
                    m.start_sample as f64,
                    m.bed_start_sample as f64,
                    m.ramp_duration as f64,
                    m.state.active as u8 as f64,
                    m.state.gain as f64,
                    m.state.pos[0] as f64,
                    m.state.pos[1] as f64,
                    m.state.pos[2] as f64,
                    m.state.snap as u8 as f64,
                    -1.0,
                    m.state.size[0] as f64,
                    m.state.size[1] as f64,
                    m.state.size[2] as f64,
                ]);
            }
            s.out.num_objects as i32
        }
        Err(_) => -2,
    }
}

/// Planar object PCM: `objects * frames` floats.
#[no_mangle]
pub extern "C" fn joc_objects_ptr() -> *const f32 {
    st().out.pcm.as_ptr()
}

#[no_mangle]
pub extern "C" fn joc_frames() -> usize {
    st().out.frames
}

#[no_mangle]
pub extern "C" fn joc_lfe_ptr() -> *const f32 {
    st().out.lfe.as_ptr()
}

#[no_mangle]
pub extern "C" fn joc_meta_count() -> usize {
    st().meta.len() / META_STRIDE
}

#[no_mangle]
pub extern "C" fn joc_meta_ptr() -> *const f64 {
    st().meta.as_ptr()
}

/// Planar bed PCM of the last access unit and its channel count.
#[no_mangle]
pub extern "C" fn joc_bed_ptr() -> *const f32 {
    st().bed.planar.as_ptr()
}

#[no_mangle]
pub extern "C" fn joc_bed_channels() -> usize {
    st().bed.channels()
}

#[no_mangle]
pub extern "C" fn joc_bed_frames() -> usize {
    st().bed.frames
}
