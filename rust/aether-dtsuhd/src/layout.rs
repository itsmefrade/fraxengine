//! Channel activity mask handling (ETSI TS 103 491 Tables 7-27 and 7-28).

/// One entry of Table 7-28: mask bit, channel labels of that bit (in waveform order),
/// nominal (azimuth, elevation) in degrees for each label.
pub struct MaskBit {
    pub bit: u32,
    pub labels: &'static [&'static str],
    pub positions: &'static [(i16, i16)],
}

pub static MASK_BITS: [MaskBit; 20] = [
    MaskBit {
        bit: 0x0000_0001,
        labels: &["C"],
        positions: &[(0, 0)],
    },
    MaskBit {
        bit: 0x0000_0002,
        labels: &["L", "R"],
        positions: &[(30, 0), (-30, 0)],
    },
    MaskBit {
        bit: 0x0000_0004,
        labels: &["Ls", "Rs"],
        positions: &[(110, 0), (-110, 0)],
    },
    MaskBit {
        bit: 0x0000_0008,
        labels: &["LFE1"],
        positions: &[(0, 0)],
    },
    MaskBit {
        bit: 0x0000_0010,
        labels: &["Cs"],
        positions: &[(180, 0)],
    },
    MaskBit {
        bit: 0x0000_0020,
        labels: &["Lh", "Rh"],
        positions: &[(45, 45), (-45, 45)],
    },
    MaskBit {
        bit: 0x0000_0040,
        labels: &["Lsr", "Rsr"],
        positions: &[(150, 0), (-150, 0)],
    },
    MaskBit {
        bit: 0x0000_0080,
        labels: &["Ch"],
        positions: &[(0, 45)],
    },
    MaskBit {
        bit: 0x0000_0100,
        labels: &["Oh"],
        positions: &[(0, 90)],
    },
    MaskBit {
        bit: 0x0000_0200,
        labels: &["Lc", "Rc"],
        positions: &[(15, 0), (-15, 0)],
    },
    MaskBit {
        bit: 0x0000_0400,
        labels: &["Lw", "Rw"],
        positions: &[(60, 0), (-60, 0)],
    },
    MaskBit {
        bit: 0x0000_0800,
        labels: &["Lss", "Rss"],
        positions: &[(90, 0), (-90, 0)],
    },
    MaskBit {
        bit: 0x0000_1000,
        labels: &["LFE2"],
        positions: &[(0, 0)],
    },
    MaskBit {
        bit: 0x0000_2000,
        labels: &["Lhs", "Rhs"],
        positions: &[(90, 45), (-90, 45)],
    },
    MaskBit {
        bit: 0x0000_4000,
        labels: &["Chr"],
        positions: &[(180, 45)],
    },
    MaskBit {
        bit: 0x0000_8000,
        labels: &["Lhr", "Rhr"],
        positions: &[(135, 45), (-135, 45)],
    },
    MaskBit {
        bit: 0x0001_0000,
        labels: &["Cbf"],
        positions: &[(0, -30)],
    },
    MaskBit {
        bit: 0x0002_0000,
        labels: &["Lbf", "Rbf"],
        positions: &[(45, -30), (-45, -30)],
    },
    MaskBit {
        bit: 0x0004_0000,
        labels: &["Ltf", "Rtf"],
        positions: &[(45, 60), (-45, 60)],
    },
    MaskBit {
        bit: 0x0008_0000,
        labels: &["Ltr", "Rtr"],
        positions: &[(135, 60), (-135, 60)],
    },
];

/// Table 7-27 (layout index 0 to 13).
pub const CH_MASK_INDEX_TABLE: [u32; 14] = [
    0x0000_0001, // C
    0x0000_0002, // LR
    0x0000_0006, // LR + LsRs
    0x0000_000F, // 5.1
    0x0000_001F, // 6.1
    0x0000_084B, // 7.1
    0x0000_002F, // 5.1 + LhRh
    0x0000_802F, // 5.1.4 (called 9.1 in the table)
    0x0000_486B, // 10.1
    0x0000_886B, // 11.1
    0x0003_FBFB, // NHK 22.2
    0x0000_0003, // CLR (3.0)
    0x0000_0007, // 5.0
    0x0000_0843, // 7.0
];

/// Number of waveforms for a channel activity mask (GetNumSpeakersInLayout).
pub fn num_speakers_in_layout(mask: u32) -> usize {
    MASK_BITS
        .iter()
        .filter(|m| mask & m.bit != 0)
        .map(|m| m.labels.len())
        .sum()
}

/// Labels in waveform order (ascending mask bit, left before right).
pub fn labels_for_mask(mask: u32) -> Vec<&'static str> {
    let mut v = Vec::new();
    for m in MASK_BITS.iter() {
        if mask & m.bit != 0 {
            v.extend_from_slice(m.labels);
        }
    }
    v
}

pub fn layout_name(mask: u32) -> String {
    let full: usize = MASK_BITS
        .iter()
        .filter(|m| mask & m.bit != 0 && m.bit != 0x8 && m.bit != 0x1000)
        .map(|m| m.labels.len())
        .sum();
    let lfe = (mask & 0x8 != 0) as usize + (mask & 0x1000 != 0) as usize;
    let height: usize = MASK_BITS
        .iter()
        .filter(|m| mask & m.bit != 0 && m.positions.iter().any(|p| p.1 > 0))
        .map(|m| m.labels.len())
        .sum();
    if height > 0 {
        format!("{}.{}.{}", full - height, lfe, height)
    } else {
        format!("{}.{}", full, lfe)
    }
}
