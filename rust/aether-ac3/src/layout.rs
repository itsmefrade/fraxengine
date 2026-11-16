//! Channel labels and layouts. Discriminants equal FFmpeg's `AV_CHAN_*` /
//! `AV_CH_*` bit positions, and a layout always lists its channels in
//! ascending bit order, which is FFmpeg's "native" channel order.

/// A speaker position. The numeric value is the FFmpeg channel bit index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Channel {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    LowFrequency = 3,
    BackLeft = 4,
    BackRight = 5,
    FrontLeftOfCenter = 6,
    FrontRightOfCenter = 7,
    BackCenter = 8,
    SideLeft = 9,
    SideRight = 10,
    TopCenter = 11,
    TopFrontLeft = 12,
    TopFrontCenter = 13,
    TopFrontRight = 14,
    TopBackLeft = 15,
    TopBackCenter = 16,
    TopBackRight = 17,
    WideLeft = 31,
    WideRight = 32,
    SurroundDirectLeft = 33,
    SurroundDirectRight = 34,
    LowFrequency2 = 35,
}

impl Channel {
    /// Bit index of this channel in a layout mask (`1 << bit`).
    pub fn bit(self) -> u32 {
        self as u8 as u32
    }

    pub(crate) fn from_bit(bit: u32) -> Option<Channel> {
        use Channel::*;
        Some(match bit {
            0 => FrontLeft,
            1 => FrontRight,
            2 => FrontCenter,
            3 => LowFrequency,
            4 => BackLeft,
            5 => BackRight,
            6 => FrontLeftOfCenter,
            7 => FrontRightOfCenter,
            8 => BackCenter,
            9 => SideLeft,
            10 => SideRight,
            11 => TopCenter,
            12 => TopFrontLeft,
            13 => TopFrontCenter,
            14 => TopFrontRight,
            15 => TopBackLeft,
            16 => TopBackCenter,
            17 => TopBackRight,
            31 => WideLeft,
            32 => WideRight,
            33 => SurroundDirectLeft,
            34 => SurroundDirectRight,
            35 => LowFrequency2,
            _ => return None,
        })
    }

    /// Short name (`L`, `R`, `C`, `LFE`, `Lb`, `Rb`, `Ls`, `Rs`, ...).
    pub fn short_name(self) -> &'static str {
        use Channel::*;
        match self {
            FrontLeft => "L",
            FrontRight => "R",
            FrontCenter => "C",
            LowFrequency => "LFE",
            BackLeft => "Lb",
            BackRight => "Rb",
            FrontLeftOfCenter => "Lc",
            FrontRightOfCenter => "Rc",
            BackCenter => "Cb",
            SideLeft => "Ls",
            SideRight => "Rs",
            TopCenter => "Ts",
            TopFrontLeft => "Lvh",
            TopFrontCenter => "Cvh",
            TopFrontRight => "Rvh",
            TopBackLeft => "Ltb",
            TopBackCenter => "Ctb",
            TopBackRight => "Rtb",
            WideLeft => "Lw",
            WideRight => "Rw",
            SurroundDirectLeft => "Lsd",
            SurroundDirectRight => "Rsd",
            LowFrequency2 => "LFE2",
        }
    }
}

/// An ordered set of channels. Plane `i` of a decoded frame carries
/// `layout.channels()[i]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelLayout {
    mask: u64,
    channels: Vec<Channel>,
}

impl ChannelLayout {
    pub fn from_mask(mask: u64) -> Self {
        let mut channels = Vec::new();
        for bit in 0..64u32 {
            if mask & (1u64 << bit) != 0 {
                if let Some(c) = Channel::from_bit(bit) {
                    channels.push(c);
                }
            }
        }
        let mask = channels.iter().fold(0u64, |m, c| m | (1u64 << c.bit()));
        ChannelLayout { mask, channels }
    }

    /// FFmpeg `AV_CH_*` style bit mask.
    pub fn mask(&self) -> u64 {
        self.mask
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Plane index of `ch`, if present.
    pub fn index_of(&self, ch: Channel) -> Option<usize> {
        self.channels.iter().position(|&c| c == ch)
    }

    /// Space separated short names, e.g. `L R C LFE Lb Rb Ls Rs`.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        for (i, c) in self.channels.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(c.short_name());
        }
        s
    }
}

/// Layout mask for an audio coding mode (`acmod`), without LFE.
pub(crate) fn acmod_mask(acmod: u8) -> u64 {
    const FL: u64 = 1 << 0;
    const FR: u64 = 1 << 1;
    const FC: u64 = 1 << 2;
    const BC: u64 = 1 << 8;
    const SL: u64 = 1 << 9;
    const SR: u64 = 1 << 10;
    match acmod {
        0 => FL | FR,
        1 => FC,
        2 => FL | FR,
        3 => FL | FR | FC,
        4 => FL | FR | BC,
        5 => FL | FR | FC | BC,
        6 => FL | FR | SL | SR,
        _ => FL | FR | FC | SL | SR,
    }
}

pub(crate) const LFE_MASK: u64 = 1 << 3;
