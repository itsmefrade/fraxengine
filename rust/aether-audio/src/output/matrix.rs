//! Speaker downmix matrices between canonical layouts.
//!
//! Every source label is routed to the target layout by a small set of fold rules; a rule that
//! lands on a label the target does not have is applied again from there, so a 7.1.4 source reaches
//! stereo through exactly the steps 7.1.4 -> 7.1 -> 5.1 -> 2.0 composed (the coefficients multiply).
//! Rules (all gains linear, `H` = 1/sqrt(2) = -3 dB):
//!
//! * A label the target has passes at 1.0 (exactly: a lone coefficient 1.0 is a copy).
//! * `FC` -> `FL`, `FR` at `H` (BS.775 phantom centre). `FLC`/`FRC` -> `FL`/`FR` at `H` plus `FC` at `H`.
//! * Surrounds: `SL`/`BL` and `SR`/`BR` stand for each other. A source with both pairs (7.1) folded
//!   onto a target with one pair (5.1) sends both at `H`; a lone pair relabels at 1.0. With no
//!   surround in the target they go to `FL`/`FR` at `H` (BS.775 `Ls` -> `L`). `BC` -> `BL`,`BR` at `H`.
//! * Heights fold into the layer below on the same side at `H`: `TFL` -> `FL`, `TBL` -> `BL`,
//!   `TSL` -> `SL`, `TFC` -> `FC`, `TBC` -> `BC`, `TC` -> `TFL`,`TFR` at `H`. A target with only the
//!   front height pair (7.1.2) takes `TBL` -> `TFL` at `H`, `TSL` -> `TFL` at 1.0.
//! * `LFE` -> `LFE` at 1.0, or to `FL`,`FR` at the configured gain when the target has none (default
//!   0.0: BS.775 drops it in a stereo downmix).
//! * `FL`, `FR` -> `FC` at `H` for a mono target.

use super::OutputError;

pub const H: f32 = std::f32::consts::FRAC_1_SQRT_2;

pub const T714: [&str; 12] = ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR", "TFL", "TFR", "TBL", "TBR"];
pub const T712: [&str; 10] = ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR", "TFL", "TFR"];
pub const T514: [&str; 10] = ["FL", "FR", "FC", "LFE", "SL", "SR", "TFL", "TFR", "TBL", "TBR"];
pub const T71: [&str; 8] = ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR"];
pub const T51: [&str; 6] = ["FL", "FR", "FC", "LFE", "SL", "SR"];
pub const T20: [&str; 2] = ["FL", "FR"];
pub const T10: [&str; 1] = ["FC"];

pub fn is_height(l: &str) -> bool {
    matches!(l, "TFL" | "TFR" | "TFC" | "TBL" | "TBR" | "TBC" | "TC" | "TSL" | "TSR")
}

pub fn is_lfe(l: &str) -> bool {
    matches!(l, "LFE" | "LFE2")
}

/// Sparse mix matrix: `rows[o]` lists `(input index, gain)` for output `o`.
pub type Rows = Vec<Vec<(usize, f32)>>;

struct Ctx<'a> {
    tgt: &'a [&'static str],
    lfe_gain: f32,
    src_both: bool,
    merge: bool,
}

impl Ctx<'_> {
    fn t(&self, l: &str) -> bool {
        self.tgt.contains(&l)
    }
}

fn is_surround(l: &str) -> bool {
    matches!(l, "SL" | "SR" | "BL" | "BR")
}

fn route(l: &'static str, gain: f32, c: &Ctx, acc: &mut Vec<(&'static str, f32)>, depth: u8) {
    if depth > 8 || gain == 0.0 {
        return;
    }
    if c.t(l) {
        // Only a source channel landing on its own label is halved by a merge; a folded one already carries its gain.
        let g = if depth == 0 && is_surround(l) && c.merge { H } else { 1.0 };
        acc.push((l, gain * g));
        return;
    }
    let mut to = |next: &'static str, g: f32| route(next, gain * g, c, acc, depth + 1);
    match l {
        "FL" => to("FC", H),
        "FR" => to("FC", H),
        "FC" => {
            to("FL", H);
            to("FR", H);
        }
        "FLC" => {
            if c.t("FC") {
                to("FL", H);
                to("FC", H);
            } else {
                to("FL", 1.0);
            }
        }
        "FRC" => {
            if c.t("FC") {
                to("FR", H);
                to("FC", H);
            } else {
                to("FR", 1.0);
            }
        }
        "WL" => {
            to("FL", H);
            to("SL", H);
        }
        "WR" => {
            to("FR", H);
            to("SR", H);
        }
        "SL" => {
            if c.t("BL") {
                to("BL", if c.src_both { H } else { 1.0 });
            } else {
                to("FL", H);
            }
        }
        "SR" => {
            if c.t("BR") {
                to("BR", if c.src_both { H } else { 1.0 });
            } else {
                to("FR", H);
            }
        }
        "BL" => {
            if c.t("SL") {
                to("SL", if c.src_both { H } else { 1.0 });
            } else {
                to("FL", H);
            }
        }
        "BR" => {
            if c.t("SR") {
                to("SR", if c.src_both { H } else { 1.0 });
            } else {
                to("FR", H);
            }
        }
        "SDL" => to("SL", 1.0),
        "SDR" => to("SR", 1.0),
        "BC" => {
            to("BL", H);
            to("BR", H);
        }
        "LFE2" => to("LFE", 1.0),
        "LFE" => {
            if c.lfe_gain > 0.0 {
                to("FL", c.lfe_gain);
                to("FR", c.lfe_gain);
            }
        }
        "TFL" => to("FL", H),
        "TFR" => to("FR", H),
        "TFC" => {
            if c.t("TFL") {
                to("TFL", H);
                to("TFR", H);
            } else {
                to("FC", H);
            }
        }
        "TBL" => {
            if c.t("TFL") {
                to("TFL", H);
            } else {
                to("BL", H);
            }
        }
        "TBR" => {
            if c.t("TFR") {
                to("TFR", H);
            } else {
                to("BR", H);
            }
        }
        "TBC" => {
            to("TBL", H);
            to("TBR", H);
        }
        "TSL" => {
            if c.t("TFL") && c.t("TBL") {
                to("TFL", H);
                to("TBL", H);
            } else if c.t("TFL") {
                to("TFL", 1.0);
            } else {
                to("SL", H);
            }
        }
        "TSR" => {
            if c.t("TFR") && c.t("TBR") {
                to("TFR", H);
                to("TBR", H);
            } else if c.t("TFR") {
                to("TFR", 1.0);
            } else {
                to("SR", H);
            }
        }
        "TC" => {
            to("TFL", H);
            to("TFR", H);
        }
        _ => {}
    }
}

/// Builds the matrix from `src` plane order to `tgt` plane order. `lfe_gain` is the gain of `LFE`
/// into `FL`/`FR` when the target has no LFE channel (0.0 drops it).
pub fn build(src: &[&'static str], tgt: &[&'static str], lfe_gain: f32) -> Result<Rows, OutputError> {
    let has = |l: &str| tgt.contains(&l);
    let valid = (has("FL") && has("FR")) || tgt == ["FC"];
    if !valid {
        return Err(OutputError::BadTarget(format!("{tgt:?} has neither a front pair nor a lone centre")));
    }
    let src_side = src.iter().any(|l| matches!(*l, "SL" | "SR"));
    let src_back = src.iter().any(|l| matches!(*l, "BL" | "BR"));
    let tgt_side = has("SL");
    let tgt_back = has("BL");
    let ctx = Ctx { tgt, lfe_gain, src_both: src_side && src_back, merge: src_side && src_back && tgt_side != tgt_back };
    let mut dense = vec![vec![0.0f32; src.len()]; tgt.len()];
    let mut acc = Vec::new();
    for (i, &l) in src.iter().enumerate() {
        acc.clear();
        route(l, 1.0, &ctx, &mut acc, 0);
        for &(t, g) in &acc {
            let o = tgt.iter().position(|x| *x == t).expect("route ends on a target label");
            dense[o][i] += g;
        }
    }
    Ok(dense
        .into_iter()
        .map(|row| row.into_iter().enumerate().filter(|&(_, g)| g != 0.0).collect())
        .collect())
}

/// Largest sum of absolute gains of any output row: the worst-case level of a full-scale,
/// phase-aligned input (1.0 means no output can exceed full scale).
pub fn peak_gain(rows: &Rows) -> f32 {
    rows.iter().map(|r| r.iter().map(|&(_, g)| g.abs()).sum::<f32>()).fold(0.0, f32::max)
}

/// Picks the target layout for a speaker device. Never upmixes: the target has no more height
/// pairs and no more surround pairs than the source. Height layouts need a device with at least 10
/// channels because an 8 channel route is read as 7.1 by every receiver.
pub fn select_target(src: &[&'static str], device_channels: usize) -> Vec<&'static str> {
    let heights = src.iter().filter(|l| is_height(l)).count();
    let side = src.iter().any(|l| matches!(*l, "SL" | "SR" | "SDL" | "SDR" | "WL" | "WR"));
    let back = src.iter().any(|l| matches!(*l, "BL" | "BR" | "BC"));
    let rank_h = if side && back { 2 } else if side || back { 1 } else { 0 };
    let cands: [(&[&'static str], usize, usize); 7] = [
        (&T714, 2, 4),
        (&T712, 2, 2),
        (&T514, 1, 4),
        (&T71, 2, 0),
        (&T51, 1, 0),
        (&T20, 0, 0),
        (&T10, 0, 0),
    ];
    for (labels, rh, hh) in cands {
        let fits = labels.len() <= device_channels && rh <= rank_h && hh <= heights;
        let heights_ok = hh == 0 || device_channels >= 10;
        if fits && heights_ok {
            return labels.to_vec();
        }
    }
    if device_channels >= 2 { T20.to_vec() } else { T10.to_vec() }
}
