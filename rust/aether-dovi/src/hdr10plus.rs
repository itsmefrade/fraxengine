//! HDR10+ (SMPTE ST 2094-40) dynamic metadata: extraction from HEVC SEI and AV1 metadata OBUs,
//! a field-exact parser, and the per-frame tone mapping parameters with a scalar reference curve.
//!
//! The T.35 payload (country 0xB5, provider 0x003C, oriented code 0x0001, application id 4,
//! version 0 or 1) is the same in both codecs. Every read is bounds checked and no input panics.
//!
//! # Tone curve (what the GPU side implements)
//! Per pixel, in linear light (nits): `m = max(r, g, b)`, `x = min(m / scene_max, 1)`, `scene_max`
//! being the largest MaxSCL or, when those are all zero, the last MaxRGB percentile (Annex B.3).
//! With the knee point `(kx, ky)` and Bezier anchors `a_1..a_n` (all normalised to 0..1):
//! `y = x * ky / kx` for `x < kx`, otherwise `y = ky + (1 - ky) * B(t)` with `t = (x - kx) / (1 - kx)`
//! and `B` the Bezier curve over `[0, a_1, .., a_n, 1]` (ST 2094-40 8.7.4), evaluated here by
//! de Casteljau over the rebased control values `[ky, ky + (1 - ky) a_1, .., 1]`. `kx = 1` is the
//! all-linear case `y = ky * x` (eq 5). The result `y * target` nits is the
//! picture as mastered for a display of `targeted_system_display_maximum_luminance` nits. For
//! another display peak `D`: below the target the output is scaled by `D / target`; above it the
//! output fades linearly to "no tone mapping" as `D` rises to `scene_max`. All channels are scaled
//! by `out / m`, which keeps the hue. Only window 0 (the full frame window) is used.

use crate::hevc::Framing;

/// Upper bounds that keep a hostile payload cheap (the spec maximum is 15 for both).
pub const MAX_ANCHORS: usize = 15;
pub const MAX_PERCENTILES: usize = 15;
pub const MAX_WINDOWS: usize = 3;
/// Length of the GPU parameter block in f32 values (nine vec4).
pub const GPU_PARAM_LEN: usize = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    NotHdr10Plus,
    UnsupportedVersion,
    BadWindowCount,
    BadCount,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WindowGeometry {
    pub upper_left_x: u16,
    pub upper_left_y: u16,
    pub lower_right_x: u16,
    pub lower_right_y: u16,
    pub center_x: u16,
    pub center_y: u16,
    pub rotation_angle: u8,
    pub semimajor_internal: u16,
    pub semimajor_external: u16,
    pub semiminor_external: u16,
    pub overlap_process_option: u8,
}

/// `rows * cols` 4 bit values (denominator 15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeakGrid {
    pub rows: u8,
    pub cols: u8,
    pub values: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Percentile {
    pub percentage: u8,
    /// Denominator 100000 (fraction of 10000 nits).
    pub value: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToneMapping {
    /// Denominator 4095.
    pub knee_x: u16,
    pub knee_y: u16,
    /// Denominator 1023.
    pub anchors: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowMeta {
    /// Denominator 100000 (fraction of 10000 nits), R, G, B.
    pub maxscl: [u32; 3],
    pub average_maxrgb: u32,
    pub percentiles: Vec<Percentile>,
    /// Denominator 1000.
    pub fraction_bright_pixels: u16,
    pub tone_mapping: Option<ToneMapping>,
    /// Denominator 8.
    pub color_saturation_weight: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hdr10Plus {
    pub application_version: u8,
    pub num_windows: u8,
    /// Geometry of windows 1.. (window 0 is the whole frame).
    pub windows: Vec<WindowGeometry>,
    /// Nits (denominator 1).
    pub targeted_system_display_maximum_luminance: u32,
    pub targeted_actual_peak: Option<PeakGrid>,
    pub mastering_actual_peak: Option<PeakGrid>,
    pub params: Vec<WindowMeta>,
    /// The T.35 payload this was parsed from (starts at the 0xB5 country code), so a consumer can
    /// keep it and rebuild [`ToneParams`] later.
    pub raw: Vec<u8>,
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn left(&self) -> usize {
        self.d.len() * 8 - self.pos
    }
    fn read(&mut self, n: usize) -> Result<u32, Error> {
        if n > 32 || self.left() < n {
            return Err(Error::Truncated);
        }
        let mut v = 0u64;
        for _ in 0..n {
            let bit = (self.d[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | u64::from(bit);
            self.pos += 1;
        }
        Ok(v as u32)
    }
    fn flag(&mut self) -> Result<bool, Error> {
        Ok(self.read(1)? != 0)
    }
}

fn read_grid(b: &mut Bits) -> Result<PeakGrid, Error> {
    let rows = b.read(5)? as u8;
    let cols = b.read(5)? as u8;
    if rows == 0 || cols == 0 || rows > 25 || cols > 25 {
        return Err(Error::BadCount);
    }
    let mut values = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    for _ in 0..usize::from(rows) * usize::from(cols) {
        values.push(b.read(4)? as u8);
    }
    Ok(PeakGrid { rows, cols, values })
}

/// Parses an ITU-T T.35 payload that starts at the country code byte (0xB5).
pub fn parse_t35(t35: &[u8]) -> Result<Hdr10Plus, Error> {
    if t35.len() < 7 {
        return Err(Error::Truncated);
    }
    if t35[0] != 0xB5 || t35[1] != 0x00 || t35[2] != 0x3C || t35[3] != 0x00 || t35[4] != 0x01 {
        return Err(Error::NotHdr10Plus);
    }
    if t35[5] != 4 {
        return Err(Error::NotHdr10Plus);
    }
    let application_version = t35[6];
    if application_version > 1 {
        return Err(Error::UnsupportedVersion);
    }
    let mut b = Bits { d: &t35[7..], pos: 0 };
    let num_windows = b.read(2)? as u8;
    if num_windows < 1 {
        return Err(Error::BadWindowCount);
    }
    if usize::from(num_windows) > MAX_WINDOWS {
        return Err(Error::BadWindowCount);
    }
    let mut windows = Vec::new();
    for _ in 1..num_windows {
        windows.push(WindowGeometry {
            upper_left_x: b.read(16)? as u16,
            upper_left_y: b.read(16)? as u16,
            lower_right_x: b.read(16)? as u16,
            lower_right_y: b.read(16)? as u16,
            center_x: b.read(16)? as u16,
            center_y: b.read(16)? as u16,
            rotation_angle: b.read(8)? as u8,
            semimajor_internal: b.read(16)? as u16,
            semimajor_external: b.read(16)? as u16,
            semiminor_external: b.read(16)? as u16,
            overlap_process_option: b.read(1)? as u8,
        });
    }
    let targeted_system_display_maximum_luminance = b.read(27)?;
    let targeted_actual_peak = if b.flag()? { Some(read_grid(&mut b)?) } else { None };
    let mut params = Vec::with_capacity(usize::from(num_windows));
    for _ in 0..num_windows {
        let maxscl = [b.read(17)?, b.read(17)?, b.read(17)?];
        let average_maxrgb = b.read(17)?;
        let n = b.read(4)? as usize;
        if n > MAX_PERCENTILES {
            return Err(Error::BadCount);
        }
        let mut percentiles = Vec::with_capacity(n);
        for _ in 0..n {
            percentiles.push(Percentile { percentage: b.read(7)? as u8, value: b.read(17)? });
        }
        let fraction_bright_pixels = b.read(10)? as u16;
        params.push(WindowMeta {
            maxscl,
            average_maxrgb,
            percentiles,
            fraction_bright_pixels,
            tone_mapping: None,
            color_saturation_weight: None,
        });
    }
    let mastering_actual_peak = if b.flag()? { Some(read_grid(&mut b)?) } else { None };
    for p in &mut params {
        if b.flag()? {
            let knee_x = b.read(12)? as u16;
            let knee_y = b.read(12)? as u16;
            let n = b.read(4)? as usize;
            if n > MAX_ANCHORS {
                return Err(Error::BadCount);
            }
            let mut anchors = Vec::with_capacity(n);
            for _ in 0..n {
                anchors.push(b.read(10)? as u16);
            }
            p.tone_mapping = Some(ToneMapping { knee_x, knee_y, anchors });
        }
        if b.flag()? {
            p.color_saturation_weight = Some(b.read(6)? as u8);
        }
    }
    Ok(Hdr10Plus {
        application_version,
        num_windows,
        windows,
        targeted_system_display_maximum_luminance,
        targeted_actual_peak,
        mastering_actual_peak,
        params,
        raw: t35.to_vec(),
    })
}

/// Removes emulation prevention bytes (`00 00 03` becomes `00 00`).
pub fn unescape_rbsp(nal_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_payload.len());
    let mut zeros = 0;
    for &b in nal_payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn sei_ff_value(d: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    loop {
        let b = *d.get(*pos)?;
        *pos += 1;
        v = v.checked_add(usize::from(b))?;
        if b != 0xFF {
            return Some(v);
        }
    }
}

/// Scans one SEI RBSP (unescaped, without the NAL header) for an HDR10+ T.35 message.
fn find_in_sei(rbsp: &[u8]) -> Option<Hdr10Plus> {
    let mut pos = 0;
    // The last byte is the RBSP trailing bits; a message needs at least type and size.
    while pos + 2 < rbsp.len() {
        let ptype = sei_ff_value(rbsp, &mut pos)?;
        let psize = sei_ff_value(rbsp, &mut pos)?;
        let end = pos.checked_add(psize)?;
        if end > rbsp.len() {
            return None;
        }
        if ptype == 4 {
            if let Ok(h) = parse_t35(&rbsp[pos..end]) {
                return Some(h);
            }
        }
        pos = end;
    }
    None
}

/// Walks the NAL units of an HEVC access unit; stops quietly at the first framing problem.
fn for_each_hevc_nal<'a>(data: &'a [u8], framing: Framing, mut f: impl FnMut(&'a [u8]) -> bool) {
    match framing {
        Framing::LengthPrefixed(size) => {
            let size = usize::from(size);
            if !(1..=4).contains(&size) {
                return;
            }
            let mut p = 0usize;
            while p + size <= data.len() {
                let mut len = 0usize;
                for &b in &data[p..p + size] {
                    len = (len << 8) | usize::from(b);
                }
                p += size;
                let Some(end) = p.checked_add(len) else { return };
                if len == 0 || end > data.len() {
                    return;
                }
                if f(&data[p..end]) {
                    return;
                }
                p = end;
            }
        }
        Framing::AnnexB => {
            let n = data.len();
            let mut i = 0usize;
            let mut start: Option<usize> = None;
            while i + 3 <= n {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    if let Some(s) = start {
                        let mut e = i;
                        while e > s && data[e - 1] == 0 {
                            e -= 1;
                        }
                        if e > s && f(&data[s..e]) {
                            return;
                        }
                    }
                    start = Some(i + 3);
                    i += 3;
                } else {
                    i += 1;
                }
            }
            if let Some(s) = start {
                let mut e = n;
                while e > s && data[e - 1] == 0 {
                    e -= 1;
                }
                if e > s {
                    f(&data[s..e]);
                }
            }
        }
    }
}

/// First HDR10+ message in an HEVC access unit (prefix or suffix SEI), or `None`.
pub fn extract_hevc(data: &[u8], framing: Framing) -> Option<Hdr10Plus> {
    let mut found = None;
    for_each_hevc_nal(data, framing, |nal| {
        if nal.len() < 3 || nal[0] & 0x80 != 0 {
            return false;
        }
        let t = (nal[0] >> 1) & 63;
        if t == 39 || t == 40 {
            if let Some(h) = find_in_sei(&unescape_rbsp(&nal[2..])) {
                found = Some(h);
                return true;
            }
        }
        false
    });
    found
}

fn leb128(d: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return usize::try_from(v).ok();
        }
    }
    None
}

/// First HDR10+ message in an AV1 temporal unit (low-overhead OBU stream), or `None`.
pub fn extract_av1(data: &[u8]) -> Option<Hdr10Plus> {
    let mut pos = 0usize;
    while pos < data.len() {
        let h = data[pos];
        pos += 1;
        if h & 0x80 != 0 {
            return None;
        }
        let obu_type = (h >> 3) & 15;
        if h & 4 != 0 {
            pos = pos.checked_add(1)?;
        }
        let size = if h & 2 != 0 { leb128(data, &mut pos)? } else { data.len().checked_sub(pos)? };
        let end = pos.checked_add(size)?;
        if end > data.len() {
            return None;
        }
        if obu_type == 5 {
            let mut p = pos;
            if leb128(data, &mut p) == Some(4) && p <= end {
                if let Ok(x) = parse_t35(&data[p..end]) {
                    return Some(x);
                }
            }
        }
        pos = end;
    }
    None
}

impl Hdr10Plus {
    /// The full-frame window, the one the tone curve uses.
    pub fn window0(&self) -> &WindowMeta {
        &self.params[0]
    }
}

/// Per-frame tone mapping parameters for one display peak. See the module docs for the curve.
#[derive(Clone, Debug, PartialEq)]
pub struct ToneParams {
    pub has_curve: bool,
    pub scene_max_nits: f32,
    pub scene_avg_nits: f32,
    pub target_nits: f32,
    pub display_peak_nits: f32,
    pub knee_x: f32,
    pub knee_y: f32,
    pub num_anchors: u32,
    /// `[knee_y, anchors.., 1.0]`, padded with zeros to 17 entries.
    pub control: [f32; 17],
    /// Output scale for displays below the target.
    pub out_scale: f32,
    /// 0 = curve, 1 = no tone mapping (display at or above the scene peak).
    pub fade: f32,
    pub maxscl_nits: [f32; 3],
}


impl ToneParams {
    pub fn new(h: &Hdr10Plus, display_peak_nits: f32) -> Self {
        let w = h.window0();
        // Annex B.3: all zero MaxSCL falls back to the largest MaxRGB percentile value.
        let maxscl = w.maxscl.iter().copied().max().unwrap_or(0);
        let scene_max = if maxscl > 0 { maxscl } else { w.percentiles.last().map_or(0, |p| p.value) } as f32 / 10.0;
        let scene_avg = w.average_maxrgb as f32 / 10.0;
        let target = h.targeted_system_display_maximum_luminance as f32;
        let d = if display_peak_nits.is_finite() && display_peak_nits > 0.0 { display_peak_nits } else { target };
        let mut control = [0.0f32; 17];
        let (mut kx, mut ky, mut n) = (0.0, 0.0, 0u32);
        let mut has_curve = false;
        if let Some(tm) = &w.tone_mapping {
            kx = f32::from(tm.knee_x) / 4095.0;
            ky = f32::from(tm.knee_y) / 4095.0;
            n = tm.anchors.len().min(MAX_ANCHORS) as u32;
            control[0] = ky;
            for (i, a) in tm.anchors.iter().take(MAX_ANCHORS).enumerate() {
                control[i + 1] = ky + (1.0 - ky) * (f32::from(*a) / 1023.0);
            }
            control[n as usize + 1] = 1.0;
            has_curve = target > 0.0 && scene_max > 0.0;
        }
        let out_scale = if d < target && target > 0.0 { d / target } else { 1.0 };
        let fade = if !has_curve || scene_max <= d {
            1.0
        } else if d > target {
            ((d - target) / (scene_max - target)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ToneParams {
            has_curve,
            scene_max_nits: scene_max,
            scene_avg_nits: scene_avg,
            target_nits: target,
            display_peak_nits: d,
            knee_x: kx,
            knee_y: ky,
            num_anchors: n,
            control,
            out_scale,
            fade,
            maxscl_nits: [w.maxscl[0] as f32 / 10.0, w.maxscl[1] as f32 / 10.0, w.maxscl[2] as f32 / 10.0],
        }
    }

    /// Layout of the f32 block handed to the GPU (`GPU_PARAM_LEN` values, nine vec4):
    ///
    /// | index | meaning |
    /// | --- | --- |
    /// | 0 | 1.0 when the block is valid |
    /// | 1 | has_curve (1.0 when a knee and Bezier curve is present) |
    /// | 2 | scene_max_nits (largest maxscl) |
    /// | 3 | scene_avg_nits (average_maxrgb) |
    /// | 4 | target_nits (targeted system display maximum luminance) |
    /// | 5, 6 | knee_x, knee_y (0..1) |
    /// | 7 | number of anchors n (0..15) |
    /// | 8 | display_peak_nits |
    /// | 9 | fade (0 curve, 1 pass-through) |
    /// | 10 | out_scale |
    /// | 11..13 | maxscl R, G, B in nits |
    /// | 14, 15 | reserved (0) |
    /// | 16..32 | control values `[knee_y, a_1..a_n, 1.0]`, zero padded |
    /// | 33..35 | reserved (0) |
    pub fn to_gpu(&self) -> [f32; GPU_PARAM_LEN] {
        let mut o = [0.0f32; GPU_PARAM_LEN];
        o[0] = 1.0;
        o[1] = if self.has_curve { 1.0 } else { 0.0 };
        o[2] = self.scene_max_nits;
        o[3] = self.scene_avg_nits;
        o[4] = self.target_nits;
        o[5] = self.knee_x;
        o[6] = self.knee_y;
        o[7] = self.num_anchors as f32;
        o[8] = self.display_peak_nits;
        o[9] = self.fade;
        o[10] = self.out_scale;
        o[11..14].copy_from_slice(&self.maxscl_nits);
        o[16..33].copy_from_slice(&self.control);
        o
    }

    /// Curve value for normalised `x` in 0..1 (output 0..1 of the target peak).
    pub fn curve(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        if x < self.knee_x || self.knee_x >= 1.0 {
            return x * self.knee_y / self.knee_x;
        }
        let t = ((x - self.knee_x) / (1.0 - self.knee_x)).clamp(0.0, 1.0);
        let m = self.num_anchors as usize + 2;
        let mut c = self.control;
        for level in 1..m {
            for i in 0..m - level {
                c[i] += (c[i + 1] - c[i]) * t;
            }
        }
        c[0].clamp(0.0, 1.0)
    }
}

const M1: f32 = 0.1593017578125;
const M2: f32 = 78.84375;
const C1: f32 = 0.8359375;
const C2: f32 = 18.8515625;
const C3: f32 = 18.6875;

pub fn pq_to_nits(v: f32) -> f32 {
    let p = v.clamp(0.0, 1.0).powf(1.0 / M2);
    10000.0 * ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

pub fn nits_to_pq(n: f32) -> f32 {
    let y = (n / 10000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// Applies the HDR10+ curve to one PQ encoded BT.2020 pixel for a display of `display_peak_nits`.
/// `params` must have been built for that same display peak (see [`ToneParams::new`]).
pub fn apply(pq_rgb: [f32; 3], params: &ToneParams) -> [f32; 3] {
    if !params.has_curve {
        return pq_rgb;
    }
    let lin = pq_rgb.map(pq_to_nits);
    let m = lin[0].max(lin[1]).max(lin[2]);
    if !(m > 0.0) {
        return pq_rgb;
    }
    let x = (m / params.scene_max_nits).min(1.0);
    let mapped = params.curve(x) * params.target_nits * params.out_scale;
    let pass = m.min(params.display_peak_nits);
    let out = mapped + (pass - mapped) * params.fade;
    let s = out / m;
    lin.map(|v| nits_to_pq(v * s))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct W {
        bytes: Vec<u8>,
        n: usize,
    }
    impl W {
        fn new() -> Self {
            W { bytes: Vec::new(), n: 0 }
        }
        fn put(&mut self, v: u32, bits: usize) {
            for i in (0..bits).rev() {
                if self.n % 8 == 0 {
                    self.bytes.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.n % 8);
                self.n += 1;
            }
        }
    }

    /// Payload from a real HDR10Plus_PB_EAC3JOC.mkv frame (values from the ffprobe dump).
    fn sample(windows: u32, tone: bool) -> Vec<u8> {
        let mut w = W::new();
        for b in [0xB5u32, 0x00, 0x3C, 0x00, 0x01, 4, 1] {
            w.put(b, 8);
        }
        w.put(windows, 2);
        for k in 1..windows {
            for v in [10 * k, 20, 300, 400, 150, 200] {
                w.put(v, 16);
            }
            w.put(7, 8);
            for v in [11, 12, 13] {
                w.put(v, 16);
            }
            w.put(1, 1);
        }
        w.put(400, 27);
        w.put(0, 1);
        for _ in 0..windows {
            for v in [10568, 10410, 11038, 24] {
                w.put(v, 17);
            }
            w.put(3, 4);
            for (a, b) in [(1u32, 0u32), (50, 5), (99, 8412)] {
                w.put(a, 7);
                w.put(b, 17);
            }
            w.put(0, 10);
        }
        w.put(0, 1);
        for _ in 0..windows {
            w.put(tone as u32, 1);
            if tone {
                w.put(69, 12);
                w.put(147, 12);
                w.put(9, 4);
                for a in [180u32, 362, 546, 696, 743, 787, 842, 898, 936] {
                    w.put(a, 10);
                }
            }
            w.put(1, 1);
            w.put(8, 6);
        }
        w.bytes
    }

    #[test]
    fn parses_all_fields() {
        let h = parse_t35(&sample(1, true)).unwrap();
        assert_eq!(h.application_version, 1);
        assert_eq!(h.num_windows, 1);
        assert_eq!(h.targeted_system_display_maximum_luminance, 400);
        let w = h.window0();
        assert_eq!(w.maxscl, [10568, 10410, 11038]);
        assert_eq!(w.average_maxrgb, 24);
        assert_eq!(w.percentiles[1], Percentile { percentage: 50, value: 5 });
        let tm = w.tone_mapping.as_ref().unwrap();
        assert_eq!((tm.knee_x, tm.knee_y, tm.anchors.len()), (69, 147, 9));
        assert_eq!(tm.anchors[8], 936);
        assert_eq!(w.color_saturation_weight, Some(8));
    }

    #[test]
    fn parses_multiple_windows() {
        let h = parse_t35(&sample(3, true)).unwrap();
        assert_eq!(h.num_windows, 3);
        assert_eq!(h.windows.len(), 2);
        assert_eq!(h.windows[1].upper_left_x, 20);
        assert_eq!(h.windows[1].overlap_process_option, 1);
        assert_eq!(h.params.len(), 3);
        assert!(h.params[2].tone_mapping.is_some());
    }

    #[test]
    fn rejects_foreign_and_truncated() {
        let mut s = sample(1, true);
        assert!(parse_t35(&s[..s.len() - 6]).is_err());
        s[2] = 0x3B;
        assert_eq!(parse_t35(&s), Err(Error::NotHdr10Plus));
        let mut s = sample(1, true);
        s[6] = 2;
        assert_eq!(parse_t35(&s), Err(Error::UnsupportedVersion));
        let mut s = sample(1, true);
        s[7] &= 0x3F;
        assert_eq!(parse_t35(&s), Err(Error::BadWindowCount));
    }

    fn escape(b: &[u8]) -> Vec<u8> {
        let mut o = Vec::new();
        let mut z = 0;
        for &x in b {
            if z >= 2 && x <= 3 {
                o.push(3);
                z = 0;
            }
            o.push(x);
            z = if x == 0 { z + 1 } else { 0 };
        }
        o
    }

    fn sei_nal(t35: &[u8], suffix: bool) -> Vec<u8> {
        let mut r = vec![4u8];
        let mut n = t35.len();
        while n >= 255 {
            r.push(255);
            n -= 255;
        }
        r.push(n as u8);
        r.extend_from_slice(t35);
        r.push(0x80);
        let mut nal = vec![if suffix { 40 << 1 } else { 39 << 1 }, 1];
        nal.extend(escape(&r));
        nal
    }

    #[test]
    fn extracts_from_hevc_both_framings() {
        let t35 = sample(1, true);
        let want = parse_t35(&t35).unwrap();
        let other = [0x26u8, 0x01, 0xAF, 0x00]; // IDR slice stub
        for suffix in [false, true] {
            let sei = sei_nal(&t35, suffix);
            let mut lp = Vec::new();
            for n in [&other[..], &sei[..]] {
                lp.extend((n.len() as u32).to_be_bytes());
                lp.extend_from_slice(n);
            }
            assert_eq!(extract_hevc(&lp, Framing::LengthPrefixed(4)), Some(want.clone()));
            let mut ab = Vec::new();
            for n in [&other[..], &sei[..]] {
                ab.extend([0, 0, 0, 1]);
                ab.extend_from_slice(n);
            }
            assert_eq!(extract_hevc(&ab, Framing::AnnexB), Some(want.clone()));
        }
    }

    #[test]
    fn extracts_from_av1() {
        let t35 = sample(1, true);
        let want = parse_t35(&t35).unwrap();
        let mut payload = vec![4u8];
        payload.extend_from_slice(&t35);
        let mut tu = vec![0x12, 0x00]; // temporal delimiter with size field
        tu.push(5 << 3 | 2);
        tu.push(payload.len() as u8 | 0x80);
        tu.push((payload.len() >> 7) as u8);
        tu.extend_from_slice(&payload);
        assert_eq!(extract_av1(&tu), Some(want));
    }

    #[test]
    fn mutation_never_panics() {
        let t35 = sample(2, true);
        let sei = sei_nal(&t35, false);
        let mut obu = vec![5u8 << 3 | 2, (t35.len() + 1) as u8, 4];
        obu.extend_from_slice(&t35);
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for round in 0..6000 {
            let mut a = t35.clone();
            let mut s = sei.clone();
            let mut o = obu.clone();
            for _ in 0..1 + round % 4 {
                for v in [&mut a, &mut s, &mut o] {
                    let i = rnd() as usize % v.len();
                    v[i] = rnd() as u8;
                }
            }
            if round % 5 == 0 {
                let n = rnd() as usize % a.len();
                a.truncate(n);
                let n = rnd() as usize % s.len();
                s.truncate(n);
                let n = rnd() as usize % o.len();
                o.truncate(n);
            }
            let _ = parse_t35(&a);
            let mut lp = (s.len() as u32).to_be_bytes().to_vec();
            lp.extend_from_slice(&s);
            for n in [1u8, 2, 3, 4] {
                let _ = extract_hevc(&lp, Framing::LengthPrefixed(n));
            }
            let _ = extract_hevc(&s, Framing::AnnexB);
            let _ = extract_hevc(&a, Framing::AnnexB);
            let _ = extract_av1(&o);
            let _ = extract_av1(&a);
        }
    }

    fn params(peak: f32) -> ToneParams {
        ToneParams::new(&parse_t35(&sample(1, true)).unwrap(), peak)
    }

    #[test]
    fn curve_shape() {
        let p = params(400.0);
        assert!(p.has_curve);
        assert_eq!(p.curve(0.0), 0.0);
        assert!((p.curve(1.0) - 1.0).abs() < 1e-6);
        // linear below the knee
        assert!((p.curve(p.knee_x * 0.5) - p.knee_y * 0.5).abs() < 1e-6);
        // continuous at the knee and monotonic
        assert!((p.curve(p.knee_x) - p.knee_y).abs() < 1e-4);
        let mut prev = 0.0;
        for i in 0..=1000 {
            let y = p.curve(i as f32 / 1000.0);
            assert!(y + 1e-6 >= prev);
            prev = y;
        }
        // de Casteljau against the explicit Bernstein form at t = 0.5
        let t = 0.5f32;
        let x = p.knee_x + (1.0 - p.knee_x) * t;
        let n = p.num_anchors as usize + 1;
        let mut e = 0.0f64;
        let mut binom = 1.0f64;
        for i in 0..=n {
            if i > 0 {
                binom = binom * (n - i + 1) as f64 / i as f64;
            }
            e += binom * 0.5f64.powi(n as i32) * f64::from(p.control[i]);
        }
        assert!((f64::from(p.curve(x)) - e).abs() < 1e-5);
    }

    #[test]
    fn apply_behaviour() {
        let p = params(400.0);
        let white = nits_to_pq(p.scene_max_nits);
        let out = apply([white; 3], &p);
        let nits = pq_to_nits(out[0]);
        assert!((nits - 400.0).abs() < 2.0, "scene peak maps to target, got {nits}");
        // hue is preserved: ratios between channels stay equal in linear light
        let px = [nits_to_pq(500.0), nits_to_pq(250.0), nits_to_pq(100.0)];
        let o = apply(px, &p).map(pq_to_nits);
        assert!((o[1] / o[0] - 0.5).abs() < 0.01 && (o[2] / o[0] - 0.2).abs() < 0.01);
        // darker display scales down
        let dim = params(200.0);
        assert!((pq_to_nits(apply([white; 3], &dim)[0]) - 200.0).abs() < 2.0);
        // display at the scene peak does nothing
        let full = params(1200.0);
        assert_eq!(full.fade, 1.0);
        let px = [nits_to_pq(700.0); 3];
        assert!((pq_to_nits(apply(px, &full)[0]) - 700.0).abs() < 1.0);
        // black is untouched
        assert_eq!(apply([0.0; 3], &p), [0.0; 3]);
        // no curve: untouched
        let h = parse_t35(&sample(1, false)).unwrap();
        let np = ToneParams::new(&h, 400.0);
        assert!(!np.has_curve);
        assert_eq!(apply([0.5; 3], &np), [0.5; 3]);
    }

    #[test]
    fn anchors_are_rebased_onto_the_knee() {
        // Value from an independent evaluation of ST 2094-40 8.7.4 on this very metadata (first scene of hdr10plus.mkv).
        let out = pq_to_nits(apply([nits_to_pq(74.61); 3], &params(400.0))[0]);
        assert!((out - 49.45).abs() < 0.05, "got {out}");
    }

    #[test]
    fn all_zero_maxscl_falls_back_to_the_last_percentile() {
        let mut h = parse_t35(&sample(1, true)).unwrap();
        h.params[0].maxscl = [0; 3];
        let last = h.params[0].percentiles.last().unwrap().value as f32 / 10.0;
        let p = ToneParams::new(&h, 400.0);
        assert!(p.has_curve);
        assert!((p.scene_max_nits - last).abs() < 1e-3);
    }

    #[test]
    fn knee_at_one_is_the_linear_case() {
        let mut h = parse_t35(&sample(1, true)).unwrap();
        h.params[0].tone_mapping = Some(ToneMapping { knee_x: 4095, knee_y: 2048, anchors: vec![] });
        let p = ToneParams::new(&h, 400.0);
        assert!(p.has_curve);
        for x in [0.0f32, 0.5, 1.0] {
            assert!((p.curve(x) - x * 2048.0 / 4095.0).abs() < 1e-5, "x={x} y={}", p.curve(x));
        }
    }

    #[test]
    fn zero_knee_with_linear_anchors_is_identity() {
        let mut h = parse_t35(&sample(1, true)).unwrap();
        h.params[0].tone_mapping = Some(ToneMapping {
            knee_x: 0,
            knee_y: 0,
            anchors: (1..=9).map(|i| (i * 1023 / 10) as u16).collect(),
        });
        let p = ToneParams::new(&h, 400.0);
        assert!(p.has_curve);
        for i in 0..=10 {
            let x = i as f32 / 10.0;
            assert!((p.curve(x) - x).abs() < 2e-3, "x={x} y={}", p.curve(x));
        }
    }

    #[test]
    fn gpu_block_layout() {
        let g = params(400.0).to_gpu();
        assert_eq!(g.len(), GPU_PARAM_LEN);
        assert_eq!((g[0], g[1], g[4], g[7], g[8]), (1.0, 1.0, 400.0, 9.0, 400.0));
        assert!((g[2] - 1103.8).abs() < 0.01);
        assert_eq!(g[16], g[6]);
        assert_eq!(g[16 + 10], 1.0);
        assert_eq!(g[16 + 11], 0.0);
    }

    #[test]
    fn pq_roundtrip() {
        for n in [0.01f32, 1.0, 100.0, 1000.0, 10000.0] {
            assert!((pq_to_nits(nits_to_pq(n)) - n).abs() / n < 1e-3);
        }
    }
}
