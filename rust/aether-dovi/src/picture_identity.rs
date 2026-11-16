//! Checked HEVC picture-identity prefix parsing. No pixels or CABAC are parsed.
//! Feed one layer in decode order; source access-unit boundaries are explicit.
//! Decoder output must still be cross-checked against these identities.
use crate::hevc::Header;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    Nal,
    Syntax,
    Unsupported,
    MissingParameterSet,
    AwaitingIrap,
    MultiSlice,
    Overflow,
    Epoch,
    Poisoned,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureIdentity {
    pub epoch: u32,
    pub sequence: u32,
    pub decode_index: u64,
    pub poc: i32,
    pub poc_lsb: u32,
    pub poc_lsb_bits: u8,
    pub nal_type: u8,
    pub temporal_id: u8,
    pub pps_id: u8,
    pub starts_sequence: bool,
    pub irap: bool,
    pub no_rasl_output: bool,
    pub no_output_of_prior_pics: bool,
    pub drop_prior_output: bool,
    pub output_expected: bool,
    pub suppressed_rasl: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityEvent {
    ParameterSet,
    PictureStart(PictureIdentity),
    SliceContinuation(PictureIdentity),
    Boundary,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sps {
    poc_bits: u8,
    separate: bool,
    width: u32,
    height: u32,
    ctb_log2: u8,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pps {
    sps: u8,
    dependent: bool,
    output_flag: bool,
    extra: u8,
}
#[derive(Debug, Clone)]
struct Current {
    id: PictureIdentity,
    addresses: Vec<u32>,
    domain: Sps,
}
#[derive(Clone)]
pub struct PictureIdentityTracker {
    epoch: u32,
    sps: [Option<Sps>; 16],
    pps: [Option<Pps>; 64],
    previous_tid0: i32,
    seen: bool,
    after_eos: bool,
    skip_rasl: bool,
    sequence: u32,
    next_decode: u64,
    current: Option<Current>,
    last_domain: Option<Sps>,
    poisoned: bool,
}
impl PictureIdentityTracker {
    pub fn new(epoch: u32) -> Self {
        Self {
            epoch,
            sps: [None; 16],
            pps: [None; 64],
            previous_tid0: 0,
            seen: false,
            after_eos: false,
            skip_rasl: false,
            sequence: 0,
            next_decode: 0,
            current: None,
            last_domain: None,
            poisoned: false,
        }
    }
    pub fn reset(&mut self, epoch: u32) -> Result<(), IdentityError> {
        if epoch <= self.epoch {
            return Err(IdentityError::Epoch);
        }
        *self = Self::new(epoch);
        Ok(())
    }
    /// Finish the current coded picture at a proved complete-AU boundary. Does
    /// not signal EOS or flush display reordering; it only commits prevTid0.
    pub fn end_access_unit(&mut self, epoch: u32) -> Result<(), IdentityError> {
        self.check(epoch)?;
        self.finish_picture();
        Ok(())
    }
    fn check(&self, epoch: u32) -> Result<(), IdentityError> {
        if epoch != self.epoch {
            Err(IdentityError::Epoch)
        } else if self.poisoned {
            Err(IdentityError::Poisoned)
        } else {
            Ok(())
        }
    }
    fn finish_picture(&mut self) {
        if let Some(current) = self.current.take() {
            let id = current.id;
            if !id.suppressed_rasl
                && id.temporal_id == 0
                && !matches!(id.nal_type, 0 | 2 | 4 | 6..=9)
            {
                self.previous_tid0 = id.poc;
            }
        }
    }
    pub fn push_nal(&mut self, bytes: &[u8], epoch: u32) -> Result<IdentityEvent, IdentityError> {
        self.check(epoch)?;
        let result = self.push_inner(bytes);
        if result.is_err() && result != Err(IdentityError::AwaitingIrap) {
            self.poisoned = true;
        }
        result
    }
    fn push_inner(&mut self, bytes: &[u8]) -> Result<IdentityEvent, IdentityError> {
        let header = Header::parse(bytes).map_err(|_| IdentityError::Nal)?;
        if header.layer_id != 0
            || bytes.len() > 8 * 1024 * 1024
            || (bytes.len() < 3 && !matches!(header.nal_type, 36 | 37))
        {
            return Err(IdentityError::Nal);
        }
        match header.nal_type {
            33 => {
                let (id, sps) = parse_sps(bytes)?;
                self.sps[id as usize] = Some(sps);
                Ok(IdentityEvent::ParameterSet)
            }
            34 => {
                let (id, pps) = parse_pps(bytes)?;
                self.pps[id as usize] = Some(pps);
                Ok(IdentityEvent::ParameterSet)
            }
            35 | 37 => {
                self.finish_picture();
                Ok(IdentityEvent::Boundary)
            }
            36 => {
                self.finish_picture();
                self.after_eos = true;
                Ok(IdentityEvent::Boundary)
            }
            0..=9 | 16..=21 => self.slice(bytes, header),
            10..=31 => Err(IdentityError::Unsupported),
            _ => Ok(IdentityEvent::Other),
        }
    }
    fn slice(&mut self, bytes: &[u8], header: Header) -> Result<IdentityEvent, IdentityError> {
        let rbsp = prefix(bytes)?;
        let mut r = Bits::new(&rbsp);
        let first = r.bit()?;
        let irap = matches!(header.nal_type, 16..=21);
        let no_prior = if irap { r.bit()? } else { false };
        if irap && header.temporal_id != 0 {
            return Err(IdentityError::Syntax);
        }
        let pps_id = r.ue()?;
        if pps_id > 63 {
            return Err(IdentityError::Syntax);
        }
        let p = self.pps[pps_id as usize].ok_or(IdentityError::MissingParameterSet)?;
        let s = self.sps[p.sps as usize].ok_or(IdentityError::MissingParameterSet)?;
        let mut dependent = false;
        let mut address = 0;
        if !first {
            if p.dependent {
                dependent = r.bit()?;
            }
            let ctb = 1_u32 << s.ctb_log2;
            let count = s
                .width
                .div_ceil(ctb)
                .checked_mul(s.height.div_ceil(ctb))
                .ok_or(IdentityError::Overflow)?;
            let bits = 32 - (count - 1).leading_zeros();
            address = r.bits(bits as usize)?;
            if address == 0 || address >= count {
                return Err(IdentityError::MultiSlice);
            }
        }
        if dependent {
            let current = self.current.as_mut().ok_or(IdentityError::MultiSlice)?;
            validate_continuation(current, header, pps_id as u8, s, address)?;
            return Ok(IdentityEvent::SliceContinuation(current.id));
        }
        r.skip(usize::from(p.extra))?;
        if r.ue()? > 2 {
            return Err(IdentityError::Syntax);
        }
        let output = if p.output_flag { r.bit()? } else { true };
        if s.separate {
            r.skip(2)?;
        }
        let idr = matches!(header.nal_type, 19 | 20);
        let lsb = if idr {
            0
        } else {
            r.bits(usize::from(s.poc_bits))?
        };
        if !first {
            let current = self.current.as_mut().ok_or(IdentityError::MultiSlice)?;
            validate_continuation(current, header, pps_id as u8, s, address)?;
            if current.id.poc_lsb != lsb
                || current.id.no_output_of_prior_pics != no_prior
                || (output && !current.id.suppressed_rasl) != current.id.output_expected
            {
                return Err(IdentityError::MultiSlice);
            }
            return Ok(IdentityEvent::SliceContinuation(current.id));
        }
        self.finish_picture();
        if (!self.seen || self.after_eos) && !irap {
            return Err(IdentityError::AwaitingIrap);
        }
        let no_rasl = irap && (header.nal_type != 21 || !self.seen || self.after_eos);
        let starts = no_rasl;
        let was_seen = self.seen;
        if self.last_domain.is_some_and(|old| old != s) && !starts {
            return Err(IdentityError::Unsupported);
        }
        if starts && was_seen {
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or(IdentityError::Overflow)?;
        }
        if irap {
            self.skip_rasl = no_rasl;
        }
        let suppressed = matches!(header.nal_type, 8 | 9) && self.skip_rasl;
        let poc = if idr {
            0
        } else {
            derive_poc(self.previous_tid0, lsb, s.poc_bits, no_rasl)?
        };
        let id = PictureIdentity {
            epoch: self.epoch,
            sequence: self.sequence,
            decode_index: self.next_decode,
            poc,
            poc_lsb: lsb,
            poc_lsb_bits: s.poc_bits,
            nal_type: header.nal_type,
            temporal_id: header.temporal_id,
            pps_id: pps_id as u8,
            starts_sequence: starts,
            irap,
            no_rasl_output: no_rasl,
            no_output_of_prior_pics: no_prior,
            drop_prior_output: starts && was_seen && (header.nal_type == 21 || no_prior),
            output_expected: output && !suppressed,
            suppressed_rasl: suppressed,
        };
        self.next_decode = self
            .next_decode
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        self.seen = true;
        self.after_eos = false;
        self.last_domain = Some(s);
        self.current = Some(Current {
            id,
            addresses: vec![0],
            domain: s,
        });
        Ok(IdentityEvent::PictureStart(id))
    }
}
fn validate_continuation(
    current: &mut Current,
    h: Header,
    pps: u8,
    domain: Sps,
    address: u32,
) -> Result<(), IdentityError> {
    if current.id.nal_type != h.nal_type
        || current.id.temporal_id != h.temporal_id
        || current.id.pps_id != pps
        || current.domain != domain
        || current.addresses.contains(&address)
        || current.addresses.len() >= 256
    {
        return Err(IdentityError::MultiSlice);
    }
    current.addresses.push(address);
    Ok(())
}
/// HEVC 8.3.1 wrap arithmetic, matching the pinned decoder's compute_poc.
pub fn derive_poc(
    previous: i32,
    lsb: u32,
    bits: u8,
    reset_msb: bool,
) -> Result<i32, IdentityError> {
    if !(4..=16).contains(&bits) || lsb >= 1_u32 << bits {
        return Err(IdentityError::Syntax);
    }
    let max = 1_i32 << bits;
    let prev_lsb = previous.rem_euclid(max);
    let prev_msb = previous - prev_lsb;
    let now = lsb as i32;
    let msb = if reset_msb {
        0
    } else if now < prev_lsb && prev_lsb - now >= max / 2 {
        prev_msb.checked_add(max).ok_or(IdentityError::Overflow)?
    } else if now > prev_lsb && now - prev_lsb > max / 2 {
        prev_msb.checked_sub(max).ok_or(IdentityError::Overflow)?
    } else {
        prev_msb
    };
    msb.checked_add(now).ok_or(IdentityError::Overflow)
}
fn parse_sps(bytes: &[u8]) -> Result<(u8, Sps), IdentityError> {
    let rbsp = prefix(bytes)?;
    let mut r = Bits::new(&rbsp);
    r.skip(4)?;
    let sub = r.bits(3)?;
    if sub > 6 {
        return Err(IdentityError::Unsupported);
    }
    r.skip(1 + 96)?;
    let mut flags = [(false, false); 6];
    for flag in flags.iter_mut().take(sub as usize) {
        *flag = (r.bit()?, r.bit()?);
    }
    if sub > 0 {
        r.skip(2 * (8 - sub as usize))?;
    }
    for &(profile, level) in flags.iter().take(sub as usize) {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    let id = r.ue()?;
    let chroma = r.ue()?;
    if id > 15 || chroma > 3 {
        return Err(IdentityError::Syntax);
    }
    let separate = chroma == 3 && r.bit()?;
    let width = r.ue()?;
    let height = r.ue()?;
    if width == 0 || height == 0 || width > 8192 || height > 4320 {
        return Err(IdentityError::Unsupported);
    }
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.ue()? > 8 || r.ue()? > 8 {
        return Err(IdentityError::Unsupported);
    }
    let poc = r.ue()?;
    if poc > 12 {
        return Err(IdentityError::Unsupported);
    }
    let all = r.bit()?;
    let start = if all { 0 } else { sub };
    for _ in start..=sub {
        if r.ue()? > 15 {
            return Err(IdentityError::Unsupported);
        }
        r.ue()?;
        r.ue()?;
    }
    let min = r.ue()?;
    let diff = r.ue()?;
    if min > 3 || diff > 3 || !(4..=6).contains(&(min + 3 + diff)) {
        return Err(IdentityError::Unsupported);
    }
    Ok((
        id as u8,
        Sps {
            poc_bits: poc as u8 + 4,
            separate,
            width,
            height,
            ctb_log2: (min + 3 + diff) as u8,
        },
    ))
}
fn parse_pps(bytes: &[u8]) -> Result<(u8, Pps), IdentityError> {
    let rbsp = prefix(bytes)?;
    let mut r = Bits::new(&rbsp);
    let id = r.ue()?;
    let sps = r.ue()?;
    if id > 63 || sps > 15 {
        return Err(IdentityError::Syntax);
    }
    let dependent = r.bit()?;
    let output_flag = r.bit()?;
    let extra = r.bits(3)? as u8;
    Ok((
        id as u8,
        Pps {
            sps: sps as u8,
            dependent,
            output_flag,
            extra,
        },
    ))
}
fn prefix(bytes: &[u8]) -> Result<Vec<u8>, IdentityError> {
    let payload = bytes.get(2..).ok_or(IdentityError::Nal)?;
    let end = payload.len().min(1024);
    let mut result = Vec::with_capacity(end);
    let mut zeros = 0;
    let mut i = 0;
    while i < end {
        let b = payload[i];
        if zeros >= 2 && b == 3 {
            if payload.get(i + 1).is_none_or(|&next| next > 3) {
                return Err(IdentityError::Syntax);
            }
            zeros = 0;
            i += 1;
            continue;
        }
        result.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    Ok(result)
}
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
    fn bits(&mut self, n: usize) -> Result<u32, IdentityError> {
        if n > 32
            || self
                .at
                .checked_add(n)
                .is_none_or(|end| end > self.data.len() * 8)
        {
            return Err(IdentityError::Syntax);
        }
        let mut value = 0;
        for _ in 0..n {
            value = value << 1 | u32::from((self.data[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }
    fn bit(&mut self) -> Result<bool, IdentityError> {
        Ok(self.bits(1)? != 0)
    }
    fn skip(&mut self, n: usize) -> Result<(), IdentityError> {
        if self
            .at
            .checked_add(n)
            .is_none_or(|end| end > self.data.len() * 8)
        {
            Err(IdentityError::Syntax)
        } else {
            self.at += n;
            Ok(())
        }
    }
    fn ue(&mut self) -> Result<u32, IdentityError> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(IdentityError::Syntax);
            }
        }
        let suffix = self.bits(zeros)?;
        Ok(((1_u64 << zeros) - 1 + u64::from(suffix)) as u32)
    }
}
