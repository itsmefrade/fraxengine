//! Playback control core: generation-token session, rational media clock and a
//! PTS ordered bounded frame scheduler. Dependency-free and host independent.

pub mod clock;
pub mod scheduler;
pub mod session;
