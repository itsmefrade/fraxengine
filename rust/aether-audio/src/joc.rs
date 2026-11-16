//! E-AC-3 Joint Object Coding (Dolby Atmos in Dolby Digital Plus) rendered to 7.1.4.
//!
//! `aether-joc` rebuilds up to 15 object signals from the 5.1/7.1 bed of each access unit, `render`'s
//! [`ObjectRenderer`] (the allocentric point panner of the TrueHD Atmos path) places them in 7.1.4, and the
//! bypassed LFE joins the LFE channel.
//!
//! Timeline. The objects lag the bed by [`LATENCY_SAMPLES`] (QMF analysis plus synthesis). OAMD update times
//! are already on that object clock, so objects, metadata and LFE stay mutually aligned, and the first
//! `LATENCY_SAMPLES` output samples (silence) are dropped once at the start so the rendered stream is
//! sample aligned with the bed and with the container timestamps. The last `LATENCY_SAMPLES` of a stream
//! are therefore never produced (12 ms of tail).
//!
//! Semantics mapped onto the TrueHD renderer, and what differs from it:
//! * Position: JOC OAMD room-anchored x (left to right), y (front to back), z (floor to ceiling) are the
//!   same unit cube the TrueHD OAMD uses; z below 0 clamps to the floor. The renderer's height speakers
//!   sit at x 0.25/0.75, y 0.25/0.75 (a JOC `Tfl` bed speaker is at the cube corner, so a point object at
//!   a corner is panned by the renderer's usual balance between the nearest speakers).
//! * Gain: the linear gain of clause 5.6.1.4, applied as the role/object gain with the OAMD ramp.
//! * Bed objects (speaker anchored) become `Role::Bed` and go straight to their speaker; top side
//!   (`Tsl`/`Tsr`) is split equally between the top front and top back speaker of that side, 7.1.4 having no
//!   top side speaker. Wide (`Lw`/`Rw`) is panned at its nominal position.
//! * Differences from the TrueHD path: size, zones, snap, screen anchoring, distance and divergence are
//!   ignored instead of refused (JOC streams use them rarely and the bed-energy checks in `aether-joc`
//!   show the point model is close); `enable_elevation == false` flattens the object to z = 0 like
//!   `ObjectRenderer::with_elevation_exclusion`. Ramps are the OAMD `ramp_duration` samples, linear.
//! * Metadata is complete per update instant: objects without an update at an instant keep their last state.
//! * Not Dolby reference renderer equivalence (no bit-exact reference exists, see `aether-joc`).

use crate::render::{ElementState, Layout, ObjectRenderer, Role, TimedUpdate};
use aether_ac3::DecodedFrame;
use aether_joc::oamd::SpeakerLabel;
use aether_joc::{FRAME_SAMPLES, LATENCY_SAMPLES, ObjectDecoder, ObjectFrame};

pub const LAYOUT: Layout = Layout::SevenOneFour;
const LFE_INDEX: usize = 3;

pub struct JocRendered {
    /// 12 planes in `LAYOUT.labels()` order.
    pub planes: Vec<Vec<f32>>,
    pub objects: usize,
}

pub struct JocRenderer {
    decoder: ObjectDecoder,
    renderer: ObjectRenderer,
    last: Vec<(Role, ElementState)>,
    next_frame: u64,
    trimmed: bool,
    scheduled: bool,
}

impl Default for JocRenderer {
    fn default() -> Self {
        Self::new()
    }
}

fn bed_name(l: SpeakerLabel) -> &'static str {
    use SpeakerLabel::*;
    match l {
        L => "L",
        R => "R",
        C => "C",
        Lfe => "LFE",
        Ls => "Ls",
        Rs => "Rs",
        Lb => "Lb",
        Rb => "Rb",
        Tfl => "Tfl",
        Tfr => "Tfr",
        Tsl => "Tml",
        Tsr => "Tmr",
        Tbl => "Tbl",
        Tbr => "Tbr",
        Lw => "Lw",
        Rw => "Rw",
        Lfe2 => "LFE2",
    }
}

impl JocRenderer {
    pub fn new() -> Self {
        Self { decoder: ObjectDecoder::new(), renderer: ObjectRenderer::new(LAYOUT), last: Vec::new(), next_frame: 0, trimmed: false, scheduled: false }
    }

    /// Decodes and renders one access unit. `Ok(None)`: the unit has no JOC payload and nothing is configured yet.
    pub fn process(&mut self, au: &[u8], bed: &DecodedFrame) -> Result<Option<JocRendered>, String> {
        let of = self.decoder.process(au, bed).map_err(|e| format!("{e:?}"))?;
        let start = self.next_frame;
        self.next_frame += FRAME_SAMPLES as u64;
        self.schedule(&of, start)?;
        let silent = vec![0f32; FRAME_SAMPLES];
        let n = if of.joc_present { of.num_objects } else { self.last.len() };
        let mut out = if n == 0 || self.last.is_empty() || !self.scheduled {
            vec![vec![0f32; FRAME_SAMPLES]; LAYOUT.channels()]
        } else {
            let planes: Vec<&[f32]> = (0..self.last.len())
                .map(|k| if of.joc_present && k < of.num_objects { of.object(k) } else { silent.as_slice() })
                .collect();
            let block = self.renderer.render_planar(&planes, start).map_err(|e| format!("render {e:?}"))?;
            let ch = block.channels;
            (0..ch).map(|c| (0..FRAME_SAMPLES).map(|i| block.pcm[i * ch + c]).collect()).collect()
        };
        if of.lfe.len() == FRAME_SAMPLES {
            for (o, l) in out[LFE_INDEX].iter_mut().zip(&of.lfe) {
                *o += l;
            }
        }
        if !self.trimmed {
            self.trimmed = true;
            for p in &mut out {
                p.drain(..LATENCY_SAMPLES);
            }
        }
        Ok(Some(JocRendered { planes: out, objects: of.num_objects }))
    }

    fn schedule(&mut self, of: &ObjectFrame, start: u64) -> Result<(), String> {
        if of.joc_present && of.num_objects != self.last.len() && of.num_objects > 0 {
            self.last.resize(of.num_objects, (Role::Object, ElementState { position: [0.5, 0.5, 0.0], gain: 0.0 }));
        }
        let mut idx: Vec<usize> = (0..of.metadata.len()).collect();
        idx.sort_by_key(|&i| (of.metadata[i].start_sample, of.metadata[i].object));
        let mut i = 0;
        while i < idx.len() {
            let t = of.metadata[idx[i]].start_sample;
            let ramp = of.metadata[idx[i]].ramp_duration;
            while i < idx.len() && of.metadata[idx[i]].start_sample == t {
                let m = &of.metadata[idx[i]];
                i += 1;
                let Some(slot) = self.last.get_mut(m.object) else { continue };
                let s = &m.state;
                let role = m.bed_label.map_or(Role::Object, |l| Role::Bed(bed_name(l)));
                let mut position = s.pos;
                if role == Role::Object && !s.enable_elevation {
                    position[2] = 0.0;
                }
                position = position.map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.5 });
                let gain = if s.active && s.gain.is_finite() { s.gain } else { 0.0 };
                *slot = (role, ElementState { position, gain });
            }
            let roles = self.last.iter().map(|(r, _)| *r).collect();
            let states = self.last.iter().map(|(_, s)| *s).collect();
            // Updates are never in the past: object clock = bed clock + latency >= the block start.
            // The first state applies from the very first sample (the objects are silent until their delayed
            // start anyway), so the renderer is configured before its first block.
            let frame = if self.scheduled { t.max(start) } else { start };
            self.scheduled = true;
            self.renderer
                .schedule(TimedUpdate { frame, ramp, roles, states })
                .map_err(|e| format!("schedule {e:?}"))?;
        }
        Ok(())
    }
}
