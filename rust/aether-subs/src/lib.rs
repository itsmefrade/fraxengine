//! Subtitle decoding for the browser port.
//!
//! - [`pgs`]: HDMV PGS bitmap subtitles (PCS/WDS/PDS/ODS, RLE, palette to RGBA, epochs, forced
//!   flags, cropping), output is positioned RGBA bitmaps with start and end times.
//! - Text formats ([`text::TextTrack`]): SRT, WebVTT and ASS/SSA are all converted into the one
//!   styled-cue model of [`model`] (SRT and WebVTT markup is rewritten as ASS override text and
//!   goes through the ASS parser), so a renderer needs a single code path.
//!
//! Pure Rust, no dependencies.

pub mod ass;
pub mod json;
pub mod markup;
pub mod model;
pub mod pgs;
pub mod text;

pub use model::{Cue, Rgba, Span, Style, TrackInfo};
pub use pgs::{Display, PgsBitmap, PgsDecoder};
pub use text::{TextFormat, TextTrack};
