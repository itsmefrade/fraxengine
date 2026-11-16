//! Bounds-checked ISO-BMFF reader: progressive and fragmented MP4, QuickTime MOV.
//!
//! A progressive file is fully indexed when it is opened (`moov` is the sample index); a
//! fragmented one is indexed fragment by fragment (`Mp4Lazy`). Only `moov` and each
//! `moof` are read into memory (both capped); `mdat` payloads are never touched.
//! Every length that comes from the file is checked against the box that contains
//! it before anything is allocated, and the number of packets, boxes and tracks
//! is capped, so hostile input fails with an error instead of exhausting memory.
//!
//! Timing: `pts`/`dts` are in track ticks and include the edit list shift
//! (leading empty edits minus the first media start), so a track whose first
//! composition time is non-zero starts where the edit list says. `presentation`
//! is false for samples that lie wholly outside every media edit.
//!
//! Fragmented files: `moof` boxes are located by walking the top-level box
//! headers (one small read per box, `mdat` is skipped by size), then parsed in
//! file order. `sidx` and `mfra/tfra` are parsed when present and exposed as
//! [`FragmentEntry`]; `Demuxer::seek_to` uses them to index the fragment at a
//! target before the fragments in front of it. A corrupt or truncated tail stops
//! the scan and keeps what was read before it.

mod entry;
mod frag;

use crate::{ByteSource, FragmentEntry, PacketRef, TrackInfo, TrackKind, read_exact_at};

pub(crate) type R<T> = Result<T, String>;

pub const MAX_MOOV: usize = 64 << 20;
pub const MAX_MOOF: usize = 64 << 20;
pub const MAX_PACKETS: usize = 2_000_000;
pub const MAX_SAMPLE_BYTES: u32 = 64 << 20;
pub const MAX_TRACKS: usize = 64;
const MAX_TOP_BOXES: usize = 4_000_000;
/// Frames per packet when a fixed-size sample table describes raw PCM.
const PCM_FRAMES_PER_PACKET: usize = 1024;

pub(crate) fn arr<const N: usize>(b: &[u8], p: usize) -> R<[u8; N]> {
    let end = p.checked_add(N).ok_or("offset overflow")?;
    b.get(p..end).ok_or("truncated box")?.try_into().map_err(|_| "shape".to_string())
}
pub(crate) fn u16(b: &[u8], p: usize) -> R<u16> {
    Ok(u16::from_be_bytes(arr(b, p)?))
}
pub(crate) fn u32(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_be_bytes(arr(b, p)?))
}
pub(crate) fn u64(b: &[u8], p: usize) -> R<u64> {
    Ok(u64::from_be_bytes(arr(b, p)?))
}

#[derive(Clone, Copy)]
pub(crate) struct Bx<'a> {
    pub kind: [u8; 4],
    pub body: &'a [u8],
}

/// Child boxes of `data`. A malformed child (size below its header or past the
/// parent) is an error; fewer than 8 trailing bytes are padding and ignored.
pub(crate) fn boxes<'a>(data: &'a [u8], budget: &mut u32) -> R<Vec<Bx<'a>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while data.len() - at >= 8 {
        if *budget == 0 {
            return Err("box count limit".into());
        }
        *budget -= 1;
        let rest = &data[at..];
        let size32 = u32(rest, 0)?;
        let kind = arr::<4>(rest, 4)?;
        let (hdr, size) = match size32 {
            0 => (8usize, rest.len() as u64),
            1 => (16, u64(rest, 8)?),
            n => (8, u64::from(n)),
        };
        if size < hdr as u64 || size > rest.len() as u64 {
            return Err("child box extent".into());
        }
        out.push(Bx { kind, body: &rest[hdr..size as usize] });
        at += size as usize;
    }
    Ok(out)
}

pub(crate) fn find<'a>(items: &[Bx<'a>], kind: &[u8; 4]) -> Option<Bx<'a>> {
    items.iter().find(|b| &b.kind == kind).copied()
}

pub(crate) struct Trex {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

/// Maps media time to presentation time using the track's edit list.
pub(crate) struct Edit {
    shift: i64,
    movie_scale: i128,
    /// Media windows scaled by the movie timescale: [begin, end).
    windows: Vec<(i128, i128)>,
}

impl Edit {
    fn none() -> Self {
        Self { shift: 0, movie_scale: 1, windows: Vec::new() }
    }
    fn apply(&self, p: &mut PacketRef) {
        if !self.windows.is_empty() {
            let b = i128::from(p.pts) * self.movie_scale;
            let e = (i128::from(p.pts) + i128::from(p.duration)) * self.movie_scale;
            p.presentation = self.windows.iter().any(|&(wb, we)| b < we && e > wb);
        }
        p.pts = p.pts.saturating_add(self.shift);
        p.dts = p.dts.saturating_add(self.shift);
    }
}

pub(crate) struct TrackState {
    pub info: TrackInfo,
    pub edit: Edit,
    pub trex: Trex,
    pub next_dts: i64,
}

fn edit_plan(trak: &[Bx<'_>], movie_scale: u64, track_scale: u64, budget: &mut u32) -> R<Edit> {
    let Some(edts) = find(trak, b"edts") else { return Ok(Edit::none()) };
    let kids = boxes(edts.body, budget)?;
    let Some(elst) = find(&kids, b"elst") else { return Ok(Edit::none()) };
    let b = elst.body;
    let version = *b.first().ok_or("short elst")?;
    let stride = if version == 1 { 20 } else { 12 };
    let n = u32(b, 4)? as usize;
    if n.checked_mul(stride).and_then(|v| v.checked_add(8)).is_none_or(|v| v > b.len()) {
        return Err("elst table size".into());
    }
    let (ms, ts) = (i128::from(movie_scale), i128::from(track_scale));
    let mut pos_movie = 0i128;
    let mut shift: Option<i64> = None;
    let mut windows = Vec::new();
    for i in 0..n {
        let p = 8 + i * stride;
        let (dur, time) = if version == 1 {
            (u64(b, p)? as i128, u64(b, p + 8)? as i64)
        } else {
            (i128::from(u32(b, p)?), i64::from(u32(b, p + 4)? as i32))
        };
        if time == -1 {
            pos_movie += dur;
            continue;
        }
        if time < 0 {
            continue;
        }
        if shift.is_none() {
            let empty_ticks = (pos_movie * ts + ms / 2) / ms;
            shift = Some(i64::try_from(empty_ticks - i128::from(time)).map_err(|_| "edit shift overflow")?);
        }
        let begin = i128::from(time) * ms;
        // A zero duration on a media edit means "to the end of the media".
        let end = if dur == 0 { i128::MAX } else { begin + dur * ts };
        windows.push((begin, end));
        pos_movie += dur;
    }
    Ok(Edit { shift: shift.unwrap_or(0), movie_scale: ms, windows })
}

/// Fixed-stride table view over a box body. `n` is validated against the body length.
struct Table<'a> {
    b: &'a [u8],
    head: usize,
    stride: usize,
    n: usize,
}

impl<'a> Table<'a> {
    fn new(b: &'a [u8], count_at: usize, head: usize, stride: usize) -> R<Self> {
        let n = u32(b, count_at)? as usize;
        let need = n.checked_mul(stride).and_then(|v| v.checked_add(head)).ok_or("table size overflow")?;
        if need > b.len() {
            return Err("table larger than its box".into());
        }
        Ok(Self { b, head, stride, n })
    }
    fn u32(&self, i: usize, col: usize) -> u32 {
        let p = self.head + i * self.stride + col;
        self.b.get(p..p + 4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&self, i: usize) -> u64 {
        let p = self.head + i * self.stride;
        self.b.get(p..p + 8).map_or(0, |s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
}

/// Sample sizes from `stsz` (fixed or per sample) or `stz2` (4, 8 or 16 bit fields).
enum Sizes<'a> {
    Fixed(u32),
    Table(Table<'a>),
    Compact { b: &'a [u8], field: u8 },
}

impl Sizes<'_> {
    fn get(&self, i: usize) -> u32 {
        match self {
            Sizes::Fixed(v) => *v,
            Sizes::Table(t) => t.u32(i, 0),
            Sizes::Compact { b, field } => match field {
                4 => b.get(12 + i / 2).map_or(0, |v| u32::from(if i % 2 == 0 { v >> 4 } else { v & 15 })),
                8 => b.get(12 + i).map_or(0, |v| u32::from(*v)),
                _ => b.get(12 + i * 2..14 + i * 2).map_or(0, |s| u32::from(u16::from_be_bytes([s[0], s[1]]))),
            },
        }
    }
}

/// Walks run-length `(count, value)` tables one sample at a time.
struct Runs<'a> {
    t: Option<Table<'a>>,
    idx: usize,
    used: u32,
}

impl<'a> Runs<'a> {
    fn next(&mut self) -> Option<u32> {
        let t = self.t.as_ref()?;
        while self.idx < t.n && self.used >= t.u32(self.idx, 0) {
            self.idx += 1;
            self.used = 0;
        }
        if self.idx >= t.n {
            return None;
        }
        self.used += 1;
        Some(t.u32(self.idx, 4))
    }
}

struct Built {
    info: TrackInfo,
    edit: Edit,
}

fn stbl_samples(
    stbl: &[Bx<'_>],
    info: &TrackInfo,
    src_len: u64,
    fragmented: bool,
    out: &mut Vec<PacketRef>,
    left: &mut usize,
) -> R<()> {
    let tbl = |k: &[u8; 4]| find(stbl, k);
    let (Some(stsz), Some(stsc)) = (
        tbl(b"stsz").or_else(|| tbl(b"stz2")),
        tbl(b"stsc"),
    ) else {
        return if fragmented { Ok(()) } else { Err("missing sample tables".into()) };
    };
    let chunk_box = tbl(b"stco").or_else(|| tbl(b"co64"));
    let is64 = tbl(b"stco").is_none();
    let Some(chunk_box) = chunk_box else {
        return if fragmented { Ok(()) } else { Err("missing chunk offsets".into()) };
    };
    let stts = tbl(b"stts").ok_or("missing stts")?;
    let (sizes, n) = if find(stbl, b"stsz").is_some() {
        let fixed = u32(stsz.body, 4)?;
        let n = u32(stsz.body, 8)? as usize;
        if fixed != 0 {
            (Sizes::Fixed(fixed), n)
        } else {
            let t = Table::new(stsz.body, 8, 12, 4)?;
            let n = t.n;
            (Sizes::Table(t), n)
        }
    } else {
        let field = *stsz.body.get(7).ok_or("short stz2")?;
        let n = u32(stsz.body, 8)? as usize;
        if !matches!(field, 4 | 8 | 16) {
            return Err("stz2 field size".into());
        }
        let bytes = (n as u128 * u128::from(field)).div_ceil(8);
        if 12 + bytes > stsz.body.len() as u128 {
            return Err("stz2 larger than its box".into());
        }
        (Sizes::Compact { b: stsz.body, field }, n)
    };
    if n == 0 {
        return Ok(());
    }
    let stsc_t = Table::new(stsc.body, 4, 8, 12)?;
    let co = Table::new(chunk_box.body, 4, 8, if is64 { 8 } else { 4 })?;
    let stts_t = Table::new(stts.body, 4, 8, 8)?;
    let ctts_t = match find(stbl, b"ctts") {
        Some(c) => Some(Table::new(c.body, 4, 8, 8)?),
        None => None,
    };
    let stss = match find(stbl, b"stss") {
        Some(s) => Some(Table::new(s.body, 4, 8, 4)?),
        None => None,
    };
    // Raw PCM: a fixed size per frame and one-tick frames. Grouping keeps the packet
    // count sane (otherwise one packet per sample frame).
    let pcm_delta = match (&sizes, stts_t.n) {
        (Sizes::Fixed(_), 1) if info.kind == TrackKind::Audio && stts_t.u32(0, 0) as usize >= n => {
            Some(i64::from(stts_t.u32(0, 4)))
        }
        _ => None,
    };
    let mut time = Runs { t: Some(stts_t), idx: 0, used: 0 };
    let mut comp = Runs { t: ctts_t, idx: 0, used: 0 };
    let track = info.id;
    let mut dts = 0i64;
    let mut sample = 0usize;
    let mut last_delta = 0u32;
    let mut stsc_i = 0usize;
    for chunk in 0..co.n {
        if sample >= n {
            break;
        }
        let cn = chunk as u64 + 1;
        while stsc_i + 1 < stsc_t.n && u64::from(stsc_t.u32(stsc_i + 1, 0)) <= cn {
            stsc_i += 1;
        }
        if u64::from(stsc_t.u32(stsc_i, 0)) > cn {
            continue;
        }
        let mut per_chunk = stsc_t.u32(stsc_i, 4) as usize;
        let mut at = if is64 { co.u64(chunk) } else { u64::from(co.u32(chunk, 0)) };
        while per_chunk > 0 && sample < n {
            if *left == 0 {
                return Err("packet count limit".into());
            }
            let (take, size, pts_off, delta, dts_here);
            if let Some(d) = pcm_delta {
                take = per_chunk.min(PCM_FRAMES_PER_PACKET).min(n - sample);
                let bytes = take as u64 * u64::from(sizes.get(0));
                size = u32::try_from(bytes).ok().filter(|&s| s <= MAX_SAMPLE_BYTES).ok_or("sample too large")?;
                pts_off = 0i64;
                delta = (d * take as i64).clamp(0, i64::from(u32::MAX)) as u32;
                dts_here = sample as i64 * d;
            } else {
                take = 1;
                size = sizes.get(sample);
                if size > MAX_SAMPLE_BYTES {
                    return Err("sample too large".into());
                }
                delta = time.next().unwrap_or(last_delta);
                last_delta = delta;
                // Version 0 ctts is unsigned in the spec, but muxers write negatives into it.
                pts_off = comp.next().map_or(0, |v| i64::from(v as i32));
                dts_here = dts;
                dts = dts.checked_add(i64::from(delta)).ok_or("decode time overflow")?;
            }
            let end = at.checked_add(u64::from(size)).ok_or("sample extent overflow")?;
            if end <= src_len {
                let keyframe = match &stss {
                    None => true,
                    Some(t) => {
                        let want = sample as u32 + 1;
                        let (mut lo, mut hi) = (0usize, t.n);
                        while lo < hi {
                            let mid = lo + (hi - lo) / 2;
                            if t.u32(mid, 0) < want { lo = mid + 1 } else { hi = mid }
                        }
                        lo < t.n && t.u32(lo, 0) == want
                    }
                };
                *left -= 1;
                out.push(PacketRef {
                    track,
                    pts: dts_here.saturating_add(pts_off),
                    dts: dts_here,
                    duration: i64::from(delta),
                    keyframe,
                    presentation: true,
                    offset: at,
                    size,
                    inner: 0,
                });
            }
            at = end;
            sample += take;
            per_chunk -= take;
        }
    }
    Ok(())
}

fn parse_trak(
    trak: &Bx<'_>,
    movie_scale: u64,
    src_len: u64,
    fragmented: bool,
    out: &mut Vec<PacketRef>,
    left: &mut usize,
    budget: &mut u32,
) -> R<Built> {
    let kids = boxes(trak.body, budget)?;
    let tkhd = find(&kids, b"tkhd").ok_or("missing tkhd")?;
    let v1 = tkhd.body.first() == Some(&1);
    let id = u32(tkhd.body, if v1 { 20 } else { 12 })?;
    if id == 0 {
        return Err("zero track id".into());
    }
    let mdia = find(&kids, b"mdia").ok_or("missing mdia")?;
    let ms = boxes(mdia.body, budget)?;
    let mdhd = find(&ms, b"mdhd").ok_or("missing mdhd")?;
    let (timescale, lang_at) = if mdhd.body.first() == Some(&1) { (u32(mdhd.body, 20)?, 32) } else { (u32(mdhd.body, 12)?, 20) };
    if timescale == 0 {
        return Err("zero track timescale".into());
    }
    let lang = u16(mdhd.body, lang_at)?;
    let language = (lang != 0).then(|| {
        [(lang >> 10) & 31, (lang >> 5) & 31, lang & 31].iter().map(|c| char::from((*c as u8).wrapping_add(0x60))).collect::<String>()
    });
    let hdlr = find(&ms, b"hdlr").ok_or("missing hdlr")?;
    let handler: [u8; 4] = arr(hdlr.body, 8)?;
    let kind = match &handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"subt" | b"sbtl" | b"text" | b"clcp" => TrackKind::Subtitle,
        _ => TrackKind::Other,
    };
    let name = hdlr.body.get(24..).and_then(|n| {
        let n = n.split(|&c| c == 0).next()?;
        // QuickTime handlers store a Pascal string.
        let n = if n.first().is_some_and(|&l| usize::from(l) + 1 == n.len()) { &n[1..] } else { n };
        std::str::from_utf8(n).ok().filter(|s| !s.is_empty()).map(str::to_owned)
    });
    let minf = find(&ms, b"minf").ok_or("missing minf")?;
    let mis = boxes(minf.body, budget)?;
    let stbl_box = find(&mis, b"stbl").ok_or("missing stbl")?;
    let stbl = boxes(stbl_box.body, budget)?;
    let stsd = find(&stbl, b"stsd").ok_or("missing stsd")?;
    // Self-contained media only: the first sample description must use a `url ` entry
    // with the self-contained flag (or no dref at all).
    if let Some(dinf) = find(&mis, b"dinf") {
        let ds = boxes(dinf.body, budget)?;
        if let Some(dref) = find(&ds, b"dref") {
            let refs = boxes(dref.body.get(8..).ok_or("short dref")?, budget)?;
            if refs.first().is_some_and(|r| u32(r.body, 0).is_ok_and(|f| f & 1 == 0)) {
                return Err("external data reference".into());
            }
        }
    }
    let mut width = None;
    let mut height = None;
    if kind == TrackKind::Video {
        let n = tkhd.body.len();
        if n >= 8 {
            width = Some(u32(tkhd.body, n - 8)? >> 16).filter(|&w| w > 0);
            height = Some(u32(tkhd.body, n - 4)? >> 16).filter(|&h| h > 0);
        }
    }
    let mut info = TrackInfo {
        id: u64::from(id),
        kind,
        codec: String::new(),
        codec_private: Vec::new(),
        dolby_vision: None,
        color: None,
        timescale,
        language,
        name,
        channels: None,
        sample_rate: None,
        bit_depth: None,
        width,
        height,
        default_duration_ns: None,
        codec_string: None,
        fallback_codec_string: None,
        dolby_vision_el: None,
        joc_complexity: None,
        sample_descriptions: 1,
        source_pid: None,
        codec_delay_ns: None,
        enhancement_layer: false,
    };
    entry::parse_stsd(stsd.body, kind, &mut info, budget)?;
    let edit = edit_plan(&kids, movie_scale, u64::from(timescale), budget)?;
    stbl_samples(&stbl, &info, src_len, fragmented, out, left)?;
    Ok(Built { info, edit })
}

pub(crate) struct Mp4 {
    pub tracks: Vec<TrackInfo>,
    pub packets: Vec<PacketRef>,
    pub duration_ns: Option<u64>,
    pub omitted: usize,
    pub fragmented: bool,
    pub fragments: Vec<FragmentEntry>,
}

struct Top {
    kind: [u8; 4],
    start: u64,
    header: u64,
    end: u64,
}

/// Reads the top-level box header at `pos`. `None` at the end of the data or where the boxes stop making sense.
fn next_top(src: &dyn ByteSource, pos: u64, first: bool) -> R<Option<Top>> {
    let len = src.len();
    if pos > len || len - pos < 8 {
        return Ok(None);
    }
    let mut head = [0u8; 16];
    let n = (len - pos).min(16) as usize;
    read_exact_at(src, pos, &mut head[..n]).map_err(|e| e.to_string())?;
    let size32 = u32(&head, 0)?;
    let kind = arr::<4>(&head, 4)?;
    let (hdr, size) = match size32 {
        0 => (8u64, len - pos),
        1 if n >= 16 => (16, u64(&head, 8)?),
        1 => return Ok(None),
        s => (8, u64::from(s)),
    };
    if size < hdr {
        if first {
            return Err("box smaller than its header".into());
        }
        return Ok(None);
    }
    let mut end = pos.checked_add(size).ok_or("box extent overflow")?;
    if end > len {
        // Truncated file: only mdat can be partially present and still useful.
        if &kind != b"mdat" {
            return Ok(None);
        }
        end = len;
    }
    Ok(Some(Top { kind, start: pos, header: hdr, end }))
}

fn read_box(src: &dyn ByteSource, t: &Top, cap: usize) -> R<Vec<u8>> {
    let n = t.end - t.start - t.header;
    if n > cap as u64 {
        return Err(format!("{} box exceeds the size limit", String::from_utf8_lossy(&t.kind)));
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(n as usize).map_err(|_| "allocation failed")?;
    buf.resize(n as usize, 0);
    read_exact_at(src, t.start + t.header, &mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn to_ns(ticks: i128, scale: u32) -> i128 {
    ticks * 1_000_000_000 / i128::from(scale.max(1))
}

/// Incremental MP4 index. `open` reads `moov` (plus `sidx` and the tail `mfra` of fragmented files);
/// progressive files are complete after that because `moov` is the sample index. Fragmented files are
/// indexed one `moof` at a time by `index_more`, or at a fragment found through `sidx`/`mfra`.
pub(crate) struct Mp4Lazy {
    tracks: Vec<TrackState>,
    left: usize,
    next: u64,
    movie_scale: u32,
    mvhd_dur: u64,
    mehd_dur: Option<u64>,
    pub fragmented: bool,
    pub fragments: Vec<FragmentEntry>,
    pub omitted: usize,
    pub complete: bool,
    tail_error: Option<String>,
    mfra_seen: bool,
    /// Fragments already parsed: moof start -> end of its sample data.
    done: std::collections::BTreeMap<u64, u64>,
    low_gap: Option<u64>,
}

fn dedupe_fragments(v: &mut Vec<FragmentEntry>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|e| seen.insert((e.track, e.offset, e.time_ns, e.from_sidx)));
}

impl Mp4Lazy {
    pub fn open(src: &dyn ByteSource) -> R<(Self, Vec<PacketRef>)> {
        let len = src.len();
        let mut pos = 0u64;
        let mut boxes_seen = 0usize;
        let mut early_sidx = Vec::new();
        let moov_top = loop {
            if boxes_seen >= MAX_TOP_BOXES {
                return Err("top-level box count limit".into());
            }
            let t = next_top(src, pos, boxes_seen == 0)?.ok_or("no moov box")?;
            boxes_seen += 1;
            pos = t.end;
            if &t.kind == b"moov" {
                break t;
            }
            if &t.kind == b"sidx" {
                early_sidx.push(t);
            }
        };
        let moov = read_box(src, &moov_top, MAX_MOOV)?;
        let mut budget = 4_000_000u32;
        let kids = boxes(&moov, &mut budget)?;
        let mvhd = find(&kids, b"mvhd").ok_or("missing mvhd")?;
        let (movie_scale, mvhd_dur) = if mvhd.body.first() == Some(&1) {
            (u32(mvhd.body, 20)?, u64(mvhd.body, 24)?)
        } else {
            (u32(mvhd.body, 12)?, u64::from(u32(mvhd.body, 16)?))
        };
        if movie_scale == 0 {
            return Err("zero movie timescale".into());
        }
        let mvex = find(&kids, b"mvex");
        let fragmented = mvex.is_some();
        let mut left = MAX_PACKETS;
        let mut packets = Vec::new();
        let mut tracks: Vec<TrackState> = Vec::new();
        let mut omitted = 0usize;
        let mut first_err: Option<String> = None;
        for trak in kids.iter().filter(|b| &b.kind == b"trak") {
            if tracks.len() + omitted >= MAX_TRACKS {
                return Err("track count limit".into());
            }
            let mut local = Vec::new();
            match parse_trak(trak, u64::from(movie_scale), len, fragmented, &mut local, &mut left, &mut budget) {
                Ok(b) if tracks.iter().all(|t| t.info.id != b.info.id) => {
                    tracks.push(TrackState {
                        info: b.info,
                        edit: b.edit,
                        trex: Trex { duration: 0, size: 0, flags: 0 },
                        next_dts: local.last().map_or(0, |p| p.dts.saturating_add(p.duration)),
                    });
                    packets.append(&mut local);
                }
                Ok(_) => {
                    left += local.len();
                    omitted += 1;
                    first_err.get_or_insert_with(|| "duplicate track id".into());
                }
                Err(e) => {
                    left += local.len();
                    omitted += 1;
                    first_err.get_or_insert(e);
                }
            }
        }
        if tracks.is_empty() {
            return Err(first_err.unwrap_or_else(|| "no usable track".into()));
        }
        let mut mehd_dur = None;
        if let Some(mvex) = mvex {
            let ms = boxes(mvex.body, &mut budget)?;
            if let Some(m) = find(&ms, b"mehd") {
                mehd_dur = if m.body.first() == Some(&1) { u64(m.body, 4).ok() } else { u32(m.body, 4).ok().map(u64::from) };
            }
            for trex in ms.iter().filter(|b| &b.kind == b"trex") {
                let id = u64::from(u32(trex.body, 4)?);
                if let Some(t) = tracks.iter_mut().find(|t| t.info.id == id) {
                    t.trex = Trex { duration: u32(trex.body, 12)?, size: u32(trex.body, 16)?, flags: u32(trex.body, 20)? };
                }
            }
        }
        let mut m = Self {
            tracks,
            left,
            next: pos,
            movie_scale,
            mvhd_dur,
            mehd_dur,
            fragmented,
            fragments: Vec::new(),
            omitted,
            complete: !fragmented,
            tail_error: None,
            mfra_seen: false,
            done: Default::default(),
            low_gap: None,
        };
        if !fragmented {
            // The sample tables are the whole index: apply edits and order the samples.
            for p in &mut packets {
                if let Some(t) = m.tracks.iter().find(|t| t.info.id == p.track) {
                    t.edit.apply(p);
                }
            }
            interleave(&mut packets, &m.tracks);
            return Ok((m, packets));
        }
        // Random access data: sidx boxes before and right after moov, mfra at the very end.
        for t in &early_sidx {
            if let Ok(body) = read_box(src, t, 16 << 20) {
                frag::parse_sidx(&body, t.end, &m.tracks, &mut m.fragments);
            }
        }
        let mut at = pos;
        for _ in 0..8 {
            match next_top(src, at, false) {
                Ok(Some(t)) if matches!(&t.kind, b"sidx" | b"styp" | b"free" | b"skip") => {
                    if &t.kind == b"sidx" {
                        if let Ok(body) = read_box(src, &t, 16 << 20) {
                            frag::parse_sidx(&body, t.end, &m.tracks, &mut m.fragments);
                        }
                    }
                    at = t.end;
                }
                _ => break,
            }
        }
        m.read_tail_mfra(src);
        dedupe_fragments(&mut m.fragments);
        Ok((m, packets))
    }

    fn read_tail_mfra(&mut self, src: &dyn ByteSource) {
        let len = src.len();
        if len < 32 {
            return;
        }
        let mut tail = [0u8; 16];
        if read_exact_at(src, len - 16, &mut tail).is_err() || &tail[4..8] != b"mfro" {
            return;
        }
        let Ok(size) = u32(&tail, 12) else { return };
        let size = u64::from(size);
        if size < 24 || size > len - 16 + 16 {
            return;
        }
        let top = Top { kind: *b"mfra", start: len - size, header: 8, end: len };
        let mut head = [0u8; 8];
        if read_exact_at(src, top.start, &mut head).is_err() || &head[4..8] != b"mfra" {
            return;
        }
        if let Ok(body) = read_box(src, &top, 16 << 20) {
            frag::parse_mfra(&body, &self.tracks, &mut self.fragments);
            self.mfra_seen = true;
        }
    }

    pub fn track_infos(&self) -> Vec<TrackInfo> {
        self.tracks.iter().map(|t| t.info.clone()).collect()
    }

    /// Movie duration from the headers (`mehd`, then `mvhd`), when they state one.
    pub fn header_duration_ns(&self) -> Option<u64> {
        let d = if self.mvhd_dur == 0 || self.mvhd_dur == u64::MAX { self.mehd_dur.unwrap_or(0) } else { self.mvhd_dur };
        (d > 0).then(|| to_ns(i128::from(d), self.movie_scale).clamp(0, i128::from(u64::MAX)) as u64)
    }

    /// Follows the file for about `max_bytes`, parsing the `moof` boxes it passes.
    pub fn index_more(&mut self, src: &dyn ByteSource, max_bytes: u64) -> R<Vec<PacketRef>> {
        let len = src.len();
        let mut out = Vec::new();
        let mut spent = 0u64;
        let mut guard = 0usize;
        while !self.complete && spent < max_bytes {
            guard += 1;
            if guard > MAX_TOP_BOXES {
                return Err("top-level box count limit".into());
            }
            let top = match next_top(src, self.next, false) {
                Ok(Some(t)) => t,
                _ => {
                    match self.low_gap.take() {
                        Some(g) => self.next = g,
                        None => self.complete = true,
                    }
                    continue;
                }
            };
            spent += top.end - top.start;
            match &top.kind {
                b"moof" if !self.done.contains_key(&top.start) => {
                    if let Err(e) = self.parse_fragment(src, &top, len, &mut out) {
                        self.tail_error = Some(e);
                        self.complete = true;
                        break;
                    }
                }
                b"sidx" => {
                    if let Ok(body) = read_box(src, &top, 16 << 20) {
                        frag::parse_sidx(&body, top.end, &self.tracks, &mut self.fragments);
                        dedupe_fragments(&mut self.fragments);
                    }
                }
                b"mfra" if !self.mfra_seen => {
                    if let Ok(body) = read_box(src, &top, 16 << 20) {
                        frag::parse_mfra(&body, &self.tracks, &mut self.fragments);
                        dedupe_fragments(&mut self.fragments);
                    }
                    self.mfra_seen = true;
                }
                _ => {}
            }
            self.next = top.end;
        }
        Ok(self.finish_batch(out))
    }

    fn parse_fragment(&mut self, src: &dyn ByteSource, top: &Top, len: u64, out: &mut Vec<PacketRef>) -> R<()> {
        let body = read_box(src, top, MAX_MOOF)?;
        let mut tmp = Vec::new();
        frag::parse_moof(&body, top.start, len, &mut self.tracks, &mut tmp, &mut self.left)?;
        let end = tmp.iter().map(|p| p.offset + u64::from(p.size)).max().unwrap_or(top.end);
        self.done.insert(top.start, end);
        out.append(&mut tmp);
        Ok(())
    }

    /// Indexes the fragment that starts at or after `offset` (a `sidx`/`tfra` position). Returns its packets and
    /// the byte extent covered by its samples.
    pub fn index_fragment_at(&mut self, src: &dyn ByteSource, offset: u64) -> R<(Vec<PacketRef>, (u64, u64))> {
        let len = src.len();
        let mut at = offset;
        for _ in 0..16 {
            let Some(top) = next_top(src, at, false)? else { break };
            if &top.kind == b"moof" {
                if let Some(&end) = self.done.get(&top.start) {
                    return Ok((Vec::new(), (top.start, end)));
                }
                let mut out = Vec::new();
                self.parse_fragment(src, &top, len, &mut out)?;
                let extent = (top.start, self.done[&top.start]);
                if top.start > self.next {
                    self.low_gap = Some(self.low_gap.map_or(self.next, |g| g.min(self.next)));
                    self.next = top.end;
                } else if top.start == self.next {
                    self.next = top.end;
                }
                return Ok((self.finish_batch(out), extent));
            }
            at = top.end;
        }
        Err("no fragment at the index position".into())
    }

    /// Indexes the fragment after the one whose extent contains `offset` when it is not indexed yet.
    pub fn index_fragment_after(&mut self, src: &dyn ByteSource, offset: u64) -> R<Vec<PacketRef>> {
        let Some((_, &end)) = self.done.range(..=offset).next_back().filter(|(_, e)| **e > offset) else {
            return Ok(Vec::new());
        };
        if end >= src.len() {
            return Ok(Vec::new());
        }
        Ok(self.index_fragment_at(src, end)?.0)
    }

    fn finish_batch(&self, mut packets: Vec<PacketRef>) -> Vec<PacketRef> {
        if self.fragmented {
            fill_unknown_durations(&mut packets);
        }
        for p in &mut packets {
            if let Some(t) = self.tracks.iter().find(|t| t.info.id == p.track) {
                t.edit.apply(p);
            }
        }
        packets
    }

    pub fn scale_of(&self, track: u64) -> u32 {
        self.tracks.iter().find(|t| t.info.id == track).map_or(1, |t| t.info.timescale)
    }
}

/// Eager reader: the whole index at once, ordered for streaming.
pub(crate) fn read(src: &dyn ByteSource) -> R<Mp4> {
    let (mut lazy, mut packets) = Mp4Lazy::open(src)?;
    while !lazy.complete {
        let mut more = lazy.index_more(src, u64::MAX)?;
        packets.append(&mut more);
    }
    if packets.is_empty() {
        if let Some(e) = lazy.tail_error.clone() {
            return Err(e);
        }
    }
    let scale_of = |track: u64| lazy.scale_of(track);
    let longest = packets
        .iter()
        .filter(|p| p.presentation)
        .map(|p| to_ns(i128::from(p.pts) + i128::from(p.duration), scale_of(p.track)))
        .max();
    let duration_ns = match longest {
        Some(v) => Some(v.max(0) as u64),
        None => lazy.header_duration_ns(),
    };
    if lazy.fragmented {
        interleave(&mut packets, &lazy.tracks);
    }
    let fragments = std::mem::take(&mut lazy.fragments);
    Ok(Mp4 {
        tracks: lazy.track_infos(),
        packets,
        duration_ns,
        omitted: lazy.omitted,
        fragmented: lazy.fragmented,
        fragments,
    })
}

/// Fragments written without any duration source (no `trun`, `tfhd` or `trex` default) leave
/// zero; the distance to the next sample of the same track is the best answer, and the last
/// sample repeats its predecessor.
fn fill_unknown_durations(packets: &mut [PacketRef]) {
    let mut next: Vec<(u64, i64, i64)> = Vec::new(); // (track, next dts, last known duration)
    for p in packets.iter_mut().rev() {
        let slot = match next.iter().position(|e| e.0 == p.track) {
            Some(i) => i,
            None => {
                next.push((p.track, i64::MIN, 0));
                next.len() - 1
            }
        };
        if p.duration == 0 {
            let gap = next[slot].1.saturating_sub(p.dts);
            p.duration = if next[slot].1 != i64::MIN && gap > 0 { gap } else { next[slot].2 };
        }
        next[slot].1 = p.dts;
        if p.duration > 0 {
            next[slot].2 = p.duration;
        }
    }
}

/// File order when the tracks are interleaved well enough to stream; decode-time order
/// when one track runs far ahead of another in file order (audio stored after video).
fn interleave(packets: &mut [PacketRef], tracks: &[TrackState]) {
    packets.sort_by_key(|p| p.offset);
    let scale = |track: u64| tracks.iter().find(|t| t.info.id == track).map_or(1, |t| t.info.timescale);
    let av: Vec<u64> = tracks
        .iter()
        .filter(|t| matches!(t.info.kind, TrackKind::Video | TrackKind::Audio))
        .filter(|t| packets.iter().filter(|p| p.track == t.info.id).take(3).count() >= 3)
        .map(|t| t.info.id)
        .collect();
    if av.len() < 2 {
        return;
    }
    let mut last: Vec<Option<i128>> = vec![None; av.len()];
    // Seed with each track's first decode time so tracks that start late do not count as skew.
    for (i, id) in av.iter().enumerate() {
        last[i] = packets.iter().find(|p| p.track == *id).map(|p| to_ns(i128::from(p.dts), scale(*id)));
    }
    let mut worst = 0i128;
    for p in packets.iter() {
        if let Some(i) = av.iter().position(|&id| id == p.track) {
            last[i] = Some(to_ns(i128::from(p.dts), scale(p.track)));
            let (mut lo, mut hi) = (i128::MAX, i128::MIN);
            for v in last.iter().flatten() {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
            worst = worst.max(hi - lo);
        }
    }
    if worst > 10_000_000_000 {
        packets.sort_by_key(|p| (to_ns(i128::from(p.dts), scale(p.track)), p.offset));
    }
}
