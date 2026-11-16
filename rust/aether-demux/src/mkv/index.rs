//! Source-driven Matroska packet index. Reads element headers through a
//! `ByteSource` and touches block payloads only when lacing needs them, so an
//! index over a remote file costs about two small reads per block.

use super::{
    Error, ElementHeader, MAX_ELEMENT_HEADER_BYTES, element, element_header, lace_sizes, timestamp_scale,
    tracks_lenient, uint, vint,
};
use crate::{ByteSource, Error as DemuxError, read_exact_at};
use std::collections::{BTreeMap, HashMap};

const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_a966;
const ID_TRACKS: u32 = 0x1654_ae6b;
const ID_CLUSTER: u32 = 0x1f43_b675;
const LEVEL1: [u32; 8] = [
    0x1f43_b675, 0x1c53_bb6b, 0x1549_a966, 0x1654_ae6b, 0x114d_9b74, 0x1043_a770, 0x1254_c367, 0x1941_a469,
];
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_LACED_BLOCK: u64 = 8 * 1024 * 1024;
pub const MAX_PACKETS: usize = 8_000_000;

/// One block frame with its absolute payload range. Times are nanoseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MkvPacket {
    pub track: u64,
    pub pts_ns: i64,
    pub duration_ns: Option<u64>,
    pub keyframe: bool,
    pub offset: u64,
    pub size: u32,
}

pub struct MkvScan {
    pub tracks: Vec<crate::TrackInfo>,
    pub packets: Vec<MkvPacket>,
    pub duration_ns: Option<u64>,
}

fn header_at(src: &dyn ByteSource, offset: u64, end: u64) -> Result<ElementHeader, DemuxError> {
    let mut buf = [0u8; MAX_ELEMENT_HEADER_BYTES];
    let n = (end.saturating_sub(offset)).min(MAX_ELEMENT_HEADER_BYTES as u64) as usize;
    read_exact_at(src, offset, &mut buf[..n])?;
    Ok(element_header(&buf[..n])?)
}

fn read_payload(src: &dyn ByteSource, start: u64, end: u64, cap: u64) -> Result<Vec<u8>, DemuxError> {
    if end - start > cap {
        return Err(DemuxError::Limit("matroska element exceeds bound"));
    }
    let mut data = vec![0; (end - start) as usize];
    read_exact_at(src, start, &mut data)?;
    Ok(data)
}

fn float(bytes: &[u8]) -> Option<f64> {
    match bytes.len() {
        4 => Some(f64::from(f32::from_be_bytes(bytes.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

struct Layout {
    scale: u64,
    /// (track number, default duration ns)
    defaults: Vec<(u64, Option<u64>)>,
}

/// One step that turns a stored block into the codec payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Header stripping: the removed bytes are put back in front.
    Strip(Vec<u8>),
    /// zlib compression.
    Zlib,
}

/// Undoes the content encodings of a track on one block payload.
pub(crate) fn decode_steps(steps: &[Step], data: Vec<u8>) -> Result<Vec<u8>, DemuxError> {
    let mut data = data;
    for step in steps {
        match step {
            Step::Strip(prefix) => {
                let mut v = Vec::with_capacity(prefix.len() + data.len());
                v.extend_from_slice(prefix);
                v.extend_from_slice(&data);
                data = v;
            }
            Step::Zlib => data = crate::util::zlib_inflate(&data, 256 << 20).map_err(DemuxError::Format)?,
        }
    }
    Ok(data)
}

const ID_SEEKHEAD: u32 = 0x114d_9b74;
const ID_CUES: u32 = 0x1c53_bb6b;

#[derive(Clone, Copy, Debug)]
struct Cue {
    time_ns: i64,
    track: u64,
    cluster: u64,
    /// CueRelativePosition: offset of the block from the first byte after the cluster header.
    rel: Option<u64>,
}

/// A cluster whose header and timestamp have been read.
#[derive(Clone, Copy, Debug)]
struct ClusterInfo {
    /// First byte after the cluster header.
    body: u64,
    /// End of the cluster. For an unknown-size cluster this is the end of the segment until the walk
    /// meets the next level 1 element.
    end: u64,
    unknown: bool,
    ts: i128,
}

/// Bytes walked after a Cue jump: enough for the block the Cue names and a few that follow it.
const CUE_WINDOW: u64 = 128 * 1024;
/// Bytes walked per `index_after` call.
const AFTER_WINDOW: u64 = 1 << 20;

/// Incremental Matroska index: headers, SeekHead and Cues stored before the clusters are read when the
/// file is opened (Cues behind the clusters by the first seek, `ensure_cues`). Clusters
/// are walked block header by block header in bounded steps: `index_more` follows the file,
/// `index_at_cue` jumps to the block a Cue names. Block payloads are not read (except laced ones).
pub(crate) struct MkvLazy {
    seg_end: u64,
    pub tracks: Vec<crate::TrackInfo>,
    layout: Layout,
    pub steps: HashMap<u64, Vec<Step>>,
    pub omitted: Vec<String>,
    pub duration_ns: Option<u64>,
    cues: Vec<Cue>,
    /// Absolute position of a Cues element that has not been read yet (it sits behind the clusters).
    cues_pending: Option<u64>,
    seg_start: u64,
    /// First level 1 element after the metadata (where the walk starts).
    origin: u64,
    /// Where `index_more` continues.
    next: u64,
    /// Clusters met so far, by position of their ID.
    clusters: BTreeMap<u64, ClusterInfo>,
    /// Indexed byte ranges (sorted, disjoint, merged), all on element boundaries.
    done: Vec<(u64, u64)>,
    pub complete: bool,
    /// Places where data was skipped: a cluster that could not be read to its end (corrupt or cut short) or a
    /// single block inside one.
    pub damaged: usize,
}

impl MkvLazy {
    pub fn open(src: &dyn ByteSource) -> Result<Self, DemuxError> {
        let size = src.len();
        let ebml = header_at(src, 0, size)?;
        if ebml.id != 0x1a45_dfa3 {
            return Err(DemuxError::Format("not a Matroska file"));
        }
        let mut pos = ebml.payload_range(0, size)?.end;
        let seg = header_at(src, pos, size)?;
        if seg.id != ID_SEGMENT {
            return Err(DemuxError::Format("missing Segment"));
        }
        let seg_start = pos + seg.header_bytes as u64;
        let seg_end = seg.payload_bytes.map_or(size, |n| (seg_start + n).min(size));
        pos = seg_start;

        let mut scale = 1_000_000u64;
        let mut duration_ticks = None;
        let mut parsed: Option<(Vec<crate::TrackInfo>, HashMap<u64, Vec<Step>>, Vec<String>)> = None;
        let mut cues = Vec::new();
        let mut cues_at = None;
        while pos + 2 <= seg_end {
            let Ok(h) = header_at(src, pos, seg_end) else { break };
            let start = pos + h.header_bytes as u64;
            let end = h.payload_bytes.map_or(seg_end, |n| (start + n).min(seg_end));
            match h.id {
                ID_INFO => {
                    let info = read_payload(src, start, end, MAX_METADATA)?;
                    scale = timestamp_scale(&info)?;
                    let mut view: &[u8] = &info;
                    while !view.is_empty() {
                        let Ok((id, bytes)) = element(&mut view) else { break };
                        if id == 0x4489 {
                            duration_ticks = float(bytes);
                        }
                    }
                }
                ID_TRACKS => {
                    let data = read_payload(src, start, end, MAX_METADATA)?;
                    parsed = Some(build_tracks(&data)?);
                }
                ID_SEEKHEAD => {
                    if let Ok(data) = read_payload(src, start, end, MAX_METADATA) {
                        cues_at = cues_at.or(seek_position(&data, ID_CUES));
                    }
                }
                ID_CUES => {
                    if let Ok(data) = read_payload(src, start, end, 32 << 20) {
                        cues = parse_cues(&data, scale);
                        cues_at = None;
                    }
                }
                ID_CLUSTER => break,
                _ => {}
            }
            pos = end;
        }
        let (tracks, steps, omitted) = parsed.ok_or(DemuxError::Format("no Tracks element"))?;
        if tracks.is_empty() {
            return Err(DemuxError::Format("no usable track"));
        }
        for c in &mut cues {
            c.cluster += seg_start;
        }
        // Cues behind the clusters are only needed by a seek. Reading them here made the first frame of a remote
        // file wait for a second request to the end of the file (measured 280 ms of 930 ms at 75 ms to the first byte).
        let cues_pending = if cues.is_empty() { cues_at.map(|rel| seg_start + rel) } else { None };
        let layout = Layout { scale, defaults: tracks.iter().map(|t| (t.id, t.default_duration_ns)).collect() };
        Ok(Self {
            seg_end,
            tracks,
            layout,
            steps,
            omitted,
            duration_ns: duration_ticks.map(|d| (d * scale as f64) as u64),
            cues,
            cues_pending,
            seg_start,
            origin: pos,
            next: pos,
            clusters: BTreeMap::new(),
            done: Vec::new(),
            complete: false,
            damaged: 0,
        })
    }

    pub fn has_cues(&self) -> bool {
        !self.cues.is_empty()
    }

    /// Cues exist behind the clusters and have not been read yet.
    pub fn cues_pending(&self) -> bool {
        self.cues_pending.is_some()
    }

    /// End of the span indexed without a gap from the first cluster, `None` before anything is indexed.
    pub fn head_end(&self) -> Option<u64> {
        self.done_end(self.origin)
    }

    /// Reads the Cues the SeekHead pointed at, once, when the first seek asks for them. A source error is
    /// returned so the caller can retry; anything else leaves the file without Cues (sequential seek).
    pub fn ensure_cues(&mut self, src: &dyn ByteSource) -> Result<(), DemuxError> {
        let Some(at) = self.cues_pending else { return Ok(()) };
        let read = (|| -> Result<Vec<Cue>, DemuxError> {
            let h = header_at(src, at, self.seg_end)?;
            if h.id != ID_CUES {
                return Ok(Vec::new());
            }
            let start = at + h.header_bytes as u64;
            let end = h.payload_bytes.map_or(self.seg_end, |n| (start + n).min(self.seg_end));
            Ok(parse_cues(&read_payload(src, start, end, 32 << 20)?, self.layout.scale))
        })();
        match read {
            Err(DemuxError::Source(e)) => return Err(DemuxError::Source(e)),
            Err(_) => {}
            Ok(mut cues) => {
                for c in &mut cues {
                    c.cluster += self.seg_start;
                }
                self.cues = cues;
            }
        }
        self.cues_pending = None;
        Ok(())
    }

    /// Start of the first span that has not been indexed, `None` when the whole segment has.
    fn first_gap(&self) -> Option<u64> {
        if self.origin + 2 > self.seg_end {
            return None;
        }
        match self.done.first() {
            None => Some(self.origin),
            Some(&(a, _)) if a > self.origin => Some(self.origin),
            Some(&(_, b)) => (b + 2 <= self.seg_end).then_some(b),
        }
    }

    /// End of the indexed range that contains `pos`, if any.
    fn done_end(&self, pos: u64) -> Option<u64> {
        let i = self.done.partition_point(|&(a, _)| a <= pos);
        i.checked_sub(1).map(|i| self.done[i]).filter(|&(_, b)| pos < b).map(|(_, b)| b)
    }

    /// Start of the first indexed range above `pos` (the segment end when there is none).
    fn next_done_start(&self, pos: u64) -> u64 {
        let i = self.done.partition_point(|&(a, _)| a <= pos);
        self.done.get(i).map_or(self.seg_end, |&(a, _)| a)
    }

    fn add_done(&mut self, a: u64, b: u64) {
        if b <= a {
            return;
        }
        let mut lo = a;
        let mut hi = b;
        let i = self.done.partition_point(|&(_, e)| e < a);
        while i < self.done.len() && self.done[i].0 <= hi {
            lo = lo.min(self.done[i].0);
            hi = hi.max(self.done[i].1);
            self.done.remove(i);
        }
        self.done.insert(i, (lo, hi));
        if self.first_gap().is_none() {
            self.complete = true;
        }
    }

    /// Follows the file for about `max_bytes`, indexing what it passes (block headers only).
    pub fn index_more(&mut self, src: &dyn ByteSource, max_bytes: u64) -> Result<Vec<MkvPacket>, DemuxError> {
        let mut out = Vec::new();
        let mut spent = 0u64;
        while !self.complete && spent < max_bytes {
            if let Some(e) = self.done_end(self.next) {
                self.next = e;
                continue;
            }
            if self.next + 2 > self.seg_end {
                match self.first_gap() {
                    Some(g) => self.next = g,
                    None => self.complete = true,
                }
                continue;
            }
            let from = self.next;
            let stop_at = self.next_done_start(from);
            let reached = self.walk(src, from, stop_at, max_bytes - spent, &mut out)?;
            self.add_done(from, reached);
            self.next = reached;
            spent += reached.saturating_sub(from);
            if out.len() > MAX_PACKETS {
                return Err(DemuxError::Limit("packet count"));
            }
        }
        Ok(self.finish_batch(out))
    }

    /// Walks element headers from `from` (an element boundary) until `stop_at`, the end of the
    /// segment or about `budget` bytes. Returns the position reached, also an element boundary.
    /// A block that cannot be taken is skipped; damage to the element framing inside a cluster keeps what was
    /// read before it and skips the rest of the cluster.
    fn walk(&mut self, src: &dyn ByteSource, from: u64, stop_at: u64, budget: u64, out: &mut Vec<MkvPacket>) -> Result<u64, DemuxError> {
        let mut pos = from;
        while pos < stop_at && pos + 2 <= self.seg_end && pos - from < budget {
            let ctx = self.clusters.range(..=pos).next_back().map(|(&k, &c)| (k, c)).filter(|(_, c)| pos >= c.body && pos < c.end);
            let Some((cstart, c)) = ctx else {
                let h = match header_at(src, pos, self.seg_end) {
                    Ok(h) => h,
                    Err(e @ DemuxError::Source(_)) => return Err(e),
                    // Cut inside an element header.
                    Err(_) => return Ok(self.seg_end),
                };
                let start = pos + h.header_bytes as u64;
                let end = h.payload_bytes.map_or(self.seg_end, |n| (start + n).min(self.seg_end));
                if h.id == ID_CLUSTER {
                    if !self.clusters.contains_key(&pos) {
                        match self.register_cluster(src, pos, start, end, h.payload_bytes.is_none()) {
                            Ok(()) => {}
                            Err(e @ DemuxError::Source(_)) => return Err(e),
                            Err(_) => {
                                self.damaged += 1;
                                pos = end;
                                continue;
                            }
                        }
                    }
                    pos = start;
                } else {
                    pos = end;
                }
                continue;
            };
            if pos + 2 > c.end {
                pos = c.end;
                continue;
            }
            let h = match header_at(src, pos, c.end) {
                Ok(h) => h,
                Err(e @ DemuxError::Source(_)) => return Err(e),
                Err(_) => {
                    self.damaged += 1;
                    pos = c.end;
                    continue;
                }
            };
            if c.unknown && LEVEL1.contains(&h.id) {
                if let Some(info) = self.clusters.get_mut(&cstart) {
                    info.end = pos;
                    info.unknown = false;
                }
                continue;
            }
            let body = pos + h.header_bytes as u64;
            let body_end = match h.payload_bytes.map(|n| body.saturating_add(n)).filter(|&e| e <= c.end) {
                Some(e) => e,
                None => {
                    self.damaged += 1;
                    pos = c.end;
                    continue;
                }
            };
            let before = out.len();
            match self.element_in_cluster(src, &h, body, body_end, c.ts, out) {
                Ok(()) => {}
                Err(e @ DemuxError::Source(_)) => return Err(e),
                Err(DemuxError::Limit(m)) => return Err(DemuxError::Limit(m)),
                // The element's own size is intact (checked against the cluster above), so a block that cannot be
                // taken costs that block only. Giving up the cluster here lost every video picture stored behind
                // one odd audio block.
                Err(_) => {
                    out.truncate(before);
                    self.damaged += 1;
                }
            }
            pos = body_end;
        }
        Ok(pos.min(self.seg_end.max(from)))
    }

    fn element_in_cluster(&self, src: &dyn ByteSource, h: &ElementHeader, body: u64, body_end: u64, ts: i128, out: &mut Vec<MkvPacket>) -> Result<(), DemuxError> {
        match h.id {
            0xa3 => block(src, body, body_end, ts, &self.layout, None, None, out)?,
            0xa0 => {
                let (mut at, mut block_range, mut referenced, mut duration) = (body, None, false, None);
                while at + 2 <= body_end {
                    let g = header_at(src, at, body_end)?;
                    let g_body = at + g.header_bytes as u64;
                    let g_end = g_body + g.payload_bytes.ok_or(Error::UnknownSize)?;
                    if g_end > body_end {
                        return Err(Error::Truncated.into());
                    }
                    match g.id {
                        0xa1 => block_range = Some((g_body, g_end)),
                        0xfb => referenced = true,
                        0x9b => duration = Some(uint(&read_payload(src, g_body, g_end, 8)?)?),
                        _ => {}
                    }
                    at = g_end;
                }
                let (b, e) = block_range.ok_or(Error::InvalidBlock)?;
                block(src, b, e, ts, &self.layout, Some(!referenced), duration, out)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Reads the Timestamp of the cluster at `at` (its first children) and remembers the cluster.
    fn register_cluster(&mut self, src: &dyn ByteSource, at: u64, body: u64, end: u64, unknown: bool) -> Result<(), DemuxError> {
        let mut pos = body;
        for _ in 0..32 {
            if pos + 2 > end {
                break;
            }
            let h = header_at(src, pos, end)?;
            if unknown && LEVEL1.contains(&h.id) {
                break;
            }
            let b = pos + h.header_bytes as u64;
            let e = b + h.payload_bytes.ok_or(Error::UnknownSize)?;
            if e > end {
                return Err(Error::Truncated.into());
            }
            match h.id {
                0xe7 => {
                    let ts = i128::from(uint(&read_payload(src, b, e, 8)?)?);
                    self.clusters.insert(at, ClusterInfo { body, end, unknown, ts });
                    return Ok(());
                }
                0xa3 | 0xa0 => break,
                _ => pos = e,
            }
        }
        Err(Error::InvalidTimestamp.into())
    }

    fn finish_batch(&self, mut out: Vec<MkvPacket>) -> Vec<MkvPacket> {
        fill_durations(&mut out, !self.complete);
        out
    }

    /// Indexes a short window at the block a Cue names (`rel` is its CueRelativePosition, otherwise the
    /// start of the cluster at `pos`). Returns the new packets and the window `[a, b)` that holds the
    /// block, which may have been indexed before.
    pub fn index_at_cue(&mut self, src: &dyn ByteSource, pos: u64, rel: Option<u64>) -> Result<(Vec<MkvPacket>, (u64, u64)), DemuxError> {
        if !self.clusters.contains_key(&pos) {
            let h = header_at(src, pos, self.seg_end)?;
            if h.id != ID_CLUSTER {
                return Err(DemuxError::Format("cue does not point at a cluster"));
            }
            let body = pos + h.header_bytes as u64;
            let end = h.payload_bytes.map_or(self.seg_end, |n| (body + n).min(self.seg_end));
            self.register_cluster(src, pos, body, end, h.payload_bytes.is_none())?;
        }
        let c = self.clusters[&pos];
        let mut start = c.body;
        if let Some(r) = rel {
            let at = c.body.saturating_add(r);
            if at + 2 <= c.end {
                // Only a block element is a valid landing place; anything else means a stale Cue.
                if let Ok(h) = header_at(src, at, c.end) {
                    if matches!(h.id, 0xa3 | 0xa0) {
                        start = at;
                    }
                }
            }
        }
        if let Some(e) = self.done_end(start) {
            return Ok((Vec::new(), (start, e)));
        }
        let mut out = Vec::new();
        let stop_at = self.next_done_start(start);
        let reached = self.walk(src, start, stop_at, CUE_WINDOW, &mut out)?;
        self.add_done(start, reached);
        if start >= self.next {
            // Jump ahead: playback continues after this window; the skipped span is indexed last.
            self.next = reached;
        }
        Ok((self.finish_batch(out), (start, reached)))
    }

    /// Indexes a step of the data stored right after the range that contains `offset`, when that has
    /// not been indexed yet. Used to close the hole a seek leaves behind.
    pub fn index_after(&mut self, src: &dyn ByteSource, offset: u64) -> Result<Vec<MkvPacket>, DemuxError> {
        let Some(from) = self.done_end(offset) else { return Ok(Vec::new()) };
        if from + 2 > self.seg_end {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let stop_at = self.next_done_start(from);
        let reached = self.walk(src, from, stop_at, AFTER_WINDOW, &mut out)?;
        self.add_done(from, reached);
        Ok(self.finish_batch(out))
    }

    /// Cue targets, nearest first: entries at or before `time_ns` for `track` (or any track when it
    /// has none), as (cluster position, CueRelativePosition), then earlier ones.
    pub fn cue_targets_before(&self, track: u64, time_ns: i64) -> Vec<(u64, Option<u64>)> {
        let mut v: Vec<&Cue> = self.cues.iter().filter(|c| c.track == track && c.time_ns <= time_ns).collect();
        if v.is_empty() {
            v = self.cues.iter().filter(|c| c.time_ns <= time_ns).collect();
        }
        v.sort_by_key(|c| std::cmp::Reverse(c.time_ns));
        let mut seen: Vec<(u64, Option<u64>)> = Vec::new();
        for c in v.into_iter().take(64) {
            let t = (c.cluster, c.rel);
            if !seen.contains(&t) {
                seen.push(t);
            }
        }
        seen
    }
}

fn build_tracks(data: &[u8]) -> Result<(Vec<crate::TrackInfo>, HashMap<u64, Vec<Step>>, Vec<String>), DemuxError> {
    let (parsed, errors) = tracks_lenient(data)?;
    let mut omitted: Vec<String> = errors
        .iter()
        .map(|(n, e)| match n {
            Some(n) => format!("track {n}: {e:?}"),
            None => format!("track: {e:?}"),
        })
        .collect();
    let mut steps: HashMap<u64, Vec<Step>> = HashMap::new();
    let mut usable = Vec::new();
    for t in parsed {
        let mut list = Vec::new();
        let mut private = t.codec_private.map(<[u8]>::to_vec);
        let mut bad = None;
        for e in &t.encodings {
            if e.kind != 0 {
                bad = Some("encrypted");
                break;
            }
            let step = match e.comp_algo {
                3 => Step::Strip(e.comp_settings.map(<[u8]>::to_vec).unwrap_or_default()),
                0 => Step::Zlib,
                1 => {
                    bad = Some("bzip2 compression");
                    break;
                }
                _ => {
                    bad = Some("lzo compression");
                    break;
                }
            };
            if e.scope & 2 != 0 {
                if let Some(p) = private.take() {
                    private = decode_steps(std::slice::from_ref(&step), p).ok();
                }
            }
            if e.scope & 1 != 0 {
                list.push(step);
            }
        }
        if let Some(why) = bad {
            omitted.push(format!("track {}: {why}", t.number));
            continue;
        }
        if !list.is_empty() {
            steps.insert(t.number, list);
        }
        usable.push((t, private));
    }
    let mut infos = Vec::new();
    for (t, private) in &usable {
        let mut info = crate::tracks_from_mkv(std::slice::from_ref(t), 1)[0].clone();
        if let Some(p) = private {
            info.codec_private = p.clone();
        }
        infos.push(info);
    }
    Ok((infos, steps, omitted))
}

fn seek_position(seekhead: &[u8], wanted: u32) -> Option<u64> {
    let mut view = seekhead;
    while !view.is_empty() {
        let (id, mut seek) = element(&mut view).ok()?;
        if id != 0x4dbb {
            continue;
        }
        let (mut sid, mut pos) = (None, None);
        while !seek.is_empty() {
            match element(&mut seek).ok()? {
                (0x53ab, b) => sid = Some(b.iter().fold(0u32, |v, &x| (v << 8) | u32::from(x))),
                (0x53ac, b) => pos = uint(b).ok(),
                _ => {}
            }
        }
        if sid == Some(wanted) {
            return pos;
        }
    }
    None
}

fn parse_cues(data: &[u8], scale: u64) -> Vec<Cue> {
    let mut out = Vec::new();
    let mut view = data;
    while !view.is_empty() {
        let Ok((id, mut point)) = element(&mut view) else { break };
        if id != 0xbb {
            continue;
        }
        let mut time = None;
        let mut positions: Vec<(u64, u64, Option<u64>)> = Vec::new();
        while !point.is_empty() {
            let Ok((id, bytes)) = element(&mut point) else { break };
            match id {
                0xb3 => time = uint(bytes).ok(),
                0xb7 => {
                    let (mut track, mut cluster, mut rel) = (None, None, None);
                    let mut b = bytes;
                    while !b.is_empty() {
                        let Ok((id, v)) = element(&mut b) else { break };
                        match id {
                            0xf7 => track = uint(v).ok(),
                            0xf1 => cluster = uint(v).ok(),
                            0xf0 => rel = uint(v).ok(),
                            _ => {}
                        }
                    }
                    if let (Some(t), Some(c)) = (track, cluster) {
                        positions.push((t, c, rel));
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = time {
            let Some(time_ns) = t.checked_mul(scale).and_then(|v| i64::try_from(v).ok()) else { continue };
            for (track, cluster, rel) in positions {
                out.push(Cue { time_ns, track, cluster, rel });
            }
        }
    }
    out
}

pub fn scan(src: &dyn ByteSource) -> Result<MkvScan, DemuxError> {
    let mut lazy = MkvLazy::open(src)?;
    let mut packets = Vec::new();
    while !lazy.complete {
        packets.extend(lazy.index_more(src, u64::MAX / 2)?);
    }
    Ok(MkvScan { tracks: lazy.tracks, packets, duration_ns: lazy.duration_ns })
}

#[allow(clippy::too_many_arguments)]
fn block(
    src: &dyn ByteSource,
    start: u64,
    end: u64,
    cluster_ts: i128,
    layout: &Layout,
    group_keyframe: Option<bool>,
    block_duration: Option<u64>,
    out: &mut Vec<MkvPacket>,
) -> Result<(), DemuxError> {
    let len = end - start;
    let mut head = [0u8; 16];
    let n = len.min(16) as usize;
    read_exact_at(src, start, &mut head[..n])?;
    let mut view = &head[..n];
    let (track, _) = vint(&mut view, false, 8)?;
    let consumed = n - view.len();
    let (flags_at, flags) = (consumed + 2, *view.get(2).ok_or(Error::Truncated)?);
    let rel = i16::from_be_bytes([view[0], view[1]]);
    let data_start = start + flags_at as u64 + 1;
    if track == 0 || data_start >= end {
        return Err(Error::InvalidBlock.into());
    }
    let ts = (cluster_ts + i128::from(rel))
        .checked_mul(i128::from(layout.scale))
        .and_then(|v| i64::try_from(v).ok())
        .ok_or(Error::InvalidTimestamp)?;
    let keyframe = group_keyframe.unwrap_or(flags & 0x80 != 0);
    let default = layout.defaults.iter().find(|d| d.0 == track).and_then(|d| d.1);
    if flags & 0x06 == 0 {
        out.push(MkvPacket {
            track,
            pts_ns: ts,
            duration_ns: block_duration.map(|d| d.saturating_mul(layout.scale)).or(default),
            keyframe,
            offset: data_start,
            size: u32::try_from(end - data_start).map_err(|_| DemuxError::Limit("block size"))?,
        });
        return Ok(());
    }
    if len > MAX_LACED_BLOCK {
        return Err(DemuxError::Limit("laced block"));
    }
    let whole = read_payload(src, start, end, MAX_LACED_BLOCK)?;
    let after_flags = &whole[flags_at + 1..];
    let (&count_minus_one, frames) = after_flags.split_first().ok_or(Error::InvalidBlock)?;
    let count = usize::from(count_minus_one) + 1;
    if count < 2 {
        return Err(Error::InvalidBlock.into());
    }
    let mut sizes = [0usize; 256];
    let data = lace_sizes(frames, flags & 0x06, &mut sizes[..count])?;
    let mut offset = end - data.len() as u64;
    // A track without DefaultDuration can still be laced: mkvmerge writes TrueHD that way (the first block of
    // each TrueHD track of a UHD remux holds two access units). Refusing the block made the walk give up the
    // whole cluster, every video picture behind it included. Without a BlockDuration either, the frames keep
    // the block's time here and `fill_durations` spreads them up to the next block of the track.
    let duration = default.or_else(|| block_duration.map(|d| d.saturating_mul(layout.scale) / count as u64).filter(|&d| d > 0));
    for (i, &size) in sizes[..count].iter().enumerate() {
        out.push(MkvPacket {
            track,
            pts_ns: ts + (i as u64 * duration.unwrap_or(0)) as i64,
            duration_ns: duration,
            keyframe: keyframe && i == 0,
            offset,
            size: size as u32,
        });
        offset += size as u64;
    }
    Ok(())
}

/// Packets without a declared duration take the gap to the next presentation
/// time on the same track. The last one stays unknown, or repeats its predecessor
/// when `more_follows` (the next batch will not fix it).
fn fill_durations(packets: &mut [MkvPacket], more_follows: bool) {
    let mut by_track: std::collections::BTreeMap<u64, Vec<usize>> = Default::default();
    for (i, p) in packets.iter().enumerate() {
        by_track.entry(p.track).or_default().push(i);
    }
    for indices in by_track.values_mut() {
        indices.sort_by_key(|&i| packets[i].pts_ns);
        // Frames of one lace that came without any duration (same time, stored back to back) share the span up
        // to the next packet of the track. With no packet behind them in this batch they stay at the block's time.
        let mut i = 0;
        while i < indices.len() {
            let mut j = i + 1;
            while j < indices.len() && {
                let (a, b) = (&packets[indices[j - 1]], &packets[indices[j]]);
                a.duration_ns.is_none() && b.duration_ns.is_none() && a.pts_ns == b.pts_ns && a.offset + u64::from(a.size) == b.offset
            } {
                j += 1;
            }
            if let (true, Some(&next)) = (j - i > 1, indices.get(j)) {
                let start = packets[indices[i]].pts_ns;
                let step = (packets[next].pts_ns - start).max(0) as u64 / (j - i) as u64;
                for (k, &at) in indices[i..j].iter().enumerate() {
                    packets[at].pts_ns = start + (k as u64 * step) as i64;
                    packets[at].duration_ns = Some(step);
                }
            }
            i = j;
        }
        for w in indices.windows(2) {
            if packets[w[0]].duration_ns.is_none() {
                packets[w[0]].duration_ns = Some((packets[w[1]].pts_ns - packets[w[0]].pts_ns).max(0) as u64);
            }
        }
        if more_follows && indices.len() >= 2 {
            let last = indices[indices.len() - 1];
            if packets[last].duration_ns.is_none() {
                packets[last].duration_ns = packets[indices[indices.len() - 2]].duration_ns;
            }
        }
    }
}
