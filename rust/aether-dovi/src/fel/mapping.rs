use crate::fel::{Error, Result};
use dolby_vision::rpu::{
    dovi_rpu::DoviRpu,
    rpu_data_mapping::{DoviMappingMethod, DoviNlqMethod},
    rpu_data_nlq::DoviELType,
};

/// Fixed-point mapping pieces. Coefficients are signed Q(denominator), not floats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Polynomial {
        order: u8,
        coefficients: [i32; 3],
    },
    Mmr {
        order: u8,
        constant: i32,
        coefficients: [[i32; 7]; 3],
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Curve {
    pivots: Vec<u16>,
    pieces: Vec<Piece>,
}
impl Curve {
    pub fn new(pivots: Vec<u16>, pieces: Vec<Piece>) -> Result<Self> {
        if !(2..=9).contains(&pivots.len())
            || pieces.len() + 1 != pivots.len()
            || pivots.iter().any(|&v| v > 1023)
            || pivots.windows(2).any(|p| p[0] > p[1])
            || pieces.iter().any(|p| match p {
                Piece::Polynomial { order, .. } => !(1..=2).contains(order),
                Piece::Mmr { order, .. } => !(1..=3).contains(order),
            })
        {
            return Err(Error::Parameters);
        }
        Ok(Self { pivots, pieces })
    }
    pub fn pivots(&self) -> &[u16] {
        &self.pivots
    }
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }
    pub(crate) fn select(&self, code: u16) -> usize {
        self.pivots[..self.pieces.len()]
            .iter()
            .rposition(|&p| code >= p)
            .unwrap_or(0)
    }
    fn clamped(&self, code: u16) -> f64 {
        f64::from(code.clamp(self.pivots[0], self.pivots[self.pivots.len() - 1])) / 1024.0
    }
}
/// Linear dead-zone residual parameters. Q values are nonnegative and bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nlq {
    offset: u16,
    slope: i32,
    threshold: i32,
    max_q: i64,
    denom: u8,
}
impl Nlq {
    /// Nonnegative fixed-point coefficients. `max_q` retains fractional precision.
    pub fn new(
        offset: u16,
        slope: i32,
        threshold: i32,
        max_q: i64,
        denominator: u8,
    ) -> Result<Self> {
        if offset > 1023
            || slope < 0
            || threshold < 0
            || !(0..=i64::MAX / 2).contains(&max_q)
            || denominator > 23
        {
            return Err(Error::Parameters);
        }
        Ok(Self {
            offset,
            slope,
            threshold,
            max_q,
            denom: denominator,
        })
    }
    pub fn residual_upstream(self, code: u16, depth: u8) -> Result<i32> {
        if code > 1023 {
            return Err(Error::SampleRange);
        }
        if depth > self.denom {
            return Err(Error::Parameters);
        }
        let max = self.max_q >> (self.denom - depth);
        if self.slope == 0 || max == 0 {
            return Ok(0);
        }
        let r = i64::from(code) - i64::from(self.offset);
        if r == 0 {
            return Ok(0);
        }
        let n = (2 * r.abs() - 1) * i64::from(self.slope) + 2 * i64::from(self.threshold);
        let mag = (n >> (self.denom - depth + 1)).min(max);
        i32::try_from(if r < 0 { -mag } else { mag }).map_err(|_| Error::Overflow)
    }
    /// DoViBaker/ETSI-style signed 16-bit-domain residual, preserving the
    /// arithmetic shift for negative fractional values. No C++ narrowing wrap.
    pub fn residual_reference16(self, code: u16) -> Result<i32> {
        if code > 1023 {
            return Err(Error::SampleRange);
        }
        if self.denom < 15 {
            return Err(Error::Unsupported);
        }
        let r = i64::from(code) - i64::from(self.offset);
        if r == 0 {
            return Ok(0);
        }
        let sign = r.signum();
        let q = (2 * r - sign) * i64::from(self.slope) + sign * 2 * i64::from(self.threshold);
        let bounded = q.clamp(-2 * self.max_q, 2 * self.max_q);
        i32::try_from(bounded >> (self.denom - 15)).map_err(|_| Error::Overflow)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FelFrameParams {
    curves: [Curve; 3],
    nlq: [Nlq; 3],
    denom: u8,
    depth: u8,
    disabled: bool,
    residual_known: bool,
    /// MEL carries no residual by definition; the flag survives mapping reuse.
    mel: bool,
}
impl FelFrameParams {
    pub fn new(
        curves: [Curve; 3],
        nlq: [Nlq; 3],
        denominator: u8,
        vdr_depth: u8,
        disable_residual: bool,
    ) -> Result<Self> {
        if !(10..=16).contains(&vdr_depth)
            || denominator > 23
            || denominator < vdr_depth
            || nlq.iter().any(|v| v.denom != denominator)
        {
            return Err(Error::Parameters);
        }
        Ok(Self {
            curves,
            nlq,
            denom: denominator,
            depth: vdr_depth,
            disabled: disable_residual,
            residual_known: true,
            mel: false,
        })
    }
    /// Explicit ablation control: retain mapping but omit all enhancement residuals.
    pub fn mapping_only(&self) -> Self {
        let mut result = self.clone();
        result.disabled = true;
        result
    }
    pub fn coefficient_denominator(&self) -> u8 {
        self.denom
    }
    pub fn vdr_depth(&self) -> u8 {
        self.depth
    }
    pub fn disable_residual(&self) -> bool {
        self.disabled
    }
    pub fn curves(&self) -> &[Curve; 3] {
        &self.curves
    }
    pub fn nlq_tables(&self, mode: crate::fel::ReconstructionMode) -> Result<[[i32; 1024]; 3]> {
        if mode == crate::fel::ReconstructionMode::Reference16 && self.denom < 15 {
            return Err(Error::Unsupported);
        }
        let mut tables = [[0; 1024]; 3];
        if !self.disabled {
            for (i, table) in tables.iter_mut().enumerate() {
                for (code, out) in table.iter_mut().enumerate() {
                    *out = match mode {
                        crate::fel::ReconstructionMode::Reference16 => {
                            self.nlq[i].residual_reference16(code as u16)?
                        }
                        crate::fel::ReconstructionMode::UpstreamVdrF64 => {
                            self.nlq[i].residual_upstream(code as u16, self.depth)?
                        }
                    };
                }
            }
        }
        Ok(tables)
    }
    /// Scalar f64 reshaping. This is not a claim of Float32 Metal/GPU bit equivalence.
    pub fn prediction(
        &self,
        component: usize,
        code: u16,
        input: [u16; 3],
        mode: crate::fel::ReconstructionMode,
    ) -> Result<u16> {
        if mode == crate::fel::ReconstructionMode::Reference16 && self.denom < 15 {
            return Err(Error::Unsupported);
        }
        if component >= 3 || code > 1023 || input.iter().any(|&v| v > 1023) {
            return Err(Error::SampleRange);
        }
        Ok(match mode {
            crate::fel::ReconstructionMode::Reference16 => {
                self.predict_reference16(component, code, input)
            }
            crate::fel::ReconstructionMode::UpstreamVdrF64 => self.predict(component, code, input),
        } as u16)
    }
    pub(crate) fn predict_reference16(&self, component: usize, code: u16, input: [u16; 3]) -> i32 {
        let curve = &self.curves[component];
        let q20 = match &curve.pieces[curve.select(code)] {
            Piece::Polynomial {
                order,
                coefficients: c,
            } => {
                let x =
                    i128::from(code.clamp(curve.pivots[0], curve.pivots[curve.pivots.len() - 1]));
                let mut sum = i128::from(c[0]) * (1 << 20) + i128::from(c[1]) * x * (1 << 10);
                if *order == 2 {
                    sum += i128::from(c[2]) * x * x;
                }
                sum
            }
            Piece::Mmr {
                order,
                constant,
                coefficients,
            } => {
                let [y, u, v]: [i128; 3] = std::array::from_fn(|i| {
                    i128::from(input[i].clamp(
                        self.curves[i].pivots[0],
                        self.curves[i].pivots[self.curves[i].pivots.len() - 1],
                    ))
                });
                let first = [
                    y << 10,
                    u << 10,
                    v << 10,
                    y * u,
                    y * v,
                    u * v,
                    (y * u * (v << 10)) >> 20,
                ];
                let second = [
                    y * y,
                    u * u,
                    v * v,
                    (first[3] * first[3]) >> 20,
                    (first[4] * first[4]) >> 20,
                    (first[5] * first[5]) >> 20,
                    (first[6] * first[6]) >> 20,
                ];
                let third = std::array::from_fn::<_, 7, _>(|i| (first[i] * second[i]) >> 20);
                let mut sum = i128::from(*constant) * (1 << 20);
                for (row, terms) in coefficients
                    .iter()
                    .zip([first, second, third])
                    .take(usize::from(*order))
                {
                    for i in 0..7 {
                        sum += i128::from(row[i]) * terms[i];
                    }
                }
                sum
            }
        };
        (q20.max(0) >> (self.denom + 4)).min(65535) as i32
    }
    pub(crate) fn predict(&self, component: usize, code: u16, input: [u16; 3]) -> i32 {
        let curve = &self.curves[component];
        let piece = &curve.pieces[curve.select(code)];
        let inv = 2.0_f64.powi(-i32::from(self.denom));
        let value = match piece {
            Piece::Polynomial {
                order,
                coefficients: c,
            } => {
                let x = curve.clamped(code);
                let s = (f64::from(c[1]) * inv).mul_add(x, f64::from(c[0]) * inv);
                if *order == 2 {
                    (f64::from(c[2]) * inv).mul_add(x * x, s)
                } else {
                    s
                }
            }
            Piece::Mmr {
                order,
                constant,
                coefficients,
            } => {
                let [y, u, v] = std::array::from_fn(|i| self.curves[i].clamped(input[i]));
                let base = [y, u, v, y * u, y * v, u * v, y * u * v];
                let mut terms = base;
                let mut s = f64::from(*constant) * inv;
                for row in coefficients.iter().take(usize::from(*order)) {
                    for i in 0..7 {
                        s = (f64::from(row[i]) * inv).mul_add(terms[i], s);
                        terms[i] *= base[i];
                    }
                }
                s
            }
        };
        (value * f64::from(1_u32 << self.depth))
            .clamp(0.0, f64::from((1_u32 << self.depth) - 1))
            .round_ties_even() as i32
    }
}

fn fixed(integer: i64, fraction: u64, denom: u8) -> Result<i32> {
    if fraction >= 1_u64 << denom {
        return Err(Error::Parameters);
    }
    let value = i128::from(integer) * (1_i128 << denom) + i128::from(fraction);
    i32::try_from(value).map_err(|_| Error::Overflow)
}
fn unsigned(integer: u64, fraction: u64, denom: u8) -> Result<i64> {
    if fraction >= 1_u64 << denom {
        return Err(Error::Parameters);
    }
    i64::try_from(u128::from(integer) * (1_u128 << denom) + u128::from(fraction))
        .map_err(|_| Error::Overflow)
}
fn from_rpu(rpu: &DoviRpu) -> Result<FelFrameParams> {
    let h = &rpu.header;
    // libdovi infers profile8 when a profile7-style header disables residual.
    // Profile 7 (FEL, or MEL whose NLQ is all zero) keeps the explicit syntax gate.
    // Profile 5 (single layer IPTPQc2, vdr_rpu_profile 0) and profile 8.x (no EL,
    // vdr_rpu_profile 0 or 1, EL resampling flag unconstrained) are mapping only
    // and must disable the residual; their polynomial / MMR reshaping applies to the BL.
    let mel = rpu.dovi_profile == 7 && rpu.el_type == Some(DoviELType::MEL);
    let mapping_only_profile = h.disable_residual_flag
        && h.vdr_bit_depth_minus8 == 4
        && h.rpu_format & 0x700 == 0
        && ((rpu.dovi_profile == 5 && h.vdr_rpu_profile == 0)
            || (rpu.dovi_profile == 8 && h.vdr_rpu_profile <= 1));
    let compatible_profile = rpu.dovi_profile == 7 || mapping_only_profile;
    if !compatible_profile
        || h.bl_bit_depth_minus8 != 2
        || h.el_bit_depth_minus8 != 2
        || h.coefficient_data_type != 0
        || h.coefficient_log2_denom > 23
        || h.vdr_bit_depth_minus8 > 8
        || h.ext_mapping_idc_0_4 != 0
        || h.ext_mapping_idc_5_7 != 0
        || h.chroma_resampling_explicit_filter_flag
    {
        return Err(Error::Unsupported);
    }
    let denom = h.coefficient_log2_denom as u8;
    let depth = h.vdr_bit_depth_minus8 as u8 + 8;
    if depth < 10 || denom < depth {
        return Err(Error::Unsupported);
    }
    let mapping = rpu.rpu_data_mapping.as_ref().ok_or(Error::Parameters)?;
    if mapping.vdr_rpu_id != 0
        || mapping.mapping_color_space != 0
        || mapping.mapping_chroma_format_idc != 0
        || mapping.num_x_partitions_minus1 != 0
        || mapping.num_y_partitions_minus1 != 0
    {
        return Err(Error::Unsupported);
    }
    let mut curves = Vec::with_capacity(3);
    for curve in &mapping.curves {
        let count = usize::try_from(curve.num_pivots_minus2)
            .ok()
            .and_then(|v| v.checked_add(2))
            .ok_or(Error::Parameters)?;
        if !(2..=9).contains(&count) || curve.pivots.len() != count {
            return Err(Error::Parameters);
        }
        let mut sum = 0_u16;
        let pivots = curve
            .pivots
            .iter()
            .map(|&v| {
                sum = sum.checked_add(v).ok_or(Error::Overflow)?;
                Ok(sum)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut pieces = Vec::with_capacity(count - 1);
        for p in 0..count - 1 {
            let piece = match curve.mapping_idc {
                DoviMappingMethod::Polynomial => {
                    let poly = curve.polynomial.as_ref().ok_or(Error::Parameters)?;
                    let order = *poly.poly_order_minus1.get(p).ok_or(Error::Parameters)?;
                    if order > 1 || *poly.linear_interp_flag.get(p).unwrap_or(&false) {
                        return Err(Error::Unsupported);
                    }
                    let ints = poly.poly_coef_int.get(p).ok_or(Error::Parameters)?;
                    let fracs = poly.poly_coef.get(p).ok_or(Error::Parameters)?;
                    let mut c = [0; 3];
                    for (k, coefficient) in c.iter_mut().enumerate().take(order as usize + 2) {
                        *coefficient = fixed(
                            *ints.get(k).ok_or(Error::Parameters)?,
                            *fracs.get(k).ok_or(Error::Parameters)?,
                            denom,
                        )?;
                    }
                    Piece::Polynomial {
                        order: order as u8 + 1,
                        coefficients: c,
                    }
                }
                DoviMappingMethod::MMR => {
                    let mmr = curve.mmr.as_ref().ok_or(Error::Parameters)?;
                    let order = *mmr.mmr_order_minus1.get(p).ok_or(Error::Parameters)?;
                    if order > 2 {
                        return Err(Error::Unsupported);
                    }
                    let constant = fixed(
                        *mmr.mmr_constant_int.get(p).ok_or(Error::Parameters)?,
                        *mmr.mmr_constant.get(p).ok_or(Error::Parameters)?,
                        denom,
                    )?;
                    let ints = mmr.mmr_coef_int.get(p).ok_or(Error::Parameters)?;
                    let fracs = mmr.mmr_coef.get(p).ok_or(Error::Parameters)?;
                    let mut c = [[0; 7]; 3];
                    for (o, row) in c.iter_mut().enumerate().take(order as usize + 1) {
                        for (k, coefficient) in row.iter_mut().enumerate() {
                            *coefficient = fixed(
                                *ints
                                    .get(o)
                                    .and_then(|v| v.get(k))
                                    .ok_or(Error::Parameters)?,
                                *fracs
                                    .get(o)
                                    .and_then(|v| v.get(k))
                                    .ok_or(Error::Parameters)?,
                                denom,
                            )?;
                        }
                    }
                    Piece::Mmr {
                        order: order + 1,
                        constant,
                        coefficients: c,
                    }
                }
                _ => return Err(Error::Unsupported),
            };
            pieces.push(piece);
        }
        curves.push(Curve::new(pivots, pieces)?);
    }
    let mut nlq = [Nlq::new(0, 0, 0, 0, denom)?; 3];
    if !h.disable_residual_flag {
        if mapping.nlq_method_idc != Some(DoviNlqMethod::LinearDeadzone)
            || mapping.nlq_num_pivots_minus2 != Some(0)
        {
            return Err(Error::Unsupported);
        }
        let n = mapping.nlq.as_ref().ok_or(Error::Parameters)?;
        for (i, component) in nlq.iter_mut().enumerate() {
            let slope = i32::try_from(unsigned(
                n.linear_deadzone_slope_int[i],
                n.linear_deadzone_slope[i],
                denom,
            )?)
            .map_err(|_| Error::Overflow)?;
            let threshold = i32::try_from(unsigned(
                n.linear_deadzone_threshold_int[i],
                n.linear_deadzone_threshold[i],
                denom,
            )?)
            .map_err(|_| Error::Overflow)?;
            let max = unsigned(n.vdr_in_max_int[i], n.vdr_in_max[i], denom)?;
            *component = Nlq::new(n.nlq_offset[i], slope, threshold, max, denom)?;
        }
    }
    // MEL: NLQ must be genuinely zero (no slope, no threshold), then no residual is applied.
    if mel {
        let n = mapping.nlq.as_ref().ok_or(Error::Parameters)?;
        if (0..3).any(|i| {
            n.linear_deadzone_slope_int[i] != 0
                || n.linear_deadzone_slope[i] != 0
                || n.linear_deadzone_threshold_int[i] != 0
                || n.linear_deadzone_threshold[i] != 0
        }) {
            return Err(Error::Unsupported);
        }
    }
    let mut params = FelFrameParams::new(
        curves.try_into().map_err(|_| Error::Parameters)?,
        nlq,
        denom,
        depth,
        h.disable_residual_flag || mel,
    )?;
    params.residual_known = !h.disable_residual_flag;
    params.mel = mel;
    Ok(params)
}

/// Decode-order mapping state. Only mapping ID zero is supported. Missing/corrupt
/// RPUs fail closed; callers reset on seek or epoch change and must supply correct pairing.
#[derive(Default)]
pub struct MappingHistory {
    epoch: Option<u64>,
    previous: Option<FelFrameParams>,
}
impl MappingHistory {
    pub fn reset(&mut self, epoch: u64) {
        self.epoch = Some(epoch);
        self.previous = None;
    }
    pub fn resolve(&mut self, epoch: u64, nal: &[u8]) -> Result<FelFrameParams> {
        let result = self.resolve_inner(epoch, nal);
        if result.is_err() {
            self.previous = None;
        }
        result
    }
    fn resolve_inner(&mut self, epoch: u64, nal: &[u8]) -> Result<FelFrameParams> {
        if self.epoch != Some(epoch) {
            return Err(Error::Epoch);
        }
        // Every byte input goes through the CRC/envelope check and the conservative syntax guard.
        let rpu = crate::semantic::parse_rpu_checked(nal).map_err(|_| Error::Rpu)?;
        let params = if rpu.header.use_prev_vdr_rpu_flag {
            if rpu.header.prev_vdr_rpu_id != 0 {
                return Err(Error::Unsupported);
            }
            let mut previous = self.previous.clone().ok_or(Error::MissingHistory)?;
            // Mapping reuse must not silently change the previous fixed-point domain.
            if rpu.header.vdr_rpu_profile != 1
                || rpu.header.rpu_format & 0x700 != 0
                || !rpu.header.el_spatial_resampling_filter_flag
                || rpu.header.bl_bit_depth_minus8 != 2
                || rpu.header.el_bit_depth_minus8 != 2
                || rpu.header.coefficient_data_type != 0
                || rpu.header.ext_mapping_idc_0_4 != 0
                || rpu.header.ext_mapping_idc_5_7 != 0
                || rpu.header.chroma_resampling_explicit_filter_flag
                || rpu.header.coefficient_log2_denom != u64::from(previous.denom)
                || rpu.header.vdr_bit_depth_minus8 + 8 != u64::from(previous.depth)
            {
                return Err(Error::Unsupported);
            }
            if !rpu.header.disable_residual_flag && !previous.residual_known {
                return Err(Error::Unsupported);
            }
            previous.disabled = rpu.header.disable_residual_flag || previous.mel;
            previous
        } else {
            from_rpu(&rpu)?
        };
        self.previous = Some(params.clone());
        Ok(params)
    }
}
