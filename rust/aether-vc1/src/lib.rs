//! Pure Rust SMPTE 421M (VC-1) Advanced Profile video decoder.
//!
//! Derived from the FFmpeg VC-1 decoder (libavcodec/vc1*.c), LGPL-2.1-or-later.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments, clippy::collapsible_else_if)]

pub mod abi;
pub mod bits;
pub mod blocks;
pub mod core;
pub mod decoder;
pub mod dsp;
pub mod ilace;
pub mod lf;
pub mod mb;
pub mod mc;
pub mod pred;
#[allow(clippy::all)]
pub mod tables;
pub mod testgen;
pub mod vlcs;

pub use crate::core::Error;
pub use crate::decoder::{Decoder, Frame, StreamInfo};
