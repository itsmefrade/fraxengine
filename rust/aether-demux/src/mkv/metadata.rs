use super::{Error, element, uint};

const MAX_TRACKS: usize = 64;

/// One `ContentEncoding` element of a track.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ContentEncoding<'a> {
    pub order: u64,
    /// Bit 0: frame data, bit 1: CodecPrivate, bit 2: next encoding.
    pub scope: u64,
    /// 0 compression, 1 encryption.
    pub kind: u64,
    /// `ContentCompAlgo` (0 zlib, 1 bzlib, 2 lzo, 3 header stripping).
    pub comp_algo: u64,
    pub comp_settings: Option<&'a [u8]>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Track<'a> {
    pub number: u64,
    pub kind: u64,
    pub codec_id: &'a str,
    pub codec_private: Option<&'a [u8]>,
    pub default_duration_ns: Option<u64>,
    pub name: Option<&'a str>,
    pub language: Option<&'a str>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub channels: Option<u64>,
    pub sample_rate_hz: Option<u32>,
    pub bit_depth: Option<u64>,
    /// Raw body of a `dvcC` or `dvvC` BlockAdditionMapping, with its four character type.
    pub dolby_vision: Option<([u8; 4], &'a [u8])>,
    pub color: Option<crate::ColorInfo>,
    pub codec_delay_ns: Option<u64>,
    /// Content encodings sorted by decreasing order, i.e. in the order they are undone.
    pub encodings: Vec<ContentEncoding<'a>>,
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::InvalidMetadata);
    }
    *slot = Some(value);
    Ok(())
}

/// Read TimestampScale from one complete Segment Info payload.
pub fn timestamp_scale(mut info: &[u8]) -> Result<u64, Error> {
    let mut scale = None;
    while !info.is_empty() {
        let (id, bytes) = element(&mut info)?;
        if id == 0x2a_d7_b1 {
            set_once(&mut scale, uint(bytes)?)?;
        }
    }
    match scale {
        Some(0) => Err(Error::InvalidTimestamp),
        Some(scale) => Ok(scale),
        None => Ok(1_000_000),
    }
}

fn track(mut entry: &[u8]) -> Result<Track<'_>, Error> {
    let (mut number, mut kind, mut codec_id, mut codec_private, mut default_duration_ns) =
        (None, None, None, None, None);
    let mut extra = Extra::default();
    while !entry.is_empty() {
        let (id, bytes) = element(&mut entry)?;
        match id {
            0xd7 => set_once(&mut number, uint(bytes)?)?,
            0x83 => set_once(&mut kind, uint(bytes)?)?,
            0x86 => {
                let codec = std::str::from_utf8(bytes).map_err(|_| Error::InvalidMetadata)?;
                set_once(&mut codec_id, codec)?;
            }
            0x63a2 => set_once(&mut codec_private, bytes)?,
            0x23_e3_83 => set_once(&mut default_duration_ns, uint(bytes)?)?,
            0x23_31_4f if bytes != 1f32.to_be_bytes() && bytes != 1f64.to_be_bytes() => {
                return Err(Error::UnsupportedTrack);
            }
            0x56aa => extra.codec_delay_ns = Some(uint(bytes)?),
            0x6d80 => extra.encodings = content_encodings(bytes)?,
            0x536e => extra.name = std::str::from_utf8(bytes).ok(),
            0x22_b5_9c => extra.language = std::str::from_utf8(bytes).ok(),
            0xe0 => video(bytes, &mut extra)?,
            0xe1 => audio(bytes, &mut extra)?,
            0x41e4 => addition_mapping(bytes, &mut extra)?,
            _ => {}
        }
    }
    Ok(Track {
        number: number.filter(|&n| n > 0).ok_or(Error::InvalidMetadata)?,
        kind: kind.filter(|&n| n > 0).ok_or(Error::InvalidMetadata)?,
        codec_id: codec_id
            .filter(|id| !id.is_empty())
            .ok_or(Error::InvalidMetadata)?,
        codec_private,
        default_duration_ns: default_duration_ns.filter(|&n| n > 0),
        name: extra.name,
        language: extra.language,
        width: extra.width,
        height: extra.height,
        channels: extra.channels,
        sample_rate_hz: extra.sample_rate_hz,
        bit_depth: extra.bit_depth,
        dolby_vision: extra.dolby_vision,
        color: extra.color,
        codec_delay_ns: extra.codec_delay_ns,
        encodings: extra.encodings,
    })
}

fn content_encodings(mut body: &[u8]) -> Result<Vec<ContentEncoding<'_>>, Error> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let (id, mut entry) = element(&mut body)?;
        if id != 0x6240 {
            continue;
        }
        let mut enc = ContentEncoding { order: 0, scope: 1, kind: 0, comp_algo: 0, comp_settings: None };
        while !entry.is_empty() {
            let (id, bytes) = element(&mut entry)?;
            match id {
                0x5031 => enc.order = uint(bytes)?,
                0x5032 => enc.scope = uint(bytes)?,
                0x5033 => enc.kind = uint(bytes)?,
                0x5034 => {
                    let mut c = bytes;
                    while !c.is_empty() {
                        match element(&mut c)? {
                            (0x4254, b) => enc.comp_algo = uint(b)?,
                            (0x4255, b) => enc.comp_settings = Some(b),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        out.push(enc);
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.order));
    Ok(out)
}

#[derive(Default)]
struct Extra<'a> {
    codec_delay_ns: Option<u64>,
    encodings: Vec<ContentEncoding<'a>>,
    name: Option<&'a str>,
    language: Option<&'a str>,
    width: Option<u64>,
    height: Option<u64>,
    channels: Option<u64>,
    sample_rate_hz: Option<u32>,
    bit_depth: Option<u64>,
    dolby_vision: Option<([u8; 4], &'a [u8])>,
    color: Option<crate::ColorInfo>,
}

fn video<'a>(mut body: &'a [u8], extra: &mut Extra<'a>) -> Result<(), Error> {
    while !body.is_empty() {
        match element(&mut body)? {
            (0xb0, b) => extra.width = Some(uint(b)?),
            (0xba, b) => extra.height = Some(uint(b)?),
            (0x55b0, b) => extra.color = colour(b)?,
            _ => {}
        }
    }
    Ok(())
}

fn float(b: &[u8]) -> Option<f64> {
    match b.len() {
        4 => Some(f64::from(f32::from_be_bytes(b.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
    .filter(|v| v.is_finite() && *v >= 0.0)
}

/// `Colour` element. Unsigned code points as in ISO/IEC 23001-8, mastering values as real numbers.
fn colour(mut body: &[u8]) -> Result<Option<crate::ColorInfo>, Error> {
    use crate::ColorInfo;
    let mut c = ColorInfo::default();
    let (mut max_cll, mut max_fall) = (None, None);
    while !body.is_empty() {
        match element(&mut body)? {
            (0x55b1, b) => c.matrix = ColorInfo::code(uint(b)?, true),
            (0x55b9, b) => {
                c.full_range = match uint(b)? {
                    1 => Some(false),
                    2 => Some(true),
                    _ => None,
                }
            }
            (0x55ba, b) => c.transfer = ColorInfo::code(uint(b)?, false),
            (0x55bb, b) => c.primaries = ColorInfo::code(uint(b)?, false),
            (0x55bc, b) => max_cll = u16::try_from(uint(b)?).ok(),
            (0x55bd, b) => max_fall = u16::try_from(uint(b)?).ok(),
            (0x55d0, b) => c.mastering = mastering(b)?,
            _ => {}
        }
    }
    if max_cll.is_some() || max_fall.is_some() {
        c.content_light = Some((max_cll.unwrap_or(0), max_fall.unwrap_or(0)));
    }
    Ok((!c.is_empty()).then_some(c))
}

/// `MasteringMetadata`, converted to the `mdcv` layout (see `ColorInfo::mastering`). All ten values or nothing: a partial
/// colour volume would describe a display that does not exist.
fn mastering(mut body: &[u8]) -> Result<Option<[u32; 10]>, Error> {
    let mut v = [None; 10];
    while !body.is_empty() {
        let (id, b) = element(&mut body)?;
        // Matroska stores R, G, B, white point; the box starts with green.
        let (slot, unit) = match id {
            0x55d3 => (0, 0.00002),
            0x55d4 => (1, 0.00002),
            0x55d5 => (2, 0.00002),
            0x55d6 => (3, 0.00002),
            0x55d1 => (4, 0.00002),
            0x55d2 => (5, 0.00002),
            0x55d7 => (6, 0.00002),
            0x55d8 => (7, 0.00002),
            0x55d9 => (8, 0.0001),
            0x55da => (9, 0.0001),
            _ => continue,
        };
        v[slot] = float(b).map(|f| (f / unit).round() as u32);
    }
    let mut out = [0u32; 10];
    for (o, x) in out.iter_mut().zip(v) {
        match x {
            Some(x) => *o = x,
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

fn audio<'a>(mut body: &'a [u8], extra: &mut Extra<'a>) -> Result<(), Error> {
    while !body.is_empty() {
        match element(&mut body)? {
            (0x9f, b) => extra.channels = Some(uint(b)?),
            (0x62_64, b) => extra.bit_depth = Some(uint(b)?),
            (0xb5, b) => {
                let hz = match b.len() {
                    4 => f64::from(f32::from_be_bytes(b.try_into().map_err(|_| Error::InvalidMetadata)?)),
                    8 => f64::from_be_bytes(b.try_into().map_err(|_| Error::InvalidMetadata)?),
                    _ => return Err(Error::InvalidMetadata),
                };
                if hz.is_finite() && (0.0..=1.0e7).contains(&hz) {
                    extra.sample_rate_hz = Some(hz.round() as u32);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// BlockAdditionMapping carrying a Dolby Vision configuration record.
fn addition_mapping<'a>(mut body: &'a [u8], extra: &mut Extra<'a>) -> Result<(), Error> {
    let (mut kind, mut data) = (None, None);
    while !body.is_empty() {
        match element(&mut body)? {
            (0x41e7, b) => kind = Some(uint(b)?),
            (0x41ed, b) => data = Some(b),
            _ => {}
        }
    }
    if let (Some(kind), Some(data)) = (kind, data) {
        let tag = u32::try_from(kind).map(u32::to_be_bytes).unwrap_or_default();
        if tag == *b"dvcC" || tag == *b"dvvC" {
            extra.dolby_vision = Some((tag, data));
        }
    }
    Ok(())
}

/// Read a complete Tracks payload, borrowing the original codec private bytes.
/// Reject timing transforms and encoded payloads that the cluster parser cannot apply.
pub fn tracks(input: &[u8]) -> Result<Vec<Track<'_>>, Error> {
    let (tracks, errors) = tracks_lenient(input)?;
    match errors.into_iter().next() {
        Some(e) => Err(e.1),
        None => Ok(tracks),
    }
}

/// Like [`tracks`] but a track that cannot be used is reported (track number when known, and the reason)
/// instead of failing the whole list.
pub fn tracks_lenient(mut input: &[u8]) -> Result<(Vec<Track<'_>>, Vec<(Option<u64>, Error)>), Error> {
    let mut tracks: Vec<Track<'_>> = Vec::new();
    let mut errors = Vec::new();
    let mut seen = 0usize;
    while !input.is_empty() {
        let (id, entry) = match element(&mut input) {
            Ok(v) => v,
            Err(e) if !tracks.is_empty() || !errors.is_empty() => {
                errors.push((None, e));
                break;
            }
            Err(e) => return Err(e),
        };
        if id != 0xae {
            continue;
        }
        seen += 1;
        if seen > MAX_TRACKS {
            errors.push((None, Error::InvalidMetadata));
            break;
        }
        match track(entry) {
            Ok(t) if tracks.iter().any(|other| other.number == t.number) => errors.push((Some(t.number), Error::InvalidMetadata)),
            Ok(t) => tracks.push(t),
            Err(e) => errors.push((first_number(entry), e)),
        }
    }
    if tracks.is_empty() && errors.is_empty() {
        return Err(Error::InvalidMetadata);
    }
    Ok((tracks, errors))
}

fn first_number(mut entry: &[u8]) -> Option<u64> {
    while !entry.is_empty() {
        let (id, bytes) = element(&mut entry).ok()?;
        if id == 0xd7 {
            return uint(bytes).ok();
        }
    }
    None
}

#[cfg(test)]
mod colour_tests {
    use super::*;

    fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x80 | u8::try_from(body.len()).unwrap());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn colour_element_maps_to_box_layout() {
        let f = |id: &[u8], v: f32| el(id, &v.to_be_bytes());
        let mut m = Vec::new();
        for (id, v) in [
            ([0x55u8, 0xd1], 0.68f32),
            ([0x55, 0xd2], 0.32),
            ([0x55, 0xd3], 0.265),
            ([0x55, 0xd4], 0.69),
            ([0x55, 0xd5], 0.15),
            ([0x55, 0xd6], 0.06),
            ([0x55, 0xd7], 0.3127),
            ([0x55, 0xd8], 0.329),
            ([0x55, 0xd9], 1000.0),
            ([0x55, 0xda], 0.005),
        ] {
            m.extend(f(&id, v));
        }
        let mut body = Vec::new();
        for (id, v) in [(0xb1u8, 9u8), (0xb9, 1), (0xba, 16), (0xbb, 9), (0xbc, 1)] {
            body.extend(el(&[0x55, id], &[v]));
        }
        body.extend(el(&[0x55, 0xbd], &[0x90]));
        body.extend(el(&[0x55, 0xd0], &m));
        let c = colour(&body).unwrap().unwrap();
        assert_eq!((c.primaries, c.transfer, c.matrix, c.full_range), (Some(9), Some(16), Some(9), Some(false)));
        assert_eq!(c.mastering, Some([13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450, 10_000_000, 50]));
        assert_eq!(c.content_light, Some((1, 0x90)));
    }

    #[test]
    fn unspecified_codes_are_absent() {
        let mut body = Vec::new();
        for (id, v) in [(0xb1u8, 2u8), (0xb9, 0), (0xba, 2), (0xbb, 2)] {
            body.extend(el(&[0x55, id], &[v]));
        }
        assert_eq!(colour(&body).unwrap(), None);
    }

    #[test]
    fn partial_mastering_is_dropped() {
        assert_eq!(mastering(&el(&[0x55, 0xd9], &1000f32.to_be_bytes())).unwrap(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_track_codec_private_and_frame_duration() {
        let data = [
            0xae, 0x95, 0xd7, 0x81, 2, 0x83, 0x81, 2, 0x86, 0x85, b'A', b'_', b'D', b'T', b'S',
            0x63, 0xa2, 0x81, 0x12, 0x23, 0xe3, 0x83, 0x80,
        ];
        // Empty DefaultDuration is rejected as malformed.
        assert_eq!(tracks(&data), Err(Error::InvalidMetadata));
        let mut valid = data.to_vec();
        valid[1] += 1;
        valid[22] = 0x81;
        valid.push(10);
        let parsed = tracks(&valid).unwrap();
        assert_eq!(parsed[0].codec_id, "A_DTS");
        assert_eq!(parsed[0].codec_private, Some(&[0x12][..]));
        assert_eq!(parsed[0].default_duration_ns, Some(10));
    }
}
