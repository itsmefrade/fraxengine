//! E-AC-3 Joint Object Coding (Dolby Atmos carried in Dolby Digital Plus)
//! object decoder in pure Rust, written from ETSI TS 103 420 V1.2.1 and
//! ETSI TS 102 366 annex H. No unsafe code outside the optional wasm exports,
//! no threads, builds for `wasm32-unknown-unknown`.
//!
//! ```no_run
//! use aether_joc::ObjectDecoder;
//! let stream = std::fs::read("atmos.eac3").unwrap();
//! let mut bed_decoder = aether_ac3::Decoder::new();
//! let mut objects = ObjectDecoder::new();
//! for au in aether_ac3::access_units(&stream) {
//!     let bed = bed_decoder.decode(au).unwrap(); // 5.1 / 7.1 downmix
//!     let o = objects.process(au, &bed).unwrap(); // up to 15 objects + LFE
//!     for k in 0..o.num_objects {
//!         let _pcm: &[f32] = o.object(k); // 1536 samples, LATENCY_SAMPLES behind the bed
//!     }
//!     for m in &o.metadata { /* position, gain, ramp, start_sample */ let _ = m; }
//! }
//! ```
//!
//! Pipeline per access unit: EMDF payload extraction ([`emdf`]), JOC side
//! information ([`joc`]), 64-band complex QMF analysis of the decoded bed
//! ([`qmf`]), interpolated QMF-domain upmix matrix, QMF synthesis, and the
//! object audio metadata timeline ([`oamd`]). The spec has no decorrelators:
//! the JOC upmix is a pure (real valued) matrix operation in the QMF domain.

pub mod analysis;
mod bits;
pub mod decoder;
pub mod emdf;
pub mod joc;
pub mod oamd;
pub mod qmf;
pub mod render;
mod tables_gen;
#[cfg(all(target_arch = "wasm32", feature = "standalone-wasm"))]
pub mod wasm;

pub use decoder::{DecodeError, MetadataUpdate, ObjectDecoder, ObjectFrame, FRAME_SAMPLES, LATENCY_SAMPLES};
