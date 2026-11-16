use core::fmt;

/// Errors returned by the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No AC-3/E-AC-3 sync word found near the start of the packet.
    Sync,
    /// Bitstream id above 16.
    BitstreamId,
    /// Reserved sample rate code.
    SampleRate,
    /// Invalid or inconsistent frame size.
    FrameSize,
    /// Reserved E-AC-3 frame type.
    FrameType,
    /// The packet ends before the frame does.
    Incomplete,
    /// CRC mismatch (only reported in strict mode).
    Crc,
    /// A valid stream feature this decoder does not implement
    /// (enhanced coupling, reduced sample rates).
    Unsupported(&'static str),
    /// The frame contents are inconsistent or out of range.
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sync => f.write_str("no AC-3 sync word"),
            Error::BitstreamId => f.write_str("invalid bitstream id"),
            Error::SampleRate => f.write_str("invalid sample rate code"),
            Error::FrameSize => f.write_str("invalid frame size"),
            Error::FrameType => f.write_str("invalid frame type"),
            Error::Incomplete => f.write_str("incomplete frame"),
            Error::Crc => f.write_str("frame CRC mismatch"),
            Error::Unsupported(what) => write!(f, "unsupported feature: {what}"),
            Error::Invalid(what) => write!(f, "invalid bitstream: {what}"),
        }
    }
}

impl std::error::Error for Error {}
