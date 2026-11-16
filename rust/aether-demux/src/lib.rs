//! Container demux for the browser player: Matroska, MP4 (progressive and fragmented), QuickTime MOV, raw audio files
//! and MPEG-TS / Blu-ray M2TS over a random-access [`ByteSource`].
//!
//! `Demuxer::open` indexes the whole file. `Demuxer::open_lazy` reads headers only (Matroska: Info,
//! Tracks, SeekHead, and Cues stored before the clusters; Cues behind the clusters wait for the first
//! seek; MP4: `moov` plus `sidx`/`mfra` of fragmented files; MPEG-TS: PAT/PMT, the
//! first access units and the last 256 KB for the duration) and indexes on demand with `index_more`
//! and `seek_to`. `read_packet` fetches payload bytes on demand, so the host decides what to buffer.
//! Matroska Cues are used only to find the cluster to index for `seek_to`: they are not sync points,
//! so `seek` takes the keyframe flag from the blocks and then re-checks the bitstream of video packets
//! before trusting it. Matroska content encodings (header stripping, zlib) are undone by `read_packet`.
//! A track that cannot be used is skipped and counted by `omitted_tracks`, it never fails the file.

pub mod mkv;
pub mod mp4;
mod raw;
mod source;
mod ts;
mod util;

pub use source::{BufferedSource, ByteSource, SliceSource, read_exact_at};
pub use ts::TimelineBreak;
#[cfg(unix)]
pub use source::FileSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Source(String),
    Format(&'static str),
    Limit(&'static str),
    Mkv(mkv::Error),
    Mp4(String),
    NoSuchTrack,
}

impl From<mkv::Error> for Error {
    fn from(e: mkv::Error) -> Self {
        Self::Mkv(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Matroska,
    Mp4,
    /// MPEG-2 transport stream, including 192 byte BDAV (M2TS) packets.
    MpegTs,
    /// A bare audio stream: AC-3 / E-AC-3, FLAC, DTS / DTS-HD, TrueHD / MLP, WAV / RF64 / Wave64 or ADTS AAC.
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    #[default]
    Other,
}

/// Dolby Vision configuration record exactly as stored (`dvcC` or `dvvC`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DolbyVisionRecord {
    pub box_type: [u8; 4],
    pub bytes: Vec<u8>,
}

/// What the container says about a video track's colour (MP4 `colr` / `mdcv` / `clli`, Matroska `Colour`).
/// A field the container leaves out or marks unspecified is `None`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ColorInfo {
    pub primaries: Option<u16>,
    pub transfer: Option<u16>,
    pub matrix: Option<u16>,
    pub full_range: Option<bool>,
    /// In the `mdcv` box order: green, blue, red and white point (x then y, 0.00002 units), then maximum and
    /// minimum luminance (0.0001 cd/m2).
    pub mastering: Option<[u32; 10]>,
    /// MaxCLL, MaxFALL in cd/m2.
    pub content_light: Option<(u16, u16)>,
}

impl ColorInfo {
    /// ISO/IEC 23001-8 code point, or `None` for 2 (unspecified) and for 0 where 0 is reserved.
    pub(crate) fn code(v: u64, zero_valid: bool) -> Option<u16> {
        match v {
            2 => None,
            0 if !zero_valid => None,
            v => u16::try_from(v).ok(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TrackInfo {
    pub id: u64,
    pub kind: TrackKind,
    /// Matroska CodecID (`V_MPEGH/ISO/HEVC`) or the MP4 sample entry (`hvc1`).
    pub codec: String,
    /// Matroska CodecPrivate, or `hvcC` for MP4 HEVC.
    pub codec_private: Vec<u8>,
    pub dolby_vision: Option<DolbyVisionRecord>,
    pub color: Option<ColorInfo>,
    /// Ticks per second of `pts`, `dts` and `duration` for this track.
    pub timescale: u32,
    pub language: Option<String>,
    pub name: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub default_duration_ns: Option<u64>,
    /// MP4 only: string for `VideoDecoder`/`AudioDecoder` `configure` where one exists
    /// (`hvc1.2.4.L153.B0`, `avc1.640028`, `av01.0.08M.10`, `vp09.00.10.08...`,
    /// `mp4a.40.2`, `opus`, `flac`, `ac-3`, `ec-3`, `mlpa`, `dtsc`...). Dolby Vision
    /// entries carry the `dvh1.08.06` style string here. PCM entries use the
    /// non-WebCodecs names `pcm-s16le`, `pcm-s24be`, `pcm-f32le`...
    pub codec_string: Option<String>,
    /// Dolby Vision tracks: the string for the underlying HEVC/AVC/AV1 base layer.
    pub fallback_codec_string: Option<String>,
    /// MP4 `hvcE` (Dolby Vision enhancement layer configuration) when present.
    pub dolby_vision_el: Option<Vec<u8>>,
    /// E-AC-3 only: `complexity_index_type_a` when the `dec3` extension flags Joint Object Coding.
    pub joc_complexity: Option<u8>,
    /// Number of sample descriptions in `stsd`; the track fields describe the first.
    pub sample_descriptions: u32,
    /// MPEG-TS: the packet identifier carrying the track (the AC-3 core split out of a TrueHD PID
    /// shares the PID of the TrueHD track and has id `0x1_0000 | pid`).
    pub source_pid: Option<u16>,
    /// Matroska `CodecDelay` in nanoseconds when the track declares one (Opus pre-skip).
    pub codec_delay_ns: Option<u64>,
    /// MPEG-TS: this HEVC track is the Dolby Vision enhancement layer (UHD Blu-ray PID 0x1015).
    pub enhancement_layer: bool,
}

/// One random-access entry from `sidx` or `mfra/tfra`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentEntry {
    /// `sidx` reference id or `tfra` track id.
    pub track: u64,
    pub time_ns: i64,
    /// Absolute offset of the fragment (`moof`, or the first byte of a `sidx` subsegment).
    pub offset: u64,
    pub from_sidx: bool,
}

/// One coded frame. `offset`/`size` address the payload in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketRef {
    pub track: u64,
    pub pts: i64,
    /// Equals `pts` for Matroska, which does not store decode times.
    pub dts: i64,
    /// Zero when unknown (last packet of a Matroska track without a default duration).
    pub duration: i64,
    pub keyframe: bool,
    /// False for MP4 samples outside the edit list window.
    pub presentation: bool,
    pub offset: u64,
    pub size: u32,
    /// MPEG-TS only: position of the packet inside the elementary stream bytes of the PES that
    /// starts at `offset` (`offset` then addresses a transport packet, not the payload). Zero elsewhere.
    pub inner: u32,
}

pub(crate) fn tracks_from_mkv(parsed: &[mkv::Track<'_>], _scale: u64) -> Vec<TrackInfo> {
    parsed
        .iter()
        .map(|t| TrackInfo {
            id: t.number,
            kind: match t.kind {
                1 => TrackKind::Video,
                2 => TrackKind::Audio,
                17 => TrackKind::Subtitle,
                _ => TrackKind::Other,
            },
            codec: t.codec_id.to_owned(),
            codec_private: t.codec_private.map(<[u8]>::to_vec).unwrap_or_default(),
            dolby_vision: t.dolby_vision.map(|(box_type, bytes)| DolbyVisionRecord { box_type, bytes: bytes.to_vec() }),
            color: t.color.clone(),
            timescale: 1_000_000_000,
            language: t.language.map(str::to_owned),
            name: t.name.map(str::to_owned),
            channels: t.channels.and_then(|v| u32::try_from(v).ok()),
            sample_rate: t.sample_rate_hz,
            bit_depth: t.bit_depth.and_then(|v| u32::try_from(v).ok()),
            width: t.width.and_then(|v| u32::try_from(v).ok()),
            height: t.height.and_then(|v| u32::try_from(v).ok()),
            default_duration_ns: t.default_duration_ns,
            codec_string: None,
            fallback_codec_string: None,
            dolby_vision_el: None,
            joc_complexity: None,
            sample_descriptions: 1,
            source_pid: None,
            codec_delay_ns: t.codec_delay_ns,
            enhancement_layer: false,
        })
        .collect()
}

pub struct Demuxer {
    container: Container,
    tracks: Vec<TrackInfo>,
    packets: Vec<PacketRef>,
    duration_ns: Option<u64>,
    omitted_tracks: usize,
    fragmented: bool,
    fragments: Vec<FragmentEntry>,
    ts: Option<ts::TsIndex>,
    mkv: Option<mkv::MkvLazy>,
    mp4: Option<mp4::Mp4Lazy>,
    raw: Option<raw::RawIndex>,
    complete: bool,
}

fn mkv_packet(p: mkv::MkvPacket) -> PacketRef {
    PacketRef {
        track: p.track,
        pts: p.pts_ns,
        dts: p.pts_ns,
        duration: p.duration_ns.map_or(0, |d| d as i64),
        keyframe: p.keyframe,
        presentation: true,
        offset: p.offset,
        size: p.size,
        inner: 0,
    }
}

/// Inserts `new` (sorted by position) into `all` keeping the (offset, inner) order and dropping duplicates.
fn merge_packets(all: &mut Vec<PacketRef>, mut new: Vec<PacketRef>) -> usize {
    if new.is_empty() {
        return 0;
    }
    new.sort_by_key(|p| (p.offset, p.inner, p.track));
    let key = |p: &PacketRef| (p.offset, p.inner, p.track);
    let first = key(&new[0]);
    let at = all.partition_point(|p| key(p) < first);
    let tail: Vec<PacketRef> = all.split_off(at);
    let before = all.len() + tail.len();
    let (mut i, mut j) = (0, 0);
    while i < tail.len() || j < new.len() {
        let take_old = j >= new.len() || (i < tail.len() && key(&tail[i]) <= key(&new[j]));
        if take_old {
            if j < new.len() && key(&tail[i]) == key(&new[j]) {
                j += 1;
            }
            all.push(tail[i].clone());
            i += 1;
        } else {
            all.push(new[j].clone());
            j += 1;
        }
    }
    all.len() - before
}

impl Demuxer {
    /// Opens `src` and indexes the whole file (every packet is listed by `packets()`).
    /// Over a remote source prefer [`Demuxer::open_lazy`].
    pub fn open(src: &dyn ByteSource) -> Result<Self, Error> {
        Self::open_with(src, true)
    }

    /// Reads headers only (Matroska: Info, Tracks, SeekHead, and Cues stored before the clusters, the
    /// others are read by the first seek; MP4: `moov`, and for fragmented
    /// files `sidx`/`mfra`; MPEG-TS: PAT/PMT, the first access units and the last 256 KB for the duration).
    /// `packets()` then lists what has been indexed so far (nothing for Matroska and fragmented MP4);
    /// call [`Demuxer::index_more`] to extend the list or [`Demuxer::seek_to`] to index around a time.
    pub fn open_lazy(src: &dyn ByteSource) -> Result<Self, Error> {
        Self::open_with(src, false)
    }

    fn open_with(src: &dyn ByteSource, eager: bool) -> Result<Self, Error> {
        let mut head = vec![0u8; (src.len() as usize).min(4096)];
        read_exact_at(src, 0, &mut head)?;
        let n = head.len();
        let mut d = if n >= 4 && head[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
            Self::open_mkv(src)?
        } else if n >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"mdat" | b"moof" | b"styp" | b"sidx" | b"free" | b"skip" | b"wide" | b"pnot") {
            if eager { Self::open_mp4(src)? } else { Self::open_mp4_lazy(src)? }
        } else if let Some((stride, first, sync_at)) = ts::detect(&head) {
            Self::open_ts(src, ts::Layout { stride, first: first as u64, sync_at })?
        } else if let Some(found) = raw::open(src)? {
            Self::open_raw(found)
        } else {
            return Err(Error::Format("unrecognised container"));
        };
        if eager {
            d.index_all(src)?;
        }
        Ok(d)
    }

    fn open_raw((idx, first): (raw::RawIndex, Vec<PacketRef>)) -> Self {
        let mut d = Self {
            container: Container::Raw,
            tracks: idx.tracks.clone(),
            packets: Vec::new(),
            duration_ns: idx.duration_ns(),
            omitted_tracks: 0,
            fragmented: false,
            fragments: Vec::new(),
            ts: None,
            mkv: None,
            mp4: None,
            complete: idx.is_complete(),
            raw: Some(idx),
        };
        merge_packets(&mut d.packets, first);
        d
    }

    /// Merges a raw index step into the packet list (retiming joined regions first) and refreshes the
    /// completeness and duration the index reports.
    fn apply_raw(&mut self, upd: raw::Update) -> usize {
        for s in &upd.shifts {
            for p in self.packets.iter_mut().filter(|p| p.offset >= s.from && p.offset < s.to) {
                p.pts += s.delta;
                p.dts += s.delta;
            }
        }
        let added = merge_packets(&mut self.packets, upd.packets);
        if let Some(r) = &self.raw {
            self.complete = r.is_complete();
            self.duration_ns = r.duration_ns();
        }
        added
    }

    fn open_mkv(src: &dyn ByteSource) -> Result<Self, Error> {
        let m = mkv::MkvLazy::open(src)?;
        Ok(Self {
            container: Container::Matroska,
            tracks: m.tracks.clone(),
            packets: Vec::new(),
            duration_ns: m.duration_ns,
            omitted_tracks: m.omitted.len(),
            fragmented: false,
            fragments: Vec::new(),
            ts: None,
            mkv: Some(m),
            mp4: None,
            raw: None,
            complete: false,
        })
    }

    fn open_mp4_lazy(src: &dyn ByteSource) -> Result<Self, Error> {
        let (m, packets) = mp4::Mp4Lazy::open(src).map_err(Error::Mp4)?;
        let mut d = Self {
            container: Container::Mp4,
            tracks: m.track_infos(),
            packets,
            duration_ns: m.header_duration_ns(),
            omitted_tracks: m.omitted,
            fragmented: m.fragmented,
            fragments: m.fragments.clone(),
            ts: None,
            mkv: None,
            complete: m.complete,
            raw: None,
            mp4: Some(m),
        };
        if !d.fragmented {
            d.duration_ns = d.mp4_duration().or(d.duration_ns);
        }
        Ok(d)
    }

    /// Latest end of a presentable packet, the figure the eager reader reports.
    fn mp4_duration(&self) -> Option<u64> {
        let scale = |track: u64| self.tracks.iter().find(|t| t.id == track).map_or(1, |t| t.timescale);
        self.packets
            .iter()
            .filter(|p| p.presentation)
            .map(|p| (i128::from(p.pts) + i128::from(p.duration)) * 1_000_000_000 / i128::from(scale(p.track).max(1)))
            .max()
            .map(|v| v.max(0) as u64)
    }

    fn open_mp4(src: &dyn ByteSource) -> Result<Self, Error> {
        let m = mp4::read(src).map_err(Error::Mp4)?;
        Ok(Self {
            container: Container::Mp4,
            tracks: m.tracks,
            packets: m.packets,
            duration_ns: m.duration_ns,
            omitted_tracks: m.omitted,
            fragmented: m.fragmented,
            fragments: m.fragments,
            ts: None,
            mkv: None,
            mp4: None,
            raw: None,
            complete: true,
        })
    }

    fn open_ts(src: &dyn ByteSource, layout: ts::Layout) -> Result<Self, Error> {
        let mut idx = ts::open(src, layout)?;
        let packets = idx.take_initial();
        let mut d = Self {
            container: Container::MpegTs,
            tracks: idx.tracks.clone(),
            packets: Vec::new(),
            duration_ns: idx.duration_ns(),
            omitted_tracks: idx.omitted,
            fragmented: false,
            fragments: Vec::new(),
            complete: idx.is_complete(),
            mkv: None,
            mp4: None,
            raw: None,
            ts: Some(idx),
        };
        merge_packets(&mut d.packets, packets);
        Ok(d)
    }

    pub fn container(&self) -> Container {
        self.container
    }
    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }
    /// Indexed packets in storage order (by source position). After `open_lazy` this is a prefix of the
    /// file plus any windows indexed by `seek_to`; indices into it are invalidated by `index_more` and `seek_to`.
    pub fn packets(&self) -> &[PacketRef] {
        &self.packets
    }
    pub fn track_packets(&self, track: u64) -> impl Iterator<Item = &PacketRef> {
        self.packets.iter().filter(move |p| p.track == track)
    }
    /// True when the MP4 `moov` has an `mvex` (fragmented file).
    pub fn fragmented(&self) -> bool {
        self.fragmented
    }
    /// Entries from `sidx` and `mfra/tfra` when the file has them.
    pub fn fragment_index(&self) -> &[FragmentEntry] {
        &self.fragments
    }
    pub fn duration_ns(&self) -> Option<u64> {
        self.duration_ns
    }
    /// Tracks the reader could not use (encrypted, external data, malformed, unsupported codec).
    pub fn omitted_tracks(&self) -> usize {
        self.omitted_tracks
    }
    /// True when every packet of the file is in `packets()`.
    pub fn is_complete(&self) -> bool {
        self.complete
    }
    /// MPEG-TS: timeline jumps that were removed from the timestamps (so far indexed).
    pub fn timeline_breaks(&self) -> &[TimelineBreak] {
        self.ts.as_ref().map_or(&[], |t| &t.breaks[..])
    }

    /// Indexes roughly `max_bytes` of the source beyond what is indexed. Returns the number of packets added.
    pub fn index_more(&mut self, src: &dyn ByteSource, max_bytes: u64) -> Result<usize, Error> {
        if let Some(r) = self.raw.as_mut() {
            let upd = r.index_more(src, max_bytes)?;
            return Ok(self.apply_raw(upd));
        }
        if self.complete {
            return Ok(0);
        }
        let mut added = 0;
        if let Some(ts) = self.ts.as_mut() {
            let new = ts.index_more(src, max_bytes)?;
            self.complete = ts.is_complete();
            self.tracks = ts.tracks.clone();
            added += merge_packets(&mut self.packets, new);
        }
        if let Some(m) = self.mkv.as_mut() {
            let new = m.index_more(src, max_bytes)?;
            self.complete = m.complete;
            added += merge_packets(&mut self.packets, new.into_iter().map(mkv_packet).collect());
        }
        if let Some(m) = self.mp4.as_mut() {
            let new = m.index_more(src, max_bytes).map_err(Error::Mp4)?;
            self.complete = m.complete;
            self.fragments = m.fragments.clone();
            added += merge_packets(&mut self.packets, new);
            if self.complete && self.duration_ns.is_none() {
                self.duration_ns = self.mp4_duration();
            }
        }
        Ok(added)
    }

    /// Makes sure the data stored right after `packet` is indexed. A `seek_to` leaves unindexed spans
    /// behind it (the one it jumped over, or the one between a window and an older window), so a reader
    /// that walks `packets()` in order calls this when the next entry lies far beyond `packet`.
    /// Does nothing when that span is already indexed. Returns the number of packets added.
    pub fn index_after(&mut self, src: &dyn ByteSource, packet: &PacketRef) -> Result<usize, Error> {
        if let Some(r) = self.raw.as_mut() {
            let upd = r.index_after(src, packet)?;
            return Ok(self.apply_raw(upd));
        }
        if self.complete {
            return Ok(0);
        }
        let mut added = 0;
        if let Some(ts) = self.ts.as_mut() {
            let new = ts.ensure_window(src, packet.offset, packet.offset.saturating_add(4 << 20))?;
            self.complete = ts.is_complete();
            self.tracks = ts.tracks.clone();
            added += merge_packets(&mut self.packets, new);
        }
        if let Some(m) = self.mkv.as_mut() {
            let new = m.index_after(src, packet.offset)?;
            self.complete = m.complete;
            added += merge_packets(&mut self.packets, new.into_iter().map(mkv_packet).collect());
        }
        if let Some(m) = self.mp4.as_mut() {
            let new = m.index_fragment_after(src, packet.offset).map_err(Error::Mp4)?;
            self.complete = m.complete;
            self.fragments = m.fragments.clone();
            added += merge_packets(&mut self.packets, new);
        }
        Ok(added)
    }

    /// Indexes the rest of the file.
    pub fn index_all(&mut self, src: &dyn ByteSource) -> Result<(), Error> {
        if let Some(r) = self.raw.as_mut() {
            let upd = r.index_all(src)?;
            self.apply_raw(upd);
            return Ok(());
        }
        while !self.complete {
            self.index_more(src, u64::MAX)?;
        }
        Ok(())
    }

    /// Matroska: places where the index skipped data so far (a block it could not take, or the rest of a
    /// corrupt or cut cluster). The packets stored there are missing from `packets()`.
    pub fn damaged_spans(&self) -> usize {
        self.mkv.as_ref().map_or(0, |m| m.damaged)
    }
    /// Why Matroska tracks were left out (one line per track).
    pub fn omitted_track_reasons(&self) -> &[String] {
        self.mkv.as_ref().map_or(&[], |m| &m.omitted[..])
    }

    pub fn read_packet(&self, src: &dyn ByteSource, packet: &PacketRef, out: &mut Vec<u8>) -> Result<(), Error> {
        if let Some(ts) = &self.ts {
            return ts.read_packet(src, packet, out);
        }
        out.clear();
        out.resize(packet.size as usize, 0);
        read_exact_at(src, packet.offset, out)?;
        if let Some(steps) = self.mkv.as_ref().and_then(|m| m.steps.get(&packet.track)) {
            *out = mkv::decode_steps(steps, std::mem::take(out))?;
        }
        Ok(())
    }

    /// Index (into `packets()`) of the keyframe at or before `target_pts`
    /// (track ticks) whose bitstream confirms it. Falls back to earlier
    /// candidates when a flagged keyframe is not decodable on its own.
    /// Only indexed packets are considered; see [`Demuxer::seek_to`] for lazily opened files.
    pub fn seek(&self, src: &dyn ByteSource, track: u64, target_pts: i64) -> Result<usize, Error> {
        self.seek_in(src, track, target_pts, 0, u64::MAX)
    }

    fn seek_in(&self, src: &dyn ByteSource, track: u64, target_pts: i64, min_offset: u64, max_offset: u64) -> Result<usize, Error> {
        let info = self.tracks.iter().find(|t| t.id == track).ok_or(Error::NoSuchTrack)?;
        let mut candidates: Vec<usize> = self
            .packets
            .iter()
            .enumerate()
            .filter(|(_, p)| p.track == track && p.keyframe && p.presentation && p.pts <= target_pts && p.offset >= min_offset && p.offset < max_offset)
            .map(|(i, _)| i)
            .collect();
        candidates.sort_by_key(|&i| self.packets[i].pts);
        let mut buf = Vec::new();
        for &i in candidates.iter().rev().take(16) {
            self.read_packet(src, &self.packets[i], &mut buf)?;
            if verify_keyframe(info, &buf) {
                return Ok(i);
            }
        }
        if min_offset > 0 || max_offset != u64::MAX {
            return Err(Error::Format("no keyframe in the indexed window"));
        }
        // Nothing before the target: the first keyframe is the only valid start.
        self.packets
            .iter()
            .position(|p| p.track == track && p.keyframe)
            .ok_or(Error::Format("track has no keyframe"))
    }

    /// Where delivery has to start so that no sound is missing at the keyframe a seek chose (`key_index`
    /// into `packets()`): the earliest packet, in storage order, of an audio track that still sounds at
    /// the keyframe's time. MP4 and MOV store each track in chunks, so the sound that belongs to a
    /// keyframe can lie in front of it (a second in an interleaved file, seconds for LPCM in MOV).
    /// Matroska and MPEG-TS write the sound with or after its picture, so they get `key_index` back.
    /// A reader that starts there hands on only audio packets until it reaches the keyframe.
    pub fn audio_start(&self, key_index: usize) -> usize {
        // Far enough for the longest chunking seen (MOV LPCM, one packet per sample run) without walking a whole film per seek.
        const SCAN: usize = 8192;
        if self.container != Container::Mp4 {
            return key_index;
        }
        let Some(key) = self.packets.get(key_index) else { return key_index };
        let scale = |id: u64| self.tracks.iter().find(|t| t.id == id).map(|t| i128::from(t.timescale.max(1)));
        let Some(key_scale) = scale(key.track) else { return key_index };
        let mut open: Vec<(u64, i128)> = self
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio && t.id != key.track)
            .map(|t| (t.id, i128::from(t.timescale.max(1))))
            .collect();
        let mut start = key_index;
        for i in (key_index.saturating_sub(SCAN)..key_index).rev() {
            if open.is_empty() {
                break;
            }
            let p = &self.packets[i];
            let Some(k) = open.iter().position(|(id, _)| *id == p.track) else { continue };
            // Ends after the keyframe starts: (pts + duration) / its scale > key pts / key scale. A track is done at its first
            // packet that does not (audio is stored in time order).
            if (i128::from(p.pts) + i128::from(p.duration.max(0))) * key_scale > i128::from(key.pts) * open[k].1 {
                start = i;
            } else {
                open.swap_remove(k);
            }
        }
        start
    }

    /// Like [`Demuxer::seek`] but indexes what is needed first: for a lazily opened file this reads around
    /// the target (Matroska Cues, MP4 `sidx`/`mfra`, MPEG-TS timestamp search) instead of everything before it.
    /// The returned index is valid until the next `index_more`, `seek_to` or `index_all` call.
    pub fn seek_to(&mut self, src: &dyn ByteSource, track: u64, target_pts: i64) -> Result<usize, Error> {
        if self.raw.is_some() {
            return self.raw_seek(src, track, target_pts);
        }
        if self.ts.is_some() {
            return self.ts_seek(src, track, target_pts);
        }
        if self.mp4.as_ref().is_some_and(|m| m.fragmented) && !self.complete && !self.fragments.is_empty() {
            if let Some(i) = self.mp4_fragment_seek(src, track, target_pts)? {
                return Ok(i);
            }
        }
        // Cues behind the clusters are read by the first seek that needs them, not by the position at the start
        // of playback (the first frame of a remote file waited for the end of the file) and not by a target that
        // is already indexed without a gap from the first cluster.
        let mut in_head = false;
        if self.mkv.as_ref().is_some_and(|m| m.cues_pending()) {
            let mut steps = 0;
            while !self.complete && steps < 8 && self.track_packets(track).next().is_none() {
                self.index_more(src, 1 << 20)?;
                steps += 1;
            }
            if self.track_packets(track).next().is_some_and(|p| p.pts >= target_pts) {
                return self.seek(src, track, target_pts);
            }
            let head = self.mkv.as_ref().and_then(|m| m.head_end());
            in_head = head.is_some_and(|e| self.track_packets(track).any(|p| p.pts > target_pts && p.offset < e));
            if !in_head {
                if let Some(m) = self.mkv.as_mut() {
                    m.ensure_cues(src)?;
                }
            }
        }
        if !in_head && self.mkv.as_ref().is_some_and(|m| m.has_cues()) && !self.complete {
            if let Some(i) = self.mkv_cue_seek(src, track, target_pts)? {
                return Ok(i);
            }
        }
        if !self.complete {
            // Sequential fallback: index until the track has packets beyond the target.
            while !self.complete && !self.track_packets(track).any(|p| p.pts > target_pts) {
                self.index_more(src, 8 << 20)?;
            }
        }
        self.seek(src, track, target_pts)
    }

    /// Raw audio: indexes a region around the target (modelled byte position, resync, forward to the target)
    /// and picks the last packet at or before it; reaches further back when that region has none.
    fn raw_seek(&mut self, src: &dyn ByteSource, track: u64, target: i64) -> Result<usize, Error> {
        if self.tracks.iter().all(|t| t.id != track) {
            return Err(Error::NoSuchTrack);
        }
        if self.packets.first().is_none_or(|p| target <= p.pts) {
            return Ok(0);
        }
        for attempt in 0..8 {
            let (upd, win) = self.raw.as_mut().unwrap().prepare_seek(src, target, attempt)?;
            self.apply_raw(upd);
            match self.seek_in(src, track, target, win.0, win.1) {
                Ok(i) => return Ok(i),
                Err(Error::Format(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.seek(src, track, target)
    }

    /// Fragmented MP4: jump to the `sidx`/`mfra` entry at or before the target and index that fragment.
    fn mp4_fragment_seek(&mut self, src: &dyn ByteSource, track: u64, target: i64) -> Result<Option<usize>, Error> {
        let scale = self.tracks.iter().find(|t| t.id == track).ok_or(Error::NoSuchTrack)?.timescale.max(1);
        let target_ns = i128::from(target) * 1_000_000_000 / i128::from(scale);
        let mut entries: Vec<FragmentEntry> = self
            .fragments
            .iter()
            .filter(|e| i128::from(e.time_ns) <= target_ns && (e.track == track || self.tracks.iter().all(|t| t.id != e.track)))
            .cloned()
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse((e.time_ns, e.offset)));
        entries.dedup_by_key(|e| e.offset);
        for e in entries.into_iter().take(8) {
            let m = self.mp4.as_mut().unwrap();
            let (new, (a, b)) = match m.index_fragment_at(src, e.offset) {
                Ok(v) => v,
                Err(_) => continue,
            };
            self.complete = m.complete;
            merge_packets(&mut self.packets, new);
            if let Ok(i) = self.seek_in(src, track, target, a, b.saturating_add(1)) {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// Matroska: jump to the Cue at or before the target and index a short window at the block it names.
    fn mkv_cue_seek(&mut self, src: &dyn ByteSource, track: u64, target: i64) -> Result<Option<usize>, Error> {
        let targets = self.mkv.as_ref().map(|m| m.cue_targets_before(track, target)).unwrap_or_default();
        for (pos, rel) in targets.into_iter().take(8) {
            let m = self.mkv.as_mut().unwrap();
            let (new, (a, b)) = match m.index_at_cue(src, pos, rel) {
                Ok(n) => n,
                Err(Error::Source(e)) => return Err(Error::Source(e)),
                Err(_) => continue,
            };
            self.complete = m.complete;
            merge_packets(&mut self.packets, new.into_iter().map(mkv_packet).collect());
            if let Ok(i) = self.seek_in(src, track, target, a, b) {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    fn ts_seek(&mut self, src: &dyn ByteSource, track: u64, target: i64) -> Result<usize, Error> {
        let info = self.tracks.iter().find(|t| t.id == track).ok_or(Error::NoSuchTrack)?;
        let _ = info;
        let first = self.ts.as_ref().map_or(0, |t| t.layout.first);
        let mut back: i64 = 3 * 90_000 / 2;
        let mut tries = 0;
        loop {
            let ts = self.ts.as_mut().unwrap();
            let from_t = target - back;
            let (from, from_time) = ts.offset_for_time(src, from_t)?;
            let (to, _) = ts.offset_for_time(src, target)?;
            let to = (to + 1024 * 1024).min(src.len());
            let _ = from_time;
            let new = ts.ensure_window(src, from, to)?;
            self.complete = ts.is_complete();
            merge_packets(&mut self.packets, new);
            match self.seek_in(src, track, target, from, u64::MAX) {
                Ok(i) => return Ok(i),
                Err(Error::Format(_)) if from > first && tries < 6 => {
                    back *= 2;
                    tries += 1;
                }
                Err(Error::Format(_)) => {
                    // Reached the start of the file: the first keyframe is the only valid start.
                    return self.seek(src, track, target);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Bitstream check for video codecs where a container flag is known to be unreliable.
/// Other codecs are all-intra (audio) or not inspected and are trusted.
pub fn verify_keyframe(track: &TrackInfo, payload: &[u8]) -> bool {
    if track.kind != TrackKind::Video {
        return true;
    }
    if matches!(track.codec.as_str(), "h264" | "hevc") {
        return ts::annexb_keyframe(&track.codec, payload);
    }
    let hevc = track.codec.contains("HEVC") || matches!(track.codec.as_str(), "hvc1" | "hev1" | "dvh1" | "dvhe");
    let avc = track.codec.contains("AVC") || matches!(track.codec.as_str(), "avc1" | "avc3");
    if !hevc && !avc {
        return true;
    }
    let private = &track.codec_private;
    let length_size = if hevc {
        private.get(21).map_or(4, |b| usize::from(b & 3) + 1)
    } else {
        private.get(4).map_or(4, |b| usize::from(b & 3) + 1)
    };
    let mut rest = payload;
    while rest.len() > length_size {
        let (len, tail) = rest.split_at(length_size);
        let len = len.iter().fold(0usize, |v, &b| (v << 8) | usize::from(b));
        if len == 0 || len > tail.len() {
            return false;
        }
        let first = tail[0];
        if hevc {
            let nal_type = (first >> 1) & 63;
            if nal_type < 32 {
                return (16..=23).contains(&nal_type);
            }
        } else {
            let nal_type = first & 31;
            if (1..=5).contains(&nal_type) {
                return nal_type == 5;
            }
        }
        rest = &tail[len..];
    }
    false
}
