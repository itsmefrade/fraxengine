//! Pure Rust DTS-UHD (ETSI TS 103 491) parser and ACE decoder.
//!
//! See DESIGN.md for the stream structure, what is verified and what the specification
//! leaves undefined (the full bandwidth channels need `compute_unmasked_levels`, which is
//! not in the published text).
#![allow(clippy::excessive_precision, clippy::needless_range_loop, clippy::too_many_arguments, clippy::type_complexity)]
pub mod ace;
pub mod aceframe;
pub mod bench;
pub mod bitreader;
pub mod config;
pub mod decoder;
pub mod fixed;
pub mod ftoc;
pub mod layout;
pub mod lfe;
pub mod metadata;
pub mod tables_gen;
pub mod transform;
pub mod vq;
pub mod wasm;

pub use decoder::{ChannelStatus, DecodedFrame, Decoder, StreamInfo};
pub use ftoc::ParseError;
