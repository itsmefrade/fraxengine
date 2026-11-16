//! MPEG-2 transport stream and BDAV (Blu-ray M2TS) demuxer.
//!
//! Layout: 188 byte packets, 192 byte packets with a 4 byte arrival time prefix (M2TS) and
//! 204 byte packets (Reed-Solomon trailer) are detected from the sync bytes. PAT and PMT give the
//! tracks; each PES is reassembled and cut into access units (video) or frames (audio) with the
//! same rules libavcodec's parsers use, so the packet list matches `ffprobe -show_packets`.
//!
//! Packet addressing: a packet is not contiguous in the source. `PacketRef::offset` is the byte
//! offset of the transport packet that starts the PES the packet begins in and `inner` is the
//! position of the packet inside that PES's elementary stream bytes; `read_packet` re-walks the
//! PID from there (a packet may continue into the next PES of the same PID, as the dependent
//! E-AC-3 substream does on Blu-ray).
//!
//! Timestamps are 90 kHz ticks. The 33 bit PTS/DTS counters are unwrapped to a monotonic i64 and
//! timeline jumps (`discontinuity_indicator` or a jump larger than ten seconds) are removed so
//! the track keeps counting; the breaks are listed by [`TsIndex::breaks`].

pub(crate) mod frame;
pub(crate) mod params;
mod pes;

use crate::{ByteSource, Error, PacketRef, TrackInfo, TrackKind, read_exact_at};
use frame::{Frame, Framer, Kind};
use pes::{PesEvent, PesParser, SectionParser, parse_pat, parse_pmt, parse_unit};
use std::collections::{HashMap, VecDeque};

pub(crate) use pes::detect;

const CHUNK_UNITS: usize = 1024;
const OPEN_LIMIT: u64 = 6 << 20;
const BREAK_TICKS: i64 = 900_000;
const WRAP: i64 = 1 << 33;
const TAIL_BYTES: u64 = 256 * 1024;
const WINDOW_OVERLAP: u64 = 2 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub stride: usize,
    /// Offset of the first unit.
    pub first: u64,
    /// Offset of the sync byte inside a unit (4 for M2TS).
    pub sync_at: usize,
}

/// A timeline jump that was removed from the timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineBreak {
    /// Offset of the transport packet where the new timeline begins.
    pub offset: u64,
    /// Ticks added to the raw timestamps from here on, relative to the unwrapped raw values.
    pub shift: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sub {
    Any,
    Ac3Core,
    NotAc3Core,
}

fn is_ac3_core(ext_id: Option<u8>, first: &[u8]) -> bool {
    ext_id == Some(0x76) || (ext_id.is_none() && first.starts_with(&[0x0b, 0x77]))
}

fn accepts(sub: Sub, ext_id: Option<u8>, first: &[u8]) -> bool {
    match sub {
        Sub::Any => true,
        Sub::Ac3Core => is_ac3_core(ext_id, first),
        Sub::NotAc3Core => !is_ac3_core(ext_id, first),
    }
}

#[derive(Clone)]
struct Route {
    pid: u16,
    sub: Sub,
    kind: Kind,
    /// Index into the track list.
    track: usize,
}

struct Seg {
    es_start: u64,
    unit_off: u64,
    pts: Option<i64>,
    dts: Option<i64>,
    used: bool,
}

#[derive(Default)]
struct Clock {
    last_raw: Option<i64>,
    last_adj_end: i64,
    shift: i64,
    flagged: bool,
}

struct Rt {
    route: Route,
    framer: Framer,
    pending: Vec<u8>,
    off: usize,
    pending_start: u64,
    segs: VecDeque<Seg>,
    clock: Clock,
    prev_pts: Option<i64>,
    prev_dts: Option<i64>,
    prev_dur: i64,
    probed: bool,
    attempts: u32,
    held: Option<PacketRef>,
    seen: bool,
}

/// State shared by all tracks of a scan.
struct Shared<'a> {
    tracks: &'a mut Vec<TrackInfo>,
    breaks: &'a mut Vec<TimelineBreak>,
}

/// One pass over a byte range. Sequential indexing keeps one session alive; a seek window starts a new one.
struct Session {
    rts: Vec<Rt>,
    parsers: HashMap<u16, PesParser>,
    /// PID to the Rt indices that listen to it.
    by_pid: HashMap<u16, Vec<usize>>,
    /// Which Rt gets the PES currently open on a PID (None: waiting for the first bytes to decide).
    current: HashMap<u16, Option<usize>>,
    pending_hdr: HashMap<u16, (Option<u8>, u64, Option<i64>, Option<i64>)>,
    out: Vec<PacketRef>,
    latest_shift: Option<i64>,
    /// Raw reference used to unwrap the first timestamp (unwrapped ticks).
    reference: Option<i64>,
    max_unit: u64,
}

fn unwrap_near(raw: i64, reference: i64) -> i64 {
    raw + (reference - raw + WRAP / 2).div_euclid(WRAP) * WRAP
}

impl Session {
    fn new(routes: &[Route], reference: Option<i64>, start_shift: i64) -> Self {
        let mut by_pid: HashMap<u16, Vec<usize>> = HashMap::new();
        let mut rts = Vec::new();
        for (i, r) in routes.iter().enumerate() {
            by_pid.entry(r.pid).or_default().push(i);
            rts.push(Rt {
                route: r.clone(),
                framer: Framer::new(r.kind),
                pending: Vec::new(),
                off: 0,
                pending_start: 0,
                segs: VecDeque::new(),
                clock: Clock { shift: start_shift, ..Default::default() },
                prev_pts: None,
                prev_dts: None,
                prev_dur: 0,
                probed: false,
                attempts: 0,
                held: None,
                seen: false,
            });
        }
        Self {
            rts,
            parsers: HashMap::new(),
            by_pid,
            current: HashMap::new(),
            pending_hdr: HashMap::new(),
            out: Vec::new(),
            latest_shift: None,
            reference,
            max_unit: 0,
        }
    }

    fn feed(&mut self, sh: &mut Shared<'_>, layout: Layout, base: u64, data: &[u8]) {
        let mut i = 0;
        while i + layout.stride <= data.len() {
            let unit_off = base + i as u64;
            let raw = &data[i + layout.sync_at..i + layout.stride.min(layout.sync_at + 188)];
            i += layout.stride;
            let Some(u) = parse_unit(raw) else { continue };
            self.max_unit = unit_off;
            let Some(routes) = self.by_pid.get(&u.pid) else { continue };
            if u.discontinuity {
                for &r in routes {
                    self.rts[r].clock.flagged = true;
                }
            }
            if u.payload.is_empty() && !u.pusi {
                continue;
            }
            let mut parser = self.parsers.remove(&u.pid).unwrap_or_default();
            let pid = u.pid;
            parser.push(u.pusi, u.payload, &mut |ev| self.event(sh, pid, unit_off, ev));
            self.parsers.insert(pid, parser);
        }
    }

    fn event(&mut self, sh: &mut Shared<'_>, pid: u16, unit_off: u64, ev: PesEvent<'_>) {
        let routes = self.by_pid[&pid].clone();
        match ev {
            PesEvent::Start(h) => {
                if routes.len() == 1 && self.rts[routes[0]].route.sub == Sub::Any {
                    self.current.insert(pid, Some(routes[0]));
                    self.begin(routes[0], unit_off, h.pts, h.dts);
                } else {
                    self.current.insert(pid, None);
                    self.pending_hdr.insert(pid, (h.ext_id, unit_off, h.pts, h.dts));
                }
            }
            PesEvent::Data(d) => {
                let cur = match self.current.get(&pid).copied() {
                    Some(Some(r)) => Some(r),
                    Some(None) => {
                        let (ext, off, pts, dts) = self.pending_hdr.remove(&pid).unwrap_or((None, unit_off, None, None));
                        let pick = routes.iter().copied().find(|&r| accepts(self.rts[r].route.sub, ext, d));
                        self.current.insert(pid, pick);
                        if let Some(r) = pick {
                            self.begin(r, off, pts, dts);
                        }
                        pick
                    }
                    None => None,
                };
                if let Some(r) = cur {
                    let rt = &mut self.rts[r];
                    rt.pending.extend_from_slice(d);
                }
            }
            PesEvent::End => {
                if let Some(Some(r)) = self.current.remove(&pid) {
                    self.drain(sh, r, false);
                    if self.rts[r].route.kind.per_pes() {
                        self.drain(sh, r, true);
                    }
                }
                self.pending_hdr.remove(&pid);
            }
        }
    }

    fn begin(&mut self, r: usize, unit_off: u64, pts: Option<i64>, dts: Option<i64>) {
        let rt = &mut self.rts[r];
        rt.seen = true;
        let abs = rt.pending_start + rt.pending.len() as u64;
        rt.segs.push_back(Seg { es_start: abs, unit_off, pts, dts, used: false });
    }

    /// Cuts frames out of the pending bytes of track `r`.
    fn drain(&mut self, sh: &mut Shared<'_>, r: usize, flush: bool) {
        loop {
            let rt = &mut self.rts[r];
            let Some(fr) = rt.framer.next(&rt.pending[rt.off..], flush) else { break };
            let start_abs = rt.pending_start + (rt.off + fr.skip) as u64;
            let data_from = rt.off + fr.skip;
            let data_to = data_from + fr.len;
            let key = fr.key;
            // Probe parameters from this frame.
            if !rt.probed && rt.attempts < 64 {
                rt.attempts += 1;
                let info = &mut sh.tracks[rt.route.track];
                if params::probe(rt.route.kind, &rt.pending[data_from..data_to], info) {
                    rt.probed = true;
                }
            }
            self.emit(sh, r, start_abs, &fr, key);
            let rt = &mut self.rts[r];
            rt.off = data_to;
            if rt.off > 1 << 20 || rt.off > rt.pending.len() / 2 && rt.off > 64 * 1024 {
                rt.pending.drain(..rt.off);
                rt.pending_start += rt.off as u64;
                rt.off = 0;
            }
        }
        let rt = &mut self.rts[r];
        if rt.off == rt.pending.len() {
            rt.pending_start += rt.off as u64;
            rt.pending.clear();
            rt.off = 0;
        } else if !rt.probed && rt.attempts < 64 && rt.pending.len() - rt.off >= 64 * 1024 {
            rt.attempts += 1;
            let info = &mut sh.tracks[rt.route.track];
            if params::probe(rt.route.kind, &rt.pending[rt.off..], info) {
                rt.probed = true;
            }
        }
    }

    fn emit(&mut self, sh: &mut Shared<'_>, r: usize, start_abs: u64, fr: &Frame, key: bool) {
        let latest = self.latest_shift;
        let reference = self.reference;
        let rt = &mut self.rts[r];
        while rt.segs.len() > 1 && rt.segs[1].es_start <= start_abs {
            rt.segs.pop_front();
        }
        let Some(seg) = rt.segs.front_mut() else { return };
        let first_use = !seg.used;
        seg.used = true;
        let (unit_off, seg_start) = (seg.unit_off, seg.es_start);
        let (pts_raw, dts_raw) = if first_use { (seg.pts, seg.dts) } else { (None, None) };
        let inner = start_abs.saturating_sub(seg_start);
        let info = &sh.tracks[rt.route.track];
        let is_video = is_video_kind(rt.route.kind);
        let ticks = fr.ticks.or_else(|| info.default_duration_ns.map(|ns| (ns as i128 * 90_000 / 1_000_000_000) as i64));
        let fixed_duration = ticks.is_some();
        let track_id = info.id;
        let (pts, dts);
        if pts_raw.is_none() && dts_raw.is_none() {
            // No timestamp for this frame: continue from the previous one.
            let step = rt.prev_dur;
            let base_dts = rt.held.as_ref().map(|h| h.dts).or(rt.prev_dts);
            dts = base_dts.map_or(0, |d| d + step);
            pts = rt.prev_pts.map_or(0, |p| p + step);
            if let Some(l) = rt.clock.last_raw.as_mut() {
                *l += step;
            }
        } else {
            let d_raw = dts_raw.or(pts_raw).unwrap();
            let p_raw = pts_raw.unwrap_or(d_raw);
            let near = rt.clock.last_raw.or(reference);
            let d = near.map_or(d_raw, |n| unwrap_near(d_raw, n));
            if rt.route.kind != Kind::Plain {
                if let Some(last) = rt.clock.last_raw {
                    let thresh = if rt.clock.flagged { 90_000 } else { BREAK_TICKS };
                    if (d - last).abs() > thresh {
                        let end = rt.clock.last_adj_end;
                        let shift = match latest {
                            Some(s) if (d + s - end).abs() <= thresh => s,
                            _ => end - d,
                        };
                        rt.clock.shift = shift;
                        if sh.breaks.last().is_none_or(|b| b.offset != unit_off) {
                            sh.breaks.push(TimelineBreak { offset: unit_off, shift });
                        }
                        self.latest_shift = Some(shift);
                    }
                }
                rt.clock.flagged = false;
            } else if let Some(s) = latest {
                rt.clock.shift = s;
            }
            rt.clock.last_raw = Some(d);
            let p = d + ((p_raw - d_raw + WRAP / 2).rem_euclid(WRAP) - WRAP / 2);
            dts = d + rt.clock.shift;
            pts = p + rt.clock.shift;
        }
        let duration = ticks.unwrap_or(0);
        let packet = PacketRef { track: track_id, pts, dts, duration, keyframe: key, presentation: true, offset: unit_off, size: fr.len as u32, inner: inner as u32 };
        rt.prev_pts = Some(pts);
        rt.prev_dts = Some(dts);
        if duration > 0 {
            rt.prev_dur = duration;
        }
        rt.clock.last_adj_end = dts + duration;
        // Video durations come from the distance to the next packet in decode order.
        if is_video && !fixed_duration {
            if let Some(mut prev) = rt.held.take() {
                let gap = packet.dts - prev.dts;
                if gap > 0 {
                    prev.duration = gap;
                    rt.prev_dur = gap;
                }
                self.out.push(prev);
            }
            let rt = &mut self.rts[r];
            let mut p = packet;
            if p.duration == 0 {
                p.duration = rt.prev_dur;
            }
            rt.held = Some(p);
        } else {
            self.out.push(packet);
        }
    }

    /// Finishes the frames still waiting for more bytes and releases held packets. With `drop_partial`
    /// a PES that was still arriving is discarded (its bytes continue in a range indexed elsewhere);
    /// otherwise it counts as complete (end of file).
    fn finish(&mut self, sh: &mut Shared<'_>, drop_partial: bool) {
        let pids: Vec<u16> = self.parsers.keys().copied().collect();
        for pid in pids {
            let mut parser = self.parsers.remove(&pid).unwrap_or_default();
            if drop_partial {
                if let Some(Some(r)) = self.current.get(&pid).copied() {
                    let rt = &mut self.rts[r];
                    if let Some(seg) = rt.segs.pop_back() {
                        let keep = seg.es_start.saturating_sub(rt.pending_start) as usize;
                        rt.pending.truncate(keep.max(rt.off));
                    }
                }
            } else {
                parser.flush(&mut |ev| self.event(sh, pid, 0, ev));
            }
        }
        self.current.clear();
        for r in 0..self.rts.len() {
            self.drain(sh, r, true);
            if let Some(mut p) = self.rts[r].held.take() {
                if p.duration == 0 {
                    p.duration = self.rts[r].prev_dur;
                }
                self.out.push(p);
            }
        }
    }

    fn take(&mut self) -> Vec<PacketRef> {
        let mut v = std::mem::take(&mut self.out);
        v.sort_by_key(|p| (p.offset, p.inner));
        v
    }

    fn all_probed(&self, tracks: &[TrackInfo]) -> bool {
        self.rts.iter().all(|r| r.probed || r.attempts >= 64 || !needs_probe(tracks[r.route.track].kind, r.route.kind) || !r.seen)
    }
}

fn is_video_kind(k: Kind) -> bool {
    matches!(k, Kind::H264 | Kind::Hevc | Kind::Vc1 | Kind::Mpeg2)
}

fn needs_probe(kind: TrackKind, k: Kind) -> bool {
    kind != TrackKind::Subtitle && k != Kind::Plain || k == Kind::Plain && kind == TrackKind::Audio
}

pub(crate) struct TsIndex {
    pub layout: Layout,
    len: u64,
    routes: Vec<Route>,
    pub tracks: Vec<TrackInfo>,
    pub omitted: usize,
    session: Session,
    /// Next unit offset of the live session.
    frontier: u64,
    /// Indexed byte ranges (sorted, merged).
    covered: Vec<(u64, u64)>,
    pub complete: bool,
    pub breaks: Vec<TimelineBreak>,
    pub start_ticks: Option<i64>,
    pub end_ticks: Option<i64>,
    /// Sparse (offset, adjusted dts) samples used as unwrap references for new sessions.
    samples: Vec<(u64, i64)>,
}

struct Psi {
    pmt_pid: Option<u16>,
    pat: SectionParser,
    pmt: SectionParser,
    streams: Option<Vec<pes::PmtStream>>,
}

fn kind_of(stream_type: u8, descriptors: &[(u8, Vec<u8>)]) -> Option<(Kind, TrackKind, &'static str)> {
    let has = |tag: u8| descriptors.iter().any(|d| d.0 == tag);
    let reg = |id: &[u8; 4]| descriptors.iter().any(|d| d.0 == 0x05 && d.1.starts_with(id));
    Some(match stream_type {
        0x01 | 0x02 => (Kind::Mpeg2, TrackKind::Video, "mpeg2video"),
        0x1b => (Kind::H264, TrackKind::Video, "h264"),
        0x24 => (Kind::Hevc, TrackKind::Video, "hevc"),
        0xea => (Kind::Vc1, TrackKind::Video, "vc1"),
        0x0f => (Kind::Aac, TrackKind::Audio, "aac"),
        0x80 => (Kind::Plain, TrackKind::Audio, "pcm_bluray"),
        0x81 => (Kind::Ac3, TrackKind::Audio, "ac3"),
        0x82 | 0x85 | 0x86 | 0xa2 => (Kind::Dts, TrackKind::Audio, "dts"),
        0x83 => (Kind::TrueHd, TrackKind::Audio, "truehd"),
        0x84 | 0x87 | 0xa1 => (Kind::Ac3, TrackKind::Audio, "eac3"),
        0x90 => (Kind::Plain, TrackKind::Subtitle, "hdmv_pgs_subtitle"),
        0x92 => (Kind::Plain, TrackKind::Subtitle, "hdmv_textst"),
        0x06 => {
            if has(0x7a) || reg(b"EAC3") {
                (Kind::Ac3, TrackKind::Audio, "eac3")
            } else if has(0x6a) || reg(b"AC-3") {
                (Kind::Ac3, TrackKind::Audio, "ac3")
            } else if has(0x7b) || reg(b"DTS1") || reg(b"DTS2") || reg(b"DTS3") {
                (Kind::Dts, TrackKind::Audio, "dts")
            } else if reg(b"HEVC") {
                (Kind::Hevc, TrackKind::Video, "hevc")
            } else if has(0x59) {
                (Kind::Plain, TrackKind::Subtitle, "dvb_subtitle")
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

fn blank_track(id: u64, kind: TrackKind, codec: &str) -> TrackInfo {
    TrackInfo { id, kind, codec: codec.to_owned(), timescale: 90_000, sample_descriptions: 1, ..TrackInfo::default() }
}

fn build_routes(streams: &[pes::PmtStream]) -> (Vec<Route>, Vec<TrackInfo>, usize) {
    let mut routes = Vec::new();
    let mut tracks = Vec::new();
    let mut omitted = 0;
    let hevc_count = streams.iter().filter(|s| s.stream_type == 0x24).count();
    for s in streams {
        let Some((kind, tk, codec)) = kind_of(s.stream_type, &s.descriptors) else {
            omitted += 1;
            continue;
        };
        let mut t = blank_track(u64::from(s.pid), tk, codec);
        t.source_pid = Some(s.pid);
        for (tag, body) in &s.descriptors {
            match tag {
                0x0a if body.len() >= 3 => {
                    t.language = std::str::from_utf8(&body[..3]).ok().map(str::to_owned).filter(|l| l.is_ascii());
                }
                0xb0 if body.len() >= 4 => {
                    t.dolby_vision = Some(crate::DolbyVisionRecord { box_type: *b"dvcC", bytes: body.clone() });
                }
                _ => {}
            }
        }
        if s.stream_type == 0x24 && s.pid == 0x1015 && hevc_count > 1 {
            t.enhancement_layer = true;
        }
        let sub = if s.stream_type == 0x83 { Sub::NotAc3Core } else { Sub::Any };
        routes.push(Route { pid: s.pid, sub, kind, track: tracks.len() });
        tracks.push(t);
        if s.stream_type == 0x83 {
            let mut core = blank_track(0x1_0000 | u64::from(s.pid), TrackKind::Audio, "ac3");
            core.source_pid = Some(s.pid);
            core.language = tracks.last().and_then(|t| t.language.clone());
            routes.push(Route { pid: s.pid, sub: Sub::Ac3Core, kind: Kind::Ac3, track: tracks.len() });
            tracks.push(core);
        }
    }
    (routes, tracks, omitted)
}

fn read_chunk(src: &dyn ByteSource, layout: Layout, from: u64, to: u64) -> Result<Vec<u8>, Error> {
    let stride = layout.stride as u64;
    let to = to.min(src.len());
    let units = to.saturating_sub(from) / stride;
    let mut buf = vec![0u8; (units * stride) as usize];
    if !buf.is_empty() {
        read_exact_at(src, from, &mut buf)?;
    }
    Ok(buf)
}

pub(crate) fn open(src: &dyn ByteSource, layout: Layout) -> Result<TsIndex, Error> {
    let len = src.len();
    let stride = layout.stride as u64;
    // Phase 1: find PAT and PMT.
    let mut psi = Psi { pmt_pid: None, pat: SectionParser::default(), pmt: SectionParser::default(), streams: None };
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = layout.first;
    while psi.streams.is_none() {
        let chunk = if pos < len && pos - layout.first <= OPEN_LIMIT { read_chunk(src, layout, pos, pos + CHUNK_UNITS as u64 * stride)? } else { Vec::new() };
        if chunk.is_empty() {
            return Err(Error::Format("no PMT in the first bytes of the transport stream"));
        }
        for u in chunk.chunks_exact(layout.stride) {
            let Some(unit) = parse_unit(&u[layout.sync_at..layout.sync_at + 188]) else { continue };
            if unit.pid == 0 {
                if let Some(sec) = psi.pat.push(unit.pusi, unit.payload) {
                    if psi.pmt_pid.is_none() {
                        psi.pmt_pid = parse_pat(&sec).first().map(|p| p.1);
                    }
                }
            } else if Some(unit.pid) == psi.pmt_pid {
                if let Some(sec) = psi.pmt.push(unit.pusi, unit.payload) {
                    if let Some((_, streams)) = parse_pmt(&sec) {
                        psi.streams = Some(streams);
                        break;
                    }
                }
            }
        }
        pos += chunk.len() as u64;
        buf.extend(chunk);
    }
    let streams = psi.streams.unwrap();
    let (routes, mut tracks, omitted) = build_routes(&streams);
    if tracks.is_empty() {
        return Err(Error::Format("transport stream has no supported elementary stream"));
    }
    let mut breaks = Vec::new();
    let mut session = Session::new(&routes, None, 0);
    {
        let mut sh = Shared { tracks: &mut tracks, breaks: &mut breaks };
        session.feed(&mut sh, layout, layout.first, &buf);
        while !(session.all_probed(sh.tracks) && session.rts.iter().any(|r| r.seen)) && pos < len && pos - layout.first < OPEN_LIMIT {
            let chunk = read_chunk(src, layout, pos, pos + CHUNK_UNITS as u64 * stride)?;
            if chunk.is_empty() {
                break;
            }
            session.feed(&mut sh, layout, pos, &chunk);
            pos += chunk.len() as u64;
        }
    }
    let at_eof = pos + stride > len;
    if at_eof {
        // The whole file fit in the opening window: release the frames that were waiting for more bytes.
        let mut sh = Shared { tracks: &mut tracks, breaks: &mut breaks };
        session.finish(&mut sh, false);
    }
    let mut idx = TsIndex {
        layout,
        len,
        routes,
        tracks,
        omitted,
        session,
        frontier: pos,
        covered: vec![(layout.first, pos)],
        complete: false,
        breaks,
        start_ticks: None,
        end_ticks: None,
        samples: Vec::new(),
    };
    idx.complete = idx.first_gap().is_none();
    idx.prune_unused();
    idx.measure_span(src)?;
    Ok(idx)
}

impl TsIndex {
    fn prune_unused(&mut self) {
        let unused: Vec<usize> = self
            .session
            .rts
            .iter()
            .filter(|r| r.route.sub == Sub::Ac3Core && !r.seen)
            .map(|r| r.route.track)
            .collect();
        if unused.is_empty() {
            return;
        }
        let keep: Vec<usize> = (0..self.tracks.len()).filter(|i| !unused.contains(i)).collect();
        let remap: HashMap<usize, usize> = keep.iter().enumerate().map(|(n, &o)| (o, n)).collect();
        self.tracks = keep.iter().map(|&i| self.tracks[i].clone()).collect();
        self.routes.retain(|r| !unused.contains(&r.track));
        for r in &mut self.routes {
            r.track = remap[&r.track];
        }
        let mut rts = std::mem::take(&mut self.session.rts);
        rts.retain(|r| !unused.contains(&r.route.track));
        for r in &mut rts {
            r.route.track = remap[&r.route.track];
        }
        self.session.rts = rts;
        self.session.by_pid.clear();
        for (i, r) in self.session.rts.iter().enumerate() {
            self.session.by_pid.entry(r.route.pid).or_default().push(i);
        }
        self.session.current.clear();
    }

    /// First timestamp (from the packets seen at open) and the last PES timestamp near the end.
    fn measure_span(&mut self, src: &dyn ByteSource) -> Result<(), Error> {
        self.start_ticks = self.session.out.iter().map(|p| p.dts.min(p.pts)).min();
        let stride = self.layout.stride as u64;
        let units = self.len.saturating_sub(self.layout.first) / stride;
        let tail_units = (TAIL_BYTES / stride).min(units);
        let from = self.layout.first + (units - tail_units) * stride;
        if from <= self.frontier {
            return Ok(());
        }
        let buf = read_chunk(src, self.layout, from, self.len)?;
        let pids: Vec<u16> = self.routes.iter().map(|r| r.pid).collect();
        let mut best: Option<i64> = None;
        for u in buf.chunks_exact(self.layout.stride) {
            let Some(unit) = parse_unit(&u[self.layout.sync_at..self.layout.sync_at + 188]) else { continue };
            if !pids.contains(&unit.pid) || !unit.pusi {
                continue;
            }
            if let Ok(h) = pes::parse_pes_header(unit.payload) {
                if let Some(t) = h.pts {
                    let v = match (best, self.start_ticks) {
                        (Some(b), _) => unwrap_near(t, b),
                        (None, Some(s)) => unwrap_near(t, s),
                        _ => t,
                    };
                    best = Some(best.map_or(v, |b| b.max(v)));
                }
            }
        }
        self.end_ticks = best;
        Ok(())
    }

    pub fn duration_ns(&self) -> Option<u64> {
        let (s, e) = (self.start_ticks?, self.end_ticks?);
        (e > s).then(|| ((e - s) as u128 * 1_000_000_000 / 90_000) as u64)
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Packets queued during `open` that the caller has not received yet.
    pub fn take_initial(&mut self) -> Vec<PacketRef> {
        let v = self.session.take();
        self.note(&v);
        v
    }

    fn stride(&self) -> u64 {
        self.layout.stride as u64
    }

    fn mark(&mut self, from: u64, to: u64) {
        if to <= from {
            return;
        }
        self.covered.push((from, to));
        self.covered.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(a, b) in &self.covered {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        self.covered = merged;
    }

    /// First byte range of the file (aligned to units) that no session has covered.
    fn first_gap(&self) -> Option<(u64, u64)> {
        let end = self.layout.first + (self.len.saturating_sub(self.layout.first) / self.stride()) * self.stride();
        let mut at = self.layout.first;
        for &(a, b) in &self.covered {
            if a > at {
                return Some((at, a.min(end)));
            }
            at = at.max(b);
        }
        (at + self.stride() <= end).then_some((at, end))
    }

    fn uncovered(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut at = from;
        for &(a, b) in &self.covered {
            if b <= at {
                continue;
            }
            if a >= to {
                break;
            }
            if a > at {
                out.push((at, a));
            }
            at = at.max(b);
        }
        if at < to {
            out.push((at, to));
        }
        out
    }

    fn shift_at(&self, offset: u64) -> i64 {
        self.breaks.iter().filter(|b| b.offset <= offset).map(|b| b.shift).next_back().unwrap_or(0)
    }

    fn note(&mut self, packets: &[PacketRef]) {
        for p in packets {
            if self.samples.last().is_none_or(|s| p.offset >= s.0 + 256 * 1024) {
                self.samples.push((p.offset, p.dts));
            }
        }
        // Keep the vector sorted when windows add samples out of order.
        self.samples.sort_unstable();
        self.samples.dedup_by_key(|s| s.0 / (256 * 1024));
    }

    fn new_session(&self, at: u64) -> Session {
        let near = self.samples.iter().min_by_key(|s| s.0.abs_diff(at));
        let reference = near.map(|&(o, t)| t - self.shift_at(o));
        Session::new(&self.routes, reference, self.shift_at(at))
    }

    fn scan(&mut self, src: &dyn ByteSource, session: &mut Session, from: u64, to: u64) -> Result<u64, Error> {
        let stride = self.stride();
        let mut pos = from;
        let to = to.min(self.len);
        while pos + stride <= to {
            let chunk = read_chunk(src, self.layout, pos, (pos + CHUNK_UNITS as u64 * stride).min(to))?;
            if chunk.is_empty() {
                break;
            }
            let mut sh = Shared { tracks: &mut self.tracks, breaks: &mut self.breaks };
            session.feed(&mut sh, self.layout, pos, &chunk);
            pos += chunk.len() as u64;
        }
        Ok(pos)
    }

    fn finish_session(&mut self, session: &mut Session, drop_partial: bool) -> Vec<PacketRef> {
        let mut sh = Shared { tracks: &mut self.tracks, breaks: &mut self.breaks };
        session.finish(&mut sh, drop_partial);
        session.take()
    }

    /// Indexes up to `max_bytes` further along the file. Returns the new packets (sorted by position).
    pub fn index_more(&mut self, src: &dyn ByteSource, max_bytes: u64) -> Result<Vec<PacketRef>, Error> {
        let stride = self.stride();
        let end_aligned = self.layout.first + (self.len.saturating_sub(self.layout.first) / stride) * stride;
        let mut done = 0u64;
        let mut out = Vec::new();
        while done < max_bytes && !self.complete {
            let limit = self.covered.iter().find(|&&(a, _)| a > self.frontier).map_or(end_aligned, |&(a, _)| a);
            if self.frontier + stride > limit {
                // End of the file or the start of a range a seek window already indexed: close this session
                // and continue at the next gap.
                let mut sess = std::mem::replace(&mut self.session, Session::new(&self.routes, None, 0));
                out.extend(self.finish_session(&mut sess, limit != end_aligned));
                match self.first_gap() {
                    None => self.complete = true,
                    Some((a, _)) => {
                        self.frontier = a;
                        self.session = self.new_session(a);
                    }
                }
                continue;
            }
            let from = self.frontier;
            let end = (from + CHUNK_UNITS as u64 * stride).min(limit);
            let mut sess = std::mem::replace(&mut self.session, Session::new(&self.routes, None, 0));
            let pos = self.scan(src, &mut sess, from, end)?;
            self.session = sess;
            self.mark(from, pos);
            self.frontier = pos;
            done += pos - from;
            out.extend(self.session.take());
        }
        self.note(&out);
        out.sort_by_key(|p| (p.offset, p.inner));
        Ok(out)
    }

    /// Makes sure `[from, to)` is indexed. A range beyond the live position becomes the new live position
    /// (the session continues from `to`), so playback can go on with `index_more` after a seek.
    pub fn ensure_window(&mut self, src: &dyn ByteSource, from: u64, to: u64) -> Result<Vec<PacketRef>, Error> {
        let stride = self.stride();
        let from = self.layout.first + from.saturating_sub(self.layout.first) / stride * stride;
        let to = (to.min(self.len) / stride) * stride;
        let mut out = Vec::new();
        let gaps = self.uncovered(from, to);
        let last = gaps.len().saturating_sub(1);
        for (n, (a, b)) in gaps.into_iter().enumerate() {
            let jump = n == last && a >= self.frontier;
            let mut session = self.new_session(a);
            // Scan a little past the gap so the PES that straddles its end is complete.
            let scan_to = if jump { b } else { (b + WINDOW_OVERLAP).min(self.len) };
            let end = self.scan(src, &mut session, a, scan_to)?;
            self.mark(a, end);
            if jump {
                let mut old = std::mem::replace(&mut self.session, session);
                out.extend(self.finish_session(&mut old, true));
                self.frontier = end;
                out.extend(self.session.take());
            } else {
                out.extend(self.finish_session(&mut session, end < self.len));
            }
        }
        if self.first_gap().is_none() {
            self.complete = true;
        }
        self.note(&out);
        out.sort_by_key(|p| (p.offset, p.inner));
        Ok(out)
    }

    /// First PES timestamp found at or after `offset`, unwrapped near `near`.
    fn time_at(&self, src: &dyn ByteSource, offset: u64, near: i64) -> Result<Option<(u64, i64)>, Error> {
        let stride = self.stride();
        let end = (offset + 512 * 1024).min(self.len);
        let buf = read_chunk(src, self.layout, offset, end)?;
        let pids: Vec<u16> = self.routes.iter().map(|r| r.pid).collect();
        for (n, u) in buf.chunks_exact(self.layout.stride).enumerate() {
            let Some(unit) = parse_unit(&u[self.layout.sync_at..self.layout.sync_at + 188]) else { continue };
            if !unit.pusi || !pids.contains(&unit.pid) {
                continue;
            }
            if let Ok(h) = pes::parse_pes_header(unit.payload) {
                if let Some(t) = h.dts.or(h.pts) {
                    return Ok(Some((offset + n as u64 * stride, unwrap_near(t, near - self.shift_at(offset)) + self.shift_at(offset))));
                }
            }
        }
        Ok(None)
    }

    /// Byte offset (unit aligned) of the last place whose timestamp is at or before `target` ticks,
    /// with that timestamp.
    pub fn offset_for_time(&self, src: &dyn ByteSource, target: i64) -> Result<(u64, i64), Error> {
        let stride = self.stride();
        let (Some(t0), Some(t1)) = (self.start_ticks, self.end_ticks) else { return Ok((self.layout.first, target)) };
        if target <= t0 {
            return Ok((self.layout.first, t0));
        }
        let (mut lo, mut lo_t) = (self.layout.first, t0);
        let (mut hi, mut hi_t) = (self.len, t1.max(t0 + 1));
        if target >= hi_t {
            return Ok((self.len.saturating_sub(TAIL_BYTES).max(self.layout.first), hi_t));
        }
        for _ in 0..24 {
            if hi - lo <= 256 * 1024 {
                break;
            }
            let frac = (target - lo_t) as f64 / (hi_t - lo_t).max(1) as f64;
            let mut guess = lo + ((hi - lo) as f64 * frac) as u64;
            let margin = (hi - lo) / 16;
            guess = guess.clamp(lo + margin, hi - margin);
            guess = self.layout.first + (guess - self.layout.first) / stride * stride;
            match self.time_at(src, guess, target)? {
                Some((at, t)) if t <= target => {
                    lo = at;
                    lo_t = t;
                }
                Some((_, t)) => {
                    hi = guess;
                    hi_t = t;
                }
                None => hi = guess,
            }
        }
        Ok((lo, lo_t))
    }

    /// Reads the bytes of `p` from the source.
    pub fn read_packet(&self, src: &dyn ByteSource, p: &PacketRef, out: &mut Vec<u8>) -> Result<(), Error> {
        let route = self
            .routes
            .iter()
            .find(|r| self.tracks[r.track].id == p.track)
            .ok_or(Error::NoSuchTrack)?;
        let stride = self.layout.stride as u64;
        let need = p.inner as usize + p.size as usize;
        let mut es: Vec<u8> = Vec::with_capacity(need.min(1 << 24));
        let mut parser = PesParser::default();
        let mut pos = p.offset;
        let mut accept = false;
        let mut decided = false;
        let mut pending_ext: Option<u8> = None;
        let mut started = false;
        'outer: while es.len() < need {
            if pos + stride > self.len {
                return Err(Error::Source("transport stream ended inside a packet".into()));
            }
            let chunk = read_chunk(src, self.layout, pos, (pos + 64 * stride).min(self.len))?;
            for (n, u) in chunk.chunks_exact(self.layout.stride).enumerate() {
                let Some(unit) = parse_unit(&u[self.layout.sync_at..self.layout.sync_at + 188]) else { continue };
                if unit.pid != route.pid {
                    continue;
                }
                if !started && !unit.pusi {
                    continue;
                }
                started = true;
                parser.push(unit.pusi, unit.payload, &mut |ev| match ev {
                    PesEvent::Start(h) => {
                        decided = route.sub == Sub::Any;
                        accept = decided;
                        pending_ext = h.ext_id;
                    }
                    PesEvent::Data(d) => {
                        if !decided {
                            decided = true;
                            accept = accepts(route.sub, pending_ext, d);
                        }
                        if accept && es.len() < need {
                            es.extend_from_slice(d);
                        }
                    }
                    PesEvent::End => {}
                });
                if es.len() >= need {
                    let _ = n;
                    break 'outer;
                }
            }
            pos += chunk.len() as u64;
            if chunk.is_empty() {
                return Err(Error::Source("transport stream ended inside a packet".into()));
            }
        }
        out.clear();
        out.extend_from_slice(&es[p.inner as usize..need]);
        Ok(())
    }
}

/// Annex B keyframe check for the `h264`/`hevc` tracks of a transport stream: IDR (H.264), IRAP (HEVC),
/// or an open GOP I picture introduced by a recovery point SEI with count zero.
pub(crate) fn annexb_keyframe(codec: &str, payload: &[u8]) -> bool {
    let hevc = codec == "hevc";
    let mut recovery0 = false;
    let mut i = 0;
    while i + 4 <= payload.len() {
        if payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 1 {
            let b = payload[i + 3];
            if hevc {
                let t = (b >> 1) & 63;
                if t < 32 {
                    return (16..=23).contains(&t);
                }
            } else {
                match b & 31 {
                    5 => return true,
                    6 => recovery0 |= frame::sei_recovery_count(&payload[i + 4..(i + 64).min(payload.len())]) == Some(0),
                    1 => {
                        let rbsp = crate::util::unescape(&payload[i + 4..(i + 20).min(payload.len())]);
                        let mut r = crate::util::Bits::new(&rbsp);
                        let _ = r.ue();
                        return recovery0 && matches!(r.ue() % 5, 2 | 4);
                    }
                    _ => {}
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}
