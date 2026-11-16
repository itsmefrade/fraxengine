//! Raw elementary stream audio files: AC-3 / E-AC-3 (`.ac3`, `.eac3`), native FLAC, DTS and DTS-HD
//! (`.dts`, `.dtshd`), TrueHD / MLP (`.thd`, `.mlp`), WAV / RF64 / BW64 / Wave64 and ADTS AAC.
//!
//! Each file is one audio track (id 1). Packets are contiguous byte ranges of the file, cut like
//! libavformat's raw demuxers and parsers do, so the packet list equals `ffprobe -show_packets`:
//! an AC-3 frame, an E-AC-3 access unit (the independent frame plus its dependent substreams), a DTS core
//! frame with the extension substreams behind it, a TrueHD / MLP access unit, a FLAC frame, an ADTS frame
//! and, for PCM, 4096 sample frames. Timestamps count samples (the track timescale is the sample rate).
//!
//! The file is indexed lazily in *regions*: runs of consecutive packets. Open indexes the first 128 KB and,
//! unless every frame has the same size, samples a few more windows across the file to estimate the duration
//! and the bytes-to-time model (constant rate streams need only the head). A seek
//! jumps to the modelled byte position, resynchronises on the next frame and indexes a new region there;
//! PCM and FLAC are exact (FLAC reads the sample number from the frame header, and bisects when the file has
//! no seek table), the other formats take the model as the first timestamp of the region. When indexing
//! joins two regions the later one is shifted so the timeline is continuous.

mod flac;
mod wav;

use crate::ts::{frame, params};
use crate::{ByteSource, Error, PacketRef, TrackInfo, TrackKind, read_exact_at};

const HEAD_WINDOW: u64 = 128 * 1024;
const PROBE_BYTES: u64 = 32 * 1024;
const PROBES: u64 = 6;
const CHUNK: usize = 256 * 1024;
const MAX_FRAME: usize = 8 << 20;
const MAX_STEP: u64 = 32 << 20;
/// Bytes read behind a seek target (per attempt, doubled each time) so the landing frame is at or before it.
const SEEK_SLACK: u64 = 96 * 1024;
/// A seek target this close (in bytes) behind-the-target to the end of the indexed head is reached by indexing
/// onward from the head, so its timestamps stay exact (counted from sample 0). Farther targets start a region
/// whose timestamps come from the bytes-to-time model.
const BRIDGE: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Ac3,
    Eac3,
    Flac,
    Dts,
    TrueHd,
    Mlp,
    Wav,
    Adts,
}

pub(crate) struct Region {
    /// Offset of the first packet.
    start: u64,
    /// Offset after the last packet (where indexing continues).
    end: u64,
    first_pts: i64,
    pts_end: i64,
}

/// Timestamps of the packets in `from..to` (by offset) move by `delta`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Shift {
    pub from: u64,
    pub to: u64,
    pub delta: i64,
}

#[derive(Default)]
pub(crate) struct Update {
    pub packets: Vec<PacketRef>,
    /// Shifts of packets indexed before this update (apply before merging `packets`, which are already final).
    pub shifts: Vec<Shift>,
}

impl Update {
    /// Appends a later step. Its shifts also apply to the packets collected so far.
    fn absorb(&mut self, mut other: Update) {
        for s in &other.shifts {
            self.apply(*s);
        }
        self.packets.append(&mut other.packets);
        self.shifts.append(&mut other.shifts);
    }

    fn apply(&mut self, s: Shift) {
        for p in self.packets.iter_mut().filter(|p| p.offset >= s.from && p.offset < s.to) {
            p.pts += s.delta;
            p.dts += s.delta;
        }
    }

    fn shift(&mut self, s: Shift) {
        self.apply(s);
        self.shifts.push(s);
    }
}

pub(crate) struct RawIndex {
    kind: Kind,
    pub tracks: Vec<TrackInfo>,
    rate: u32,
    data_start: u64,
    data_end: u64,
    regions: Vec<Region>,
    active: usize,
    flac: Option<flac::Flac>,
    wav: Option<wav::Wav>,
    /// TrueHD / MLP: samples per access unit.
    thd_samples: i64,
    /// (offset, ticks) knots of the bytes-to-time model, ascending.
    knots: Vec<(u64, f64)>,
    /// Exact duration once the first region reaches the end, else the model's estimate (ticks).
    total_ticks: Option<i64>,
    exact_total: bool,
    gap_merge: u64,
    /// Constant frame size: the linear model is already frame exact, so a seek never bridges from the head.
    cbr_model: bool,
}

pub(crate) enum FrameRes {
    Frame(Fr),
    /// The frame needs this many bytes (from its start) to be judged.
    More(usize),
    Bad,
}

pub(crate) struct Fr {
    pub len: usize,
    pub dur: i64,
    pub key: bool,
    /// FLAC: the timestamp the frame header states.
    pub pts: Option<i64>,
}

/// Read-ahead buffer over a byte range of the source.
struct Window {
    off: u64,
    buf: Vec<u8>,
    chunk: usize,
}

impl Window {
    fn new(chunk: usize) -> Self {
        Self { off: 0, buf: Vec::new(), chunk }
    }
    /// Bytes from `pos` on, at least `n` of them unless `limit` comes first. May hold more.
    fn at(&mut self, src: &dyn ByteSource, pos: u64, n: usize, limit: u64) -> Result<&[u8], Error> {
        let have_end = self.off + self.buf.len() as u64;
        let want_end = (pos + n as u64).min(limit);
        if !(pos >= self.off && pos <= have_end && have_end >= want_end) {
            let end = (pos + n.max(self.chunk) as u64).min(limit);
            let len = end.saturating_sub(pos) as usize;
            self.buf.resize(len, 0);
            read_exact_at(src, pos, &mut self.buf)?;
            self.off = pos;
        }
        Ok(&self.buf[(pos - self.off) as usize..])
    }
}

struct ScanOut {
    packets: Vec<PacketRef>,
    end: u64,
    pts_end: i64,
}

fn ticks_ns(ticks: i64, rate: u32) -> u64 {
    (i128::from(ticks.max(0)) * 1_000_000_000 / i128::from(rate.max(1))) as u64
}

/// ID3v2 tag length at the start of `b` (header, body and footer), 0 when there is none.
fn id3v2_len(b: &[u8]) -> u64 {
    if b.len() >= 10 && &b[..3] == b"ID3" && b[3] < 0xff && b[6..10].iter().all(|&v| v < 0x80) {
        let size = b[6..10].iter().fold(0u64, |v, &x| (v << 7) | u64::from(x));
        10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 }
    } else {
        0
    }
}

/// Recognises a raw audio file. `None` when `src` is none of the supported formats.
pub(crate) fn open(src: &dyn ByteSource) -> Result<Option<(RawIndex, Vec<PacketRef>)>, Error> {
    let len = src.len();
    let mut head = vec![0u8; len.min(16 * 1024) as usize];
    read_exact_at(src, 0, &mut head)?;
    let base = id3v2_len(&head);
    // Some taggers leave padding after the tag; the stream itself starts at its first sync.
    if base > 0 && base < len {
        let mut more = vec![0u8; (len - base).min(16 * 1024) as usize];
        read_exact_at(src, base, &mut more)?;
        head = more;
    } else if base >= len {
        return Ok(None);
    }
    if head.len() < 12 {
        return Ok(None);
    }
    let mut idx = RawIndex {
        kind: Kind::Ac3,
        tracks: Vec::new(),
        rate: 0,
        data_start: base,
        data_end: len,
        regions: Vec::new(),
        active: 0,
        flac: None,
        wav: None,
        thd_samples: 40,
        knots: Vec::new(),
        total_ticks: None,
        exact_total: false,
        gap_merge: 1 << 20,
        cbr_model: false,
    };
    let mut codec_private = Vec::new();
    let found = if head.starts_with(b"fLaC") {
        idx.kind = Kind::Flac;
        match flac::parse(src, base)? {
            Some(f) => {
                idx.data_start = f.first_frame;
                codec_private = f.codec_private();
                idx.flac = Some(f);
                true
            }
            None => false,
        }
    } else if let Some(w) = wav::parse(src, &head)? {
        idx.kind = Kind::Wav;
        idx.data_start = w.data_start;
        idx.data_end = w.data_end;
        codec_private = w.fmt.clone();
        idx.wav = Some(w);
        true
    } else if head.starts_with(b"DTSHDHDR") {
        match dtshd_stream(src, base)? {
            Some((start, end)) => {
                idx.kind = Kind::Dts;
                idx.data_start = start;
                idx.data_end = end;
                true
            }
            None => false,
        }
    } else if head[4..8] == [0xf8, 0x72, 0x6f, 0xba] {
        idx.kind = Kind::TrueHd;
        true
    } else if head[4..8] == [0xf8, 0x72, 0x6f, 0xbb] {
        idx.kind = Kind::Mlp;
        true
    } else if head[..2] == [0x0b, 0x77] {
        idx.kind = match frame::ac3_header(&head) {
            Some(h) if h.eac3 => Kind::Eac3,
            Some(_) => Kind::Ac3,
            None => return Ok(None),
        };
        true
    } else if head[..4] == [0x7f, 0xfe, 0x80, 0x01] {
        idx.kind = Kind::Dts;
        true
    } else if head[0] == 0xff && head[1] & 0xf6 == 0xf0 {
        idx.kind = Kind::Adts;
        true
    } else {
        false
    };
    if !found || idx.data_start >= idx.data_end {
        return Ok(None);
    }
    if !idx.init_params(src, codec_private)? {
        return Ok(None);
    }
    // Stream formats must show a chain of frames, so that a few matching bytes do not make a file.
    if !matches!(idx.kind, Kind::Wav | Kind::Flac) && !idx.chain_ok(src, idx.data_start)? {
        return Ok(None);
    }
    let first = idx.initial_region(src)?;
    Ok(Some((idx, first)))
}

/// `STRMDATA` payload range of a `.dtshd` file (chunks: 8 byte tag, 8 byte big endian size).
fn dtshd_stream(src: &dyn ByteSource, base: u64) -> Result<Option<(u64, u64)>, Error> {
    let mut pos = base;
    for _ in 0..64 {
        if pos + 16 > src.len() {
            return Ok(None);
        }
        let mut h = [0u8; 16];
        read_exact_at(src, pos, &mut h)?;
        let size = u64::from_be_bytes(h[8..16].try_into().unwrap());
        if &h[..8] == b"STRMDATA" {
            let start = pos + 16;
            let end = start.saturating_add(size).min(src.len());
            return Ok((end > start).then_some((start, end)));
        }
        pos = match pos.checked_add(16).and_then(|p| p.checked_add(size)) {
            Some(p) => p,
            None => return Ok(None),
        };
    }
    Ok(None)
}

impl RawIndex {
    pub(crate) fn is_complete(&self) -> bool {
        self.regions.get(self.active).is_some_and(|r| r.end >= self.data_end)
    }

    pub(crate) fn duration_ns(&self) -> Option<u64> {
        self.total_ticks.map(|t| ticks_ns(t, self.rate))
    }

    fn exact_pts(&self) -> bool {
        matches!(self.kind, Kind::Wav | Kind::Flac)
    }

    fn limit(&self, src: &dyn ByteSource) -> u64 {
        self.data_end.min(src.len())
    }

    // ------------------------------------------------------------------ stream parameters

    /// Fills the track from the first frames. `false` when the stream does not parse.
    fn init_params(&mut self, src: &dyn ByteSource, codec_private: Vec<u8>) -> Result<bool, Error> {
        let n = (self.data_end - self.data_start).min(32 * 1024) as usize;
        let mut es = vec![0u8; n];
        read_exact_at(src, self.data_start, &mut es)?;
        let mut t = TrackInfo { id: 1, kind: TrackKind::Audio, sample_descriptions: 1, codec_private, ..Default::default() };
        match self.kind {
            Kind::Ac3 | Kind::Eac3 => {
                let Some(h) = frame::ac3_header(&es) else { return Ok(false) };
                t.sample_rate = Some(h.rate);
                t.channels = Some(h.channels);
                t.codec = if h.eac3 { "eac3" } else { "ac3" }.into();
                t.codec_string = Some(if h.eac3 { "ec-3" } else { "ac-3" }.into());
                if h.eac3 {
                    let mut at = h.size;
                    while at + 8 <= es.len() {
                        let Some(d) = frame::ac3_header(&es[at..]) else { break };
                        if !(d.eac3 && d.strmtyp == 1) {
                            break;
                        }
                        t.channels = Some(h.channels + params::dep_extra_channels(&d));
                        at += d.size;
                    }
                }
                self.rate = h.rate;
            }
            Kind::Dts => {
                let Some(core) = frame::dts_core(&es) else { return Ok(false) };
                t.sample_rate = Some(core.rate);
                t.channels = Some(core.channels);
                t.codec = "dts".into();
                let mut codec = "dtsc";
                let mut channels = core.channels;
                // The extension substreams of the first few frames tell DTS-HD MA / HRA / Express from a plain core.
                let mut at = 0;
                for _ in 0..4 {
                    let Some(c) = es.get(at..).and_then(frame::dts_core) else { break };
                    let tail = es.get(at + c.size..).unwrap_or(&[]);
                    if tail.starts_with(&frame::EXSS_SYNC) {
                        let has = |sync: [u8; 4]| tail.windows(4).take(frame::exss_size(tail).unwrap_or(0)).any(|w| w == sync);
                        let c2 = if has([0x41, 0xa2, 0x95, 0x47]) {
                            "dtsl"
                        } else if has([0x0a, 0x80, 0x19, 0x21]) {
                            "dtse"
                        } else {
                            "dtsh"
                        };
                        if codec == "dtsc" || c2 == "dtsl" {
                            codec = c2;
                        }
                        if let Some(total) = params::exss_total_channels(tail).filter(|&n| n > channels) {
                            channels = total;
                        }
                    }
                    at += c.size + frame::exss_size(tail).unwrap_or(0);
                }
                t.codec_string = Some(codec.into());
                t.channels = Some(channels);
                self.rate = core.rate;
            }
            Kind::TrueHd | Kind::Mlp => {
                let sync = if self.kind == Kind::TrueHd { 0xba } else { 0xbb };
                let Some(i) = (0..es.len().saturating_sub(16)).find(|&i| es[i + 4..i + 7] == [0xf8, 0x72, 0x6f] && es[i + 7] == sync) else { return Ok(false) };
                let rate = |nib: u8| if nib == 0xf { 0 } else { (if nib & 8 != 0 { 44_100 } else { 48_000 }) << (nib & 7) };
                if self.kind == Kind::TrueHd {
                    let ratebits = es[i + 8] >> 4;
                    self.rate = rate(ratebits);
                    self.thd_samples = 40 << (ratebits & 7);
                    let bits40 = u64::from_be_bytes([0, 0, 0, 0, es[i + 8], es[i + 9], es[i + 10], es[i + 11]]);
                    let chan6 = ((bits40 >> (32 - 17)) & 0x1f) as u32;
                    let chan8 = (bits40 & 0x1fff) as u32;
                    let map = if chan8 != 0 { chan8 } else { chan6 };
                    const COUNT: [u32; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 2, 1];
                    t.channels = Some((0..13).filter(|b| map >> b & 1 == 1).map(|b| COUNT[b]).sum());
                    t.bit_depth = Some(24);
                    t.codec = "truehd".into();
                } else {
                    let ratebits = es[i + 9] >> 4;
                    self.rate = rate(ratebits);
                    self.thd_samples = 40 << (ratebits & 7);
                    const MLP_CH: [u32; 21] = [1, 2, 3, 4, 3, 4, 5, 3, 4, 5, 4, 5, 6, 4, 5, 4, 5, 6, 5, 5, 6];
                    t.channels = MLP_CH.get(usize::from(es[i + 11] & 0x1f)).copied();
                    t.bit_depth = Some(match es[i + 8] >> 4 {
                        0 => 16,
                        1 => 20,
                        _ => 24,
                    });
                    t.codec = "mlp".into();
                }
                t.sample_rate = Some(self.rate).filter(|&r| r > 0);
                t.codec_string = Some("mlpa".into());
                if self.rate == 0 {
                    return Ok(false);
                }
            }
            Kind::Adts => {
                let Some((_, rate, ch, pb)) = frame::adts_header(&es) else { return Ok(false) };
                t.sample_rate = Some(rate);
                t.channels = Some(if ch == 7 { 8 } else { ch }).filter(|&c| c > 0);
                t.codec = "aac".into();
                t.codec_string = Some(format!("mp4a.40.{}", pb / 1000));
                self.rate = rate;
            }
            Kind::Flac => {
                let f = self.flac.as_ref().unwrap();
                t.sample_rate = Some(f.rate);
                t.channels = Some(f.channels);
                t.bit_depth = Some(f.bps);
                t.codec = "flac".into();
                t.codec_string = Some("flac".into());
                self.rate = f.rate;
                self.total_ticks = Some(f.total as i64).filter(|&n| n > 0);
                self.exact_total = f.total > 0;
            }
            Kind::Wav => {
                let w = self.wav.as_ref().unwrap();
                let Some(cs) = w.codec_string() else { return Err(Error::Format("unsupported WAV sample format")) };
                t.sample_rate = Some(w.rate);
                t.channels = Some(w.channels);
                t.bit_depth = Some(w.bits);
                t.codec = "wav".into();
                t.codec_string = Some(cs);
                self.rate = w.rate;
                self.total_ticks = Some(((self.data_end - self.data_start) / u64::from(w.block_align)) as i64);
                self.exact_total = true;
            }
        }
        if self.rate == 0 {
            return Ok(false);
        }
        t.timescale = self.rate;
        self.tracks = vec![t];
        Ok(true)
    }

    // ------------------------------------------------------------------ framing

    pub(crate) fn frame(&self, b: &[u8], eof: bool) -> FrameRes {
        use FrameRes::{Bad, Frame, More};
        match self.kind {
            Kind::Ac3 | Kind::Eac3 => {
                if b.len() < 8 {
                    return if eof { Bad } else { More(8) };
                }
                let Some(h) = frame::ac3_header(b) else { return Bad };
                if h.eac3 && h.strmtyp != 0 && h.strmtyp != 2 {
                    return Bad;
                }
                let mut end = h.size;
                if b.len() < end {
                    return More(end);
                }
                if h.eac3 {
                    loop {
                        if b.len() < end + 8 {
                            if eof {
                                break;
                            }
                            return More(end + 8);
                        }
                        match frame::ac3_header(&b[end..]) {
                            Some(d) if d.eac3 && d.strmtyp == 1 => {
                                if b.len() < end + d.size {
                                    return More(end + d.size);
                                }
                                end += d.size;
                            }
                            _ => break,
                        }
                    }
                }
                Frame(Fr { len: end, dur: i64::from(h.samples), key: true, pts: None })
            }
            Kind::Dts => {
                if b.len() < 12 {
                    return if eof { Bad } else { More(12) };
                }
                let Some(core) = frame::dts_core(b) else { return Bad };
                let mut end = core.size;
                if b.len() < end {
                    return More(end);
                }
                loop {
                    if b.len() < end + 10 {
                        if eof {
                            break;
                        }
                        return More(end + 10);
                    }
                    if b[end..end + 4] != frame::EXSS_SYNC {
                        break;
                    }
                    let Some(size) = frame::exss_size(&b[end..]) else { break };
                    if b.len() < end + size {
                        return More(end + size);
                    }
                    end += size;
                }
                Frame(Fr { len: end, dur: i64::from(core.samples), key: true, pts: None })
            }
            Kind::TrueHd | Kind::Mlp => {
                if b.len() < 8 {
                    return if eof { Bad } else { More(8) };
                }
                let len = ((usize::from(b[0] & 15) << 8) | usize::from(b[1])) * 2;
                if len < 8 {
                    return Bad;
                }
                if b.len() < len {
                    return More(len);
                }
                let last = if self.kind == Kind::TrueHd { 0xba } else { 0xbb };
                let key = b[4..7] == [0xf8, 0x72, 0x6f] && b[7] == last;
                Frame(Fr { len, dur: self.thd_samples, key, pts: None })
            }
            Kind::Adts => {
                if b.len() < 7 {
                    return if eof { Bad } else { More(7) };
                }
                let Some((len, rate, _, pb)) = frame::adts_header(b) else { return Bad };
                if rate != self.rate || len < if b[1] & 1 == 0 { 9 } else { 7 } {
                    return Bad;
                }
                if b.len() < len {
                    return More(len);
                }
                Frame(Fr { len, dur: 1024 * i64::from(pb % 1000), key: true, pts: None })
            }
            Kind::Flac => flac::frame(self.flac.as_ref().unwrap(), b, eof),
            Kind::Wav => Bad,
        }
    }

    /// Cheap test of whether a frame could start at `b[0]`, used to skip junk and to resynchronise.
    /// TrueHD / MLP only start at major syncs here (the other access units cannot be told apart).
    fn candidate(&self, b: &[u8]) -> bool {
        match self.kind {
            Kind::Ac3 | Kind::Eac3 => b.len() >= 2 && b[0] == 0x0b && b[1] == 0x77,
            Kind::Dts => b.len() >= 4 && b[..4] == [0x7f, 0xfe, 0x80, 0x01],
            Kind::TrueHd | Kind::Mlp => b.len() >= 8 && b[4..7] == [0xf8, 0x72, 0x6f] && b[7] == if self.kind == Kind::TrueHd { 0xba } else { 0xbb },
            Kind::Adts => b.len() >= 2 && b[0] == 0xff && b[1] & 0xf6 == 0xf0,
            Kind::Flac => b.len() >= 2 && b[0] == 0xff && b[1] & 0xfe == 0xf8,
            Kind::Wav => false,
        }
    }

    /// The frame at `pos` once enough bytes are read for it. `None` when it is not a frame or is cut off by the end.
    fn frame_full(&self, win: &mut Window, src: &dyn ByteSource, pos: u64, limit: u64) -> Result<Option<Fr>, Error> {
        let mut need = 64usize;
        loop {
            let b = win.at(src, pos, need, limit)?;
            let eof = pos + b.len() as u64 >= limit;
            match self.frame(b, eof) {
                FrameRes::Frame(f) => return Ok(Some(f)),
                FrameRes::More(n) if !eof && n > need && n <= MAX_FRAME => need = n,
                _ => return Ok(None),
            }
        }
    }

    /// First position at or after `from` where a frame starts and is followed by another frame (or the end).
    fn resync(&self, src: &dyn ByteSource, from: u64, limit: u64) -> Result<Option<(u64, Fr)>, Error> {
        let mut win = Window::new(12 * 1024);
        let mut pos = from;
        while pos + 8 <= limit {
            let b = win.at(src, pos, 4096, limit)?;
            let upto = b.len().saturating_sub(8).max(1);
            let hit = (0..upto.min(b.len())).find(|&i| self.candidate(&b[i..]));
            let Some(i) = hit else {
                pos += upto as u64;
                continue;
            };
            let at = pos + i as u64;
            if let Some(f) = self.frame_full(&mut win, src, at, limit)? {
                let next = at + f.len as u64;
                if next + 8 > limit || self.frame_full(&mut win, src, next, limit)?.is_some() {
                    return Ok(Some((at, f)));
                }
            }
            pos = at + 1;
        }
        Ok(None)
    }

    /// Three frames in a row from `at` (fewer when the file ends).
    fn chain_ok(&self, src: &dyn ByteSource, at: u64) -> Result<bool, Error> {
        let limit = self.limit(src);
        let mut win = Window::new(16 * 1024);
        let mut pos = at;
        for _ in 0..3 {
            if pos + 8 > limit {
                return Ok(true);
            }
            // TrueHD / MLP: the first access unit of a stream is a major sync, others follow by length.
            let Some(f) = self.frame_full(&mut win, src, pos, limit)? else { return Ok(false) };
            pos += f.len as u64;
        }
        Ok(true)
    }

    // ------------------------------------------------------------------ scanning

    /// Packets from `from` (a frame boundary) up to `stop_at` (no packet starts at or beyond it), about
    /// `budget` bytes, or the end of the data.
    fn scan(&self, src: &dyn ByteSource, from: u64, pts0: i64, stop_at: u64, budget: u64, require_key: bool) -> Result<ScanOut, Error> {
        let limit = self.limit(src);
        if self.kind == Kind::Wav {
            return Ok(self.wav.as_ref().unwrap().scan(self.data_start, limit, from, stop_at, budget));
        }
        let mut win = Window::new(((budget as usize).saturating_add(4 * 1024)).clamp(16 * 1024, CHUNK));
        let (mut pos, mut pts) = (from, pts0);
        let mut packets = Vec::new();
        let mut need_key = require_key;
        while pos < stop_at.min(limit) && pos - from < budget {
            let mut need = 64usize;
            let res = loop {
                let b = win.at(src, pos, need, limit)?;
                let eof = pos + b.len() as u64 >= limit;
                match self.frame(b, eof) {
                    FrameRes::More(n) if !eof && n > need && n <= MAX_FRAME => need = n,
                    FrameRes::More(_) if eof => break None,
                    r => break Some(r),
                }
            };
            match res {
                None => {
                    // A frame cut off by the end of the data: nothing more to index.
                    pos = limit;
                }
                Some(FrameRes::Frame(f)) => {
                    let end = pos + f.len as u64;
                    if end > stop_at && stop_at < limit {
                        pos = stop_at;
                        break;
                    }
                    if need_key && !f.key {
                        pos = end;
                        continue;
                    }
                    need_key = false;
                    let p = f.pts.unwrap_or(pts);
                    packets.push(PacketRef { track: 1, pts: p, dts: p, duration: f.dur, keyframe: f.key, presentation: true, offset: pos, size: f.len as u32, inner: 0 });
                    pts = p + f.dur;
                    pos = end;
                }
                Some(_) => {
                    // Junk: move to the next place a frame could start.
                    let b = win.at(src, pos, 64, limit)?;
                    let skip = (1..b.len()).find(|&i| self.candidate(&b[i..])).unwrap_or_else(|| b.len().saturating_sub(7).max(1));
                    pos += skip as u64;
                }
            }
        }
        Ok(ScanOut { packets, end: pos, pts_end: pts })
    }

    // ------------------------------------------------------------------ open

    fn initial_region(&mut self, src: &dyn ByteSource) -> Result<Vec<PacketRef>, Error> {
        let key = matches!(self.kind, Kind::TrueHd | Kind::Mlp);
        let out = self.scan(src, self.data_start, 0, u64::MAX, HEAD_WINDOW, key)?;
        let Some(first) = out.packets.first() else { return Err(Error::Format("no audio frames")) };
        let region = Region { start: first.offset, end: out.end, first_pts: first.pts, pts_end: out.pts_end };
        let head_bytes = region.end - self.data_start;
        let head_ticks = region.pts_end - region.first_pts;
        self.regions = vec![region];
        self.active = 0;
        if self.regions[0].end >= self.data_end {
            self.total_ticks = Some(self.regions[0].pts_end);
            self.exact_total = true;
        } else if !self.exact_total {
            // Constant bit rate codecs whose head frames all have one size need no sampling: the head density
            // is the whole file's. (A lossless stream can start with constant silence, so it always samples.)
            let sizes = out.packets.iter().map(|p| p.size);
            let (mn, mx) = (sizes.clone().min().unwrap_or(0), sizes.max().unwrap_or(0));
            let cbr = match self.kind {
                Kind::Ac3 | Kind::Eac3 => true,
                Kind::Dts => self.tracks[0].codec_string.as_deref() == Some("dtsc"),
                _ => false,
            };
            if cbr && mx - mn <= mx / 50 {
                let total = (self.data_end - self.data_start) as f64 * head_ticks as f64 / head_bytes.max(1) as f64;
                self.knots = vec![(self.data_start, 0.0), (self.data_end, total)];
                self.total_ticks = Some(total.round() as i64);
                self.cbr_model = true;
            } else {
                self.build_model(src, head_bytes, head_ticks)?;
            }
        } else {
            let total = self.total_ticks.unwrap_or(0) as f64;
            self.knots = vec![(self.data_start, 0.0), (self.data_end, total)];
        }
        self.gap_merge = (1 << 20) + 4 * out.packets.iter().map(|p| u64::from(p.size)).max().unwrap_or(0);
        Ok(out.packets)
    }

    /// Bytes-to-time model: constant density per window around a few probes across the file, the first
    /// window being the exact head. Gives the duration estimate and seek positions for VBR streams.
    fn build_model(&mut self, src: &dyn ByteSource, head_bytes: u64, head_ticks: i64) -> Result<(), Error> {
        let (a, b) = (self.regions[0].end, self.data_end);
        // (center, ticks per byte)
        let mut pts = vec![(self.data_start + head_bytes / 2, head_ticks as f64 / head_bytes.max(1) as f64)];
        let key = matches!(self.kind, Kind::TrueHd | Kind::Mlp);
        for k in 1..=PROBES {
            let at = a + (b - a) * k / (PROBES + 1);
            let limit = self.limit(src);
            let Some((start, _)) = self.resync(src, at, limit)? else { continue };
            let out = self.scan(src, start, 0, u64::MAX, PROBE_BYTES, key)?;
            let bytes = out.end.saturating_sub(start);
            if out.packets.len() >= 3 && bytes > 0 {
                pts.push((start + bytes / 2, out.pts_end as f64 / bytes as f64));
            }
        }
        // Segment boundaries halfway between probe centers.
        let mut edges = vec![self.data_start];
        for w in pts.windows(2) {
            edges.push((w[0].0 + w[1].0) / 2);
        }
        edges.push(self.data_end);
        let mut knots = vec![(self.data_start, 0.0)];
        let mut ticks = 0.0;
        for (i, &(_, d)) in pts.iter().enumerate() {
            ticks += (edges[i + 1] - edges[i]) as f64 * d;
            knots.push((edges[i + 1], ticks));
        }
        // The head is exact up to its end.
        self.knots = knots;
        self.total_ticks = Some(ticks.round() as i64);
        Ok(())
    }

    fn ticks_at(&self, off: u64) -> f64 {
        let k = &self.knots;
        if k.len() < 2 {
            return 0.0;
        }
        let i = k.partition_point(|&(o, _)| o <= off).clamp(1, k.len() - 1);
        let ((o0, t0), (o1, t1)) = (k[i - 1], k[i]);
        if o1 == o0 {
            return t0;
        }
        t0 + (t1 - t0) * (off.clamp(o0, o1) - o0) as f64 / (o1 - o0) as f64
    }

    fn offset_at(&self, ticks: i64) -> u64 {
        let k = &self.knots;
        if k.len() < 2 {
            return self.data_start;
        }
        let t = ticks.max(0) as f64;
        let i = k.partition_point(|&(_, tk)| tk <= t).clamp(1, k.len() - 1);
        let ((o0, t0), (o1, t1)) = (k[i - 1], k[i]);
        if t1 <= t0 {
            return o0;
        }
        let f = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
        o0 + ((o1 - o0) as f64 * f) as u64
    }

    // ------------------------------------------------------------------ regions

    /// Once one region spans the whole file its end is the exact duration.
    fn finish(&mut self) {
        if let [r] = &self.regions[..] {
            if r.first_pts == 0 && r.end >= self.data_end && !self.exact_total {
                self.total_ticks = Some(r.pts_end);
                self.exact_total = true;
            }
        }
    }

    /// Extends region `ri` by about `budget` bytes. A gap to the next region smaller than `gap_merge`
    /// is closed completely and the two regions become one.
    fn extend(&mut self, src: &dyn ByteSource, ri: usize, budget: u64) -> Result<Update, Error> {
        let mut upd = Update::default();
        let r = &self.regions[ri];
        let (end, pts_end) = (r.end, r.pts_end);
        let next = self.regions.get(ri + 1).map(|n| (n.start, n.end, n.first_pts, n.pts_end));
        let (stop, budget) = match next {
            Some((ns, ..)) if ns.saturating_sub(end) <= budget.saturating_add(self.gap_merge) => (ns, u64::MAX),
            Some((ns, ..)) => (ns, budget),
            None => (u64::MAX, budget),
        };
        let out = self.scan(src, end, pts_end, stop, budget, false)?;
        let r = &mut self.regions[ri];
        r.end = out.end;
        r.pts_end = out.pts_end;
        upd.packets = out.packets;
        if let Some((ns, ne, nfirst, npts_end)) = next {
            if out.end >= ns {
                let delta = if self.exact_pts() { 0 } else { out.pts_end - nfirst };
                if delta != 0 {
                    upd.shift(Shift { from: ns, to: ne, delta });
                }
                let r = &mut self.regions[ri];
                r.end = ne;
                r.pts_end = npts_end + delta;
                self.regions.remove(ri + 1);
            }
        }
        self.active = ri;
        self.finish();
        Ok(upd)
    }

    pub(crate) fn index_more(&mut self, src: &dyn ByteSource, max_bytes: u64) -> Result<Update, Error> {
        let ri = self.active.min(self.regions.len() - 1);
        if self.regions[ri].end >= self.data_end {
            return Ok(Update::default());
        }
        self.extend(src, ri, max_bytes.clamp(1, MAX_STEP))
    }

    /// Indexes the span right behind `packet` when it ends its region and more data follows.
    pub(crate) fn index_after(&mut self, src: &dyn ByteSource, packet: &PacketRef) -> Result<Update, Error> {
        let Some(ri) = self.regions.iter().position(|r| packet.offset >= r.start && packet.offset < r.end) else { return Ok(Update::default()) };
        let r = &self.regions[ri];
        if packet.offset + u64::from(packet.size) != r.end || r.end >= self.data_end {
            return Ok(Update::default());
        }
        self.extend(src, ri, 4 << 20)
    }

    /// Fills every gap and the tail: the whole file is one region afterwards.
    pub(crate) fn index_all(&mut self, src: &dyn ByteSource) -> Result<Update, Error> {
        let mut upd = Update::default();
        while self.regions.len() > 1 || self.regions[0].end < self.data_end {
            let u = self.extend(src, 0, MAX_STEP)?;
            upd.absorb(u);
        }
        Ok(upd)
    }

    // ------------------------------------------------------------------ seeking

    /// Makes sure packets around `target` (ticks) are indexed. Returns the packets added and the byte range
    /// of the region holding them. `attempt` > 0 reaches further back (the previous landing had no usable frame).
    pub(crate) fn prepare_seek(&mut self, src: &dyn ByteSource, target: i64, attempt: u32) -> Result<(Update, (u64, u64)), Error> {
        let target = target.max(0);
        let mut upd = Update::default();
        // Already indexed.
        if attempt == 0 {
            if let Some(ri) = self.regions.iter().position(|r| r.first_pts <= target && target < r.pts_end) {
                self.active = ri;
                return Ok((upd, (self.regions[ri].start, self.regions[ri].end)));
            }
        }
        let limit = self.limit(src);
        let center = match (self.kind, &self.wav) {
            (Kind::Wav, Some(w)) => self.data_start + (target as u64).min(u64::MAX / 2).saturating_mul(u64::from(w.block_align)),
            _ => self.offset_at(target),
        }
        .min(limit);
        let back = SEEK_SLACK << attempt.min(10);
        let off = center.saturating_sub(back).max(self.data_start);
        let mut ri = self.locate_region(off);
        // The head region counts samples from the start of the stream: growing it to the target beats the model.
        if !matches!(self.kind, Kind::Wav | Kind::Flac) && !self.cbr_model {
            if let Some(h) = self.regions.first() {
                if h.first_pts == 0 && h.start <= self.data_start + self.gap_merge && h.end < limit && center.saturating_sub(h.end) <= BRIDGE {
                    ri = Some(0);
                }
            }
        }
        if ri.is_none() {
            // A region just before the landing spot is extended instead of starting a new one.
            ri = self.regions.iter().position(|r| r.end <= off && off - r.end < self.gap_merge && r.end < self.data_end && r.pts_end <= target);
        }
        let ri = match ri {
            Some(ri) => ri,
            None => self.new_region(src, off, target, &mut upd)?,
        };
        let mut ri = ri;
        // Reach the target (a merge keeps the index of the merged region).
        let mut guard = 0;
        while self.regions[ri].pts_end <= target && self.regions[ri].end < limit && guard < 4096 {
            let step = (center + 2 * PROBE_BYTES).saturating_sub(self.regions[ri].end).clamp(64 * 1024, MAX_STEP);
            let u = self.extend(src, ri, step)?;
            upd.absorb(u);
            guard += 1;
            ri = self.active.min(self.regions.len() - 1);
        }
        self.active = ri;
        let win = (self.regions[ri].start, self.regions[ri].end);
        Ok((upd, win))
    }

    fn locate_region(&self, off: u64) -> Option<usize> {
        self.regions.iter().position(|r| off >= r.start && off < r.end)
    }

    /// Starts a region at the first frame at or after `off` (FLAC: bisection, PCM: the packet boundary).
    fn new_region(&mut self, src: &dyn ByteSource, off: u64, target: i64, upd: &mut Update) -> Result<usize, Error> {
        let limit = self.limit(src);
        let (start, pts0): (u64, i64) = match self.kind {
            Kind::Wav => {
                let w = self.wav.as_ref().unwrap();
                let pkt = u64::from(w.block_align) * wav::FRAMES;
                let k = (off - self.data_start) / pkt;
                let start = self.data_start + k * pkt;
                (start, (k * wav::FRAMES) as i64)
            }
            Kind::Flac => {
                self.flac_locate(src, target)?
            }
            _ => {
                let Some((start, _)) = self.resync(src, off, limit)? else {
                    // Nothing after `off`: the last region ends the file.
                    return Ok(self.regions.len() - 1);
                };
                (start, self.ticks_at(start).round() as i64)
            }
        };
        if let Some(ri) = self.locate_region(start) {
            return Ok(ri);
        }
        let at = self.regions.partition_point(|r| r.start < start);
        let stop = self.regions.get(at).map_or(u64::MAX, |n| n.start);
        let out = self.scan(src, start, pts0, stop, 64 * 1024, false)?;
        let Some(first) = out.packets.first() else {
            return Ok(at.saturating_sub(1).min(self.regions.len() - 1));
        };
        let region = Region { start: first.offset, end: out.end, first_pts: first.pts, pts_end: out.pts_end };
        self.regions.insert(at, region);
        upd.packets.extend(out.packets);
        // Joined to the next region already?
        if let Some(n) = self.regions.get(at + 1) {
            if self.regions[at].end >= n.start {
                let (ns, ne, nf, np) = (n.start, n.end, n.first_pts, n.pts_end);
                let delta = if self.exact_pts() { 0 } else { self.regions[at].pts_end - nf };
                if delta != 0 {
                    upd.shift(Shift { from: ns, to: ne, delta });
                }
                self.regions[at].end = ne;
                self.regions[at].pts_end = np + delta;
                self.regions.remove(at + 1);
            }
        }
        self.active = at;
        Ok(at)
    }

    /// FLAC: the frame at or before `target` found through the seek table or by bisection. (offset, sample).
    fn flac_locate(&self, src: &dyn ByteSource, target: i64) -> Result<(u64, i64), Error> {
        let f = self.flac.as_ref().unwrap();
        let limit = self.limit(src);
        let total = if f.total > 0 { f.total as i64 } else { self.total_ticks.unwrap_or(target.max(1) * 2) };
        let mut lo = (self.data_start, 0i64);
        let mut hi = (limit, total.max(target + 1));
        for &(s, o) in &f.seek {
            let s = s as i64;
            if s <= target {
                if o >= lo.0 {
                    lo = (o, s);
                }
            } else {
                if o < hi.0 {
                    hi = (o, s);
                }
                break;
            }
        }
        // The table's frame must be a frame; otherwise bisect from the start.
        if lo.0 != self.data_start {
            let mut win = Window::new(48 * 1024);
            match self.frame_full(&mut win, src, lo.0, limit)? {
                Some(fr) if fr.pts == Some(lo.1) => {}
                _ => lo = (self.data_start, 0),
            }
        }
        for _ in 0..40 {
            if hi.0.saturating_sub(lo.0) < 160 * 1024 {
                break;
            }
            let span = (hi.1 - lo.1).max(1) as f64;
            let frac = ((target - lo.1) as f64 / span).clamp(0.02, 0.98);
            let mid = lo.0 + ((hi.0 - lo.0) as f64 * frac) as u64;
            match self.resync(src, mid, hi.0)? {
                Some((o, fr)) => {
                    let s = fr.pts.unwrap_or(0);
                    if s <= target {
                        if o <= lo.0 {
                            break;
                        }
                        lo = (o, s);
                    } else {
                        if o >= hi.0 {
                            break;
                        }
                        hi = (o, s);
                    }
                }
                None => {
                    if mid >= hi.0 {
                        break;
                    }
                    hi = (mid, hi.1);
                }
            }
        }
        Ok(lo)
    }
}
