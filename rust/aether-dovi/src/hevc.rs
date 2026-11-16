//! Zero-copy HEVC NAL framing. Limits are checked before returning any packet.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    AnnexB,
    /// The HEVCDecoderConfigurationRecord's lengthSizeMinusOne plus one.
    LengthPrefixed(u8),
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_access_unit_bytes: usize,
    pub max_nal_bytes: usize,
    pub max_nals: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_access_unit_bytes: 16 * 1024 * 1024,
            max_nal_bytes: 8 * 1024 * 1024,
            max_nals: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    EmptyAccessUnit,
    AccessUnitTooLarge,
    InvalidLengthSize,
    MissingStartCode,
    JunkBeforeStartCode,
    TruncatedLength,
    NalOverrun,
    EmptyNal,
    NalTooLarge,
    TooManyNals,
    TruncatedHeader,
    ForbiddenZeroBit,
    InvalidTemporalId,
    InvalidEnhancementHeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub nal_type: u8,
    pub layer_id: u8,
    pub temporal_id: u8,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < 2 {
            return Err(Error::TruncatedHeader);
        }
        if bytes[0] & 0x80 != 0 {
            return Err(Error::ForbiddenZeroBit);
        }
        let temporal_id_plus_one = bytes[1] & 7;
        if temporal_id_plus_one == 0 {
            return Err(Error::InvalidTemporalId);
        }
        Ok(Self {
            nal_type: (bytes[0] >> 1) & 63,
            layer_id: ((bytes[0] & 1) << 5) | (bytes[1] >> 3),
            temporal_id: temporal_id_plus_one - 1,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Non-RPU/non-wrapper NAL, including parameter sets and access unit delimiters.
    Base,
    /// Dolby Profile 7 carriage: type-63 wrapper followed by an HEVC NAL header.
    Enhancement,
    Rpu,
}

#[derive(Clone, Copy, Debug)]
pub struct Nal<'a> {
    /// Includes the HEVC NAL header, excludes start code or length prefix.
    pub bytes: &'a [u8],
    pub header: Header,
    pub kind: Kind,
}

impl<'a> Nal<'a> {
    pub fn decode_bytes(self) -> &'a [u8] {
        if self.kind == Kind::Enhancement {
            &self.bytes[2..]
        } else {
            self.bytes
        }
    }
}

fn push_nal<'a>(out: &mut Vec<Nal<'a>>, bytes: &'a [u8], limits: Limits) -> Result<(), Error> {
    if bytes.is_empty() {
        return Err(Error::EmptyNal);
    }
    if bytes.len() > limits.max_nal_bytes {
        return Err(Error::NalTooLarge);
    }
    if out.len() >= limits.max_nals {
        return Err(Error::TooManyNals);
    }
    let header = Header::parse(bytes)?;
    let kind = match header.nal_type {
        62 => Kind::Rpu,
        63 => {
            let inner = Header::parse(&bytes[2..]).map_err(|_| Error::InvalidEnhancementHeader)?;
            if inner.nal_type >= 62 {
                return Err(Error::InvalidEnhancementHeader);
            }
            Kind::Enhancement
        }
        _ => Kind::Base,
    };
    out.push(Nal {
        bytes,
        header,
        kind,
    });
    Ok(())
}

/// Finds start codes without stripping emulation-prevention bytes from payloads.
fn start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 2 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 {
            if bytes[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 3 < bytes.len() && bytes[i + 2] == 0 && bytes[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

pub fn split(bytes: &[u8], framing: Framing, limits: Limits) -> Result<Vec<Nal<'_>>, Error> {
    if bytes.is_empty() {
        return Err(Error::EmptyAccessUnit);
    }
    if bytes.len() > limits.max_access_unit_bytes {
        return Err(Error::AccessUnitTooLarge);
    }
    let mut out = Vec::new();
    match framing {
        Framing::LengthPrefixed(size @ 1..=4) => {
            let width = usize::from(size);
            let mut offset = 0;
            while offset < bytes.len() {
                if bytes.len() - offset < width {
                    return Err(Error::TruncatedLength);
                }
                let mut len = 0usize;
                for byte in &bytes[offset..offset + width] {
                    len = (len << 8) | usize::from(*byte);
                }
                offset += width;
                if len > bytes.len() - offset {
                    return Err(Error::NalOverrun);
                }
                push_nal(&mut out, &bytes[offset..offset + len], limits)?;
                offset += len;
            }
        }
        Framing::LengthPrefixed(_) => return Err(Error::InvalidLengthSize),
        Framing::AnnexB => {
            let (mut offset, mut width) = start_code(bytes, 0).ok_or(Error::MissingStartCode)?;
            if bytes[..offset].iter().any(|b| *b != 0) {
                return Err(Error::JunkBeforeStartCode);
            }
            loop {
                let start = offset + width;
                let next = start_code(bytes, start);
                let mut end = next.map_or(bytes.len(), |(i, _)| i);
                // Annex B trailing_zero_8bits are not part of nal_unit().
                while end > start && bytes[end - 1] == 0 {
                    end -= 1;
                }
                push_nal(&mut out, &bytes[start..end], limits)?;
                match next {
                    Some((i, w)) => {
                        offset = i;
                        width = w;
                    }
                    None => break,
                }
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inspection {
    pub total: u32,
    pub base: u32,
    pub enhancement: u32,
    pub rpu: u32,
    pub crc_valid_rpu: u32,
}

pub fn inspect(bytes: &[u8], framing: Framing, limits: Limits) -> Result<Inspection, Error> {
    let nals = split(bytes, framing, limits)?;
    let mut report = Inspection {
        total: nals.len() as u32,
        ..Inspection::default()
    };
    for nal in nals {
        match nal.kind {
            Kind::Base => report.base += 1,
            Kind::Enhancement => report.enhancement += 1,
            Kind::Rpu => {
                report.rpu += 1;
                if crate::rpu::crc_is_valid(nal.bytes) {
                    report.crc_valid_rpu += 1;
                }
            }
        }
    }
    Ok(report)
}
