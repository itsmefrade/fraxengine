//! Compatibility shim re-exporting `aether-dovi` and `aether-player`.

pub use aether_dovi::{hevc, pairing, picture_identity, rpu, source_association};
#[cfg(feature = "semantic-rpu")]
pub use aether_dovi::semantic;
pub use aether_player::{clock, session};
