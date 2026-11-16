//! Stream parameters read from the first access units: picture size, codec strings, audio layout.

use super::frame::{self, Kind};
use crate::util::{Bits, unescape};
use crate::{TrackInfo, TrackKind};

fn nals(es: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let n = starts.len();
    (0..n).map(move |k| {
        let end = if k + 1 < n { starts[k + 1] - 3 } else { es.len() };
        &es[starts[k]..end]
    })
}

fn skip_scaling_list(r: &mut Bits<'_>, size: usize) {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se() + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
}

/// Returns true when the parameters were found.
pub(super) fn probe(kind: Kind, es: &[u8], info: &mut TrackInfo) -> bool {
    match kind {
        Kind::H264 => h264(es, info),
        Kind::Hevc => hevc(es, info),
        Kind::Mpeg2 => mpeg2(es, info),
        Kind::Vc1 => vc1(es, info),
        Kind::Ac3 => ac3(es, info),
        Kind::TrueHd => truehd(es, info),
        Kind::Dts => dts(es, info),
        Kind::Aac => aac(es, info),
        Kind::Plain => info.codec == "pcm_bluray" && lpcm(es, info) || info.kind == TrackKind::Subtitle,
    }
}

fn h264(es: &[u8], info: &mut TrackInfo) -> bool {
    for n in nals(es) {
        if n.first().map(|b| b & 31) != Some(7) || n.len() < 5 {
            continue;
        }
        let rbsp = unescape(&n[1..]);
        let mut r = Bits::new(&rbsp);
        let profile = r.bits(8);
        let constraints = r.bits(8);
        let level = r.bits(8);
        let _id = r.ue();
        let mut chroma = 1;
        if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
            chroma = r.ue();
            if chroma == 3 {
                r.skip(1);
            }
            let _ = (r.ue(), r.ue());
            r.skip(1);
            if r.bit() == 1 {
                for i in 0..(if chroma == 3 { 12 } else { 8 }) {
                    if r.bit() == 1 {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 });
                    }
                }
            }
        }
        let _log2_frame_num = r.ue();
        match r.ue() {
            0 => {
                let _ = r.ue();
            }
            1 => {
                r.skip(1);
                let _ = (r.se(), r.se());
                for _ in 0..r.ue().min(255) {
                    let _ = r.se();
                }
            }
            _ => {}
        }
        let _refs = r.ue();
        r.skip(1);
        let w_mbs = r.ue() + 1;
        let h_map = r.ue() + 1;
        let frame_mbs_only = r.bit();
        if frame_mbs_only == 0 {
            r.skip(1);
        }
        r.skip(1);
        let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
        if r.bit() == 1 {
            cl = r.ue();
            cr = r.ue();
            ct = r.ue();
            cb = r.ue();
        }
        if r.overrun {
            return false;
        }
        let (sub_w, sub_h) = match chroma {
            0 => (1, 1),
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let unit_y = sub_h * (2 - frame_mbs_only);
        info.width = Some((w_mbs * 16).saturating_sub((cl + cr) * sub_w));
        info.height = Some((h_map * 16 * (2 - frame_mbs_only)).saturating_sub((ct + cb) * unit_y));
        info.codec_string = Some(format!("avc1.{profile:02X}{constraints:02X}{level:02X}"));
        // VUI timing for the default frame duration.
        if r.bit() == 1 {
            if r.bit() == 1 && r.bits(8) == 255 {
                r.skip(32);
            }
            if r.bit() == 1 {
                r.skip(1);
            }
            if r.bit() == 1 {
                r.skip(4);
                if r.bit() == 1 {
                    r.skip(24);
                }
            }
            if r.bit() == 1 {
                let _ = (r.ue(), r.ue());
            }
            if r.bit() == 1 {
                let units = r.bits(32);
                let scale = r.bits(32);
                if !r.overrun && units > 0 && scale > 0 {
                    info.default_duration_ns = Some((u64::from(units) * 2 * 1_000_000_000).div_ceil(u64::from(scale)));
                }
            }
        }
        return true;
    }
    false
}

fn hevc(es: &[u8], info: &mut TrackInfo) -> bool {
    for n in nals(es) {
        if n.len() < 20 || (n[0] >> 1) & 63 != 33 {
            continue;
        }
        let rbsp = unescape(&n[2..]);
        let mut r = Bits::new(&rbsp);
        r.skip(4);
        let sub_layers_minus1 = r.bits(3);
        r.skip(1);
        let b1 = r.bits(8) as u8; // profile space, tier, profile idc
        let compat = r.bits(32);
        let mut constraint = [0u8; 6];
        for c in &mut constraint {
            *c = r.bits(8) as u8;
        }
        let level = r.bits(8) as u8;
        let mut present = Vec::new();
        for _ in 0..sub_layers_minus1 {
            present.push((r.bit() == 1, r.bit() == 1));
        }
        if sub_layers_minus1 > 0 {
            for _ in sub_layers_minus1..8 {
                r.skip(2);
            }
        }
        for (profile, lvl) in present {
            if profile {
                r.skip(88);
            }
            if lvl {
                r.skip(8);
            }
        }
        let _sps_id = r.ue();
        let chroma = r.ue();
        if chroma == 3 {
            r.skip(1);
        }
        let w = r.ue();
        let h = r.ue();
        let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
        if r.bit() == 1 {
            cl = r.ue();
            cr = r.ue();
            ct = r.ue();
            cb = r.ue();
        }
        if r.overrun {
            return false;
        }
        let (sw, sh) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        info.width = Some(w.saturating_sub((cl + cr) * sw));
        info.height = Some(h.saturating_sub((ct + cb) * sh));
        let mut c = [0u8; 13];
        c[0] = 1;
        c[1] = b1;
        c[2..6].copy_from_slice(&compat.to_be_bytes());
        c[6..12].copy_from_slice(&constraint);
        c[12] = level;
        info.codec_string = hevc_string("hev1", &c);
        return true;
    }
    false
}

fn hevc_string(entry: &str, c: &[u8]) -> Option<String> {
    if c.len() < 13 {
        return None;
    }
    let space = match c[1] >> 6 {
        0 => "",
        1 => "A",
        2 => "B",
        _ => "C",
    };
    let compat = u32::from_be_bytes([c[2], c[3], c[4], c[5]]).reverse_bits();
    let tier = if c[1] & 0x20 != 0 { 'H' } else { 'L' };
    let mut s = format!("{entry}.{space}{}.{compat:X}.{tier}{}", c[1] & 31, c[12]);
    let mut cons = &c[6..12];
    while let [rest @ .., 0] = cons {
        cons = rest;
    }
    for b in cons {
        s.push_str(&format!(".{b:02X}"));
    }
    Some(s)
}

fn mpeg2(es: &[u8], info: &mut TrackInfo) -> bool {
    let mut i = 0;
    while i + 8 <= es.len() {
        if es[i..i + 4] == [0, 0, 1, 0xb3] {
            let w = (u32::from(es[i + 4]) << 4) | u32::from(es[i + 5] >> 4);
            let h = (u32::from(es[i + 5] & 15) << 8) | u32::from(es[i + 6]);
            info.width = Some(w);
            info.height = Some(h);
            const FPS: [(u64, u64); 9] = [(0, 1), (24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (50, 1), (60000, 1001), (60, 1)];
            if let Some(&(n, d)) = FPS.get(usize::from(es[i + 7] & 15)).filter(|f| f.0 > 0) {
                info.default_duration_ns = Some((1_000_000_000 * d).div_ceil(n));
            }
            return true;
        }
        i += 1;
    }
    false
}

fn vc1(es: &[u8], info: &mut TrackInfo) -> bool {
    let mut i = 0;
    while i + 12 <= es.len() {
        if es[i..i + 4] == [0, 0, 1, 0x0f] {
            let rbsp = unescape(&es[i + 4..(i + 40).min(es.len())]);
            let mut r = Bits::new(&rbsp);
            let profile = r.bits(2);
            if profile == 3 {
                r.skip(3 + 2 + 3 + 5 + 1);
                let w = (r.bits(12) + 1) * 2;
                let h = (r.bits(12) + 1) * 2;
                info.width = Some(w);
                info.height = Some(h);
            }
            info.codec_string = Some("vc-1".into());
            return true;
        }
        i += 1;
    }
    false
}

fn ac3(es: &[u8], info: &mut TrackInfo) -> bool {
    let Some(i) = (0..es.len().saturating_sub(8)).find(|&i| frame::ac3_header(&es[i..]).is_some()) else { return false };
    let h = frame::ac3_header(&es[i..]).unwrap();
    info.sample_rate = Some(h.rate);
    info.channels = Some(h.channels);
    info.codec_string = Some(if info.codec == "eac3" || h.eac3 { "ec-3" } else { "ac-3" }.into());
    if info.codec == "eac3" {
        // Atmos / 7.1 streams carry the extra channels in the dependent substream.
        let mut at = i + h.size;
        while at + 8 <= es.len() {
            let Some(d) = frame::ac3_header(&es[at..]) else { break };
            if d.eac3 && d.strmtyp == 1 {
                info.channels = Some(h.channels + dep_extra_channels(&d));
                at += d.size;
            } else {
                break;
            }
        }
    }
    true
}

/// Channels added by a dependent substream, from its channel map (ETSI TS 102 366 table E.1.6).
pub(crate) fn dep_extra_channels(d: &frame::Ac3Header) -> u32 {
    let Some(map) = d.chanmap else { return 0 };
    // Bit 15 is the first entry. Only the entries that add speakers beyond the independent frame count.
    const COUNT: [u32; 16] = [0, 0, 0, 0, 0, 2, 2, 1, 1, 2, 2, 2, 1, 1, 0, 0];
    (0..16).filter(|i| map & (0x8000 >> i) != 0).map(|i| COUNT[i]).sum()
}

fn truehd(es: &[u8], info: &mut TrackInfo) -> bool {
    let mut i = 0;
    while i + 12 <= es.len() {
        let len = ((usize::from(es[i] & 15) << 8) | usize::from(es[i + 1])) * 2;
        if len >= 8 && es[i + 4..i + 8] == [0xf8, 0x72, 0x6f, 0xba] {
            let ratebits = u32::from(es[i + 8] >> 4);
            info.sample_rate = Some((if ratebits & 8 != 0 { 44_100 } else { 48_000 }) << (ratebits & 7));
            // format_info (32 bits after the sync): rate(4) types(2) reserved(2) modifiers(2+2)
            // 6ch assignment(5) 8ch modifier(2) 8ch assignment(13).
            let bits40 = u64::from_be_bytes([0, 0, 0, 0, es[i + 8], es[i + 9], es[i + 10], es[i + 11]]);
            let chan6 = ((bits40 >> (32 - 17)) & 0x1f) as u32;
            let chan8 = (bits40 & 0x1fff) as u32;
            let map = if chan8 != 0 { chan8 } else { chan6 };
            const COUNT: [u32; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 2, 1];
            info.channels = Some((0..13).filter(|b| map >> b & 1 == 1).map(|b| COUNT[b]).sum());
            info.codec_string = Some("mlpa".into());
            return true;
        }
        if len < 8 || i + len + 12 > es.len() {
            return false;
        }
        i += len;
    }
    false
}

fn dts(es: &[u8], info: &mut TrackInfo) -> bool {
    let Some(i) = es.windows(4).position(|w| w == [0x7f, 0xfe, 0x80, 0x01]) else { return false };
    let Some(core) = frame::dts_core(&es[i..]) else { return false };
    info.sample_rate = Some(core.rate);
    info.channels = Some(core.channels);
    // Extension substreams follow the core: LBR, XLL (lossless), XBR/X96/XXCH (high resolution).
    let tail = &es[(i + core.size).min(es.len())..];
    let has = |sync: [u8; 4]| tail.windows(4).any(|w| w == sync);
    let s = if !tail.starts_with(&frame::EXSS_SYNC) {
        "dtsc"
    } else if has([0x41, 0xa2, 0x95, 0x47]) {
        "dtsl"
    } else if has([0x0a, 0x80, 0x19, 0x21]) {
        "dtse"
    } else {
        "dtsh"
    };
    info.codec_string = Some(s.into());
    // The core header says 5.1 even when an XLL/XXCH extension carries a 7.1 mix: the extension
    // substream header states the real channel count of the asset.
    let ext = tail.iter().position(|&b| b != 0).map_or(&tail[..0], |p| &tail[p..]);
    if let Some(total) = exss_total_channels(ext).filter(|&n| n > core.channels) {
        info.channels = Some(total);
    }
    true
}

/// Channels of the first audio asset of an extension substream (ETSI TS 102 114 "nuTotalNumChs"),
/// from the static fields of the header. `None` when the header carries no static fields or is cut.
pub(crate) fn exss_total_channels(b: &[u8]) -> Option<u32> {
    if b.len() < 10 || b[..4] != frame::EXSS_SYNC {
        return None;
    }
    let mut r = Bits::new(&b[4..]);
    r.skip(8);
    let exss_index = r.bits(2);
    let wide = r.bits(1);
    r.skip(if wide == 1 { 12 } else { 8 });
    let size_bits = if wide == 1 { 20 } else { 16 };
    r.skip(size_bits);
    if r.bit() == 0 {
        return None; // no static fields: the channel count lives in an earlier substream
    }
    r.skip(2 + 3);
    if r.bit() == 1 {
        r.skip(36);
    }
    let presentations = r.bits(3) + 1;
    let assets = r.bits(3) + 1;
    let mut active = 0u32;
    for _ in 0..presentations {
        active += r.bits(exss_index + 1).count_ones();
    }
    r.skip(active as usize * 8);
    if r.bit() == 1 {
        r.skip(2);
        let mask_bits = (r.bits(2) + 1) * 4;
        let configs = r.bits(2) + 1;
        for _ in 0..configs {
            r.skip(mask_bits as usize);
        }
    }
    r.skip(size_bits as usize * assets as usize);
    r.skip(9 + 3); // descriptor size, asset index
    if r.bit() == 1 {
        r.skip(4);
    }
    if r.bit() == 1 {
        r.skip(24);
    }
    if r.bit() == 1 {
        let text = r.bits(10) as usize + 1;
        r.skip(text * 8);
    }
    r.skip(5 + 4);
    let total = r.bits(8) + 1;
    (!r.overrun).then_some(total)
}

fn aac(es: &[u8], info: &mut TrackInfo) -> bool {
    let Some(i) = (0..es.len().saturating_sub(7)).find(|&i| frame::adts_header(&es[i..]).is_some()) else { return false };
    let (_, rate, ch, pb) = frame::adts_header(&es[i..]).unwrap();
    info.sample_rate = Some(rate);
    info.channels = Some(ch);
    info.codec_string = Some(format!("mp4a.40.{}", pb / 1000));
    true
}

/// HDMV LPCM: 16 bit frame length, channel assignment and rate nibbles, bits per sample.
fn lpcm(es: &[u8], info: &mut TrackInfo) -> bool {
    if es.len() < 4 {
        return false;
    }
    info.codec_private = es[..4].to_vec();
    let ch = match es[2] >> 4 {
        1 => 1,
        3 => 2,
        4 | 5 => 3,
        6 | 7 => 4,
        8 => 5,
        9 => 6,
        10 => 7,
        11 => 8,
        _ => 0,
    };
    info.channels = Some(ch).filter(|&c| c > 0);
    info.sample_rate = match es[2] & 15 {
        1 => Some(48_000),
        4 => Some(96_000),
        5 => Some(192_000),
        _ => None,
    };
    info.bit_depth = match es[3] >> 6 {
        1 => Some(16),
        2 => Some(20),
        3 => Some(24),
        _ => None,
    };
    info.codec_string = Some("pcm-s16be".into()).filter(|_| info.bit_depth == Some(16));
    true
}
