//! Persistent one-layer decoder. Feed complete raw NALs in decode order, drain
//! display-order pictures after each push, and flush only at end of stream.
//! This is not an arbitrary-byte stream parser or generic P7 picture pairer.
use rust_h265::{DecodeLimits, Decoder, Frame, parse_nal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Epoch,
    EpochMustAdvance,
    Finished,
    Poisoned,
    Backpressure,
    Nal,
    Decoder(String),
}
pub struct VideoSession {
    decoder: Decoder,
    pending: Option<Frame>,
    /// The frame `pull_ref` last returned; its samples stay valid until the next call.
    held: Option<Frame>,
    epoch: u32,
    finished: bool,
    poisoned: bool,
    pub nal_count: u64,
    pub output_count: u64,
}
/// (nal_unit_type, nuh_layer_id) of a two-byte HEVC NAL header, None when forbidden_zero_bit is set,
/// the header is truncated or nuh_temporal_id_plus1 is zero.
fn nal_header(bytes: &[u8]) -> Option<(u8, u8)> {
    if bytes.len() < 2 || bytes[0] & 0x80 != 0 || bytes[1] & 7 == 0 {
        return None;
    }
    Some(((bytes[0] >> 1) & 63, ((bytes[0] & 1) << 5) | (bytes[1] >> 3)))
}
fn decoder(ref_out: bool, threads: usize) -> Decoder {
    let d = Decoder::with_limits(DecodeLimits {
        max_picture_pixels: 3840 * 2160,
        max_picture_memory_bytes: 256 * 1024 * 1024,
    })
    .with_display_order()
    .with_threads(threads);
    if ref_out { d.with_ref_output() } else { d }
}
impl VideoSession {
    pub fn new(epoch: u32) -> Self {
        Self::with_mode(epoch, false)
    }
    /// Reference-output session: `pull_ref` exposes pictures without copying them.
    pub fn new_ref(epoch: u32) -> Self {
        Self::with_mode(epoch, true)
    }
    /// Session decoding on `threads` threads (frame threading); `threads` 1 is `new`.
    pub fn with_threads(epoch: u32, threads: usize) -> Self {
        let mut s = Self::with_mode(epoch, false);
        s.decoder = decoder(false, threads.max(1));
        s
    }
    fn with_mode(epoch: u32, ref_out: bool) -> Self {
        Self {
            decoder: decoder(ref_out, 1),
            pending: None,
            held: None,
            epoch,
            finished: false,
            poisoned: false,
            nal_count: 0,
            output_count: 0,
        }
    }
    fn check(&self, epoch: u32) -> Result<(), SessionError> {
        if epoch != self.epoch {
            return Err(SessionError::Epoch);
        }
        if self.poisoned {
            return Err(SessionError::Poisoned);
        }
        Ok(())
    }
    /// Reset requires a strictly newer epoch, so stale callbacks cannot repopulate
    /// a new decoder after seek. At u32::MAX create a fresh Worker/instance.
    pub fn reset(&mut self, epoch: u32) -> Result<(), SessionError> {
        if epoch <= self.epoch {
            return Err(SessionError::EpochMustAdvance);
        }
        // A new decoder state, but the picture buffers of the old one are reused.
        self.release_held();
        self.decoder.restart();
        self.pending = None;
        self.epoch = epoch;
        self.finished = false;
        self.poisoned = false;
        self.nal_count = 0;
        self.output_count = 0;
        Ok(())
    }
    /// Input includes the two-byte NAL header and excludes length/start-code.
    /// Caller must not split a NAL across pushes. A codec/framing error poisons
    /// this session; reset is required. Backpressure/epoch errors preserve it.
    pub fn push_nal(&mut self, bytes: &[u8], epoch: u32) -> Result<(), SessionError> {
        self.check(epoch)?;
        if self.finished {
            return Err(SessionError::Finished);
        }
        self.release_held();
        if self.pending.is_none() {
            self.pending = self.decoder.next_output();
        }
        if self.pending.is_some() {
            return Err(SessionError::Backpressure);
        }
        let header = nal_header(bytes);
        if bytes.is_empty()
            || bytes.len() > super::MAX_INPUT
            || header.is_none()
            || header.is_some_and(|(nal_type, layer_id)| layer_id != 0 || nal_type >= 62)
        {
            self.poisoned = true;
            return Err(SessionError::Nal);
        }
        let Some(nal) = parse_nal(bytes) else {
            self.poisoned = true;
            return Err(SessionError::Nal);
        };
        match self.decoder.decode_nal(&nal) {
            Ok(_) => {
                self.nal_count += 1;
                Ok(())
            }
            Err(error) => {
                self.poisoned = true;
                Err(SessionError::Decoder(format!("{error:?}")))
            }
        }
    }
    pub fn finish(&mut self, epoch: u32) -> Result<(), SessionError> {
        self.check(epoch)?;
        self.finished = true;
        Ok(())
    }
    /// A frame with owned planes (reference-mode frames are copied out and recycled).
    fn owned(&mut self, f: Frame) -> Frame {
        self.decoder.own_frame(f)
    }
    fn release_held(&mut self) {
        if let Some(f) = self.held.take() {
            self.decoder.recycle_frame(f);
        }
    }
    /// Next picture without copying: the frame's samples (`Frame::plane_u16`) stay valid until
    /// the next call on this session. Only for sessions made with `new_ref` and 16-bit video.
    pub fn pull_ref(&mut self, epoch: u32) -> Result<Option<&Frame>, SessionError> {
        self.check(epoch)?;
        self.release_held();
        let frame = if self.pending.is_some() {
            self.pending.take()
        } else if self.finished {
            self.decoder.flush()
        } else {
            self.decoder.next_output()
        };
        if frame.is_some() {
            self.output_count += 1;
        }
        self.held = frame;
        Ok(self.held.as_ref())
    }
    /// Each returned picture has owned planes. They never alias decoder DPB.
    /// EOF flushing is incremental, avoiding accumulation of a whole film.
    pub fn pull(&mut self, epoch: u32) -> Result<Option<Frame>, SessionError> {
        self.check(epoch)?;
        let frame = if self.pending.is_some() {
            self.pending.take()
        } else if self.finished {
            self.decoder.flush()
        } else {
            self.decoder.next_output()
        };
        if frame.is_some() {
            self.output_count += 1;
        }
        let frame = frame.map(|f| self.owned(f));
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_epoch_and_invalid_nal_are_explicit_and_reset_recovers() {
        let mut session = VideoSession::new(4);
        assert_eq!(session.push_nal(&[0x46, 1, 0x50], 3), Err(SessionError::Epoch));
        assert_eq!(session.push_nal(&[0x80, 0], 4), Err(SessionError::Nal));
        assert!(matches!(session.pull(4), Err(SessionError::Poisoned)));
        assert_eq!(session.reset(4), Err(SessionError::EpochMustAdvance));
        session.reset(5).unwrap();
        session.push_nal(&[0x46, 1, 0x50], 5).unwrap();
        session.finish(5).unwrap();
        assert!(session.pull(5).unwrap().is_none());
        assert_eq!(session.push_nal(&[0x46, 1, 0x50], 5), Err(SessionError::Finished));
    }
}
