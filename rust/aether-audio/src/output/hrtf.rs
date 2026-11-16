//! HRTF access: the committed KEMAR table (`hrtf_data.rs`, see NOTICE-HRTF) resampled to the
//! output rate, plus the canonical-label to measured-direction mapping.

use super::hrtf_data::{SLOTS, SOURCE_RATE, TAPS};

/// A measured direction, mirrored to the left hemisphere when `mirror` is set (ears swapped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Direction {
    pub slot: usize,
    pub mirror: bool,
    /// Degrees, positive to the listener's right, 0 straight ahead.
    pub azimuth: i16,
    pub elevation: i16,
}

fn slot_index(name: &str) -> usize {
    SLOTS.iter().position(|s| s.name == name).expect("slot exists in hrtf_data.rs")
}

/// Direction of a speaker label, `None` for LFE channels (they have no position).
///
/// Angles: ITU-R BS.775 / BS.2051 for the floor layer (front +-30, centre 0, front-centre pair
/// +-15, wide +-60, side +-90, rear +-135, a lone surround pair +-110, back centre 180) and 40
/// degrees of elevation for the height layer (top front +-45, top side +-90, top rear +-135,
/// Dolby recommends 30 to 55). `has_side` / `has_back` say which surround pairs the source has: a
/// lone pair (5.1 or quad) sits at +-110, a 7.1 sits at +-90 and +-135.
pub fn direction(label: &str, has_side: bool, has_back: bool) -> Option<Direction> {
    let both = has_side && has_back;
    let (az, el): (i16, i16) = match label {
        "FL" => (-30, 0),
        "FR" => (30, 0),
        "FC" => (0, 0),
        "FLC" => (-15, 0),
        "FRC" => (15, 0),
        "WL" => (-60, 0),
        "WR" => (60, 0),
        "SL" => (if both { -90 } else { -110 }, 0),
        "SR" => (if both { 90 } else { 110 }, 0),
        "BL" => (if both { -135 } else { -110 }, 0),
        "BR" => (if both { 135 } else { 110 }, 0),
        "SDL" => (-110, 0),
        "SDR" => (110, 0),
        "BC" => (180, 0),
        "TFL" => (-45, 40),
        "TFR" => (45, 40),
        "TFC" => (0, 40),
        "TSL" => (-90, 40),
        "TSR" => (90, 40),
        "TBL" => (-135, 40),
        "TBR" => (135, 40),
        "TBC" => (180, 40),
        "TC" => (0, 90),
        _ => return None,
    };
    let name = match (az.abs(), el) {
        (0, 0) => "C0",
        (15, 0) => "R15",
        (30, 0) => "R30",
        (60, 0) => "R60",
        (90, 0) => "R90",
        (110, 0) => "R110",
        (135, 0) => "R135",
        (180, 0) => "B180",
        (0, 40) => "U0",
        (45, 40) => "U45",
        (90, 40) => "U90",
        (135, 40) => "U135",
        (180, 40) => "U180",
        (0, 90) => "TOP",
        _ => unreachable!("direction table and slot table disagree"),
    };
    Some(Direction { slot: slot_index(name), mirror: az < 0, azimuth: az, elevation: el })
}

/// The measured (left ear, right ear) responses of `dir` at the native rate, as measured
/// (16 bit integer / 32768).
pub fn native_pair(dir: Direction) -> (Vec<f32>, Vec<f32>) {
    let s = &SLOTS[dir.slot];
    let to = |v: &[i16; TAPS]| v.iter().map(|&x| f32::from(x) / 32768.0).collect::<Vec<f32>>();
    if dir.mirror { (to(&s.right), to(&s.left)) } else { (to(&s.left), to(&s.right)) }
}

/// Resamples a response from `from` to `to` Hz with a Lanczos-windowed sinc (a = 16), keeping the
/// frequency response (DC gain). Equal rates return the input unchanged. The kernel is a
/// low-pass at the lower of the two Nyquist frequencies.
pub fn resample(ir: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return ir.to_vec();
    }
    let r = f64::from(to) / f64::from(from);
    let fc = r.min(1.0);
    let a = 16.0f64;
    let out_len = (ir.len() as f64 * r).ceil() as usize;
    let sinc = |x: f64| if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
    let reach = (a / fc).ceil() as isize;
    let mut out = Vec::with_capacity(out_len);
    for m in 0..out_len {
        let t = m as f64 / r;
        let centre = t.floor() as isize;
        let mut acc = 0.0f64;
        for k in (centre - reach)..=(centre + reach + 1) {
            if k < 0 || k as usize >= ir.len() {
                continue;
            }
            let u = fc * (t - k as f64);
            if u.abs() >= a {
                continue;
            }
            acc += f64::from(ir[k as usize]) * fc * sinc(u) * sinc(u / a);
        }
        out.push((acc / r) as f32);
    }
    out
}

/// Both ear responses of `dir` at `rate`.
pub fn pair_at(dir: Direction, rate: u32) -> (Vec<f32>, Vec<f32>) {
    let (l, r) = native_pair(dir);
    (resample(&l, SOURCE_RATE, rate), resample(&r, SOURCE_RATE, rate))
}
