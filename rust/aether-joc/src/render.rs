//! A small reference point panner used to check decoded objects against
//! speaker positions. It is *not* the Dolby Atmos renderer: objects are mixed
//! into the three nearest speakers with inverse-square-distance weights and
//! power normalisation, ignoring size, zones, snap and screen anchoring.

use crate::oamd::SpeakerLabel;

/// The 7.1.4 speaker set (LFE excluded) in room-anchored coordinates.
pub const LAYOUT_714: [SpeakerLabel; 11] = [
    SpeakerLabel::L,
    SpeakerLabel::R,
    SpeakerLabel::C,
    SpeakerLabel::Ls,
    SpeakerLabel::Rs,
    SpeakerLabel::Lb,
    SpeakerLabel::Rb,
    SpeakerLabel::Tfl,
    SpeakerLabel::Tfr,
    SpeakerLabel::Tbl,
    SpeakerLabel::Tbr,
];

/// The 5.0 speaker set, used to compare against the JOC bed.
pub const LAYOUT_50: [SpeakerLabel; 5] =
    [SpeakerLabel::L, SpeakerLabel::R, SpeakerLabel::C, SpeakerLabel::Ls, SpeakerLabel::Rs];

/// Speaker gains (amplitude) for a point source at `pos`: the three nearest
/// speakers get a weight of `1 / d^4`, then the gains are power normalised.
pub fn pan_gains(pos: [f32; 3], speakers: &[SpeakerLabel]) -> Vec<f32> {
    let mut d2: Vec<(f32, usize)> = speakers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let p = s.nominal_position();
            let dx = p[0] - pos[0];
            let dy = p[1] - pos[1];
            let dz = p[2] - pos[2];
            (dx * dx + dy * dy + dz * dz, i)
        })
        .collect();
    d2.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut g = vec![0f32; speakers.len()];
    if d2[0].0 < 1e-6 {
        g[d2[0].1] = 1.0;
        return g;
    }
    let mut sum = 0f32;
    for &(d, i) in d2.iter().take(3) {
        let w = 1.0 / (d * d);
        g[i] = w;
        sum += w * w;
    }
    let n = sum.sqrt();
    for v in g.iter_mut() {
        *v /= n;
    }
    g
}
