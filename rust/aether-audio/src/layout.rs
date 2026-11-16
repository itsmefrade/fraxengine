//! Canonical channel layout naming shared by every decoder (TrueHD, DTS, PCM, AC-3 / E-AC-3).
//!
//! A decoded block always carries explicit labels, one per plane, in the decoder's native
//! plane order. Labels are always canonical names from [`WAVE_ORDER`]; consumers that need a
//! fixed channel order (a renderer, a downmixer, a file writer) permute with [`order`].
//!
//! Two orders are defined:
//! - WAVE (`Order::Wave`): the `WAVEFORMATEXTENSIBLE` / FFmpeg bit order, `FL FR FC LFE BL BR FLC FRC
//!   BC SL SR TC TFL TFC TFR TBL TBC TBR`, then the extensions `WL WR TSL TSR SDL SDR LFE2`.
//!   A 7.1 bed is `FL FR FC LFE BL BR SL SR` (rear pair before the side pair).
//! - SMPTE (`Order::Smpte`): the cinema / Blu-ray / DTS order, `FL FR FC LFE SL SR BL BR ...` (side
//!   pair before the rear pair), heights last. 5.1 is identical in both orders.
//!
//! Naming: `S*` is a side surround, `B*` a rear (back) surround, `W*` a front wide, `TF/TB/TS*` a
//! top front / back / side height, `TC` top centre, `SD*` a surround direct (dipole) pair.

/// WAVE / FFmpeg channel order. The index of a name is its rank in `Order::Wave`; the first
/// 18 names are the bits 0..=17 of a WAVE channel mask.
pub const WAVE_ORDER: [&str; 25] = [
    "FL", "FR", "FC", "LFE", "BL", "BR", "FLC", "FRC", "BC", "SL", "SR", "TC", "TFL", "TFC", "TFR", "TBL", "TBC", "TBR",
    "WL", "WR", "TSL", "TSR", "SDL", "SDR", "LFE2",
];

/// SMPTE / cinema channel order.
pub const SMPTE_ORDER: [&str; 25] = [
    "FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR", "BC", "FLC", "FRC", "WL", "WR", "SDL", "SDR", "TFL", "TFR", "TFC",
    "TC", "TSL", "TSR", "TBL", "TBR", "TBC", "LFE2",
];

/// Number of WAVE channel mask bits that have a canonical name.
pub const WAVE_MASK_BITS: usize = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Wave,
    Smpte,
}

/// Maps a decoder specific or alias name to the canonical name. Accepts canonical names too
/// (case sensitive), the TrueHD names (`L R C Ls Rs Lb Rb Cb Lsc Rsc Lw Rw Lsd Rsd Tfl Tfr Tfc Tc
/// Tsl Tsr Tbl Tbr LFE2`) and the SMPTE/ITU names (`Lfe`, `Lss`, `Rss`, `Lrs`, `Rrs`, `Cs`).
pub fn canonical(name: &str) -> Option<&'static str> {
    if let Some(n) = WAVE_ORDER.iter().find(|n| **n == name) {
        return Some(n);
    }
    Some(match name {
        "L" => "FL",
        "R" => "FR",
        "C" => "FC",
        "Lfe" | "LFE1" => "LFE",
        "Ls" | "Lss" => "SL",
        "Rs" | "Rss" => "SR",
        "Lb" | "Lrs" => "BL",
        "Rb" | "Rrs" => "BR",
        "Cb" | "Cs" | "Cbs" => "BC",
        "Lsc" | "Lc" => "FLC",
        "Rsc" | "Rc" => "FRC",
        "Lw" => "WL",
        "Rw" => "WR",
        "Lsd" => "SDL",
        "Rsd" => "SDR",
        "Tfl" | "Lh" => "TFL",
        "Tfr" | "Rh" => "TFR",
        "Tfc" | "Ch" => "TFC",
        "Tc" | "Oh" => "TC",
        "Tsl" | "Lhs" => "TSL",
        "Tsr" | "Rhs" => "TSR",
        "Tbl" | "Lhr" => "TBL",
        "Tbr" | "Rhr" => "TBR",
        "Tbc" | "Chr" => "TBC",
        "Lfe2" => "LFE2",
        _ => return None,
    })
}

/// Rank of a canonical name in `order`, `None` for a name that is not canonical.
pub fn rank(name: &str, order: Order) -> Option<usize> {
    let table = match order {
        Order::Wave => &WAVE_ORDER,
        Order::Smpte => &SMPTE_ORDER,
    };
    table.iter().position(|n| *n == name)
}

/// The permutation that puts `labels` into `order`: element `i` of the result is the index into
/// `labels` (and so the plane) of the `i`-th channel in that order. Labels that are not
/// canonical keep their relative position after the known ones.
pub fn order<S: AsRef<str>>(labels: &[S], order: Order) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..labels.len()).collect();
    idx.sort_by_key(|&i| (rank(labels[i].as_ref(), order).unwrap_or(usize::MAX), i));
    idx
}

/// WAVE channel mask of `labels`, `None` when a label has no mask bit (the extensions) or repeats.
pub fn wave_mask<S: AsRef<str>>(labels: &[S]) -> Option<u32> {
    let mut mask = 0u32;
    for l in labels {
        let bit = WAVE_ORDER[..WAVE_MASK_BITS].iter().position(|n| *n == l.as_ref())?;
        if mask & (1 << bit) != 0 {
            return None;
        }
        mask |= 1 << bit;
    }
    Some(mask)
}

/// Labels of the bits set in a WAVE channel mask, in WAVE order. Bits above 17 are ignored.
pub fn labels_from_mask(mask: u32) -> Vec<&'static str> {
    (0..WAVE_MASK_BITS).filter(|b| mask & (1 << b) != 0).map(|b| WAVE_ORDER[b]).collect()
}

/// Layout used for an LPCM stream that does not say which speakers it carries (Matroska,
/// plain MP4): the WAVE default for the channel count. Counts above 8 are `None`.
pub fn default_for_channels(channels: usize) -> Option<Vec<&'static str>> {
    Some(match channels {
        1 => vec!["FC"],
        2 => vec!["FL", "FR"],
        3 => vec!["FL", "FR", "FC"],
        4 => vec!["FL", "FR", "BL", "BR"],
        5 => vec!["FL", "FR", "FC", "BL", "BR"],
        6 => vec!["FL", "FR", "FC", "LFE", "BL", "BR"],
        7 => vec!["FL", "FR", "FC", "LFE", "BC", "SL", "SR"],
        8 => vec!["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"],
        _ => return None,
    })
}

/// Common names for a set of labels (order of the labels does not matter).
pub fn name<S: AsRef<str>>(labels: &[S]) -> Option<&'static str> {
    let mut sorted: Vec<&str> = labels.iter().map(AsRef::as_ref).collect();
    sorted.sort_by_key(|l| rank(l, Order::Wave).unwrap_or(usize::MAX));
    NAMED.iter().find(|(_, set)| set[..] == sorted[..]).map(|(n, _)| *n)
}

/// Named layouts, each listed in WAVE order.
pub const NAMED: [(&str, &[&str]); 12] = [
    ("mono", &["FC"]),
    ("stereo", &["FL", "FR"]),
    ("3.0", &["FL", "FR", "FC"]),
    ("quad", &["FL", "FR", "BL", "BR"]),
    ("5.0", &["FL", "FR", "FC", "BL", "BR"]),
    ("5.1", &["FL", "FR", "FC", "LFE", "BL", "BR"]),
    ("5.1(side)", &["FL", "FR", "FC", "LFE", "SL", "SR"]),
    ("6.1", &["FL", "FR", "FC", "LFE", "BC", "SL", "SR"]),
    ("7.1", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]),
    ("7.1.2", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR", "TFL", "TFR"]),
    ("7.1.4", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR", "TFL", "TFR", "TBL", "TBR"]),
    ("9.1.6", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR", "TFL", "TFR", "TBL", "TBR", "WL", "WR", "TSL", "TSR"]),
];

/// The tables as JSON, for the `aether_audio_layout_table` export.
pub fn table_json() -> String {
    let list = |v: &[&str]| format!("[{}]", v.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(","));
    let bits = WAVE_ORDER[..WAVE_MASK_BITS]
        .iter()
        .enumerate()
        .map(|(i, n)| format!("\"{n}\":{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let named = NAMED.iter().map(|(n, l)| format!("\"{n}\":{}", list(l))).collect::<Vec<_>>().join(",");
    let defaults = (1..=8)
        .map(|c| format!("\"{c}\":{}", list(&default_for_channels(c).unwrap())))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"wave\":{},\"smpte\":{},\"waveBits\":{{{bits}}},\"named\":{{{named}}},\"defaults\":{{{defaults}}}}}",
        list(&WAVE_ORDER),
        list(&SMPTE_ORDER)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_are_permutations_of_the_same_names() {
        let mut a = WAVE_ORDER.to_vec();
        let mut b = SMPTE_ORDER.to_vec();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn seven_one_differs_between_orders_five_one_does_not() {
        let l = ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR"];
        assert_eq!(order(&l, Order::Smpte), [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(order(&l, Order::Wave), [0, 1, 2, 3, 6, 7, 4, 5]);
        let five = ["FL", "FR", "FC", "LFE", "BL", "BR"];
        assert_eq!(order(&five, Order::Wave), order(&five, Order::Smpte));
    }

    #[test]
    fn truehd_names_map_to_canonical_names() {
        let t = ["L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb"];
        let c: Vec<_> = t.iter().map(|n| canonical(n).unwrap()).collect();
        assert_eq!(c, ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR"]);
        assert_eq!(name(&c), Some("7.1"));
        assert_eq!(canonical("nonsense"), None);
    }

    #[test]
    fn masks_round_trip() {
        assert_eq!(wave_mask(&["FL", "FR", "FC", "LFE", "BL", "BR"]), Some(0x3f));
        assert_eq!(labels_from_mask(0x63f), ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]);
        assert_eq!(wave_mask(&["FL", "FL"]), None);
        assert_eq!(wave_mask(&["WL"]), None);
        for c in 1..=8 {
            assert_eq!(default_for_channels(c).unwrap().len(), c);
        }
    }

    #[test]
    fn named_layouts_are_in_wave_order() {
        for (_, set) in NAMED {
            let ranks: Vec<_> = set.iter().map(|l| rank(l, Order::Wave).unwrap()).collect();
            assert!(ranks.windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn json_is_balanced() {
        let j = table_json();
        assert!(j.starts_with('{') && j.ends_with('}'));
        assert_eq!(j.matches('[').count(), j.matches(']').count());
    }
}
