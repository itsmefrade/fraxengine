//! Output renderer exports (`aether_output_*`): speaker downmix, binaural, limiter
//! over `aether_audio::output`. Several outputs can be open at once (handles start at 1).
//!
//! Zero-copy block processing: the host writes the planar f32 input straight into the module's
//! memory at `aether_output_input_ptr` (plane `c` starts at `c * max_frames` floats), calls
//! `aether_output_process(handle, frames)` and reads planar f32 from `aether_output_output_ptr`
//! (plane `o` starts at `o * max_frames`). Both buffers are allocated at creation and never move.
//! Memory growth elsewhere can detach the JS `ArrayBuffer`, so re-create typed array views from
//! `memory.buffer` after any call that may allocate (everything except `aether_output_process`).

use crate::{input, set_error, set_result};
use aether_audio::output::{Mode, Options, Output, OutputPlan, PlanKind};
use std::cell::RefCell;

struct Slot {
    out: Output,
    input: Vec<f32>,
    output: Vec<f32>,
    max: usize,
}

thread_local! {
    static OUTPUTS: RefCell<Vec<Option<Box<Slot>>>> = const { RefCell::new(Vec::new()) };
}

fn with<T>(handle: i32, f: impl FnOnce(&mut Slot) -> T) -> Option<T> {
    OUTPUTS.with_borrow_mut(|v| {
        let idx = usize::try_from(handle).ok()?.checked_sub(1)?;
        v.get_mut(idx)?.as_deref_mut().map(f)
    })
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push(' '),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Creates an output stage. `labels` is the comma separated canonical channel layout of the input
/// planes (the `layout` of an audio block). `mode`: 0 speakers, 1 headphones. `flags`: bit 0
/// limiter on, bit 1 virtualise plain stereo through the HRTF (headphones). `lfe_gain` is the LFE
/// level folded into a stereo or mono mix; a negative value picks the mode default (dropped for
/// speakers, 0.5 for headphones). `max_frames` bounds one process call (0 = 8192).
/// Returns a handle (>= 1) or a negative status with `aether_last_error` set.
///
/// # Safety
/// `labels_ptr` must reference `labels_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_output_create(
    labels_ptr: *const u8,
    labels_len: usize,
    device_channels: u32,
    mode: u32,
    sample_rate: u32,
    flags: u32,
    lfe_gain: f32,
    max_frames: u32,
) -> i32 {
    let text = match std::str::from_utf8(unsafe { input(labels_ptr, labels_len) }) {
        Ok(t) => t,
        Err(_) => {
            set_error("channel labels are not UTF-8");
            return -1;
        }
    };
    let labels: Vec<&str> = text.split(',').map(str::trim).filter(|l| !l.is_empty()).collect();
    let mode = match mode {
        0 => Mode::Speakers,
        1 => Mode::Headphones,
        m => {
            set_error(format!("unknown output mode {m}"));
            return -1;
        }
    };
    let defaults = Options::default();
    let options = Options {
        limiter: flags & 1 != 0,
        virtualize_stereo: flags & 2 != 0,
        lfe_gain: (lfe_gain >= 0.0).then_some(lfe_gain),
        sample_rate,
        max_frames: if max_frames == 0 { defaults.max_frames } else { (max_frames as usize).min(1 << 16) },
        ..defaults
    };
    let built = OutputPlan::with_options(&labels, device_channels as usize, mode, options).and_then(|p| p.build());
    let out = match built {
        Ok(o) => o,
        Err(e) => {
            set_error(e.to_string());
            return -2;
        }
    };
    let max = options.max_frames;
    let slot = Box::new(Slot { input: vec![0.0; out.input_channels() * max], output: vec![0.0; out.output_channels() * max], out, max });
    OUTPUTS.with_borrow_mut(|v| {
        if let Some(i) = v.iter().position(Option::is_none) {
            v[i] = Some(slot);
            i as i32 + 1
        } else {
            v.push(Some(slot));
            v.len() as i32
        }
    })
}

/// Result: JSON `{kind, inLayout, outLayout, inChannels, outChannels, description, peakGain,
/// maxFrames, sampleRate, limiter}`. `kind` is `identity`, `mix` or `binaural`. Returns 0 or -1.
#[unsafe(no_mangle)]
pub extern "C" fn aether_output_info(handle: i32) -> i32 {
    let json = with(handle, |s| {
        let plan = s.out.plan();
        let list = |v: &[&str]| format!("[{}]", v.iter().map(|l| json_str(l)).collect::<Vec<_>>().join(","));
        let kind = match plan.kind {
            PlanKind::Identity => "identity",
            PlanKind::Mix => "mix",
            PlanKind::Binaural => "binaural",
        };
        format!(
            "{{\"kind\":\"{kind}\",\"inLayout\":{},\"outLayout\":{},\"inChannels\":{},\"outChannels\":{},\"description\":{},\"peakGain\":{},\"maxFrames\":{},\"sampleRate\":{},\"limiter\":{}}}",
            list(&plan.source),
            list(&plan.out_layout),
            plan.source.len(),
            plan.out_layout.len(),
            json_str(&plan.description),
            plan.peak_gain(),
            s.max,
            plan.options.sample_rate,
            plan.options.limiter
        )
    });
    match json {
        Some(j) => {
            set_result(j.into_bytes());
            0
        }
        None => {
            set_error("no such output");
            -1
        }
    }
}

/// Address of the planar f32 input buffer (`inChannels * maxFrames` floats), 0 for a bad handle.
#[unsafe(no_mangle)]
pub extern "C" fn aether_output_input_ptr(handle: i32) -> *mut f32 {
    with(handle, |s| s.input.as_mut_ptr()).unwrap_or(std::ptr::null_mut())
}

/// Address of the planar f32 output buffer (`outChannels * maxFrames` floats), 0 for a bad handle.
#[unsafe(no_mangle)]
pub extern "C" fn aether_output_output_ptr(handle: i32) -> *const f32 {
    with(handle, |s| s.output.as_ptr()).unwrap_or(std::ptr::null())
}

/// Renders `frames` frames from the input buffer into the output buffer. Returns 0, -1 for a bad
/// handle or -3 when `frames` exceeds `maxFrames`. Never allocates.
#[unsafe(no_mangle)]
pub extern "C" fn aether_output_process(handle: i32, frames: u32) -> i32 {
    match with(handle, |s| {
        let max = s.max;
        s.out.process_planar(&s.input, max, frames as usize, &mut s.output, max)
    }) {
        None => -1,
        Some(Ok(())) => 0,
        Some(Err(_)) => -3,
    }
}

/// Forgets filter tails and limiter state (after a seek).
#[unsafe(no_mangle)]
pub extern "C" fn aether_output_reset(handle: i32) -> i32 {
    with(handle, |s| s.out.reset()).map_or(-1, |()| 0)
}

/// Result: JSON `{"frames":n,"deepestDb":x}`, the frames the limiter reduced and the deepest
/// reduction since the previous call (both 0 with the limiter off).
#[unsafe(no_mangle)]
pub extern "C" fn aether_output_limiter_stats(handle: i32) -> i32 {
    match with(handle, |s| s.out.limiter_stats()) {
        Some((f, d)) => {
            set_result(format!("{{\"frames\":{f},\"deepestDb\":{d}}}").into_bytes());
            0
        }
        None => -1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_output_close(handle: i32) {
    OUTPUTS.with_borrow_mut(|v| {
        if let Some(idx) = usize::try_from(handle).ok().and_then(|h| h.checked_sub(1))
            && let Some(slot) = v.get_mut(idx)
        {
            *slot = None;
        }
    });
}
