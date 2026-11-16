//! Object audio metadata payload (ETSI TS 103 420 clause 5), EMDF payload id 11.

use crate::bits::BitReader;

/// Speaker labels of bed objects (clause 4.4 `speaker_labels`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpeakerLabel {
    L,
    R,
    C,
    Lfe,
    Ls,
    Rs,
    Lb,
    Rb,
    Tfl,
    Tfr,
    Tsl,
    Tsr,
    Tbl,
    Tbr,
    Lw,
    Rw,
    Lfe2,
}

impl SpeakerLabel {
    pub fn name(self) -> &'static str {
        use SpeakerLabel::*;
        match self {
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
            Tsl => "Tsl",
            Tsr => "Tsr",
            Tbl => "Tbl",
            Tbr => "Tbr",
            Lw => "Lw",
            Rw => "Rw",
            Lfe2 => "LFE2",
        }
    }

    pub fn is_lfe(self) -> bool {
        matches!(self, SpeakerLabel::Lfe | SpeakerLabel::Lfe2)
    }

    /// Room-anchored position of the speaker (clause 4.2.1: x 0..1 left to
    /// right, y 0..1 front to back, z -1..1 floor to ceiling), converted from
    /// the ADM coordinates of table B.10 (x_adm = 2x - 1, y_adm = 1 - 2y).
    pub fn nominal_position(self) -> [f32; 3] {
        use SpeakerLabel::*;
        match self {
            L => [0.0, 0.0, 0.0],
            R => [1.0, 0.0, 0.0],
            C => [0.5, 0.0, 0.0],
            Lfe | Lfe2 => [0.5, 0.0, -1.0],
            Ls => [0.0, 0.5, 0.0],
            Rs => [1.0, 0.5, 0.0],
            Lb => [0.0, 1.0, 0.0],
            Rb => [1.0, 1.0, 0.0],
            Tfl => [0.0, 0.0, 1.0],
            Tfr => [1.0, 0.0, 1.0],
            Tsl => [0.0, 0.5, 1.0],
            Tsr => [1.0, 0.5, 1.0],
            Tbl => [0.0, 1.0, 1.0],
            Tbr => [1.0, 1.0, 1.0],
            Lw => [0.0, 10.0 / 62.0, 0.0],
            Rw => [1.0, 10.0 / 62.0, 0.0],
        }
    }
}

/// Order of the labels for `bed_channel_assignment[]`, index 9 (the first bit
/// read) down to index 0 (table 12).
const STD_BED: [&[SpeakerLabel]; 10] = [
    &[SpeakerLabel::L, SpeakerLabel::R],
    &[SpeakerLabel::C],
    &[SpeakerLabel::Lfe],
    &[SpeakerLabel::Ls, SpeakerLabel::Rs],
    &[SpeakerLabel::Lb, SpeakerLabel::Rb],
    &[SpeakerLabel::Tfl, SpeakerLabel::Tfr],
    &[SpeakerLabel::Tsl, SpeakerLabel::Tsr],
    &[SpeakerLabel::Tbl, SpeakerLabel::Tbr],
    &[SpeakerLabel::Lw, SpeakerLabel::Rw],
    &[SpeakerLabel::Lfe2],
];

/// Order for `nonstd_bed_channel_assignment[]`, index 16 first (table 13).
const NONSTD_BED: [SpeakerLabel; 17] = [
    SpeakerLabel::L,
    SpeakerLabel::R,
    SpeakerLabel::C,
    SpeakerLabel::Lfe,
    SpeakerLabel::Ls,
    SpeakerLabel::Rs,
    SpeakerLabel::Lb,
    SpeakerLabel::Rb,
    SpeakerLabel::Tfl,
    SpeakerLabel::Tfr,
    SpeakerLabel::Tsl,
    SpeakerLabel::Tsr,
    SpeakerLabel::Tbl,
    SpeakerLabel::Tbr,
    SpeakerLabel::Lw,
    SpeakerLabel::Rw,
    SpeakerLabel::Lfe2,
];

#[derive(Clone, Debug, Default)]
pub struct BedInstance {
    pub lfe_only: bool,
    pub labels: Vec<SpeakerLabel>,
}

#[derive(Clone, Debug, Default)]
pub struct ProgramAssignment {
    pub dyn_object_only: bool,
    pub lfe_present: bool,
    pub content_description: u8,
    pub beds: Vec<BedInstance>,
    pub isf_idx: Option<u8>,
    pub num_dynamic_objects: usize,
}

impl ProgramAssignment {
    pub fn num_bed_objects(&self) -> usize {
        self.beds.iter().map(|b| if b.lfe_only { 1 } else { b.labels.len() }).sum()
    }

    pub fn num_isf_objects(&self) -> usize {
        match self.isf_idx {
            Some(0) => 4,
            Some(1) => 8,
            Some(2) => 10,
            Some(3) => 14,
            Some(4) => 15,
            Some(5) => 30,
            _ => 0,
        }
    }
}

/// Position as transmitted, resolved to the room-anchored coordinate system
/// unless `screen_factor > 0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectState {
    pub active: bool,
    /// Linear gain (clause 5.6.1.4).
    pub gain: f32,
    pub priority: f32,
    /// Room-anchored coordinates (x left to right, y front to back, z up).
    pub pos: [f32; 3],
    /// Last standard-precision values, needed for differential coding.
    pos_bits: [i32; 3],
    pub distance: Option<f32>,
    pub at_infinity: bool,
    pub size: [f32; 3],
    pub zone_constraints_idx: u8,
    pub enable_elevation: bool,
    pub screen_factor: f32,
    pub depth_factor: f32,
    pub snap: bool,
    pub divergence_present: bool,
}

impl Default for ObjectState {
    fn default() -> Self {
        Self {
            active: false,
            gain: 1.0,
            priority: 1.0,
            pos: [0.5, 0.5, 0.0],
            pos_bits: [31, 31, 0],
            distance: None,
            at_infinity: false,
            size: [0.0; 3],
            zone_constraints_idx: 0,
            enable_elevation: true,
            screen_factor: 0.0,
            depth_factor: 1.0,
            snap: false,
            divergence_present: false,
        }
    }
}

/// One metadata update block of one object.
#[derive(Clone, Debug)]
pub struct ObjectBlock {
    pub state: ObjectState,
    /// Bed objects have a speaker anchored position instead of `state.pos`.
    pub bed_label: Option<SpeakerLabel>,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateTiming {
    /// `sample_offset` + 32 * `block_offset_factor` (samples into the frame).
    pub start_sample: u32,
    pub ramp_duration: u32,
}

#[derive(Clone, Debug, Default)]
pub struct OamdFrame {
    pub version: u32,
    pub object_count: usize,
    pub program: ProgramAssignment,
    pub sample_offset: u32,
    pub timing: Vec<UpdateTiming>,
    /// `[object][block]`
    pub objects: Vec<Vec<ObjectBlock>>,
    pub trim_present: bool,
    pub bits_used: usize,
    pub bits_total: usize,
    /// True if every sized oa_element was consumed exactly (padding zero).
    pub clean: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OamdError {
    Truncated,
    Unsupported,
}

/// Persistent state across frames (the "previous metadata update").
#[derive(Clone, Debug, Default)]
pub struct OamdState {
    prev: Vec<ObjectState>,
}

impl OamdState {
    pub fn reset(&mut self) {
        self.prev.clear();
    }
}

fn gain_from_bits(b: u32) -> f32 {
    let db = if b <= 14 { 15 - b as i32 } else { 14 - b as i32 };
    10f32.powf(db as f32 / 20.0)
}

const DISTANCE_FACTOR: [f32; 16] =
    [1.1, 1.3, 1.6, 2.0, 2.5, 3.2, 4.0, 5.0, 6.3, 7.9, 10.0, 12.6, 15.8, 20.0, 25.1, 50.1];
const DEPTH_FACTOR: [f32; 4] = [0.25, 0.5, 1.0, 2.0];
const SAMPLE_OFFSET_IDX: [u32; 4] = [8, 16, 18, 24];
const RAMP_IDX: [u32; 16] =
    [32, 64, 128, 256, 320, 480, 1000, 1001, 1024, 1600, 1601, 1602, 1920, 2000, 2002, 2048];
const EXT_PREC: [i32; 4] = [1, 2, -1, -2];

fn parse_program(br: &mut BitReader<'_>) -> ProgramAssignment {
    let mut p = ProgramAssignment::default();
    p.dyn_object_only = br.flag();
    if p.dyn_object_only {
        p.lfe_present = br.flag();
        return p;
    }
    p.content_description = br.bits(4) as u8;
    if p.content_description & 8 != 0 {
        let _distribute = br.flag();
        let multiple = br.flag();
        let n = if multiple { br.bits(3) as usize + 2 } else { 1 };
        for _ in 0..n {
            let mut bed = BedInstance { lfe_only: br.flag(), labels: Vec::new() };
            if !bed.lfe_only {
                if br.flag() {
                    let mask = br.bits(10);
                    for idx in (0..10).rev() {
                        if mask & (1 << idx) != 0 {
                            bed.labels.extend_from_slice(STD_BED[9 - idx]);
                        }
                    }
                } else {
                    let mask = br.bits(17);
                    for idx in (0..17).rev() {
                        if mask & (1 << idx) != 0 {
                            bed.labels.push(NONSTD_BED[16 - idx]);
                        }
                    }
                }
            } else {
                bed.labels.push(SpeakerLabel::Lfe);
            }
            p.beds.push(bed);
        }
    }
    if p.content_description & 4 != 0 {
        p.isf_idx = Some(br.bits(3) as u8);
    }
    if p.content_description & 2 != 0 {
        let mut v = br.bits(5);
        if v == 0x1F {
            v += br.bits(7);
        }
        p.num_dynamic_objects = v as usize + 1;
    }
    if p.content_description & 1 != 0 {
        let size = br.bits(4) as usize + 1;
        br.skip(size * 8);
    }
    p
}

/// Parses an OAMD payload. `state` carries the previous update across frames.
pub fn parse_oamd(data: &[u8], state: &mut OamdState) -> Result<OamdFrame, OamdError> {
    let mut br = BitReader::new(data);
    let mut f = OamdFrame { bits_total: data.len() * 8, clean: true, ..Default::default() };
    f.version = br.bits(2);
    if f.version == 3 {
        f.version += br.bits(3);
    }
    let mut oc = br.bits(5);
    if oc == 0x1F {
        oc += br.bits(7);
    }
    f.object_count = oc as usize + 1;
    f.program = parse_program(&mut br);
    let alt_present = br.flag();
    let mut elems = br.bits(4);
    if elems == 0xF {
        elems += br.bits(5);
    }
    if br.overrun {
        return Err(OamdError::Truncated);
    }
    let nbed = f.program.num_bed_objects();
    let nisf = f.program.num_isf_objects();
    let lfe_obj = f.program.dyn_object_only && f.program.lfe_present;
    let in_bed_or_isf = nbed + nisf + lfe_obj as usize;
    // Bed labels per object index.
    let mut bed_labels: Vec<Option<SpeakerLabel>> = Vec::new();
    if lfe_obj {
        bed_labels.push(Some(SpeakerLabel::Lfe));
    }
    for b in &f.program.beds {
        for l in &b.labels {
            bed_labels.push(Some(*l));
        }
    }
    let n_obj = f.object_count;
    while bed_labels.len() < n_obj {
        bed_labels.push(None);
    }
    if state.prev.len() != n_obj {
        state.prev = vec![ObjectState::default(); n_obj];
    }
    let mut ext_inactive: Vec<Vec<bool>> = Vec::new();

    for _ in 0..elems {
        let id = br.bits(4);
        let size = br.variable_bits_max(4, 4) as usize + 1;
        let body_start = br.pos();
        let body_end = body_start + size * 8;
        if body_end > br.end() {
            return Err(OamdError::Truncated);
        }
        if alt_present {
            br.skip(4);
        }
        let _discard = br.flag();
        let mut eb = BitReader::with_len(data, body_end);
        eb.seek(br.pos());
        match id {
            1 => {
                parse_object_element(&mut eb, &mut f, state, n_obj, in_bed_or_isf, &bed_labels, &mut ext_inactive);
            }
            2 => {
                f.trim_present = true;
            }
            5 => {
                parse_extended_object_element(&mut eb, &mut f, state, &ext_inactive, in_bed_or_isf);
            }
            _ => {}
        }
        // Any remaining bits in the element must be zero padding.
        if id == 1 || id == 5 {
            if eb.overrun || eb.pos() > body_end {
                f.clean = false;
            } else {
                let mut r = eb.clone();
                while r.pos() < body_end {
                    if r.bit() != 0 {
                        f.clean = false;
                        break;
                    }
                }
            }
        }
        br.seek(body_end);
    }
    f.bits_used = br.pos();
    Ok(f)
}

fn parse_object_element(
    br: &mut BitReader<'_>,
    f: &mut OamdFrame,
    state: &mut OamdState,
    n_obj: usize,
    in_bed_or_isf: usize,
    bed_labels: &[Option<SpeakerLabel>],
    ext_inactive: &mut Vec<Vec<bool>>,
) {
    // md_update_info()
    let so_code = br.bits(2);
    f.sample_offset = match so_code {
        1 => SAMPLE_OFFSET_IDX[br.bits(2) as usize],
        2 => br.bits(5),
        _ => 0,
    };
    let nblk = br.bits(3) as usize + 1;
    f.timing.clear();
    for _ in 0..nblk {
        let bo = br.bits(6);
        let rc = br.bits(2);
        let ramp = match rc {
            0 => 0,
            1 => 512,
            2 => 1536,
            _ => {
                if br.flag() {
                    RAMP_IDX[br.bits(4) as usize]
                } else {
                    br.bits(11)
                }
            }
        };
        f.timing.push(UpdateTiming { start_sample: f.sample_offset + 32 * bo, ramp_duration: ramp });
    }
    if !br.flag() {
        // b_reserved_data_not_present == 0
        br.skip(5);
    }
    f.objects = Vec::with_capacity(n_obj);
    ext_inactive.clear();
    let mut prev_gain_per_blk = vec![1.0f32; nblk];
    for obj in 0..n_obj {
        let in_bed = obj < in_bed_or_isf;
        let mut blocks = Vec::with_capacity(nblk);
        let mut inactive = Vec::with_capacity(nblk);
        for blk in 0..nblk {
            let mut st = state.prev[obj];
            let not_active = br.flag();
            inactive.push(not_active);
            st.active = !not_active;
            let basic_status = if not_active {
                0
            } else if blk == 0 {
                1
            } else {
                br.bits(2)
            };
            if basic_status == 0 {
                st.priority = 0.0;
                st.gain = 0.0;
            }
            if basic_status == 1 || basic_status == 3 {
                let mask = if basic_status == 1 { 3 } else { br.bits(2) };
                if mask & 2 != 0 {
                    let gi = br.bits(2);
                    st.gain = match gi {
                        0 => 1.0,
                        1 => 0.0,
                        2 => gain_from_bits(br.bits(6)),
                        _ => {
                            if obj == 0 {
                                1.0
                            } else {
                                prev_gain_per_blk[blk]
                            }
                        }
                    };
                }
                if mask & 1 != 0 {
                    st.priority = if br.flag() { 1.0 } else { br.bits(5) as f32 / 32.0 };
                }
            }
            prev_gain_per_blk[blk] = st.gain;
            let render_status = if not_active || in_bed {
                0
            } else if blk == 0 {
                1
            } else {
                br.bits(2)
            };
            if render_status == 0 && !in_bed {
                // Default render info.
                st.pos = [0.5, 0.5, 0.0];
                st.pos_bits = [31, 31, 0];
                st.size = [0.0; 3];
                st.zone_constraints_idx = 0;
                st.enable_elevation = true;
                st.screen_factor = 0.0;
                st.snap = false;
                st.distance = None;
                st.at_infinity = false;
            }
            if render_status == 1 || render_status == 3 {
                // Flags are read MSB first and indexed as in the syntax listing
                // (obj_render_info[0] = position ... [3] = screen reference);
                // table 31 of the specification lists them the other way round,
                // which does not parse the real streams.
                let mask = if render_status == 1 { 15 } else { br.bits(4) };
                if mask & 1 != 0 {
                    let diff = if blk == 0 { false } else { br.flag() };
                    if diff {
                        let dx = br.signed(3);
                        let dy = br.signed(3);
                        let dz = br.signed(3);
                        st.pos_bits[0] = (st.pos_bits[0] + dx).clamp(0, 62);
                        st.pos_bits[1] = (st.pos_bits[1] + dy).clamp(0, 62);
                        st.pos_bits[2] = (st.pos_bits[2] + dz).clamp(-15, 15);
                    } else {
                        st.pos_bits[0] = br.bits(6).min(62) as i32;
                        st.pos_bits[1] = br.bits(6).min(62) as i32;
                        let sign = if br.flag() { 1 } else { -1 };
                        st.pos_bits[2] = sign * br.bits(4) as i32;
                    }
                    st.pos = [
                        st.pos_bits[0] as f32 / 62.0,
                        st.pos_bits[1] as f32 / 62.0,
                        st.pos_bits[2] as f32 / 15.0,
                    ];
                    st.distance = None;
                    st.at_infinity = false;
                    if br.flag() {
                        if br.flag() {
                            st.at_infinity = true;
                        } else {
                            st.distance = Some(DISTANCE_FACTOR[br.bits(4) as usize]);
                        }
                    }
                }
                if mask & 2 != 0 {
                    st.zone_constraints_idx = br.bits(3) as u8;
                    st.enable_elevation = br.flag();
                }
                if mask & 4 != 0 {
                    let idx = br.bits(2);
                    st.size = match idx {
                        1 => {
                            let s = br.bits(5) as f32 / 31.0;
                            [s, s, s]
                        }
                        2 => {
                            let w = br.bits(5) as f32 / 31.0;
                            let d = br.bits(5) as f32 / 31.0;
                            let h = br.bits(5) as f32 / 31.0;
                            [w, d, h]
                        }
                        _ => [0.0; 3],
                    };
                }
                if mask & 8 != 0 {
                    if br.flag() {
                        st.screen_factor = (br.bits(3) + 1) as f32 / 8.0;
                        st.depth_factor = DEPTH_FACTOR[br.bits(2) as usize];
                    } else {
                        st.screen_factor = 0.0;
                    }
                }
                st.snap = br.flag();
            }
            if br.flag() {
                // additional_table_data
                let sz = br.bits(4) as usize + 1;
                br.skip(sz * 8);
            }
            state.prev[obj] = st;
            blocks.push(ObjectBlock { state: st, bed_label: if in_bed { bed_labels[obj] } else { None } });
        }
        ext_inactive.push(inactive);
        f.objects.push(blocks);
    }
}

fn parse_extended_object_element(
    br: &mut BitReader<'_>,
    f: &mut OamdFrame,
    state: &mut OamdState,
    inactive: &[Vec<bool>],
    in_bed_or_isf: usize,
) {
    let _ = state;
    let nblk = f.timing.len();
    if br.flag() {
        for obj in 0..f.objects.len() {
            for blk in 0..nblk {
                let not_active = inactive.get(obj).and_then(|v| v.get(blk)).copied().unwrap_or(true);
                if not_active || obj < in_bed_or_isf {
                    continue;
                }
                if br.flag() {
                    let mode = br.bits(2);
                    if mode == 0 {
                        br.skip(2);
                    }
                    if mode == 2 || mode == 3 {
                        br.skip(6);
                    }
                    f.objects[obj][blk].state.divergence_present = true;
                }
            }
        }
    }
    if br.flag() {
        for obj in 0..f.objects.len() {
            for blk in 0..nblk {
                let not_active = inactive.get(obj).and_then(|v| v.get(blk)).copied().unwrap_or(true);
                if not_active || obj < in_bed_or_isf {
                    continue;
                }
                if br.flag() {
                    let presence = br.bits(3);
                    let st = &mut f.objects[obj][blk].state;
                    let mut ext = [0i32; 3];
                    for (k, bit) in [4u32, 2, 1].iter().enumerate() {
                        if presence & bit != 0 {
                            ext[k] = EXT_PREC[br.bits(2) as usize];
                        }
                    }
                    // Extended precision refines the standard value.
                    st.pos[0] = (st.pos_bits[0] as f32 / 62.0 + ext[0] as f32 / (62.0 * 5.0)).clamp(0.0, 1.0);
                    st.pos[1] = (st.pos_bits[1] as f32 / 62.0 + ext[1] as f32 / (62.0 * 5.0)).clamp(0.0, 1.0);
                    st.pos[2] = (st.pos_bits[2] as f32 / 15.0 + ext[2] as f32 / (15.0 * 5.0)).clamp(-1.0, 1.0);
                }
            }
        }
    }
}
