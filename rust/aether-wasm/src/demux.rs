//! Demux exports. The source is the host: `aether_host_read` is imported from the
//! JS side and must answer synchronously (from a worker that owns a prefetch cache, or
//! a `FileReaderSync` / `Atomics.wait` bridge).

use crate::{input, set_error, set_result, with_result};
use aether_demux::{BufferedSource, ByteSource, Demuxer, PacketRef, TrackKind};
use std::cell::RefCell;

// Explicit import module: without it the link fails with `undefined symbol` on rustc 1.99.1 (1.92.0 accepted it).
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// Copies up to `len` bytes at `offset` into linear memory at `ptr`; returns the byte
    /// count, or a negative value on failure.
    fn aether_host_read(offset: u64, ptr: *mut u8, len: u32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn aether_host_read(_offset: u64, _ptr: *mut u8, _len: u32) -> i32 {
    -1
}

struct HostSource {
    len: u64,
}

impl ByteSource for HostSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let n = unsafe { aether_host_read(offset, buf.as_mut_ptr(), buf.len().min(u32::MAX as usize) as u32) };
        usize::try_from(n).map_err(|_| format!("host read failed at {offset}"))
    }
}

struct Open {
    source: BufferedSource<HostSource>,
    demuxer: Demuxer,
    /// Index into `packets()` of the next packet `aether_demux_next` delivers. Indexing can insert
    /// packets before it, so it is re-located by identity after every indexing step.
    cursor: usize,
    /// The packet delivered last, `None` after a seek or at the start.
    last: Option<PacketRef>,
    /// `last` for which the hole check already ran (so it runs once per packet).
    hole_checked: Option<PacketKey>,
    /// Packets delivered since the last seek that may still be revisited by `reindex` (recent ones).
    delivered: std::collections::BTreeSet<PacketKey>,
    /// Storage offset of the packet a seek started at (nothing earlier is "late"), 0 at the start.
    floor: u64,
    /// After a seek that starts in front of its keyframe for the sound (`Demuxer::audio_start`): the keyframe's
    /// storage offset. Before it only audio packets are delivered.
    audio_only_before: Option<u64>,
}

type PacketKey = (u64, u32, u64);

fn key(p: &PacketRef) -> PacketKey {
    (p.offset, p.inner, p.track)
}

/// Bytes indexed per step when `aether_demux_next` runs out of indexed packets.
const INDEX_STEP: u64 = 1 << 20;
/// A jump in storage position beyond the last packet by more than this (plus twice its size) may hide
/// a span that a seek skipped; `Demuxer::index_after` then fills it.
const HOLE_BYTES: u64 = 256 * 1024;

/// How far before the last delivered packet a newly indexed packet still counts as "late": a PES that
/// finished after later ones (of other tracks) were listed sorts before them. The indexer lists a packet
/// once its end is seen, so such packets appear in a later indexing step.
const LATE_BYTES: u64 = 8 << 20;

impl Open {
    /// Runs `step` (an indexing call that may insert packets anywhere) and puts the cursor on the first
    /// packet not delivered yet: the one it pointed at, a late completion just before the last delivered
    /// packet, or just after `last` when it pointed past the end.
    fn reindex<T>(&mut self, step: impl FnOnce(&mut Demuxer, &BufferedSource<HostSource>) -> T) -> T {
        let at = self.demuxer.packets().get(self.cursor).map(key);
        let out = step(&mut self.demuxer, &self.source);
        let all = self.demuxer.packets();
        let moved = match (at, &self.last) {
            (Some(k), _) => all.iter().position(|p| key(p) == k).unwrap_or(self.cursor.min(all.len())),
            (None, Some(l)) => {
                let k = key(l);
                all.iter().position(|p| key(p) == k).map_or(all.len(), |i| i + 1)
            }
            (None, None) => 0,
        };
        let late = self.last.as_ref().and_then(|l| {
            let lo = l.offset.saturating_sub(LATE_BYTES).max(self.floor);
            let from = all.partition_point(|p| p.offset < lo);
            all[from..moved.min(all.len())].iter().position(|p| !self.delivered.contains(&key(p))).map(|i| from + i)
        });
        self.cursor = late.map_or(moved, |i| i.min(moved));
        out
    }

    fn mark_delivered(&mut self, p: &PacketRef) {
        self.delivered.insert(key(p));
        if self.delivered.len() > 8192 {
            let lo = p.offset.saturating_sub(2 * LATE_BYTES);
            self.delivered = self.delivered.split_off(&(lo, 0, 0));
        }
    }
}

thread_local! {
    static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) };
}

fn no_demuxer() -> i32 {
    set_error("no demuxer open");
    -1
}

const RECORD_BYTES: usize = 48;

/// Opens the file of `len` bytes behind `aether_host_read`, reading headers only (`Demuxer::open_lazy`).
/// Packets are indexed on demand by `aether_demux_next`, `aether_demux_index_more` and `aether_demux_seek`.
/// Returns 0 or a negative status.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_open(len: u64) -> i32 {
    let source = BufferedSource::new(HostSource { len }, 256 * 1024);
    match Demuxer::open_lazy(&source) {
        Ok(demuxer) => {
            OPEN.with_borrow_mut(|o| *o = Some(Open { source, demuxer, cursor: 0, last: None, hole_checked: None, delivered: Default::default(), floor: 0, audio_only_before: None }));
            0
        }
        Err(e) => {
            set_error(e.to_string());
            set_result(e.to_string().into_bytes());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_close() {
    OPEN.with_borrow_mut(|o| *o = None);
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Result: JSON `{container, durationNs, omittedTracks, tracks:[{id, kind, codec, codecPrivate
/// (hex), dolbyVision:{boxType, record(hex)}|null, color:{primaries, transfer, matrix, fullRange, mastering, contentLight}|null, timescale, language, name, channels,
/// sampleRate, bitDepth, width, height, defaultDurationNs, codecString, fallbackCodecString,
/// jocComplexity, sampleDescriptions, dolbyVisionEl(hex)|null}]}`; `container` level also has
/// `fragmented`, `complete` (every packet indexed), `omittedTrackReasons` (strings, Matroska) and
/// `timelineBreaks` (`[{offset, shiftTicks}]`, MPEG-TS, 90 kHz ticks). `container` is `mp4`, `matroska`,
/// `mpegts` or `raw` (AC-3 / E-AC-3, FLAC, DTS, TrueHD / MLP, WAV, ADTS: one audio track, timescale = sample rate). Tracks also carry `sourcePid` (TS, or null), `enhancementLayer` and `codecDelayNs`
/// (or null). `omittedTracks` counts unusable tracks. For MPEG-TS the track list and the breaks grow
/// while indexing: compare `aether_demux_revision`. Returns 0.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_info() -> i32 {
    OPEN.with_borrow(|o| {
        let Some(o) = o else { return no_demuxer() };
        let d = &o.demuxer;
        let opt = |v: Option<u64>| v.map_or("null".to_owned(), |v| v.to_string());
        let mut s = format!(
            "{{\"container\":\"{}\",\"fragmented\":{},\"durationNs\":{},\"omittedTracks\":{},\"tracks\":[",
            match d.container() {
                aether_demux::Container::Mp4 => "mp4",
                aether_demux::Container::Matroska => "matroska",
                aether_demux::Container::MpegTs => "mpegts",
                aether_demux::Container::Raw => "raw",
            },
            d.fragmented(),
            opt(d.duration_ns()),
            d.omitted_tracks()
        );
        for (i, t) in d.tracks().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let kind = match t.kind {
                TrackKind::Video => "video",
                TrackKind::Audio => "audio",
                TrackKind::Subtitle => "subtitle",
                TrackKind::Other => "other",
            };
            s.push_str(&format!("{{\"id\":{},\"kind\":\"{kind}\",\"codec\":", t.id));
            json_str(&mut s, &t.codec);
            s.push_str(&format!(",\"codecPrivate\":\"{}\",\"dolbyVision\":", hex(&t.codec_private)));
            match &t.dolby_vision {
                Some(dv) => {
                    s.push_str("{\"boxType\":");
                    json_str(&mut s, &String::from_utf8_lossy(&dv.box_type));
                    s.push_str(&format!(",\"record\":\"{}\"}}", hex(&dv.bytes)));
                }
                None => s.push_str("null"),
            }
            s.push_str(",\"color\":");
            match &t.color {
                Some(c) => {
                    let code = |v: Option<u16>| opt(v.map(u64::from));
                    s.push_str(&format!(
                        "{{\"primaries\":{},\"transfer\":{},\"matrix\":{},\"fullRange\":{},\"mastering\":",
                        code(c.primaries),
                        code(c.transfer),
                        code(c.matrix),
                        c.full_range.map_or("null".to_owned(), |v| v.to_string())
                    ));
                    match &c.mastering {
                        Some(m) => s.push_str(&format!("{m:?}").replace(' ', "")),
                        None => s.push_str("null"),
                    }
                    match c.content_light {
                        Some((cll, fall)) => s.push_str(&format!(",\"contentLight\":[{cll},{fall}]}}")),
                        None => s.push_str(",\"contentLight\":null}"),
                    }
                }
                None => s.push_str("null"),
            }
            s.push_str(&format!(",\"timescale\":{},\"language\":", t.timescale));
            match &t.language {
                Some(l) => json_str(&mut s, l),
                None => s.push_str("null"),
            }
            s.push_str(",\"name\":");
            match &t.name {
                Some(l) => json_str(&mut s, l),
                None => s.push_str("null"),
            }
            let n = |v: Option<u32>| opt(v.map(u64::from));
            for (key, v) in [("codecString", &t.codec_string), ("fallbackCodecString", &t.fallback_codec_string)] {
                s.push_str(&format!(",\"{key}\":"));
                match v {
                    Some(v) => json_str(&mut s, v),
                    None => s.push_str("null"),
                }
            }
            s.push_str(&format!(
                ",\"jocComplexity\":{},\"sampleDescriptions\":{},\"dolbyVisionEl\":",
                opt(t.joc_complexity.map(u64::from)),
                t.sample_descriptions
            ));
            match &t.dolby_vision_el {
                Some(b) => s.push_str(&format!("\"{}\"", hex(b))),
                None => s.push_str("null"),
            }
            s.push_str(&format!(
                ",\"channels\":{},\"sampleRate\":{},\"bitDepth\":{},\"width\":{},\"height\":{},\"defaultDurationNs\":{}",
                n(t.channels), n(t.sample_rate), n(t.bit_depth), n(t.width), n(t.height), opt(t.default_duration_ns)
            ));
            s.push_str(&format!(
                ",\"sourcePid\":{},\"enhancementLayer\":{},\"codecDelayNs\":{}}}",
                opt(t.source_pid.map(u64::from)),
                t.enhancement_layer,
                opt(t.codec_delay_ns)
            ));
        }
        s.push_str(&format!("],\"complete\":{},\"timelineBreaks\":[", d.is_complete()));
        for (i, b) in d.timeline_breaks().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{{\"offset\":{},\"shiftTicks\":{}}}", b.offset, b.shift));
        }
        s.push_str("],\"omittedTrackReasons\":[");
        for (i, r) in d.omitted_track_reasons().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            json_str(&mut s, r);
        }
        s.push_str("]}");
        set_result(s.into_bytes());
        0
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_packet_count() -> u32 {
    OPEN.with_borrow(|o| o.as_ref().map_or(0, |o| o.demuxer.packets().len() as u32))
}

fn record(p: &PacketRef, out: &mut Vec<u8>) {
    out.extend(p.track.to_le_bytes());
    out.extend(p.pts.to_le_bytes());
    out.extend(p.dts.to_le_bytes());
    out.extend(p.duration.to_le_bytes());
    out.extend(p.offset.to_le_bytes());
    out.extend(p.size.to_le_bytes());
    out.extend((u32::from(p.keyframe) | u32::from(p.presentation) << 1).to_le_bytes());
}

/// Result: `count` records of 48 bytes, LE: `track u64, pts i64, dts i64, duration i64,
/// offset u64, size u32, flags u32` (bit 0 keyframe, bit 1 presentation). Returns the
/// number of records written.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_packets(start: u32, count: u32) -> i32 {
    OPEN.with_borrow(|o| {
        let Some(o) = o else { return no_demuxer() };
        let all = o.demuxer.packets();
        let from = (start as usize).min(all.len());
        let to = from.saturating_add(count as usize).min(all.len());
        let mut out = Vec::with_capacity((to - from) * RECORD_BYTES);
        for p in &all[from..to] {
            record(p, &mut out);
        }
        set_result(out);
        (to - from) as i32
    })
}

/// Result: the payload of packet `index` (storage order). Returns its length.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_read(index: u32) -> i32 {
    OPEN.with_borrow(|o| {
        let Some(o) = o else { return no_demuxer() };
        let Some(p) = o.demuxer.packets().get(index as usize) else {
            set_error(format!("no packet {index} (of {})", o.demuxer.packets().len()));
            return -2;
        };
        with_result(|buf| match o.demuxer.read_packet(&o.source, p, buf) {
            Ok(()) => buf.len() as i32,
            Err(e) => {
                set_error(format!("reading packet {index} failed: {e}"));
                -3
            }
        })
    })
}

/// Indexes about `max_bytes` more of the file. Returns the number of packets added (indices returned
/// earlier by `aether_demux_packets` are then stale), or a negative status.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_index_more(max_bytes: u64) -> i32 {
    OPEN.with_borrow_mut(|o| {
        let Some(o) = o else { return no_demuxer() };
        match o.reindex(|d, s| d.index_more(s, max_bytes)) {
            Ok(n) => n.min(i32::MAX as usize) as i32,
            Err(e) => {
                set_error(format!("indexing failed: {e}"));
                -2
            }
        }
    })
}

/// 1 when every packet of the file is indexed, 0 when not, negative without a demuxer.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_complete() -> i32 {
    OPEN.with_borrow(|o| o.as_ref().map_or(-1, |o| i32::from(o.demuxer.is_complete())))
}

/// Matroska: places where the index skipped data so far (a bad block, the rest of a corrupt cluster), 0 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_damaged() -> u32 {
    OPEN.with_borrow(|o| o.as_ref().map_or(0, |o| o.demuxer.damaged_spans() as u32))
}

/// Changes whenever `aether_demux_info` would report a different track list or timeline breaks
/// (MPEG-TS discovers both while indexing): `tracks << 16 | breaks` (breaks saturate at 65535).
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_revision() -> u32 {
    OPEN.with_borrow(|o| {
        o.as_ref().map_or(0, |o| (o.demuxer.tracks().len() as u32) << 16 | (o.demuxer.timeline_breaks().len().min(0xffff) as u32))
    })
}

/// Sequential delivery in storage order from the cursor (start of the file, or the keyframe the last
/// `aether_demux_seek` chose), indexing more of the file whenever it runs out of indexed packets.
/// Returns 1 with the result `48 byte record + payload` (the record layout of `aether_demux_packets`,
/// its `size` field is the payload length actually returned, which for Matroska content encodings
/// differs from the stored size), 0 at the end of the file, or a negative status.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_next() -> i32 {
    next_packet(true)
}

/// Like `aether_demux_next` but the result is the 48 byte record only; its `size` field is the stored
/// payload length. The payload is fetched later with `aether_demux_payload` (a host that only needs
/// timestamps ahead of time, such as the Matroska dts reorder window, then reads no payload bytes it
/// might never use over a remote source).
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_next_header() -> i32 {
    next_packet(false)
}

/// Result: the payload of the packet at `offset` of `size` stored bytes on `track` (the fields of a
/// record from `aether_demux_next_header`), content encodings undone. Matroska only. Returns its length.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_payload(track: u64, offset: u64, size: u32) -> i32 {
    OPEN.with_borrow(|o| {
        let Some(o) = o else { return no_demuxer() };
        let p = PacketRef { track, pts: 0, dts: 0, duration: 0, keyframe: false, presentation: true, offset, size, inner: 0 };
        with_result(|buf| match o.demuxer.read_packet(&o.source, &p, buf) {
            Ok(()) => buf.len() as i32,
            Err(e) => {
                set_error(format!("reading packet at offset {offset} failed: {e}"));
                -3
            }
        })
    })
}

fn next_packet(with_payload: bool) -> i32 {
    OPEN.with_borrow_mut(|o| {
        let Some(o) = o else { return no_demuxer() };
        loop {
            if let (Some(last), Some(q)) = (o.last.clone(), o.demuxer.packets().get(o.cursor)) {
                if !o.demuxer.is_complete()
                    && q.offset > last.offset.saturating_add(u64::from(last.size) * 2 + HOLE_BYTES)
                    && o.hole_checked != Some(key(&last))
                {
                    o.hole_checked = Some(key(&last));
                    if let Err(e) = o.reindex(|d, s| d.index_after(s, &last)) {
                        set_error(format!("indexing after offset {} failed: {e}", last.offset));
                        return -2;
                    }
                    continue;
                }
            }
            if let Some(p) = o.demuxer.packets().get(o.cursor).cloned() {
                if o.delivered.contains(&key(&p)) {
                    o.cursor += 1;
                    continue;
                }
                if let Some(key_offset) = o.audio_only_before {
                    if p.offset >= key_offset {
                        o.audio_only_before = None;
                    } else if !o.demuxer.tracks().iter().any(|t| t.id == p.track && t.kind == aether_demux::TrackKind::Audio) {
                        // Counted as delivered, so that a later indexing step does not bring it back as a late packet.
                        o.cursor += 1;
                        o.mark_delivered(&p);
                        continue;
                    }
                }
                let mut payload = Vec::new();
                if with_payload {
                    if let Err(e) = o.demuxer.read_packet(&o.source, &p, &mut payload) {
                        set_error(format!("reading packet at offset {} failed: {e}", p.offset));
                        return -3;
                    }
                }
                let mut rec = p.clone();
                if with_payload {
                    rec.size = payload.len() as u32;
                }
                with_result(|buf| {
                    buf.clear();
                    record(&rec, buf);
                    buf.extend_from_slice(&payload);
                });
                o.cursor += 1;
                o.mark_delivered(&p);
                o.last = Some(p);
                return 1;
            }
            if o.demuxer.is_complete() {
                return 0;
            }
            if let Err(e) = o.reindex(|d, s| d.index_more(s, INDEX_STEP)) {
                set_error(format!("indexing failed: {e}"));
                return -2;
            }
        }
    })
}

/// Index (into the current `packets()`, stale after any later indexing) of the verified keyframe at or
/// before `pts` on `track`; indexes around the target first (`Demuxer::seek_to`) and moves the
/// `aether_demux_next` cursor there.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_seek(track: u64, pts: i64) -> i32 {
    OPEN.with_borrow_mut(|o| {
        let Some(o) = o else { return no_demuxer() };
        match o.demuxer.seek_to(&o.source, track, pts) {
            Ok(i) => {
                // The sound that plays at the keyframe may be stored in front of it (MP4, MOV): delivery starts there.
                let start = o.demuxer.audio_start(i);
                o.audio_only_before = if start < i { o.demuxer.packets().get(i).map(|p| p.offset) } else { None };
                o.cursor = start;
                o.last = None;
                o.hole_checked = None;
                o.delivered.clear();
                o.floor = o.demuxer.packets().get(start).map_or(0, |p| p.offset);
                i as i32
            }
            Err(e) => {
                set_error(format!("seek on track {track} to {pts} failed: {e}"));
                -2
            }
        }
    })
}

/// 1: `aether_demux_seek` starts delivery at the sound that plays at the keyframe, wherever the container stored it
/// (`Demuxer::audio_start`). A host that sees this export does not have to seek earlier than its target to catch the sound.
#[unsafe(no_mangle)]
pub extern "C" fn aether_demux_seek_keeps_audio() -> i32 {
    1
}

/// Test helper for hosts that already hold the file in memory.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_demux_probe(ptr: *const u8, len: usize) -> i32 {
    let src = aether_demux::SliceSource(unsafe { input(ptr, len) });
    match Demuxer::open(&src) {
        Ok(d) => d.tracks().len() as i32,
        Err(_) => -1,
    }
}

/// 1 when `payload` (one packet of `track`, as delivered by `aether_demux_next`) is decodable from
/// there for the codecs where the container flag is not trusted (H.264 IDR, HEVC IRAP), 0 when it
/// is not, -1 without a demuxer or for an unknown track. Other codecs return 1.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_demux_verify_keyframe(track: u64, ptr: *const u8, len: usize) -> i32 {
    OPEN.with_borrow(|o| {
        let Some(o) = o else { return no_demuxer() };
        match o.demuxer.tracks().iter().find(|t| t.id == track) {
            Some(t) => i32::from(aether_demux::verify_keyframe(t, unsafe { input(ptr, len) })),
            None => -1,
        }
    })
}
