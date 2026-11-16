//! Pure Rust AC-3 and E-AC-3 (Dolby Digital / Dolby Digital Plus) decoder.
//!
//! The decoder is a translation of FFmpeg's float decoder and produces the
//! same output (to float rounding) as `ffmpeg -drc_scale 0`. It has no
//! dependencies, no threads and no unsafe code, and builds for
//! `wasm32-unknown-unknown`.
//!
//! # Supported
//!
//! * AC-3 (bsid 0..=10) and E-AC-3 (bsid 16): independent and dependent
//!   substreams (so 7.1 via a dependent substream), CRC verification.
//! * Exponent and bit allocation decoding, mantissas, coupling,
//!   rematrixing, block switching, IMDCT with the KBD window.
//! * E-AC-3 adaptive hybrid transform (AHT), spectral extension (SPX).
//! * Dynamic range control (off by default, see [`Decoder::set_drc_scale`]).
//! * Detection of Dolby Atmos (JOC) signalling. The object audio is not
//!   decoded; only the 5.1/7.1 bed is.
//!
//! # Not supported (same as FFmpeg)
//!
//! Enhanced coupling, reduced E-AC-3 sample rates, transient pre-noise
//! processing, downmixing, and any substream other than program 0.
//!
//! # Channel order
//!
//! Output planes follow FFmpeg's native channel order, which is ascending
//! `AV_CH_*` bit order: `L R C LFE Lb Rb Lc Rc Cb Ls Rs ...`. Examples:
//!
//! | stream            | planes                          |
//! |-------------------|---------------------------------|
//! | mono              | `C`                             |
//! | stereo            | `L R`                           |
//! | 5.1 (3/2 + LFE)   | `L R C LFE Ls Rs`               |
//! | 7.1 (dependent)   | `L R C LFE Lb Rb Ls Rs`         |
//!
//! Use [`DecodedFrame::layout`] rather than assuming an order.
//!
//! # Example
//!
//! ```no_run
//! use aether_ac3::{access_units, Decoder};
//!
//! let stream = std::fs::read("audio.eac3").unwrap();
//! let mut dec = Decoder::new();
//! for au in access_units(&stream) {
//!     let frame = dec.decode(au).unwrap();
//!     println!("{} ch, {} samples", frame.channels(), frame.frames);
//! }
//! ```

#![allow(clippy::needless_range_loop, clippy::excessive_precision)]

mod bits;
mod decoder;
mod error;
mod header;
mod imdct;
mod layout;
mod lfg;
mod tables;
mod tables_gen;

pub use decoder::{DecodedFrame, Decoder, JocInfo, Stats};
pub use error::Error;
pub use header::{access_units, crc16, find_syncword, frame_crc_ok, parse_frame_info, FrameInfo};
pub use layout::{Channel, ChannelLayout};
