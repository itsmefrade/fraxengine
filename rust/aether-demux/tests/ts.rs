//! MPEG-TS / M2TS demuxer against ffprobe packet oracles (see tests/fixtures/generate-ts.sh).
//! Missing fixtures are skipped with a note so a bare checkout passes.

mod support;

use aether_demux::{ByteSource, Container, Demuxer, FileSource, PacketRef, SliceSource, TrackKind};
use support::*;
use std::cell::Cell;

/// Counts the bytes a demuxer pulls from the source.
struct Counting<'a> {
    inner: &'a FileSource,
    bytes: Cell<u64>,
}
impl<'a> Counting<'a> {
    fn new(inner: &'a FileSource) -> Self {
        Self { inner, bytes: Cell::new(0) }
    }
}
impl aether_demux::ByteSource for Counting<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let n = self.inner.read_at(offset, buf)?;
        self.bytes.set(self.bytes.get() + n as u64);
        Ok(n)
    }
}

fn fixtures() -> String {
    format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"))
}

fn lab() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media").to_owned()
}

const REAL: [(&str, &str); 4] = [
    ("Dolby Amaze.ATMOS.m2ts", "Dolby_Amaze.ATMOS.m2ts"),
    ("Dolby TrueHD 7.1 Channel Check.m2ts", "Dolby_TrueHD_7.1_Channel_Check.m2ts"),
    ("DTSX_Demo_2016.m2ts", "DTSX_Demo_2016.m2ts"),
    ("DTS-HD MA 7.1 'Dredd' Sound Check.m2ts", "DTS_HD_MA_7.1__Dredd__Sound_Check.m2ts"),
];

/// Compares the packets of each ffprobe stream (in demux order) with ours. `rows` are oracle rows,
/// `mine[s]` the packets of track `s` starting at the packet matching the first oracle row.
fn compare_stream(label: &str, d: &Demuxer, src: &dyn aether_demux::ByteSource, s: usize, rows: &[&Row], mine: &[&PacketRef], hash_every: usize) -> Vec<String> {
    let wrap = if label.contains("wrap") { 1i64 << 33 } else { 0 };
    let mut bad = Vec::new();
    let track = &d.tracks()[s];
    let mut buf = Vec::new();
    if mine.len() < rows.len() {
        bad.push(format!("{label}: stream {s} has {} packets after the start point, ffprobe {}", mine.len(), rows.len()));
    }
    for (i, (r, p)) in rows.iter().zip(mine).enumerate() {
        if let Some(pts) = r.pts {
            if p.pts != pts + wrap {
                bad.push(format!("{label}: s{s} #{i} pts {} vs {}", p.pts, pts));
            }
        }
        if let Some(dts) = r.dts {
            if p.dts != dts + wrap {
                bad.push(format!("{label}: s{s} #{i} dts {} vs {}", p.dts, dts));
            }
        }
        if p.size != r.size {
            bad.push(format!("{label}: s{s} #{i} size {} vs {}", p.size, r.size));
        }
        if let Some(du) = r.duration {
            // ffmpeg estimates VC-1 frame durations from a frame rate probe; ours come from decode time gaps.
            if du > 0 && p.duration != du && track.kind != TrackKind::Subtitle && track.codec != "vc1" {
                bad.push(format!("{label}: s{s} #{i} duration {} vs {}", p.duration, du));
            }
        }
        if track.kind == TrackKind::Video && p.keyframe != r.key {
            bad.push(format!("{label}: s{s} #{i} keyframe {} vs {}", p.keyframe, r.key));
        }
        // ffprobe's pos is exact for 192 byte packets only and absent for later frames of a PES.
        if r.pos != u64::MAX && label.ends_with("m2ts") && p.inner == 0 && p.offset != r.pos && track.kind != TrackKind::Subtitle {
            bad.push(format!("{label}: s{s} #{i} pos {} vs {}", p.offset, r.pos));
        }
        if i % hash_every == 0 || i < 8 {
            d.read_packet(src, p, &mut buf).unwrap();
            if sha256_hex(&buf) != r.hash {
                bad.push(format!("{label}: s{s} #{i} data hash differs (len {} vs {})", buf.len(), r.size));
            }
        }
        if bad.len() > 12 {
            break;
        }
    }
    bad
}

fn rows_by_stream(rows: &[Row]) -> Vec<Vec<&Row>> {
    let n = rows.iter().map(|r| r.stream).max().map_or(0, |m| m + 1);
    let mut v = vec![Vec::new(); n];
    for r in rows {
        v[r.stream].push(r);
    }
    v
}

fn check_prefix(label: &str, media: &str, csv: &str, hash_every: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(csv) else {
        eprintln!("skip {label}: no oracle {csv}");
        return Vec::new();
    };
    let Ok(src) = FileSource::open(media) else {
        eprintln!("skip {label}: no media {media}");
        return Vec::new();
    };
    let d = match Demuxer::open(&src) {
        Ok(d) => d,
        Err(e) => return vec![format!("{label}: open failed: {e}")],
    };
    assert_eq!(d.container(), Container::MpegTs);
    let rows = read_rows(&text);
    let by = rows_by_stream(&rows);
    let mut bad = Vec::new();
    if by.len() != d.tracks().len() {
        bad.push(format!("{label}: ffprobe has {} streams, we have {}", by.len(), d.tracks().len()));
        return bad;
    }
    for (s, rows) in by.iter().enumerate() {
        let mine: Vec<&PacketRef> = d.track_packets(d.tracks()[s].id).collect();
        // The oracle stops at its cap, so compare only that many.
        bad.extend(compare_stream(label, &d, &src, s, rows, &mine, hash_every));
    }
    bad
}

#[test]
fn synthetic_transport_streams_match_ffprobe() {
    let dir = format!("{}/ts", fixtures());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".ts") || n.ends_with(".m2ts")).collect())
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        eprintln!("no generated TS fixtures (run tests/fixtures/generate-ts.sh)");
    }
    let mut bad = Vec::new();
    for n in names.iter().filter(|n| !n.contains("jump")) {
        let stem = n.rsplit_once('.').unwrap().0;
        let csv = format!("{dir}/{stem}.csv");
        if !std::path::Path::new(&csv).exists() {
            eprintln!("skip {n}: no oracle");
            continue;
        }
        let b = check_prefix(n, &format!("{dir}/{n}"), &csv, 1);
        eprintln!("{n}: {} problems", b.len());
        bad.extend(b);
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn timeline_jump_is_removed_and_reported() {
    let path = format!("{}/ts/h264-jump.ts", fixtures());
    let Ok(src) = FileSource::open(&path) else {
        eprintln!("skip: no {path}");
        return;
    };
    let d = Demuxer::open(&src).unwrap();
    let oracle = read_rows(&std::fs::read_to_string(path.replace(".ts", ".csv")).unwrap());
    let id = d.tracks()[0].id;
    let mine: Vec<&PacketRef> = d.track_packets(id).collect();
    assert_eq!(mine.len(), oracle.len());
    assert_eq!(d.timeline_breaks().len(), 1, "{:?}", d.timeline_breaks());
    // Before the jump the timestamps equal ffprobe's; after it they continue from the last packet.
    let cut = oracle.iter().position(|r| r.dts.unwrap() > 10_000_000).unwrap();
    for i in 0..cut {
        assert_eq!(mine[i].dts, oracle[i].dts.unwrap());
    }
    for w in mine.windows(2) {
        assert!(w[1].dts > w[0].dts, "timeline must keep counting: {} then {}", w[0].dts, w[1].dts);
    }
    let shift = mine[cut].dts - oracle[cut].dts.unwrap();
    assert_eq!(d.timeline_breaks()[0].shift, shift);
    for i in cut..mine.len() {
        assert_eq!(mine[i].dts - oracle[i].dts.unwrap(), shift);
    }
}

#[test]
fn lab_m2ts_first_packets_match_ffprobe() {
    let mut bad = Vec::new();
    for (media, base) in REAL {
        let b = check_prefix(media, &format!("{}/{media}", lab()), &format!("{}/ts-real/{base}.csv", fixtures()), 3);
        eprintln!("{media}: {} problems", b.len());
        bad.extend(b);
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn lab_m2ts_packet_counts_match_ffprobe() {
    let mut bad = Vec::new();
    for (media, base) in REAL {
        let Ok(counts) = std::fs::read_to_string(format!("{}/ts-real/{base}.counts", fixtures())) else { continue };
        let Ok(src) = FileSource::open(format!("{}/{media}", lab())) else { continue };
        let d = Demuxer::open(&src).unwrap();
        for line in counts.lines() {
            let Some((s, n)) = line.split_once(',') else { continue };
            let (s, n): (usize, usize) = (s.parse().unwrap(), n.parse().unwrap());
            let ours = d.track_packets(d.tracks()[s].id).count();
            if ours != n {
                bad.push(format!("{media}: stream {s} has {ours} packets, ffprobe {n}"));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn lab_m2ts_lazy_seek_matches_full_index_and_ffprobe_packets() {
    let mut bad = Vec::new();
    for (media, base) in REAL {
        let Ok(text) = std::fs::read_to_string(format!("{}/ts-real/{base}.seek.csv", fixtures())) else { continue };
        let Ok(time) = std::fs::read_to_string(format!("{}/ts-real/{base}.seek.time", fixtures())) else { continue };
        let Ok(file) = FileSource::open(format!("{}/{media}", lab())) else { continue };
        let src = Counting::new(&file);
        let seek_ticks = (time.trim().parse::<f64>().unwrap() * 90_000.0) as i64;
        // Lazy open, then straight into the middle without indexing what lies before it.
        let mut d = Demuxer::open_lazy(&src).unwrap();
        let video = d.tracks().iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
        let at = d.seek_to(&src, video, seek_ticks).unwrap();
        let key = d.packets()[at].clone();
        let read = src.bytes.get();
        if read > file.len() / 3 {
            bad.push(format!("{media}: lazy open + seek read {read} of {} bytes", file.len()));
        }
        if !key.keyframe || key.pts > seek_ticks || seek_ticks - key.pts > 90_000 * 12 {
            bad.push(format!("{media}: seek to {seek_ticks} landed on pts {} keyframe={}", key.pts, key.keyframe));
        }
        // The fully indexed demuxer picks the same keyframe.
        let full = Demuxer::open(&file).unwrap();
        let fi = full.seek(&file, video, seek_ticks).unwrap();
        if full.packets()[fi].pts != key.pts || full.packets()[fi].offset != key.offset {
            bad.push(format!("{media}: lazy seek pts {} offset {} vs full index pts {} offset {}", key.pts, key.offset, full.packets()[fi].pts, full.packets()[fi].offset));
        }
        // ffprobe's packets after its own seek equal ours wherever both exist.
        let rows = read_rows(&text);
        let by = rows_by_stream(&rows);
        for (s, rows) in by.iter().enumerate() {
            let id = d.tracks()[s].id;
            let mine: Vec<&PacketRef> = d.track_packets(id).collect();
            // ffprobe may begin with an orphan (the dependent half of an E-AC-3 pair): align on the first row we have.
            let found = rows.iter().take(6).enumerate().find_map(|(ri, r)| mine.iter().position(|p| Some(p.dts) == r.dts && p.size == r.size).map(|mi| (ri, mi)));
            let Some((ri, mi)) = found else {
                bad.push(format!("{media}: stream {s}: none of the first ffprobe packets after its seek is in our window"));
                continue;
            };
            let n = (rows.len() - ri).min(mine.len() - mi).min(150);
            if s == 0 && n < 5 {
                bad.push(format!("{media}: only {n} video packets overlap the window"));
            }
            bad.extend(compare_stream(media, &d, &src, s, &rows[ri..ri + n], &mine[mi..mi + n], 4));
        }
        eprintln!("{media}: seek to {} -> keyframe pts {} ({} bytes read), {} problems so far", time.trim(), key.pts, read, bad.len());
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn rejects_non_transport_streams_and_survives_garbage() {
    let junk = vec![0x47u8; 100];
    assert!(Demuxer::open(&SliceSource(&junk)).is_err());
    let mut ts = vec![0u8; 188 * 8];
    for i in 0..8 {
        ts[i * 188] = 0x47;
        ts[i * 188 + 1] = 0x1f;
        ts[i * 188 + 2] = 0xff;
        ts[i * 188 + 3] = 0x10;
    }
    // Null packets only: no PAT/PMT.
    assert!(Demuxer::open(&SliceSource(&ts)).is_err());
}

// ---------------------------------------------------------------- hand built streams

fn pes(stream_id: u8, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0, 1, stream_id, 0, 0];
    let len = 3 + if pts.is_some() { 5 } else { 0 } + payload.len();
    v[4] = (len >> 8) as u8;
    v[5] = len as u8;
    v.extend([0x80, if pts.is_some() { 0x80 } else { 0 }, if pts.is_some() { 5 } else { 0 }]);
    if let Some(t) = pts {
        v.extend([
            0x21 | ((t >> 29) & 0x0e) as u8,
            (t >> 22) as u8,
            ((t >> 14) & 0xfe) as u8 | 1,
            (t >> 7) as u8,
            ((t << 1) & 0xfe) as u8 | 1,
        ]);
    }
    v.extend(payload);
    v
}

/// Splits `data` into transport packets of one PID; `prefix` adds the BDAV arrival time header.
fn packets(pid: u16, data: &[u8], cc: &mut u8, prefix: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut first = true;
    while at < data.len() || first {
        let room = 184;
        let take = (data.len() - at).min(room);
        let mut unit = Vec::new();
        if prefix {
            unit.extend([0, 0, 0, 0]);
        }
        unit.extend([0x47, ((pid >> 8) as u8 & 0x1f) | if first { 0x40 } else { 0 }, pid as u8]);
        let stuffing = room - take;
        if stuffing > 0 {
            unit.push(0x30 | (*cc & 15));
            unit.push(stuffing as u8 - 1);
            if stuffing > 1 {
                unit.push(0);
                unit.extend(std::iter::repeat_n(0xff, stuffing - 2));
            }
        } else {
            unit.push(0x10 | (*cc & 15));
        }
        unit.extend(&data[at..at + take]);
        *cc = (*cc + 1) & 15;
        out.extend(unit);
        at += take;
        first = false;
    }
    out
}

fn psi(table_id: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let mut s = vec![table_id, 0xb0 | ((body.len() + 9) >> 8) as u8, (body.len() + 9) as u8, (id >> 8) as u8, id as u8, 0xc1, 0, 0];
    s.extend(body);
    s.extend([0, 0, 0, 0]);
    let mut v = vec![0];
    v.extend(s);
    v
}

fn pmt_body(streams: &[(u8, u16, &[u8])]) -> Vec<u8> {
    let mut b = vec![0xe1, 0x00, 0xf0, 0x00];
    for &(ty, pid, desc) in streams {
        b.extend([ty, 0xe0 | (pid >> 8) as u8, pid as u8, 0xf0 | (desc.len() >> 8) as u8, desc.len() as u8]);
        b.extend(desc);
    }
    b
}

fn build(streams: &[(u8, u16, &[u8])], payloads: &[(u16, Vec<u8>)], prefix: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut c0, mut c1) = (0u8, 0u8);
    out.extend(packets(0, &psi(0, 1, &[0, 1, 0xe1, 0x00]), &mut c0, prefix));
    out.extend(packets(0x100, &psi(2, 1, &pmt_body(streams)), &mut c1, prefix));
    let mut ccs = std::collections::HashMap::new();
    for (pid, data) in payloads {
        out.extend(packets(*pid, data, ccs.entry(*pid).or_insert(0), prefix));
    }
    out
}

fn h264_au(idr: bool) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1, 0x09, 0x10];
    if idr {
        // SPS (1280x720 High@4.0), PPS, IDR slice.
        v.extend([0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60]);
        v.extend([0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]);
        v.extend([0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff]);
    } else {
        v.extend([0, 0, 0, 1, 0x41, 0x9a, 0x24, 0x6c, 0x41]);
    }
    v
}

#[test]
fn lpcm_pgs_dolby_vision_el_and_unsupported_streams() {
    let lpcm_header = [0x0b, 0x00, 0xb1, 0xc0];
    let lpcm: Vec<u8> = lpcm_header.iter().copied().chain((0..96u8).collect::<Vec<_>>()).collect();
    let hevc_au = vec![0, 0, 1, 0x46, 0x01, 0x10, 0, 0, 1, 0x26, 0x01, 0xaf, 0x08];
    let streams: Vec<(u8, u16, &[u8])> = vec![
        (0x24, 0x1011, &[]),
        (0x24, 0x1015, &[]),
        (0x1b, 0x1012, &[]),
        (0x80, 0x1100, &[0x0a, 4, b'e', b'n', b'g', 0]),
        (0x90, 0x1200, &[]),
        (0x83, 0x1101, &[]), // TrueHD PID without any packet: no AC-3 core track must be reported
        (0x15, 0x1300, &[]), // metadata: unsupported, counted as omitted
    ];
    let mut payloads = Vec::new();
    for n in 0..4u64 {
        payloads.push((0x1011, pes(0xe0, Some(900_000 + n * 3750), &hevc_au)));
        payloads.push((0x1015, pes(0xe0, Some(900_000 + n * 3750), &hevc_au)));
        payloads.push((0x1012, pes(0xe0, Some(900_000 + n * 3750), &h264_au(n == 0))));
        payloads.push((0x1100, pes(0xbd, Some(900_000 + n * 3750), &lpcm)));
        payloads.push((0x1200, pes(0xbd, Some(900_000 + n * 7500), format!("PG-segment-{n}").as_bytes())));
    }
    for prefix in [false, true] {
        let ts = build(&streams, &payloads, prefix);
        let d = Demuxer::open(&SliceSource(&ts)).unwrap();
        assert_eq!(d.container(), Container::MpegTs);
        assert_eq!(d.omitted_tracks(), 1);
        let by_pid = |pid: u64| d.tracks().iter().find(|t| t.id == pid).unwrap();
        assert!(!by_pid(0x1011).enhancement_layer);
        assert!(by_pid(0x1015).enhancement_layer, "UHD Blu-ray enhancement layer PID");
        assert_eq!(d.tracks().iter().filter(|t| t.codec == "ac3").count(), 0);
        let a = by_pid(0x1100);
        assert_eq!((a.codec.as_str(), a.channels, a.sample_rate, a.bit_depth), ("pcm_bluray", Some(8), Some(48_000), Some(24)));
        assert_eq!(a.codec_private, lpcm_header);
        assert_eq!(a.language.as_deref(), Some("eng"));
        let v = by_pid(0x1012);
        assert_eq!((v.width, v.height), (Some(1280), Some(720)), "{:?}", v.codec_string);
        assert_eq!(v.codec_string.as_deref(), Some("avc1.640028"));
        // One packet per PES for LPCM and PGS, payload intact.
        let mut buf = Vec::new();
        let pk: Vec<&PacketRef> = d.track_packets(0x1100).collect();
        assert_eq!(pk.len(), 4);
        d.read_packet(&SliceSource(&ts), pk[2], &mut buf).unwrap();
        assert_eq!(buf, lpcm);
        assert_eq!(pk[2].pts, 900_000 + 2 * 3750);
        let pg: Vec<&PacketRef> = d.track_packets(0x1200).collect();
        d.read_packet(&SliceSource(&ts), pg[3], &mut buf).unwrap();
        assert_eq!(buf, b"PG-segment-3");
        // H.264: one AU per packet, keyframe only for the IDR.
        let h: Vec<&PacketRef> = d.track_packets(0x1012).collect();
        assert_eq!(h.len(), 4);
        assert_eq!(h.iter().map(|p| p.keyframe).collect::<Vec<_>>(), [true, false, false, false]);
        d.read_packet(&SliceSource(&ts), h[0], &mut buf).unwrap();
        assert_eq!(buf, h264_au(true));
        // A file cut inside a packet still opens and keeps what precedes the cut.
        let cut = &ts[..ts.len() - 100];
        let t = Demuxer::open(&SliceSource(cut)).unwrap();
        assert!(t.track_packets(0x1100).count() >= 3);
    }
}

#[test]
fn truehd_pid_splits_the_ac3_core_by_extension_id() {
    // PES with extension stream id 0x76 carries AC-3 frames, others carry TrueHD access units.
    fn pes_ext(ext: u8, pts: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0xfd, 0, 0, 0x84, 0x81, 8];
        v.extend([
            0x21 | ((pts >> 29) & 0x0e) as u8,
            (pts >> 22) as u8,
            ((pts >> 14) & 0xfe) as u8 | 1,
            (pts >> 7) as u8,
            ((pts << 1) & 0xfe) as u8 | 1,
        ]);
        v.extend([0x01, 0x80 | 1, ext]); // PES_extension_flag_2: marker+length 1, stream_id_extension
        v.extend(payload);
        let len = v.len() - 6;
        v[4] = (len >> 8) as u8;
        v[5] = len as u8;
        v
    }
    let mut ac3 = vec![0u8; 192];
    ac3[..8].copy_from_slice(&[0x0b, 0x77, 0, 0, 0x04, 0x30, 0xe1, 0xff]);
    // Major sync access unit of 24 bytes (12 words): length, timestamp, f8726fba, format info (48 kHz, 8 channels).
    let mut thd = vec![0u8; 24];
    thd[..2].copy_from_slice(&[0x00, 12]);
    thd[4..8].copy_from_slice(&[0xf8, 0x72, 0x6f, 0xba]);
    thd[8..12].copy_from_slice(&[0x00, 0x57, 0x80, 0x4f]);
    let streams: Vec<(u8, u16, &[u8])> = vec![(0x83, 0x1100, &[])];
    let mut payloads = Vec::new();
    for n in 0..5u64 {
        payloads.push((0x1100, pes_ext(0x76, 900_000 + n * 2880, &ac3)));
        payloads.push((0x1100, pes_ext(0x72, 900_000 + n * 2880, &thd)));
    }
    let ts = build(&streams, &payloads, true);
    let d = Demuxer::open(&SliceSource(&ts)).unwrap();
    let ids: Vec<u64> = d.tracks().iter().map(|t| t.id).collect();
    assert_eq!(ids, [0x1100, 0x1_1100]);
    assert_eq!(d.tracks()[0].codec, "truehd");
    assert_eq!(d.tracks()[0].channels, Some(8));
    assert_eq!(d.tracks()[1].codec, "ac3");
    assert_eq!(d.track_packets(0x1100).count(), 5);
    assert_eq!(d.track_packets(0x1_1100).count(), 5);
    let mut buf = Vec::new();
    let core: Vec<&PacketRef> = d.track_packets(0x1_1100).collect();
    d.read_packet(&SliceSource(&ts), core[4], &mut buf).unwrap();
    assert_eq!(buf, ac3);
    assert_eq!(core[4].pts, 900_000 + 4 * 2880);
    let main: Vec<&PacketRef> = d.track_packets(0x1100).collect();
    d.read_packet(&SliceSource(&ts), main[4], &mut buf).unwrap();
    assert_eq!(buf, thd);
    assert!(main[0].keyframe);
}

#[test]
fn dts_hd_reports_the_channels_of_the_extension_not_the_core() {
    // The core of these streams is 5.1 or less; the extension substream header states the 7.1 asset.
    for name in ["DTSX_Demo_2016.m2ts", "DTS-HD MA 7.1 'Dredd' Sound Check.m2ts"] {
        let path = format!("{}/{name}", concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media"));
        let Ok(file) = FileSource::open(&path) else {
            eprintln!("no lab media: {name}");
            continue;
        };
        let d = Demuxer::open_lazy(&file).unwrap();
        let a = d.tracks().iter().find(|t| t.kind == TrackKind::Audio).unwrap();
        assert_eq!((a.codec.as_str(), a.channels), ("dts", Some(8)), "{name}");
    }
}
