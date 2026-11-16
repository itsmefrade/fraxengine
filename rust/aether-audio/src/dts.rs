//! Scalar DTS / DTS-HD MA bed decoder over the pinned `dca` crate. Not a DTS:X object renderer;
//! `DtsSession::with_heights` selects the standard 7.1.4 height presentation.
use dca::{BedChannel, FoldPlan, HdDecoder, HdError, PcmDecoder, XPresentation, parse_header};
pub const MAX_INPUT: usize = 4 * 1024 * 1024;
pub const MAX_SAMPLES: usize = 48_000;
const WAVE_DCA: [(usize, &str); 8] = [
    (1, "FL"),
    (2, "FR"),
    (0, "FC"),
    (5, "LFE"),
    (7, "BL"),
    (8, "BR"),
    (3, "SL"),
    (4, "SR"),
];

pub struct Decoded {
    pub sample_rate: u32,
    pub channel_labels: Vec<&'static str>,
    pub pcm: Vec<f32>,
    pub integer24: Option<Vec<i32>>,
    pub packets: usize,
    pub pending: usize,
    pub hd_packets: usize,
    pub x_extension_frames: usize,
    pub x_feed_max: usize,
    pub preroll_packets: usize,
    pub preroll_sample_frames: usize,
}
fn channel_label(channel: BedChannel) -> &'static str {
    match channel {
        BedChannel::FrontLeft => "FL",
        BedChannel::FrontRight => "FR",
        BedChannel::Center => "FC",
        BedChannel::LowFrequencyEffects => "LFE",
        BedChannel::SurroundLeft => "SL",
        BedChannel::SurroundRight => "SR",
        BedChannel::RearCenter => "BC",
        BedChannel::RearLeft => "BL",
        BedChannel::RearRight => "BR",
        BedChannel::WideLeft => "WL",
        BedChannel::WideRight => "WR",
    }
}

/// Input format: repeated u32 little-endian packet byte length then whole DTS packet including EXSS.
pub fn decode(input: &[u8], max_samples: usize) -> Result<Decoded, String> {
    decode_with_preroll(input, max_samples, 0)
}

/// Decode real preceding packets into state, then emit the requested target window.
/// No target samples are muted or dropped. Packet count is explicit, not inferred from a time guess.
pub fn decode_with_preroll(
    input: &[u8],
    max_samples: usize,
    preroll_packets: usize,
) -> Result<Decoded, String> {
    decode_window(
        &mut DtsSession::new(),
        input,
        max_samples,
        preroll_packets,
        false,
    )
}

/// Retains decoder history between complete, bounded packet batches.
pub struct DtsSession {
    core: PcmDecoder,
    hd: HdDecoder,
    format: Option<(u32, Vec<&'static str>, bool)>,
    failed: bool,
    heights: bool,
    // Set by the first frame that carried the 7.1.4 height presentation. Until then a stream that does not carry it (IMAX or
    // alternate DTS:X profiles, another rate, no lossless extension) falls back to the bed instead of failing the track.
    heights_seen: bool,
}

impl DtsSession {
    pub fn with_heights() -> Self {
        Self { heights: true, ..Self::new() }
    }

    pub fn new() -> Self {
        Self {
            core: PcmDecoder::new(),
            hd: HdDecoder::new(),
            format: None,
            failed: false,
            heights: false,
            heights_seen: false,
        }
    }

    pub fn push(&mut self, input: &[u8], max_samples: usize) -> Result<Decoded, String> {
        if self.failed {
            return Err("DTS session failed; reset required".into());
        }
        let result = decode_window(self, input, max_samples, 0, true).and_then(|output| {
            let hd = output.hd_packets != 0;
            if hd && output.hd_packets != output.packets {
                return Err("DTS Core/HD mode changed within a batch".into());
            }
            let format = (output.sample_rate, output.channel_labels.clone(), hd);
            if output.pcm.is_empty() {
                return Ok(output);
            }
            if self
                .format
                .as_ref()
                .is_some_and(|previous| previous != &format)
            {
                return Err("layout, rate or Core/HD mode changed between DTS batches".into());
            }
            self.format = Some(format);
            Ok(output)
        });
        self.failed = result.is_err();
        result
    }
}

fn decode_window(
    session: &mut DtsSession,
    input: &[u8],
    max_samples: usize,
    preroll_packets: usize,
    complete: bool,
) -> Result<Decoded, String> {
    if preroll_packets > 128 {
        return Err("prototype preroll limit128 packets".into());
    }
    if input.is_empty() || input.len() > MAX_INPUT {
        return Err("input must be1..4MiB".into());
    }
    if max_samples == 0 || max_samples > MAX_SAMPLES {
        return Err("sample limit must be1..48000".into());
    }
    let mut output = Decoded {
        sample_rate: 0,
        channel_labels: vec![],
        pcm: vec![],
        integer24: Some(vec![]),
        packets: 0,
        pending: 0,
        hd_packets: 0,
        x_extension_frames: 0,
        x_feed_max: 0,
        preroll_packets: 0,
        preroll_sample_frames: 0,
    };
    let mut cursor = 0;
    while cursor < input.len() {
        if cursor + 4 > input.len() {
            return Err("truncated packet length".into());
        }
        let size = u32::from_le_bytes(input[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        if size == 0 || cursor.checked_add(size).is_none_or(|end| end > input.len()) {
            return Err("invalid packet bounds".into());
        }
        let packet = &input[cursor..cursor + size];
        cursor += size;
        output.packets += 1;
        if output.packets > 256 {
            return Err("prototype packet limit256".into());
        }
        let info = parse_header(packet).map_err(|e| format!("core header:{e:?}"))?;
        if info.frame_size > packet.len() {
            return Err("core length beyond packet".into());
        }
        let mut exss_offset = info.frame_size;
        while exss_offset + 4 <= packet.len() && packet[exss_offset] == 0 {
            exss_offset += 1;
        }
        let has_exss = packet.get(exss_offset..exss_offset + 4) == Some(&[0x64, 0x58, 0x20, 0x25]);
        let (rate, labels, planes, ints) = if has_exss {
            output.hd_packets += 1;
            let frame = match session
                .hd
                .decode(&packet[..info.frame_size], &packet[exss_offset..])
            {
                Ok(f) => f,
                Err(HdError::Pending) => {
                    output.pending += 1;
                    continue;
                }
                Err(e) => return Err(format!("HD decode:{e:?}")),
            };
            if !frame.lossless {
                // DTS-HD High Resolution Audio: the core plus XXCH channels, lossy. A failed
                // XXCH component would silently drop channels, so it is an error here.
                if let Some(why) = frame.xxch_decode_error {
                    return Err(format!("HRA XXCH component failed: {why}"));
                }
                if let Some(why) = frame.x_decode_error {
                    return Err(format!("HRA extension failed: {why}"));
                }
            }
            output.x_extension_frames += usize::from(frame.x_present || frame.x_imax);
            output.x_feed_max = output.x_feed_max.max(frame.x_samples.len());
            let order: Vec<_> = WAVE_DCA
                .iter()
                .filter(|(speaker, _)| frame.output_mask & (1 << speaker) != 0)
                .copied()
                .collect();
            let recognized = order
                .iter()
                .fold(0u32, |mask, (speaker, _)| mask | (1 << speaker));
            if recognized != frame.output_mask {
                return Err(format!("unsupported bed mask{:x}", frame.output_mask));
            }
            let mut labels = order.iter().map(|(_, name)| *name).collect::<Vec<_>>();
            let mut planes = order
                .iter()
                .map(|(speaker, _)| {
                    frame.samples[*speaker]
                        .as_ref()
                        .cloned()
                        .ok_or_else(|| format!("missing bed plane for speaker {speaker}, mask {:x} coded {:x} rate {} planes {:?}", frame.output_mask, frame.coded_mask, frame.sample_rate, frame.samples.iter().map(|p| p.as_ref().map(|v| v.len())).collect::<Vec<_>>()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if session.heights
                && !session.heights_seen
                && (frame.sample_rate != 48000
                    || order.len() != 8
                    || !frame.x_present
                    || frame.x_imax
                    || XPresentation::detect(&frame) != Some(XPresentation::Height))
            {
                session.heights = false;
            }
            if session.heights {
                if frame.sample_rate != 48000
                    || order.len() != 8
                    || !frame.x_present
                    || frame.x_imax
                    || XPresentation::detect(&frame) != Some(XPresentation::Height)
                {
                    return Err("standard DTS:X 7.1.4 height presentation required".into());
                }
                session.heights_seen = true;
                // Match the standard presentation's Q15 height fold used by
                // the pinned decoder's reference frontend.
                let fold = FoldPlan::standard_heights(23_170.0 / 32_768.0);
                for ((speaker, _), plane) in order.iter().zip(&mut planes) {
                    for (sample, bed) in plane.iter_mut().enumerate() {
                        *bed = fold.clean(*speaker, *bed, sample, &frame.x_samples);
                    }
                }
                labels.extend(["TFL", "TFR", "TBL", "TBR"]);
                planes.extend(frame.x_samples);
                output.integer24 = None;
                (frame.sample_rate, labels, planes, None)
            } else if !frame.lossless {
                output.integer24 = None;
                (frame.sample_rate, labels, planes, None)
            } else {
                let integer = session
                    .hd
                    .lossless_samples()
                    .collect::<std::collections::HashMap<_, _>>();
                let ints = order
                    .iter()
                    .map(|(speaker, _)| {
                        integer
                            .get(speaker)
                            .map(|values| values.to_vec())
                            .ok_or("missing lossless integer tap".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (frame.sample_rate, labels, planes, Some(ints))
            }
        } else {
            if session.heights && session.heights_seen {
                return Err("DTS:X height mode requires a lossless extension".into());
            }
            session.heights = false;
            if packet[info.frame_size..].iter().any(|&b| b != 0) {
                return Err("nonempty extension not recognized; refusing core-only drop".into());
            }
            let frame = session
                .core
                .push_access_unit(&packet[..info.frame_size])
                .map_err(|e| format!("core decode:{e:?}"))?
                .pcm;
            let mut labels = frame
                .fullband_channel_order
                .iter()
                .copied()
                .map(channel_label)
                .collect::<Vec<_>>();
            let mut planes = frame.fullband_channels;
            if let Some(lfe) = frame.lfe_channel {
                labels.push("LFE");
                planes.push(lfe);
            }
            output.integer24 = None;
            (frame.sample_rate, labels, planes, None)
        };
        if output.sample_rate == 0 {
            output.sample_rate = rate;
            output.channel_labels = labels.clone();
        }
        if output.sample_rate != rate || output.channel_labels != labels {
            return Err("layout/rate changed in bounded experiment".into());
        }
        if planes.is_empty() || planes.iter().any(|p| p.len() != planes[0].len()) {
            return Err("inconsistent PCM plane lengths".into());
        }
        if let Some(ref values) = ints {
            if values.len() != planes.len() || values.iter().any(|p| p.len() != planes[0].len()) {
                return Err("inconsistent integer tap".into());
            }
        }
        if output.packets <= preroll_packets {
            output.preroll_packets += 1;
            output.preroll_sample_frames += planes[0].len();
            continue;
        }
        let remaining = max_samples - output.pcm.len() / labels.len();
        let frames = remaining.min(planes[0].len());
        if complete && frames != planes[0].len() {
            return Err("DTS batch exceeds its PCM frame limit".into());
        }
        for i in 0..frames {
            for plane in &planes {
                output.pcm.push(plane[i]);
            }
        }
        if let (Some(out), Some(values)) = (output.integer24.as_mut(), ints) {
            for i in 0..frames {
                for plane in &values {
                    out.push(plane[i]);
                }
            }
        }
        if output.pcm.len() / labels.len() >= max_samples {
            break;
        }
    }
    // A batch made only of HD packets that still wait for their next frame is not an error.
    if output.pcm.is_empty() && output.pending == 0 {
        return Err("no PCM output".into());
    }
    if complete && cursor != input.len() {
        return Err("DTS batch exceeds its PCM frame limit".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_packet_bounds_rejected() {
        assert!(decode(&[255, 255, 255, 255], 48000).is_err());
        assert!(decode(&[], 1).is_err());
    }
}
