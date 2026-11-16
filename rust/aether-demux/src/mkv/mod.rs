//! Bounded, zero-copy Matroska cluster parsing for the browser demux path.

mod index;
mod metadata;
pub use index::{MkvPacket, MkvScan, scan};
pub(crate) use index::{MkvLazy, decode_steps};
pub use metadata::{ContentEncoding, Track, timestamp_scale, tracks, tracks_lenient};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidMetadata,
    UnsupportedTrack,
    Truncated,
    InvalidVint,
    UnknownSize,
    InvalidTimestamp,
    InvalidBlock,
    MissingFrameDuration,
    InvalidRange,
}

fn vint(input: &mut &[u8], keep_marker: bool, max_width: usize) -> Result<(u64, usize), Error> {
    let first = *input.first().ok_or(Error::Truncated)?;
    let width = first.leading_zeros() as usize + 1;
    if width > max_width {
        return Err(Error::InvalidVint);
    }
    let (bytes, rest) = input.split_at_checked(width).ok_or(Error::Truncated)?;
    *input = rest;
    let initial = if keep_marker {
        first
    } else {
        first & (0xffu8.checked_shr(width as u32).unwrap_or(0))
    };
    let value = bytes[1..].iter().fold(u64::from(initial), |value, &byte| {
        (value << 8) | u64::from(byte)
    });
    Ok((value, width))
}

pub const MAX_ELEMENT_HEADER_BYTES: usize = 12;

#[derive(Debug, PartialEq, Eq)]
pub struct ElementHeader {
    pub id: u32,
    pub header_bytes: usize,
    pub payload_bytes: Option<u64>,
}

impl ElementHeader {
    /// Resolve a known-size payload inside an enclosing element or source.
    pub fn payload_range(
        &self,
        offset: u64,
        parent_end: u64,
    ) -> Result<std::ops::Range<u64>, Error> {
        let size = self.payload_bytes.ok_or(Error::UnknownSize)?;
        let start = offset
            .checked_add(self.header_bytes as u64)
            .ok_or(Error::InvalidRange)?;
        let end = start
            .checked_add(size)
            .filter(|&end| end <= parent_end)
            .ok_or(Error::InvalidRange)?;
        Ok(start..end)
    }
}

/// Parse a header from at most twelve leading bytes. The payload may be absent.
/// An unknown EBML size is represented as None and must be resolved by the caller.
pub fn element_header(mut bytes: &[u8]) -> Result<ElementHeader, Error> {
    let (id, id_width) = vint(&mut bytes, true, 4)?;
    let (size, size_width) = vint(&mut bytes, false, 8)?;
    Ok(ElementHeader {
        id: id as u32,
        header_bytes: id_width + size_width,
        payload_bytes: (size != (1u64 << (7 * size_width)) - 1).then_some(size),
    })
}

fn element<'a>(input: &mut &'a [u8]) -> Result<(u64, &'a [u8]), Error> {
    let header = element_header(input)?;
    let size = header.payload_bytes.ok_or(Error::UnknownSize)?;
    let size = usize::try_from(size).map_err(|_| Error::Truncated)?;
    let (payload, rest) = input[header.header_bytes..]
        .split_at_checked(size)
        .ok_or(Error::Truncated)?;
    *input = rest;
    Ok((u64::from(header.id), payload))
}

fn uint(bytes: &[u8]) -> Result<u64, Error> {
    if bytes.is_empty() || bytes.len() > 8 {
        return Err(Error::InvalidMetadata);
    }
    Ok(bytes
        .iter()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte)))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub track: u64,
    pub timestamp_ns: i64,
    pub data: &'a [u8],
    /// SimpleBlock flag, or a BlockGroup without ReferenceBlock. Muxers can
    /// get this wrong, so seeking re-checks the payload (see `Demuxer::seek`).
    pub keyframe: bool,
}

// RFC 9559 section 10.3. Lacing stores at most 256 frame lengths.
fn lace_sizes<'a>(mut frames: &'a [u8], mode: u8, sizes: &mut [usize]) -> Result<&'a [u8], Error> {
    let (last, prefix) = sizes.split_last_mut().ok_or(Error::InvalidBlock)?;
    match mode {
        0x04 => {
            let count = prefix.len() + 1;
            if frames.len() % count != 0 {
                return Err(Error::InvalidBlock);
            }
            prefix.fill(frames.len() / count);
        }
        0x02 => {
            for size in prefix.iter_mut() {
                loop {
                    let (&byte, rest) = frames.split_first().ok_or(Error::Truncated)?;
                    frames = rest;
                    *size = size
                        .checked_add(usize::from(byte))
                        .ok_or(Error::InvalidBlock)?;
                    if byte != 255 {
                        break;
                    }
                }
            }
        }
        0x06 => {
            let (first, rest) = prefix.split_first_mut().ok_or(Error::InvalidBlock)?;
            let mut previous = i128::from(vint(&mut frames, false, 8)?.0);
            *first = usize::try_from(previous).map_err(|_| Error::InvalidBlock)?;
            for size in rest {
                let (value, width) = vint(&mut frames, false, 8)?;
                let bias = (1i128 << (7 * width - 1)) - 1;
                previous += i128::from(value) - bias;
                *size = usize::try_from(previous).map_err(|_| Error::InvalidBlock)?;
            }
        }
        _ => return Err(Error::InvalidBlock),
    }
    *last = prefix.iter().try_fold(frames.len(), |remaining, &size| {
        remaining
            .checked_sub(size)
            .filter(|_| size != 0)
            .ok_or(Error::InvalidBlock)
    })?;
    if *last == 0 {
        return Err(Error::InvalidBlock);
    }
    Ok(frames)
}

fn visit_block<'a>(
    mut payload: &'a [u8],
    timestamp: i128,
    scale_ns: u64,
    duration_ns: &impl Fn(u64) -> Option<u64>,
    selected: &impl Fn(u64) -> bool,
    group_keyframe: Option<bool>,
    visit: &mut impl FnMut(Packet<'a>),
) -> Result<(), Error> {
    let (track, _) = vint(&mut payload, false, 8)?;
    let (header, data) = payload.split_at_checked(3).ok_or(Error::Truncated)?;
    if track == 0 || data.is_empty() {
        return Err(Error::InvalidBlock);
    }
    if !selected(track) {
        return Ok(());
    }
    let relative = i16::from_be_bytes([header[0], header[1]]);
    let timestamp_ns = (timestamp + i128::from(relative))
        .checked_mul(i128::from(scale_ns))
        .and_then(|ns| i64::try_from(ns).ok())
        .ok_or(Error::InvalidTimestamp)?;
    let keyframe = group_keyframe.unwrap_or(header[2] & 0x80 != 0);
    match header[2] & 0x06 {
        0 => visit(Packet {
            track,
            timestamp_ns,
            data,
            keyframe,
        }),
        mode => {
            let (&count_minus_one, frames) = data.split_first().ok_or(Error::InvalidBlock)?;
            let count = usize::from(count_minus_one) + 1;
            if count < 2 {
                return Err(Error::InvalidBlock);
            }
            let mut sizes = [0; 256];
            let mut frames = lace_sizes(frames, mode, &mut sizes[..count])?;
            let duration = duration_ns(track)
                .filter(|&ns| ns > 0)
                .ok_or(Error::MissingFrameDuration)?;
            let duration = i128::from(duration);
            let start = i128::from(timestamp_ns);
            if i64::try_from(start + (count as i128 - 1) * duration).is_err() {
                return Err(Error::InvalidTimestamp);
            }
            for (i, &size) in sizes[..count].iter().enumerate() {
                let (data, rest) = frames.split_at(size);
                frames = rest;
                visit(Packet {
                    track,
                    timestamp_ns: (start + i as i128 * duration) as i64,
                    data,
                    keyframe,
                });
            }
        }
    }
    Ok(())
}

/// Visit packets in file order in one complete, size-bounded Cluster payload.
/// Use the Segment's TimestampScale and supply a constant frame duration for
/// each laced track. This assumes unit track scale and zero codec delay.
/// The visitor may have received earlier packets when a later block fails.
pub fn visit_cluster<'a>(
    payload: &'a [u8],
    scale_ns: u64,
    duration_ns: impl Fn(u64) -> Option<u64>,
    visit: impl FnMut(Packet<'a>),
) -> Result<(), Error> {
    visit_selected_cluster(payload, scale_ns, duration_ns, |_| true, visit)
}

/// Parse packet contents and lacing only for tracks selected by the caller.
pub fn visit_selected_cluster<'a>(
    payload: &'a [u8],
    scale_ns: u64,
    duration_ns: impl Fn(u64) -> Option<u64>,
    selected: impl Fn(u64) -> bool,
    mut visit: impl FnMut(Packet<'a>),
) -> Result<(), Error> {
    if scale_ns == 0 {
        return Err(Error::InvalidTimestamp);
    }
    let mut rest = payload;
    let mut timestamp = None;
    while !rest.is_empty() {
        let (id, bytes) = element(&mut rest)?;
        if id == 0xe7 {
            if timestamp.is_some() || bytes.is_empty() || bytes.len() > 8 {
                return Err(Error::InvalidTimestamp);
            }
            timestamp = Some(
                bytes
                    .iter()
                    .fold(0i128, |value, &byte| (value << 8) | i128::from(byte)),
            );
        }
    }
    let timestamp = timestamp.ok_or(Error::InvalidTimestamp)?;
    rest = payload;
    while !rest.is_empty() {
        match element(&mut rest)? {
            (0xa3, bytes) => visit_block(
                bytes,
                timestamp,
                scale_ns,
                &duration_ns,
                &selected,
                None,
                &mut visit,
            )?,
            (0xa0, mut group) => {
                let mut packet = None;
                let mut referenced = false;
                while !group.is_empty() {
                    let (id, bytes) = element(&mut group)?;
                    if id == 0xa1 {
                        if packet.is_some() {
                            return Err(Error::InvalidBlock);
                        }
                        packet = Some(bytes);
                    } else if id == 0xfb {
                        referenced = true;
                    }
                }
                // BlockGroup duration and reference semantics are separate from
                // SimpleBlock timing. Accept unlaced blocks here for now.
                visit_block(
                    packet.ok_or(Error::InvalidBlock)?,
                    timestamp,
                    scale_ns,
                    &|_| None,
                    &selected,
                    Some(!referenced),
                    &mut visit,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_header_only_reads_within_parent_bounds() {
        let bytes = [0x1f, 0x43, 0xb6, 0x75, 0x40, 0x80];
        let header = element_header(&bytes).unwrap();
        assert_eq!(header.id, 0x1f43b675);
        assert_eq!(header.payload_range(200, 334), Ok(206..334));
        assert_eq!(header.payload_range(200, 333), Err(Error::InvalidRange));
        assert_eq!(
            header.payload_range(u64::MAX, u64::MAX),
            Err(Error::InvalidRange)
        );
        assert_eq!(element_header(&bytes[..5]), Err(Error::Truncated));
        let unknown =
            element_header(&[0x18, 0x53, 0x80, 0x67, 1, 255, 255, 255, 255, 255, 255, 255])
                .unwrap();
        assert_eq!(unknown.header_bytes, MAX_ELEMENT_HEADER_BYTES);
        assert_eq!(unknown.payload_range(0, 500), Err(Error::UnknownSize));
    }

    #[test]
    fn visits_two_tracks_in_file_order_with_signed_relative_timestamps() {
        let cluster = [
            0xa3, 0x85, 0x81, 0xff, 0xfe, 0x80, 0x11, 0xe7, 0x81, 0x0a, 0xa0, 0x87, 0xa1, 0x85,
            0x82, 0, 2, 0, 0x22,
        ];
        let mut packets = Vec::new();
        visit_cluster(&cluster, 1_000_000, |_| None, |packet| packets.push(packet)).unwrap();
        assert_eq!(
            packets,
            [
                Packet {
                    track: 1,
                    timestamp_ns: 8_000_000,
                    data: &[0x11],
                    keyframe: true,
                },
                Packet {
                    track: 2,
                    timestamp_ns: 12_000_000,
                    data: &[0x22],
                    keyframe: true,
                },
            ]
        );
    }

    #[test]
    fn rejects_malformed_lacing_and_truncation() {
        let cluster = [0xe7, 0x81, 0, 0xa3, 0x85, 0x82, 0, 0, 0x02, 0x22];
        assert_eq!(
            visit_cluster(&cluster, 1, |_| None, |_| ()),
            Err(Error::Truncated)
        );
        assert_eq!(
            visit_cluster(&cluster[..9], 1, |_| None, |_| ()),
            Err(Error::Truncated)
        );
    }

    #[test]
    fn splits_variable_laces_and_rejects_bad_sizes_before_delivery() {
        for (mode, lengths) in [(0x02, [3, 2]), (0x06, [0x83, 0xbe])] {
            let mut cluster = vec![0xe7, 0x81, 1, 0xa3, 0x8d, 0x82, 0, 0, mode, 2];
            cluster.extend(lengths);
            cluster.extend([1, 2, 3, 4, 5, 6]);
            let mut packets = Vec::new();
            visit_cluster(&cluster, 1, |_| Some(4), |p| packets.push(p)).unwrap();
            assert_eq!(
                packets.iter().map(|p| p.data).collect::<Vec<_>>(),
                [&[1, 2, 3][..], &[4, 5][..], &[6][..],]
            );
            assert_eq!(
                packets.iter().map(|p| p.timestamp_ns).collect::<Vec<_>>(),
                [1, 5, 9]
            );
            cluster[11] = if mode == 0x02 { 10 } else { 0x80 };
            assert_eq!(
                visit_cluster(
                    &cluster,
                    1,
                    |_| Some(4),
                    |_| panic!("invalid lace delivered")
                ),
                Err(Error::InvalidBlock)
            );
        }
    }

    #[test]
    fn splits_fixed_laces_at_track_frame_duration() {
        let cluster = [
            0xe7, 0x81, 2, 0xa3, 0x89, 0x82, 0, 0, 0x04, 1, 10, 11, 20, 21,
        ];
        let mut packets = Vec::new();
        visit_cluster(
            &cluster,
            1_000_000,
            |track| (track == 2).then_some(10_666_667),
            |p| packets.push(p),
        )
        .unwrap();
        assert_eq!(packets[0].data, &[10, 11]);
        assert_eq!(packets[1].data, &[20, 21]);
        assert_eq!(packets[0].timestamp_ns, 2_000_000);
        assert_eq!(packets[1].timestamp_ns, 12_666_667);
        assert_eq!(
            visit_cluster(&cluster, 1_000_000, |_| None, |_| ()),
            Err(Error::MissingFrameDuration)
        );
    }
}
