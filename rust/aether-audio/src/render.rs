//! Allocentric point-source renderer, porting upstream panner and gain-ramp behavior.
//! Exact decoded element PCM is mixed directly; no bed regression is used.
//! This is not Dolby reference-renderer equivalence: extent, snap, zones, screen
//! anchoring, distance and elevation-switch behavior are not implemented.
//! Active unsupported controls are explicitly refused before block scheduling.
use crate::AudioBlock;
use std::collections::VecDeque;
use truehd::structs::oamd::{GAIN_MINUS_INFINITY, ObjectAudioMetadataPayload};
#[cfg(any(test, target_arch = "wasm32"))]
mod checkpoint;
#[cfg(target_arch = "wasm32")]
pub(crate) use checkpoint::{decode_checkpoint, encode_checkpoint};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    FiveOneFour,
    SevenOneFour,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Bed(&'static str),
    Object,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ElementState {
    pub position: [f32; 3],
    pub gain: f32,
}
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct TimedUpdate {
    pub frame: u64,
    pub ramp: u32,
    pub roles: Vec<Role>,
    pub states: Vec<ElementState>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    UnsupportedLayout,
    UnsupportedRole,
    InvalidMetadata,
    UnsupportedIsf,
    PendingLimit,
    MissingInitialMetadata,
    TimelineDiscontinuity,
    InvalidSamples,
    UnsupportedSpatialFeature(SpatialFeature),
}
/// Explicit admission failures: these controls must not silently become points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialFeature {
    Extent,
    Snap,
    HorizontalZone,
    ElevationExclusion,
    ScreenReference,
    Distance,
    Divergence,
    Trims,
    BedMetadata,
    AlternateObjectData,
}
impl RenderError {
    /// Stable WASM statuses: 200..207 render errors, 208..217 spatial refusals.
    pub fn status_code(self) -> u32 {
        200 + match self {
            Self::UnsupportedLayout => 0,
            Self::UnsupportedRole => 1,
            Self::InvalidMetadata => 2,
            Self::UnsupportedIsf => 3,
            Self::PendingLimit => 4,
            Self::MissingInitialMetadata => 5,
            Self::TimelineDiscontinuity => 6,
            Self::InvalidSamples => 7,
            Self::UnsupportedSpatialFeature(feature) => 8 + feature as u32,
        }
    }
}
fn validate_spatial_metadata(
    payload: &ObjectAudioMetadataPayload,
    allow_elevation_exclusion: bool,
) -> Result<(), RenderError> {
    let refuse = |feature| Err(RenderError::UnsupportedSpatialFeature(feature));
    // A malformed bed element may be skipped by the dependency. Its recorded
    // element ID must still prevent an unqualified fixed-route render.
    if payload.bed_object_element.is_some()
        || payload
            .oa_element_md
            .iter()
            .any(|m| m.oa_element_id_idx == 6)
    {
        return refuse(SpatialFeature::BedMetadata);
    }
    if payload.b_alternate_object_data_present
        || payload
            .oa_element_md
            .iter()
            .any(|m| m.alternate_object_data_id_idx.is_some())
    {
        return refuse(SpatialFeature::AlternateObjectData);
    }
    // The parser retains the last object_element. Earlier controls cannot be
    // inspected or selected by this renderer, even without an alternate flag.
    if payload
        .oa_element_md
        .iter()
        .filter(|m| m.oa_element_id_idx == 1)
        .count()
        > 1
    {
        return Err(RenderError::InvalidMetadata);
    }
    // ExtendedObjectElement::read depends on an already parsed object element,
    // and the parser overwrites prior ID5 state. Neither loss is recoverable.
    let mut saw_object = false;
    let mut saw_extension = false;
    for md in &payload.oa_element_md {
        if md.oa_element_id_idx == 1 {
            saw_object = true;
        } else if md.oa_element_id_idx == 5 {
            if !saw_object || saw_extension {
                return Err(RenderError::InvalidMetadata);
            }
            saw_extension = true;
        }
    }
    // Even apparently default trim payloads require a defined trim policy.
    if payload.trim_element.is_some() {
        return refuse(SpatialFeature::Trims);
    }
    let Some(element) = &payload.object_element else {
        return Ok(());
    };
    for (object, updates) in element
        .object_data
        .iter()
        .enumerate()
        .skip(payload.program_assignment.num_bed_objects)
    {
        for (block, update) in updates.iter().enumerate() {
            if update.b_object_not_active {
                continue;
            }
            let r = &update.object_render_info;
            if r.object_size.iter().any(|&size| size != 0.) {
                return refuse(SpatialFeature::Extent);
            }
            if r.b_object_snap {
                return refuse(SpatialFeature::Snap);
            }
            if r.zone_constraints_idx != 0 {
                return refuse(SpatialFeature::HorizontalZone);
            }
            if !r.b_enable_elevation && !allow_elevation_exclusion {
                return refuse(SpatialFeature::ElevationExclusion);
            }
            if r.b_object_use_screen_ref {
                return refuse(SpatialFeature::ScreenReference);
            }
            if r.b_object_distance_specified {
                return refuse(SpatialFeature::Distance);
            }
            if payload
                .extended_object_element
                .as_ref()
                .and_then(|e| e.object_div_block.get(object))
                .and_then(|updates| updates.get(block))
                .is_some_and(|d| d.b_object_divergence && d.object_divergence != 0.)
            {
                return refuse(SpatialFeature::Divergence);
            }
        }
    }
    Ok(())
}
#[derive(Clone, Copy)]
struct Speaker {
    name: &'static str,
    position: [f32; 3],
}
const BED714: [Speaker; 12] = [
    Speaker {
        name: "L",
        position: [0., 0., 0.],
    },
    Speaker {
        name: "R",
        position: [1., 0., 0.],
    },
    Speaker {
        name: "C",
        position: [0.5, 0., 0.],
    },
    Speaker {
        name: "LFE",
        position: [0.5, 0., 0.],
    },
    Speaker {
        name: "Ls",
        position: [0., 0.5, 0.],
    },
    Speaker {
        name: "Rs",
        position: [1., 0.5, 0.],
    },
    Speaker {
        name: "Lb",
        position: [0., 1., 0.],
    },
    Speaker {
        name: "Rb",
        position: [1., 1., 0.],
    },
    Speaker {
        name: "Tfl",
        position: [0.25, 0.25, 1.],
    },
    Speaker {
        name: "Tfr",
        position: [0.75, 0.25, 1.],
    },
    Speaker {
        name: "Tbl",
        position: [0.25, 0.75, 1.],
    },
    Speaker {
        name: "Tbr",
        position: [0.75, 0.75, 1.],
    },
];
impl Layout {
    fn speakers(self) -> Vec<Speaker> {
        match self {
            Self::SevenOneFour => BED714.to_vec(),
            Self::FiveOneFour => BED714
                .iter()
                .enumerate()
                .filter_map(|(i, s)| if i == 6 || i == 7 { None } else { Some(*s) })
                .collect(),
        }
    }
    pub fn labels(self) -> Vec<&'static str> {
        self.speakers().iter().map(|s| s.name).collect()
    }
    pub fn channels(self) -> usize {
        match self {
            Self::FiveOneFour => 10,
            Self::SevenOneFour => 12,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::FiveOneFour => "5.1.4",
            Self::SevenOneFour => "7.1.4",
        }
    }
}
fn values(items: impl Iterator<Item = f32>) -> Vec<f32> {
    let mut v: Vec<_> = items.collect();
    v.sort_by(f32::total_cmp);
    v.dedup();
    v
}
fn balance(items: &[f32], value: f32) -> Vec<(f32, f32)> {
    if value <= items[0] || items.len() == 1 {
        return vec![(items[0], 1.)];
    }
    if value >= items[items.len() - 1] {
        return vec![(items[items.len() - 1], 1.)];
    }
    let upper = items.partition_point(|v| *v < value);
    let lo = items[upper - 1];
    let hi = items[upper];
    let angle = (value - lo) / (hi - lo) * std::f32::consts::FRAC_PI_2;
    vec![(lo, angle.cos()), (hi, angle.sin())]
}
const MAX_LAYOUT_CHANNELS: usize = 12;

/// Constant-power floor/ceiling, front/back row and left/right column balance.
pub fn point_gains(layout: Layout, position: [f32; 3]) -> Result<Vec<f32>, RenderError> {
    Ok(point_gains_array(layout, position)?[..layout.channels()].to_vec())
}

fn point_gains_array(
    layout: Layout,
    position: [f32; 3],
) -> Result<[f32; MAX_LAYOUT_CHANNELS], RenderError> {
    if position.iter().any(|v| !v.is_finite()) {
        return Err(RenderError::InvalidMetadata);
    }
    let p = position.map(|v| v.clamp(0., 1.));
    let speakers = layout.speakers();
    let usable: Vec<_> = speakers
        .iter()
        .enumerate()
        .filter(|(_, s)| s.name != "LFE")
        .collect();
    let mut gains = [0.; MAX_LAYOUT_CHANNELS];
    for (z, z_gain) in balance(&values(usable.iter().map(|(_, s)| s.position[2])), p[2]) {
        let plane: Vec<_> = usable
            .iter()
            .filter(|(_, s)| s.position[2] == z)
            .copied()
            .collect();
        for (y, y_gain) in balance(&values(plane.iter().map(|(_, s)| s.position[1])), p[1]) {
            let row: Vec<_> = plane
                .iter()
                .filter(|(_, s)| s.position[1] == y)
                .copied()
                .collect();
            for (x, x_gain) in balance(&values(row.iter().map(|(_, s)| s.position[0])), p[0]) {
                let (i, _) = row.iter().find(|(_, s)| s.position[0] == x).unwrap();
                gains[*i] += z_gain * y_gain * x_gain;
            }
        }
    }
    Ok(gains)
}
fn role_position(name: &str) -> Option<[f32; 3]> {
    Some(match name {
        "L" => [0., 0., 0.],
        "R" => [1., 0., 0.],
        "C" => [0.5, 0., 0.],
        "Ls" => [0., 0.5, 0.],
        "Rs" => [1., 0.5, 0.],
        "Lb" => [0., 1., 0.],
        "Rb" => [1., 1., 0.],
        "Tfl" => [0.25, 0.25, 1.],
        "Tfr" => [0.75, 0.25, 1.],
        "Tbl" => [0.25, 0.75, 1.],
        "Tbr" => [0.75, 0.75, 1.],
        "Tsl" | "Tml" => [0.25, 0.5, 1.],
        "Tsr" | "Tmr" => [0.75, 0.5, 1.],
        "Lw" => [0., 0.25, 0.],
        "Rw" => [1., 0.25, 0.],
        _ => return None,
    })
}
fn static_role(name: &str) -> Result<Role, RenderError> {
    let known = match name {
        "L" => "L",
        "R" => "R",
        "C" => "C",
        "LFE" => "LFE",
        "LFE2" => "LFE2",
        "Ls" => "Ls",
        "Rs" => "Rs",
        "Lb" => "Lb",
        "Rb" => "Rb",
        "Tfl" => "Tfl",
        "Tfr" => "Tfr",
        "Tbl" => "Tbl",
        "Tbr" => "Tbr",
        "Tsl" | "Tml" => "Tml",
        "Tsr" | "Tmr" => "Tmr",
        "Lw" => "Lw",
        "Rw" => "Rw",
        _ => return Err(RenderError::UnsupportedRole),
    };
    Ok(Role::Bed(known))
}
fn target_gains(
    layout: Layout,
    role: Role,
    state: ElementState,
) -> Result<[f32; MAX_LAYOUT_CHANNELS], RenderError> {
    if !state.gain.is_finite() {
        return Err(RenderError::InvalidMetadata);
    }
    let mut gains = [0.; MAX_LAYOUT_CHANNELS];
    match role {
        Role::Object => gains = point_gains_array(layout, state.position)?,
        Role::Bed("LFE" | "LFE2") => gains[3] = 1.,
        Role::Bed(name) => {
            if let Some(i) = layout.labels().iter().position(|s| *s == name) {
                gains[i] = 1.;
            } else if layout == Layout::FiveOneFour && ["Lb", "Rb"].contains(&name) {
                gains[if name == "Lb" { 4 } else { 5 }] = 1.;
            } else {
                gains = point_gains_array(
                    layout,
                    role_position(name).ok_or(RenderError::UnsupportedRole)?,
                )?;
            }
        }
    }
    for value in &mut gains[..layout.channels()] {
        *value *= state.gain;
    }
    Ok(gains)
}
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
struct Gain {
    origin: [f32; MAX_LAYOUT_CHANNELS],
    target: [f32; MAX_LAYOUT_CHANNELS],
    total: u32,
    remaining: u32,
    state: Option<ElementState>,
}
impl Gain {
    fn current(&self) -> [f32; MAX_LAYOUT_CHANNELS] {
        if self.remaining == 0 {
            return self.target;
        }
        let t = (self.total - self.remaining) as f32 / self.total as f32;
        std::array::from_fn(|i| self.origin[i] + (self.target[i] - self.origin[i]) * t)
    }
    fn mix_next_sample(&mut self, input: f32, output: &mut [f32]) {
        self.remaining = self.remaining.saturating_sub(1);
        if self.remaining == 0 {
            for (sample, &gain) in output.iter_mut().zip(&self.target) {
                *sample += input * gain;
            }
        } else {
            let t = (self.total - self.remaining) as f32 / self.total as f32;
            for ((sample, &origin), &target) in
                output.iter_mut().zip(&self.origin).zip(&self.target)
            {
                *sample += input * (origin + (target - origin) * t);
            }
        }
    }
}
pub struct RenderedBlock {
    pub pcm: Vec<f32>,
    pub channels: usize,
    pub labels: Vec<&'static str>,
    pub applied_updates: u64,
}
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct ObjectRenderer {
    layout: Layout,
    allow_elevation_exclusion: bool,
    roles: Vec<Role>,
    gains: Vec<Gain>,
    pending: VecDeque<TimedUpdate>,
    next_frame: Option<u64>,
    applied: u64,
}
impl ObjectRenderer {
    #[cfg(any(test, target_arch = "wasm32"))]
    pub(crate) fn checkpoint_activity(&self) -> (u32, u32) {
        let active = self.gains.iter().filter(|gain| gain.remaining != 0).count();
        (active as u32, self.pending.len() as u32)
    }

    /// Preserve renderer history while rebasing the next sample to a fresh decoder.
    #[cfg(any(test, target_arch = "wasm32"))]
    pub(crate) fn restart_checkpoint(&self, frame: u64) -> Result<Self, RenderError> {
        if self.next_frame.ok_or(RenderError::MissingInitialMetadata)? != frame {
            return Err(RenderError::TimelineDiscontinuity);
        }
        let mut checkpoint = self.clone();
        for update in &mut checkpoint.pending {
            update.frame = update
                .frame
                .checked_sub(frame)
                .ok_or(RenderError::TimelineDiscontinuity)?;
        }
        checkpoint.next_frame = Some(0);
        Ok(checkpoint)
    }

    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            allow_elevation_exclusion: false,
            roles: Vec::new(),
            gains: Vec::new(),
            pending: VecDeque::new(),
            next_frame: None,
            applied: 0,
        }
    }
    /// Experimental policy for object metadata that excludes height speakers.
    pub fn with_elevation_exclusion(layout: Layout) -> Self {
        Self {
            allow_elevation_exclusion: true,
            ..Self::new(layout)
        }
    }
    pub fn with_roles(layout: Layout, roles: Vec<Role>) -> Result<Self, RenderError> {
        let mut renderer = Self::new(layout);
        renderer.configure(roles)?;
        Ok(renderer)
    }
    fn configure(&mut self, roles: Vec<Role>) -> Result<(), RenderError> {
        if roles.is_empty() || roles.len() > 16 {
            return Err(RenderError::InvalidMetadata);
        }
        self.gains = roles
            .iter()
            .map(|&role| {
                let target = match role {
                    Role::Object => [0.; MAX_LAYOUT_CHANNELS],
                    _ => target_gains(
                        self.layout,
                        role,
                        ElementState {
                            position: [0.; 3],
                            gain: 1.,
                        },
                    )?,
                };
                Ok(Gain {
                    origin: target,
                    target,
                    total: 0,
                    remaining: 0,
                    state: None,
                })
            })
            .collect::<Result<_, RenderError>>()?;
        self.roles = roles;
        Ok(())
    }
    pub fn schedule(&mut self, update: TimedUpdate) -> Result<(), RenderError> {
        if self.next_frame.is_some_and(|frame| update.frame < frame) {
            return Err(RenderError::TimelineDiscontinuity);
        }
        if update.roles.len() != update.states.len()
            || update.roles.len() > 16
            || update.roles.is_empty()
        {
            return Err(RenderError::InvalidMetadata);
        }
        if self.pending.len() >= 64 {
            return Err(RenderError::PendingLimit);
        }
        if update
            .states
            .iter()
            .any(|s| !s.gain.is_finite() || s.position.iter().any(|v| !v.is_finite()))
        {
            return Err(RenderError::InvalidMetadata);
        }
        let at = self.pending.partition_point(|p| p.frame <= update.frame);
        self.pending.insert(at, update);
        Ok(())
    }
    fn ingest(
        &mut self,
        payload: &ObjectAudioMetadataPayload,
        start: u64,
        channels: usize,
    ) -> Result<(), RenderError> {
        let Some(element) = &payload.object_element else {
            return Ok(());
        };
        let program = &payload.program_assignment;
        if program.num_isf_objects > 0 {
            return Err(RenderError::UnsupportedIsf);
        }
        if program.b_bed_chan_distribute {
            return Err(RenderError::UnsupportedRole);
        }
        let bed_indices: Vec<_> = program
            .bed_assignment
            .iter()
            .flat_map(|b| b.to_index_vec())
            .collect();
        if payload.object_count != channels
            || channels > 16
            || bed_indices.len() != program.num_bed_objects
            || program.num_bed_objects + program.num_dynamic_objects != channels
        {
            return Err(RenderError::InvalidMetadata);
        }
        let names = [
            "L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb", "Tfl", "Tfr", "Tml", "Tmr", "Tbl", "Tbr",
            "Lw", "Rw", "LFE2",
        ];
        let roles = (0..channels)
            .map(|i| {
                if i < bed_indices.len() {
                    static_role(names[bed_indices[i]])
                } else {
                    Ok(Role::Object)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let info = &element.md_update_info;
        if info.block_update_info.len() != info.num_obj_info_blocks
            || element.object_data.len() != channels
            || element
                .object_data
                .iter()
                .any(|v| v.len() != info.num_obj_info_blocks)
        {
            return Err(RenderError::InvalidMetadata);
        }
        for (b, timing) in info.block_update_info.iter().enumerate() {
            let states = element
                .object_data
                .iter()
                .enumerate()
                .map(|(i, updates)| {
                    let data = &updates[b];
                    let mut position = data.object_render_info.pos3d;
                    if let Some(ext) = payload
                        .extended_object_element
                        .as_ref()
                        .and_then(|e| e.ext_prec_pos_block.get(i))
                        .and_then(|e| e.get(b))
                    {
                        position[0] += ext.ext_prec_pos3d_x;
                        position[1] += ext.ext_prec_pos3d_y;
                        position[2] += ext.ext_prec_pos3d_z;
                    }
                    if self.allow_elevation_exclusion
                        && roles[i] == Role::Object
                        && !data.object_render_info.b_enable_elevation
                    {
                        // The existing panner selects only the ear-level plane at Z=0.
                        position[2] = 0.;
                    }
                    ElementState {
                        position: position.map(|v| v.clamp(0., 1.) as f32),
                        gain: if data.b_object_not_active
                            || data.object_basic_info.object_gain == GAIN_MINUS_INFINITY
                        {
                            0.
                        } else {
                            10f32.powf(data.object_basic_info.object_gain as f32 / 20.)
                        },
                    }
                })
                .collect();
            let due = start
                .checked_add(payload.evo_sample_offset)
                .and_then(|v| v.checked_add(info.sample_offset as u64))
                .and_then(|v| v.checked_add(32 * u64::from(timing.block_offset_factor_bits)))
                .ok_or(RenderError::InvalidMetadata)?;
            if due - start > 8192 {
                return Err(RenderError::InvalidMetadata);
            }
            self.schedule(TimedUpdate {
                frame: due,
                ramp: u32::from(timing.ramp_duration),
                roles: roles.clone(),
                states,
            })?;
        }
        Ok(())
    }
    fn apply(&mut self, update: TimedUpdate) -> Result<(), RenderError> {
        if update.roles != self.roles {
            self.configure(update.roles)?;
        }
        for (i, state) in update.states.into_iter().enumerate() {
            if self.gains[i].state == Some(state) {
                continue;
            }
            let target = target_gains(self.layout, self.roles[i], state)?;
            let gain = &mut self.gains[i];
            let first = gain.state.is_none();
            if first || update.ramp == 0 {
                gain.origin = target;
                gain.remaining = 0;
                gain.total = 0;
            } else {
                gain.origin = gain.current();
                gain.remaining = update.ramp;
                gain.total = update.ramp;
            }
            gain.target = target;
            gain.state = Some(state);
        }
        self.applied += 1;
        Ok(())
    }
    /// Commit renderer control state only after the complete block succeeds.
    pub fn render_block(&mut self, block: &AudioBlock) -> Result<RenderedBlock, RenderError> {
        // With roles configured and no updates inside the block, the mixer
        // only advances existing ramps. All fallible checks precede mixing.
        if !self.roles.is_empty()
            && block.metadata.is_empty()
            && self.pending.front().is_none_or(|update| {
                block
                    .output_frame_offset
                    .checked_add(block.frames as u64)
                    .is_some_and(|end| update.frame >= end)
            })
        {
            return self.render_block_inner(block);
        }
        let mut staged = self.clone();
        let rendered = staged.render_block_inner(block)?;
        *self = staged;
        Ok(rendered)
    }
    fn render_block_inner(&mut self, block: &AudioBlock) -> Result<RenderedBlock, RenderError> {
        if block.channels == 0
            || block.channels > 16
            || block.frames > 160
            || block.pcm.len() != block.frames * block.channels
        {
            return Err(RenderError::InvalidSamples);
        }
        let end = block
            .output_frame_offset
            .checked_add(block.frames as u64)
            .ok_or(RenderError::TimelineDiscontinuity)?;
        if self
            .next_frame
            .is_some_and(|f| f != block.output_frame_offset)
        {
            return Err(RenderError::TimelineDiscontinuity);
        }
        // Check every payload before any scheduling or mixing. Rejection must
        // not leave a previous payload's updates queued at this sample offset.
        for payload in &block.metadata {
            validate_spatial_metadata(payload, self.allow_elevation_exclusion)?;
        }
        for payload in &block.metadata {
            self.ingest(payload, block.output_frame_offset, block.channels)?;
        }
        if self
            .pending
            .front()
            .is_some_and(|update| update.frame < block.output_frame_offset)
        {
            return Err(RenderError::TimelineDiscontinuity);
        }
        if self.roles.is_empty() {
            if let Some(update) = self.pending.front() {
                if update.frame > block.output_frame_offset {
                    return Err(RenderError::MissingInitialMetadata);
                }
                self.configure(update.roles.clone())?;
            } else if block.labels.len() == block.channels {
                self.configure(
                    block
                        .labels
                        .iter()
                        .map(|s| static_role(s))
                        .collect::<Result<_, _>>()?,
                )?;
            } else {
                return Err(RenderError::MissingInitialMetadata);
            }
        }
        if self.roles.len() != block.channels {
            return Err(RenderError::InvalidMetadata);
        }
        let output_channels = self.layout.channels();
        let mut pcm = vec![0.; block.frames * output_channels];
        for frame in 0..block.frames {
            let now = block.output_frame_offset + frame as u64;
            while self.pending.front().is_some_and(|u| u.frame == now) {
                let update = self.pending.pop_front().unwrap();
                if update.roles.len() != block.channels {
                    return Err(RenderError::InvalidMetadata);
                }
                self.apply(update)?;
            }
            let input = &block.pcm[frame * block.channels..][..block.channels];
            let output = &mut pcm[frame * output_channels..][..output_channels];
            for (gains, &sample) in self.gains.iter_mut().zip(input) {
                gains.mix_next_sample(sample as f32 / 8388608., output);
            }
        }
        self.next_frame = Some(end);
        Ok(RenderedBlock {
            pcm,
            channels: output_channels,
            labels: self.layout.labels(),
            applied_updates: self.applied,
        })
    }
    /// Renders one block of already normalised float planes (one per configured role, equal length)
    /// starting at output frame `frame_offset`. Same timeline rules as [`render_block`](Self::render_block):
    /// scheduled updates apply at their exact frame, ramps are linear per output sample. Used by the
    /// E-AC-3 JOC path, whose objects are float and not limited to 160 frames per block.
    pub fn render_planar(&mut self, planes: &[&[f32]], frame_offset: u64) -> Result<RenderedBlock, RenderError> {
        let frames = planes.first().map_or(0, |p| p.len());
        if planes.is_empty() || planes.len() > 16 || planes.iter().any(|p| p.len() != frames) {
            return Err(RenderError::InvalidSamples);
        }
        if self.next_frame.is_some_and(|f| f != frame_offset) {
            return Err(RenderError::TimelineDiscontinuity);
        }
        if self.pending.front().is_some_and(|u| u.frame < frame_offset) {
            return Err(RenderError::TimelineDiscontinuity);
        }
        if self.roles.is_empty() {
            let Some(update) = self.pending.front() else {
                return Err(RenderError::MissingInitialMetadata);
            };
            if update.frame > frame_offset {
                return Err(RenderError::MissingInitialMetadata);
            }
            self.configure(update.roles.clone())?;
        }
        if self.roles.len() != planes.len() {
            return Err(RenderError::InvalidMetadata);
        }
        let output_channels = self.layout.channels();
        let mut pcm = vec![0.; frames * output_channels];
        for frame in 0..frames {
            let now = frame_offset + frame as u64;
            while self.pending.front().is_some_and(|u| u.frame <= now) {
                let update = self.pending.pop_front().unwrap();
                if update.roles.len() != planes.len() {
                    return Err(RenderError::InvalidMetadata);
                }
                self.apply(update)?;
            }
            let output = &mut pcm[frame * output_channels..][..output_channels];
            for (gain, plane) in self.gains.iter_mut().zip(planes) {
                gain.mix_next_sample(plane[frame], output);
            }
        }
        self.next_frame = Some(frame_offset + frames as u64);
        Ok(RenderedBlock { pcm, channels: output_channels, labels: self.layout.labels(), applied_updates: self.applied })
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use truehd::structs::oamd::{BlockUpdateInfo, MDUpdateInfo, ObjectElement, ObjectInfoBlock};

    fn payload() -> ObjectAudioMetadataPayload {
        let mut info = ObjectInfoBlock::default();
        info.object_basic_info.object_gain = 0;
        let mut p = ObjectAudioMetadataPayload::default();
        p.object_count = 1;
        p.program_assignment.num_dynamic_objects = 1;
        p.object_element = Some(ObjectElement {
            md_update_info: MDUpdateInfo {
                sample_offset: 0,
                num_obj_info_blocks: 1,
                block_update_info: vec![BlockUpdateInfo::default()],
            },
            object_data: vec![vec![info]],
            ..Default::default()
        });
        p
    }
    fn block(offset: u64) -> AudioBlock {
        AudioBlock {
            sample_rate: 48000,
            frames: 40,
            channels: 1,
            presentation: 3,
            pcm: vec![4194304; 40],
            labels: vec![],
            metadata: vec![],
            is_duplicate: false,
            access_unit: 0,
            input_offset: 0,
            output_frame_offset: offset,
        }
    }
    #[test]
    fn metadata_offset_and_ramp_cross_the_access_unit_boundary() {
        let mut initial = payload();
        let info = &mut initial.object_element.as_mut().unwrap().object_data[0][0];
        info.object_render_info.b_enable_elevation = true;
        info.object_render_info.pos3d = [0., 0., 0.];

        let mut moved = initial.clone();
        moved.evo_sample_offset = 3;
        let element = moved.object_element.as_mut().unwrap();
        element.md_update_info.sample_offset = 4;
        element.md_update_info.block_update_info[0].block_offset_factor_bits = 1;
        element.md_update_info.block_update_info[0].ramp_duration = 2;
        element.object_data[0][0].object_render_info.pos3d = [1., 0., 0.];

        let mut renderer = ObjectRenderer::new(Layout::SevenOneFour);
        let mut first = block(0);
        first.metadata = vec![initial, moved];
        let first = renderer.render_block(&first).unwrap();
        assert_eq!(first.applied_updates, 2);
        assert_eq!(&first.pcm[38 * 12..38 * 12 + 2], &[0.5, 0.]);
        assert_eq!(&first.pcm[39 * 12..39 * 12 + 2], &[0.25, 0.25]);
        let second = renderer.render_block(&block(40)).unwrap();
        assert_eq!(&second.pcm[..2], &[0., 0.5]);
    }

    #[test]
    fn late_metadata_is_rejected_without_changing_renderer_state() {
        let roles = vec![Role::Object];
        let mut renderer = ObjectRenderer::with_roles(Layout::SevenOneFour, roles.clone()).unwrap();
        let mut input = block(0);
        input.metadata = vec![payload()];
        renderer.render_block(&input).unwrap();
        let before = renderer.clone();
        let mut update = TimedUpdate {
            frame: 39,
            ramp: 0,
            roles,
            states: vec![ElementState {
                position: [1., 0., 0.],
                gain: 1.,
            }],
        };
        assert_eq!(
            renderer.schedule(update.clone()),
            Err(RenderError::TimelineDiscontinuity)
        );
        assert_eq!(renderer, before);
        update.frame = 40;
        renderer.schedule(update).unwrap();
        assert_eq!(
            renderer.render_block(&block(40)).unwrap().applied_updates,
            2
        );
    }

    #[test]
    fn renderer_cannot_start_after_a_pending_update() {
        let mut renderer = ObjectRenderer::new(Layout::SevenOneFour);
        renderer.ingest(&payload(), 0, 1).unwrap();
        let before = renderer.clone();
        assert!(matches!(
            renderer.render_block(&block(1)),
            Err(RenderError::TimelineDiscontinuity)
        ));
        assert_eq!(renderer, before);
        assert_eq!(renderer.render_block(&block(0)).unwrap().applied_updates, 1);
    }

    #[test]
    fn metadata_near_the_end_of_the_frame_clock_remains_checked() {
        let mut renderer = ObjectRenderer::new(Layout::SevenOneFour);
        let mut input = block(u64::MAX - 40);
        input.metadata = vec![payload()];
        assert!(renderer.render_block(&input).is_ok());
        let before = renderer.clone();
        assert!(matches!(
            renderer.render_block(&block(u64::MAX)),
            Err(RenderError::TimelineDiscontinuity)
        ));
        assert_eq!(renderer, before);
    }

    #[test]
    fn errors_preserve_every_control_state_field_and_retry_matches_reference() {
        let roles = vec![Role::Object];
        let mut renderer = ObjectRenderer::with_roles(Layout::SevenOneFour, roles.clone()).unwrap();
        for (frame, ramp, x) in [(0, 0, 0.), (32, 40, 1.), (50, 4, 0.5)] {
            renderer
                .schedule(TimedUpdate {
                    frame,
                    ramp,
                    roles: roles.clone(),
                    states: vec![ElementState {
                        position: [x, 0., 0.],
                        gain: 1.,
                    }],
                })
                .unwrap();
        }
        renderer.render_block(&block(0)).unwrap();
        let before = renderer.clone();
        let mut bad = block(40);
        let mut invalid = payload();
        invalid.object_count = 2;
        bad.metadata = vec![payload(), invalid];
        assert!(matches!(
            renderer.render_block(&bad),
            Err(RenderError::InvalidMetadata)
        ));
        assert_eq!(renderer, before, "all renderer fields must survive error");
        let mut reference = before.clone();
        let valid = block(40);
        let a = renderer.render_block(&valid).unwrap();
        let b = reference.render_block(&valid).unwrap();
        assert_eq!(a.pcm, b.pcm);
        assert_eq!(a.labels, b.labels);
        assert_eq!(a.applied_updates, b.applied_updates);
        assert_eq!(renderer, reference);
        let checkpoint = before.restart_checkpoint(40).unwrap();
        let bytes = checkpoint::encode_checkpoint(3, &checkpoint).unwrap();
        let (selection, mut imported) = checkpoint::decode_checkpoint(&bytes).unwrap();
        assert_eq!(selection, 3);
        assert_eq!(before.checkpoint_activity(), (1, 1));
        assert_eq!(imported.checkpoint_activity(), (1, 1));
        assert_eq!(imported, checkpoint);
        for end in 0..bytes.len() {
            assert!(checkpoint::decode_checkpoint(&bytes[..end]).is_err());
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(checkpoint::decode_checkpoint(&extra).is_err());
        let resumed = imported.render_block(&block(0)).unwrap();
        assert_eq!(resumed.pcm, a.pcm);
        assert_eq!(resumed.applied_updates, a.applied_updates);
    }
    #[test]
    fn control_state_clone_logical_upper_bound() {
        use std::mem::size_of;
        let updates =
            64 * (size_of::<TimedUpdate>() + 16 * (size_of::<Role>() + size_of::<ElementState>()));
        let gains = 16 * size_of::<Gain>();
        let roles = 16 * size_of::<Role>();
        let bytes = size_of::<ObjectRenderer>() + updates + gains + roles;
        println!(
            "bounded logical control clone bytes={bytes}, excluding allocator bookkeeping and capacity overhead"
        );
        assert!(bytes <= 65536);
    }
}
