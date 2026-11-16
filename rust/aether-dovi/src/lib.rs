//! Bounded HEVC framing and Dolby Vision dual-layer packet handling.
//!
//! `hevc`, `hdr10plus`, `pairing`, `rpu`, `picture_identity`, `source_association` and
//! `vui` need no external crates. The `semantic-rpu` feature (on by default) adds the
//! guarded RPU parse in `semantic`, the display metadata in `display` and the FEL
//! reconstruction parameters in `fel`. This crate does not decode HEVC or establish display support.

pub mod hdr10plus;
pub mod hevc;
pub mod pairing;
pub mod picture_identity;
pub mod rpu;
pub mod source_association;
pub mod vui;

#[cfg(feature = "semantic-rpu")]
mod rpu_guard;
#[cfg(feature = "semantic-rpu")]
pub mod semantic;
#[cfg(feature = "semantic-rpu")]
pub mod display;
#[cfg(feature = "semantic-rpu")]
pub mod fel;
