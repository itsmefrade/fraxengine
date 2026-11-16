//! Audio decode exports over the `AudioDecoder` trait (TrueHD, DTS, DTS:X, AC-3, E-AC-3, LPCM).

use crate::{input, set_error, set_result};
use aether_audio::decoder::{self, AudioDecoder, Codec, DecodeError};
use aether_audio::layout;
use aether_audio::pcm::{PcmFormat, PcmParams};
use std::cell::RefCell;

thread_local! {
    static FLAGS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static JOC_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static JOC_OBJECTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static DECODER: RefCell<Option<Box<dyn AudioDecoder>>> = const { RefCell::new(None) };
}

/// `codec`: 0 TrueHD (`arg` is the presentation index), 1 DTS / DTS-HD MA / HRA, 2 DTS:X 7.1.4,
/// 3 AC-3, 4 E-AC-3 (both: pure Rust `aether-ac3`, DRC off).
/// Replaces any open decoder. Returns 0 or a negative status.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_open(codec: u32, arg: u32) -> i32 {
    let codec = match codec {
        0 => Codec::TrueHd { presentation: arg.min(255) as u8 },
        1 => Codec::Dts,
        2 => Codec::DtsX,
        3 => Codec::Ac3,
        4 => Codec::Eac3,
        _ => {
            set_error(format!("unknown audio codec id {codec}"));
            return -1;
        }
    };
    open_decoder(codec)
}

fn open_decoder(codec: Codec) -> i32 {
    match decoder::open(codec) {
        Ok(mut d) => {
            d.set_joc(JOC_ON.with(|j| j.get()));
            DECODER.with_borrow_mut(|slot| *slot = Some(d));
            0
        }
        Err(e) => {
            set_error(e.to_string());
            -2
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_close() {
    DECODER.with_borrow_mut(|slot| *slot = None);
}

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_push(ptr: *const u8, len: usize) -> i32 {
    DECODER.with_borrow_mut(|slot| match slot.as_mut() {
        None => {
            set_error("no audio decoder open");
            -1
        }
        Some(d) => match d.push_packet(unsafe { input(ptr, len) }) {
            Ok(()) => 0,
            Err(e) => status(e),
        },
    })
}

/// Returns 0 with a block in the result buffer, 1 when more input is needed, or a negative
/// status. Result layout, all u32 LE: `sample_rate, frames, channels, labels_bytes, lossless`,
/// then the comma separated channel labels padded to 4 bytes, then `channels * frames` f32
/// samples, channel after channel. Lossless blocks hold 24-bit integers divided by 2^23
/// (exact in f32).
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_pull() -> i32 {
    DECODER.with_borrow_mut(|slot| {
        let Some(d) = slot.as_mut() else {
            set_error("no audio decoder open");
            return -1;
        };
        let block = match d.pull() {
            Ok(Some(b)) => b,
            Ok(None) => return 1,
            Err(e) => return status(e),
        };
        let labels = block.labels.join(",");
        let lossless = block.lossless;
        FLAGS.with(|f| f.set(u32::from(block.lossless) | u32::from(block.joc) << 1 | u32::from(block.joc_rendered) << 2));
        JOC_OBJECTS.with(|j| j.set(block.joc_objects as u32));
        let planes = block.to_f32();
        let mut out = Vec::with_capacity(20 + labels.len() + 4 + block.frames * planes.len() * 4);
        for w in [block.sample_rate, block.frames as u32, planes.len() as u32, labels.len() as u32, u32::from(lossless)] {
            out.extend(w.to_le_bytes());
        }
        out.extend_from_slice(labels.as_bytes());
        while out.len() % 4 != 0 {
            out.push(0);
        }
        for plane in &planes {
            out.extend(plane.iter().flat_map(|s| s.to_le_bytes()));
        }
        set_result(out);
        0
    })
}

/// Records the error message and maps it to a status code
/// (-100 - TrueHD error, -200 DTS, -300 PCM, -400 AC-3 / E-AC-3).
fn status(e: DecodeError) -> i32 {
    set_error(e.to_string());
    match e {
        DecodeError::TrueHd(e) => -(100 + e as i32),
        DecodeError::Dts(_) => -200,
        DecodeError::Pcm(_) => -300,
        DecodeError::Ac3(_) => -400,
    }
}

/// Opens an LPCM decoder (replaces any open decoder). `format`: 0 signed integer little endian,
/// 1 signed integer big endian, 2 float little endian, 3 float big endian, 4 unsigned integer
/// (8 bit), 5 Blu-ray HDMV LPCM (each packet starts with the 4 byte header, `bits`, `channels`
/// and `sample_rate` are then read from it and the arguments are ignored). `bits` is 8, 16, 24 or
/// 32 for integers, 32 or 64 for floats. `layout_mask` is a WAVE channel mask (0 picks the default
/// layout for `channels`, see `aether_audio::layout`). Returns 0 or a negative status.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_open_pcm(format: u32, bits: u32, channels: u32, sample_rate: u32, layout_mask: u32) -> i32 {
    let format = match format {
        0 => PcmFormat::IntLe,
        1 => PcmFormat::IntBe,
        2 => PcmFormat::FloatLe,
        3 => PcmFormat::FloatBe,
        4 => PcmFormat::UintLe,
        5 => PcmFormat::BluRay,
        _ => {
            set_error(format!("unknown PCM format id {format}"));
            return -1;
        }
    };
    open_decoder(Codec::Pcm(PcmParams {
        format,
        bits: bits.min(255) as u8,
        channels: channels.min(255) as u8,
        sample_rate,
        layout_mask,
    }))
}

/// Result: JSON with the canonical channel layout tables (`aether_audio::layout`):
/// `{"wave":[labels in WAVE order],"smpte":[labels in SMPTE order],"waveBits":{label: bit},
/// "named":{name: [labels]},"defaults":{channels: [labels]}}`. Returns 0.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_layout_table() -> i32 {
    set_result(layout::table_json().into_bytes());
    0
}

/// Flags of the block returned by the last successful `aether_audio_pull`: bit 0 lossless,
/// bit 1 Dolby Atmos (JOC) signalled, bit 2 the block is the JOC objects rendered to 7.1.4 (otherwise
/// the 5.1/7.1 bed).
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_block_flags() -> u32 {
    FLAGS.with(|f| f.get())
}

/// Number of JOC objects decoded into the block of the last successful `aether_audio_pull` (0 for the bed).
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_block_joc_objects() -> u32 {
    JOC_OBJECTS.with(|j| j.get())
}

/// Turns E-AC-3 JOC object rendering on (1, the default) or off (0, the 5.1/7.1 bed is returned).
/// The setting is remembered for decoders opened later and applied to the open one from its next
/// packet on (the block layout changes there). Returns 0, or 1 when the open decoder has no JOC.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_set_joc(enabled: u32) -> i32 {
    JOC_ON.with(|j| j.set(enabled != 0));
    DECODER.with_borrow_mut(|slot| match slot.as_mut() {
        Some(d) => i32::from(!d.set_joc(enabled != 0)),
        None => 0,
    })
}
