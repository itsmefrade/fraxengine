//! Checked rational timestamp conversion and an injected monotonic host clock.
//!
//! Media time is signed nanoseconds. Rational conversion truncates toward zero;
//! negative timestamps are valid. Host time is unsigned monotonic nanoseconds.
//! A seek/discontinuity explicitly installs a new media anchor and epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    InvalidTimebase,
    Overflow,
    HostWentBackwards,
    InvalidState,
    EpochOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timebase {
    numerator: u32,
    denominator: u32,
}
impl Timebase {
    pub fn new(numerator: u32, denominator: u32) -> Result<Self, ClockError> {
        if numerator == 0 || denominator == 0 {
            return Err(ClockError::InvalidTimebase);
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }
    /// Convert signed PTS or DTS ticks to signed nanoseconds without floating point.
    pub fn to_nanos(self, ticks: i64) -> Result<i128, ClockError> {
        (ticks as i128)
            .checked_mul(self.numerator as i128)
            .and_then(|v| v.checked_mul(1_000_000_000))
            .map(|v| v / self.denominator as i128)
            .ok_or(ClockError::Overflow)
    }
    /// Rescale ticks to another timebase, truncating toward zero.
    pub fn rescale(self, ticks: i64, destination: Self) -> Result<i64, ClockError> {
        let n = (ticks as i128)
            .checked_mul(self.numerator as i128)
            .and_then(|v| v.checked_mul(destination.denominator as i128))
            .ok_or(ClockError::Overflow)?;
        let d = (self.denominator as i128)
            .checked_mul(destination.numerator as i128)
            .ok_or(ClockError::Overflow)?;
        i64::try_from(n / d).map_err(|_| ClockError::Overflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockState {
    Running,
    Paused,
}
#[derive(Debug, Clone)]
pub struct MediaClock {
    epoch: u64,
    media_anchor: i128,
    host_anchor: u64,
    last_host: u64,
    state: ClockState,
}
impl MediaClock {
    pub fn new(media_nanos: i128, host_nanos: u64) -> Self {
        Self {
            epoch: 0,
            media_anchor: media_nanos,
            host_anchor: host_nanos,
            last_host: host_nanos,
            state: ClockState::Running,
        }
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn state(&self) -> ClockState {
        self.state
    }
    fn at(&self, host: u64) -> Result<i128, ClockError> {
        if host < self.last_host {
            return Err(ClockError::HostWentBackwards);
        }
        if self.state == ClockState::Paused {
            return Ok(self.media_anchor);
        }
        self.media_anchor
            .checked_add((host - self.host_anchor) as i128)
            .ok_or(ClockError::Overflow)
    }
    /// Observe the current position. Failed observations do not advance host time.
    pub fn position(&mut self, host: u64) -> Result<i128, ClockError> {
        let pos = self.at(host)?;
        self.last_host = host;
        Ok(pos)
    }
    pub fn pause(&mut self, host: u64) -> Result<(), ClockError> {
        if self.state != ClockState::Running {
            return Err(ClockError::InvalidState);
        }
        let pos = self.at(host)?;
        self.media_anchor = pos;
        self.host_anchor = host;
        self.last_host = host;
        self.state = ClockState::Paused;
        Ok(())
    }
    pub fn resume(&mut self, host: u64) -> Result<(), ClockError> {
        if self.state != ClockState::Paused {
            return Err(ClockError::InvalidState);
        }
        if host < self.last_host {
            return Err(ClockError::HostWentBackwards);
        }
        self.host_anchor = host;
        self.last_host = host;
        self.state = ClockState::Running;
        Ok(())
    }
    /// Seek or discontinuity: retain paused/running state and increment epoch.
    pub fn reanchor(&mut self, media_nanos: i128, host: u64) -> Result<u64, ClockError> {
        if host < self.last_host {
            return Err(ClockError::HostWentBackwards);
        }
        let next = self.epoch.checked_add(1).ok_or(ClockError::EpochOverflow)?;
        self.media_anchor = media_nanos;
        self.host_anchor = host;
        self.last_host = host;
        self.epoch = next;
        Ok(next)
    }
}
