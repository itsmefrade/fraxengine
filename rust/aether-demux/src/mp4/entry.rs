//! Sample entries (`stsd`) and the codec configuration boxes inside them.
//!
//! Every parser here works on an already bounded byte slice and returns `Err`
//! instead of panicking. `codec_private` is always the payload of the
//! configuration box exactly as stored (full-box version and flags included
//! where the box has them): `hvcC`, `avcC`, `av1C`, `vpcC`, `dOps`, `dfLa`,
//! `dac3`, `dec3`, `dmlp`, `ddts`/`udts`. For `mp4a` it is the
//! AudioSpecificConfig (the `esds` DecoderSpecificInfo), not the whole `esds`.

use super::{Bx, R, arr, boxes, find, u16, u32};
use crate::{ColorInfo, DolbyVisionRecord, TrackInfo, TrackKind};

pub(super) struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub(super) fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }
    pub(super) fn read(&mut self, n: usize) -> Option<u32> {
        if n > 32 || self.pos + n > self.b.len() * 8 {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            let bit = (self.b[self.pos / 8] >> (7 - self.pos % 8)) & 1;
            v = (v << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(v)
    }
    pub(super) fn left(&self) -> usize {
        self.b.len() * 8 - self.pos
    }
}

const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

struct Asc {
    aot: u32,
    rate: Option<u32>,
    channels: Option<u32>,
}

fn parse_asc(b: &[u8]) -> Option<Asc> {
    let mut r = Bits::new(b);
    let mut aot = r.read(5)?;
    if aot == 31 {
        aot = 32 + r.read(6)?;
    }
    let first = aot;
    let rate_of = |r: &mut Bits| -> Option<Option<u32>> {
        let idx = r.read(4)?;
        Some(if idx == 15 { Some(r.read(24)?) } else { AAC_RATES.get(idx as usize).copied() })
    };
    let rate = rate_of(&mut r)?;
    let cfg = r.read(4)?;
    let channels = match cfg {
        1..=6 => Some(cfg),
        7 => Some(8),
        _ => None,
    };
    let rate = if first == 5 || first == 29 { rate_of(&mut r).flatten().or(rate) } else { rate };
    Some(Asc { aot: first, rate, channels })
}

/// MPEG-4 descriptor length: up to four bytes of seven bits each.
fn desc_len(b: &[u8], p: &mut usize) -> R<usize> {
    let mut len = 0usize;
    for _ in 0..4 {
        let byte = *b.get(*p).ok_or("truncated descriptor")?;
        *p += 1;
        len = (len << 7) | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok(len);
        }
    }
    Err("descriptor length".into())
}

struct Esds {
    oti: u8,
    dsi: Vec<u8>,
}

fn parse_esds(body: &[u8]) -> R<Esds> {
    let mut p = 4usize;
    let mut oti = None;
    let mut dsi = Vec::new();
    // Walk the descriptor tree iteratively: 0x03 and 0x04 are containers (skip their fixed
    // fields and descend), 0x05 carries the DecoderSpecificInfo, anything else is skipped.
    let mut guard = 0;
    while p < body.len() && guard < 32 {
        guard += 1;
        let tag = body[p];
        p += 1;
        let len = desc_len(body, &mut p)?;
        let end = p.checked_add(len).filter(|&e| e <= body.len()).ok_or("descriptor beyond esds")?;
        match tag {
            0x03 => {
                let flags = *body.get(p + 2).ok_or("ES descriptor")?;
                p += 3;
                if flags & 0x80 != 0 {
                    p += 2;
                }
                if flags & 0x40 != 0 {
                    let n = usize::from(*body.get(p).ok_or("ES URL")?);
                    p += 1 + n;
                }
                if flags & 0x20 != 0 {
                    p += 2;
                }
            }
            0x04 => {
                oti = Some(*body.get(p).ok_or("decoder config")?);
                p += 13;
            }
            0x05 => {
                dsi = body[p..end].to_vec();
                break;
            }
            _ => p = end,
        }
    }
    Ok(Esds { oti: oti.ok_or("esds without decoder config")?, dsi })
}

fn find_cfg<'a>(kids: &[Bx<'a>], kind: &[u8; 4], budget: &mut u32) -> R<Option<Bx<'a>>> {
    if let Some(b) = find(kids, kind) {
        return Ok(Some(b));
    }
    // QuickTime wraps esds/enda/frma in a `wave` atom.
    if let Some(wave) = find(kids, b"wave") {
        let inner = boxes(wave.body, budget)?;
        return Ok(find(&inner, kind));
    }
    Ok(None)
}

pub(super) fn hevc_string(entry: &str, c: &[u8]) -> Option<String> {
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
    // At least one constraint byte stays (`.00`), like the JS side and common muxers.
    while let [rest @ .., 0] = cons {
        if rest.is_empty() {
            break;
        }
        cons = rest;
    }
    for b in cons {
        s.push_str(&format!(".{b:02X}"));
    }
    Some(s)
}

fn dv_string(entry: &str, dv: &[u8]) -> Option<String> {
    if dv.len() < 4 {
        return None;
    }
    let profile = dv[2] >> 1;
    let level = ((dv[2] & 1) << 5) | (dv[3] >> 3);
    // The sample entry may be the plain base-layer one (`hvc1` with a dvcC box): the Dolby Vision
    // string always uses a Dolby Vision four-cc, `dvh1` for out-of-band parameter sets, `dvhe` for
    // in-band ones, `dva1`/`dvav` for AVC and `dav1` for AV1.
    let prefix = match entry {
        "hvc1" | "dvh1" => "dvh1",
        "hev1" | "dvhe" => "dvhe",
        "avc1" | "dva1" => "dva1",
        "avc3" | "dvav" => "dvav",
        "av01" | "dav1" => "dav1",
        other => other,
    };
    Some(format!("{prefix}.{profile:02}.{level:02}"))
}

fn av1_string(c: &[u8]) -> Option<String> {
    if c.len() < 4 {
        return None;
    }
    let profile = c[1] >> 5;
    let level = c[1] & 31;
    let tier = if c[2] & 0x80 != 0 { 'H' } else { 'M' };
    let depth = if c[2] & 0x40 == 0 { 8 } else if c[2] & 0x20 != 0 { 12 } else { 10 };
    Some(format!("av01.{profile}.{level:02}{tier}.{depth:02}"))
}

fn vp9_string(c: &[u8]) -> Option<String> {
    // vpcC is a full box: version/flags, profile, level, depth|chroma|range, primaries,
    // transfer, matrix.
    if c.len() < 12 {
        return None;
    }
    let (profile, level, packed) = (c[4], c[5], c[6]);
    Some(format!(
        "vp09.{profile:02}.{level:02}.{:02}.{:02}.{:02}.{:02}.{:02}.{:02}",
        packed >> 4,
        (packed >> 1) & 7,
        c[7],
        c[8],
        c[9],
        packed & 1
    ))
}

/// `colr` (nclx, or QuickTime's nclc without the range byte), `mdcv` and `clli` of a visual sample entry.
fn colour(kids: &[Bx<'_>]) -> Option<ColorInfo> {
    let mut c = ColorInfo::default();
    let nclx = kids.iter().find(|k| &k.kind == b"colr" && k.body.len() >= 10 && matches!(&k.body[..4], b"nclx" | b"nclc"));
    if let Some(k) = nclx {
        let field = |at: usize| u64::from(u16::from_be_bytes([k.body[at], k.body[at + 1]]));
        c.primaries = ColorInfo::code(field(4), false);
        c.transfer = ColorInfo::code(field(6), false);
        c.matrix = ColorInfo::code(field(8), true);
        if &k.body[..4] == b"nclx" {
            c.full_range = k.body.get(10).map(|b| b & 0x80 != 0);
        }
    }
    if let Some(k) = find(kids, b"mdcv").filter(|k| k.body.len() >= 24) {
        let b = k.body;
        let be16 = |i: usize| u32::from(u16::from_be_bytes([b[i], b[i + 1]]));
        let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        c.mastering = Some([be16(0), be16(2), be16(4), be16(6), be16(8), be16(10), be16(12), be16(14), be32(16), be32(20)]);
    }
    if let Some(k) = find(kids, b"clli").filter(|k| k.body.len() >= 4) {
        c.content_light = Some((u16::from_be_bytes([k.body[0], k.body[1]]), u16::from_be_bytes([k.body[2], k.body[3]])));
    }
    (!c.is_empty()).then_some(c)
}

fn video(ent: &Bx<'_>, info: &mut TrackInfo, budget: &mut u32) -> R<()> {
    let b = ent.body;
    if b.len() < 78 {
        return Err("short visual sample entry".into());
    }
    let (w, h) = (u16(b, 24)?, u16(b, 26)?);
    if w != 0 {
        info.width = Some(u32::from(w));
    }
    if h != 0 {
        info.height = Some(u32::from(h));
    }
    let kids = boxes(&b[78..], budget)?;
    info.color = colour(&kids);
    let entry = String::from_utf8_lossy(&ent.kind).into_owned();
    let cfg_kind: Option<&[u8; 4]> = match &ent.kind {
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => Some(b"hvcC"),
        b"avc1" | b"avc3" | b"dvav" | b"dva1" => Some(b"avcC"),
        b"av01" | b"dav1" => Some(b"av1C"),
        b"vp09" => Some(b"vpcC"),
        _ => None,
    };
    let dv = find(&kids, b"dvcC").map(|c| (*b"dvcC", c)).or_else(|| find(&kids, b"dvvC").map(|c| (*b"dvvC", c)));
    if let Some((box_type, c)) = dv {
        info.dolby_vision = Some(DolbyVisionRecord { box_type, bytes: c.body.to_vec() });
    }
    if let Some(k) = cfg_kind {
        let c = find(&kids, k).ok_or("missing codec configuration box")?;
        if c.body.len() > 1 << 20 {
            return Err("codec configuration too large".into());
        }
        info.codec_private = c.body.to_vec();
        let cp = &info.codec_private;
        match &ent.kind {
            b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => {
                if cp.len() < 23 || cp[0] != 1 {
                    return Err("invalid hvcC".into());
                }
                info.bit_depth = Some(u32::from(cp[17] & 7) + 8);
                let base = match &ent.kind {
                    b"dvh1" => "hvc1",
                    b"dvhe" => "hev1",
                    _ => entry.as_str(),
                };
                let s = hevc_string(base, cp);
                if let Some(d) = info.dolby_vision.as_ref().and_then(|d| dv_string(&entry, &d.bytes)) {
                    info.codec_string = Some(d);
                    info.fallback_codec_string = s;
                } else {
                    info.codec_string = s;
                }
                info.dolby_vision_el = find(&kids, b"hvcE").map(|c| c.body.to_vec());
            }
            b"avc1" | b"avc3" | b"dvav" | b"dva1" => {
                if cp.len() < 7 || cp[0] != 1 {
                    return Err("invalid avcC".into());
                }
                let base = if &ent.kind == b"avc3" { "avc3" } else { "avc1" };
                let s = format!("{base}.{:02X}{:02X}{:02X}", cp[1], cp[2], cp[3]);
                if let Some(d) = info.dolby_vision.as_ref().and_then(|d| dv_string(&entry, &d.bytes)) {
                    info.codec_string = Some(d);
                    info.fallback_codec_string = Some(s);
                } else {
                    info.codec_string = Some(s);
                }
            }
            b"av01" | b"dav1" => {
                let s = av1_string(cp);
                info.bit_depth = s.as_ref().and_then(|s| s.rsplit('.').next()?.parse().ok());
                if let Some(d) = info.dolby_vision.as_ref().and_then(|d| dv_string(&entry, &d.bytes)) {
                    info.codec_string = Some(d);
                    info.fallback_codec_string = s;
                } else {
                    info.codec_string = s;
                }
            }
            _ => {
                info.codec_string = vp9_string(cp);
                info.bit_depth = cp.get(6).map(|p| u32::from(p >> 4));
            }
        }
    } else if &ent.kind == b"vp08" {
        info.codec_string = Some("vp8".into());
    }
    Ok(())
}

struct Dec3 {
    channels: u32,
    sample_rate: u32,
    joc: Option<u8>,
}

const AC3_RATES: [u32; 3] = [48000, 44100, 32000];
const ACMOD_CH: [u32; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

fn parse_dec3(b: &[u8]) -> Option<Dec3> {
    let mut r = Bits::new(b);
    r.read(13)?;
    let subs = r.read(3)? + 1;
    let (mut channels, mut sample_rate) = (0, 0);
    for i in 0..subs {
        let fscod = r.read(2)?;
        r.read(5 + 1 + 1 + 3)?;
        let acmod = r.read(3)?;
        let lfe = r.read(1)?;
        r.read(3)?;
        let dep = r.read(4)?;
        let loc = if dep > 0 { r.read(9)? } else { r.read(1)? & 0 };
        if i == 0 {
            sample_rate = AC3_RATES.get(fscod as usize).copied().unwrap_or(0);
            channels += ACMOD_CH[acmod as usize] + lfe;
        }
        if dep > 0 {
            for (bit, n) in [2u32, 2, 1, 1, 2, 2, 2, 1, 1].into_iter().enumerate() {
                if loc >> bit & 1 != 0 {
                    channels += n;
                }
            }
        }
    }
    // ETSI TS 103 420 extension: 7 reserved bits, flag_ec3_extension_type_a, complexity index.
    let joc = if r.left() >= 16 {
        r.read(7)?;
        if r.read(1)? == 1 { Some(r.read(8)? as u8) } else { None }
    } else {
        None
    };
    Some(Dec3 { channels, sample_rate, joc })
}

fn pcm_string(fourcc: &[u8; 4], depth: u32, flags: Option<u32>, little: Option<bool>, cfg: Option<&[u8]>) -> Option<String> {
    // lpcm formatSpecificFlags: 1 float, 2 big endian, 4 signed integer.
    let (kind, bits, le) = match fourcc {
        b"sowt" => ('s', 16, true),
        b"twos" => ('s', 16, false),
        b"raw " => ('u', 8, true),
        b"in24" => ('s', 24, little.map_or(false, |l| l)),
        b"in32" => ('s', 32, little.map_or(false, |l| l)),
        b"fl32" => ('f', 32, little.map_or(false, |l| l)),
        b"fl64" => ('f', 64, little.map_or(false, |l| l)),
        b"ipcm" => {
            let c = cfg?;
            ('s', u32::from(*c.get(5)?), c.get(4)? & 1 != 0)
        }
        b"lpcm" => {
            let f = flags?;
            (if f & 1 != 0 { 'f' } else if f & 4 != 0 { 's' } else { 'u' }, depth, f & 2 == 0)
        }
        _ => return None,
    };
    let end = if bits <= 8 { "" } else if le { "le" } else { "be" };
    Some(format!("pcm-{kind}{bits}{end}"))
}

fn audio(ent: &Bx<'_>, info: &mut TrackInfo, budget: &mut u32) -> R<()> {
    let b = ent.body;
    if b.len() < 28 {
        return Err("short audio sample entry".into());
    }
    let version = u16(b, 8)?;
    let mut channels = u32::from(u16(b, 16)?);
    let mut depth = u32::from(u16(b, 18)?);
    let mut rate = u32::from(u16(b, 24)?);
    let mut lpcm_flags = None;
    let kids_at = match version {
        0 => 28,
        1 => 44,
        2 => {
            if b.len() < 64 {
                return Err("short audio sample entry v2".into());
            }
            let r = f64::from_be_bytes(arr(b, 32)?);
            rate = if r.is_finite() && r >= 0.0 && r < 4e9 { r as u32 } else { 0 };
            channels = u32::from_be_bytes(arr(b, 40)?);
            depth = u32::from_be_bytes(arr(b, 48)?);
            lpcm_flags = Some(u32::from_be_bytes(arr(b, 52)?));
            64
        }
        _ => return Err("unsupported audio sample entry version".into()),
    };
    let kids = boxes(b.get(kids_at..).ok_or("audio entry children")?, budget)?;
    let mut name = |s: &str| info.codec_string = Some(s.to_owned());
    let mut cfg_body: Vec<u8> = Vec::new();
    let cfg = |kind: &[u8; 4], budget: &mut u32| -> R<Option<Vec<u8>>> {
        Ok(find_cfg(&kids, kind, budget)?.map(|c| c.body.to_vec()))
    };
    match &ent.kind {
        b"mp4a" => {
            let es = cfg(b"esds", budget)?.ok_or("mp4a without esds")?;
            let esds = parse_esds(&es)?;
            cfg_body = esds.dsi;
            match esds.oti {
                0x40 | 0x66 | 0x67 | 0x68 => {
                    let asc = parse_asc(&cfg_body);
                    name(&format!("mp4a.40.{}", asc.as_ref().map_or(2, |a| a.aot)));
                    if let Some(a) = asc {
                        if let Some(c) = a.channels {
                            channels = c;
                        }
                        if rate == 0 {
                            rate = a.rate.unwrap_or(0);
                        }
                    }
                }
                0x69 | 0x6b => name("mp3"),
                other => name(&format!("mp4a.{other:02X}")),
            }
        }
        b"Opus" => {
            cfg_body = cfg(b"dOps", budget)?.ok_or("Opus without dOps")?;
            channels = u32::from(*cfg_body.get(1).ok_or("short dOps")?);
            rate = 48000;
            name("opus");
        }
        b"fLaC" => {
            cfg_body = cfg(b"dfLa", budget)?.ok_or("fLaC without dfLa")?;
            // STREAMINFO is the first metadata block after the full-box header.
            if cfg_body.len() >= 4 + 4 + 34 && cfg_body[4] & 0x7f == 0 {
                let si = &cfg_body[8..];
                let rate20 = (u32::from(si[10]) << 12) | (u32::from(si[11]) << 4) | u32::from(si[12] >> 4);
                let ch = u32::from((si[12] >> 1) & 7) + 1;
                let bps = (u32::from(si[12] & 1) << 4 | u32::from(si[13] >> 4)) + 1;
                if rate20 > 0 {
                    rate = rate20;
                }
                channels = ch;
                depth = bps;
            }
            name("flac");
        }
        b"ac-3" => {
            cfg_body = cfg(b"dac3", budget)?.ok_or("ac-3 without dac3")?;
            if cfg_body.len() >= 3 {
                let v = u32::from(cfg_body[0]) << 16 | u32::from(cfg_body[1]) << 8 | u32::from(cfg_body[2]);
                rate = AC3_RATES.get((v >> 22) as usize).copied().unwrap_or(rate);
                channels = ACMOD_CH[((v >> 15) & 7) as usize] + ((v >> 14) & 1);
            }
            name("ac-3");
        }
        b"ec-3" => {
            cfg_body = cfg(b"dec3", budget)?.ok_or("ec-3 without dec3")?;
            if let Some(d) = parse_dec3(&cfg_body) {
                if d.sample_rate != 0 {
                    rate = d.sample_rate;
                }
                if d.channels != 0 {
                    channels = d.channels;
                }
                info.joc_complexity = d.joc;
            }
            name("ec-3");
        }
        b"mlpa" => {
            cfg_body = cfg(b"dmlp", budget)?.unwrap_or_default();
            if cfg_body.len() >= 4 {
                rate = match cfg_body[0] >> 4 {
                    0 => 48000,
                    1 => 96000,
                    2 => 192000,
                    8 => 44100,
                    9 => 88200,
                    10 => 176400,
                    _ => rate,
                };
            }
            name("mlpa");
        }
        b"dtsc" | b"dtsh" | b"dtsl" | b"dtse" | b"dtsx" => {
            let kind: &[u8; 4] = if &ent.kind == b"dtsx" { b"udts" } else { b"ddts" };
            cfg_body = cfg(kind, budget)?.unwrap_or_default();
            if &ent.kind != b"dtsx" && cfg_body.len() >= 4 {
                let r = u32::from_be_bytes(arr(&cfg_body, 0)?);
                if r != 0 {
                    rate = r;
                }
            }
            // DTS:X / DTS-UHD in MP4 is not known to WebCodecs or ffprobe; the sample entry
            // fourcc stays in `codec` and the raw `udts` box in `codec_private`.
            let fourcc = String::from_utf8_lossy(&ent.kind).into_owned();
            name(if &ent.kind == b"dtsx" { "dtsuhd" } else { &fourcc });
        }
        b"sowt" | b"twos" | b"raw " | b"in24" | b"in32" | b"fl32" | b"fl64" | b"ipcm" | b"lpcm" => {
            let enda = cfg(b"enda", budget)?.map(|e| e.get(1).copied().unwrap_or(0) != 0);
            let pcmc = cfg(b"pcmC", budget)?;
            // `enda` says little endian when set; QuickTime defaults to big endian without it.
            cfg_body = pcmc.clone().unwrap_or_default();
            info.codec_string = pcm_string(&ent.kind, depth, lpcm_flags, enda.or(Some(false)), pcmc.as_deref());
            if let Some(s) = &info.codec_string {
                if let Some(bits) = s[5..].trim_end_matches("le").trim_end_matches("be").parse::<u32>().ok() {
                    depth = bits;
                }
            }
        }
        _ => {}
    }
    info.codec_private = cfg_body;
    info.channels = (channels != 0).then_some(channels);
    info.sample_rate = (rate != 0).then_some(rate);
    info.bit_depth = (depth != 0).then_some(depth);
    Ok(())
}

/// Fills the codec fields of `info` from the first sample entry of `stsd`.
pub(super) fn parse_stsd(stsd: &[u8], kind: TrackKind, info: &mut TrackInfo, budget: &mut u32) -> R<()> {
    let count = u32(stsd, 4)?;
    if count == 0 {
        return Err("empty stsd".into());
    }
    info.sample_descriptions = count;
    let entries = boxes(stsd.get(8..).ok_or("stsd header")?, budget)?;
    let first = entries.first().ok_or("stsd without entries")?;
    if matches!(&first.kind, b"encv" | b"enca" | b"enct" | b"encs") {
        return Err("encrypted track".into());
    }
    info.codec = String::from_utf8_lossy(&first.kind).into_owned();
    match kind {
        TrackKind::Video => video(first, info, budget),
        TrackKind::Audio => audio(first, info, budget),
        _ => Ok(()),
    }
}
