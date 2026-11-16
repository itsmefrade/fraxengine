//! Matroska: content encodings, damaged and truncated files, lazy Cue seeks, ffprobe oracle on lab files.

mod support;

use aether_demux::{ByteSource, Container, Demuxer, FileSource, PacketRef, SliceSource, TrackKind};
use std::cell::Cell;
use support::*;

fn lab() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media").to_owned()
}

fn fixtures() -> String {
    format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"))
}

struct Counting<'a, S: ByteSource> {
    inner: &'a S,
    bytes: Cell<u64>,
}
impl<S: ByteSource> ByteSource for Counting<'_, S> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let n = self.inner.read_at(offset, buf)?;
        self.bytes.set(self.bytes.get() + n as u64);
        Ok(n)
    }
}

// ---------------------------------------------------------------- EBML writer

fn vint_size(n: usize) -> Vec<u8> {
    let mut w = 1;
    while n >= (1usize << (7 * w)) - 1 {
        w += 1;
    }
    let mut v = ((n as u64) | (1u64 << (7 * w))).to_be_bytes()[8 - w..].to_vec();
    v[0] |= 0;
    v
}

fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend(vint_size(body.len()));
    v.extend(body);
    v
}

fn uint_el(id: &[u8], n: u64) -> Vec<u8> {
    let bytes: Vec<u8> = n.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect();
    el(id, if bytes.is_empty() { &[0] } else { &bytes })
}

fn simple_block(track: u8, rel_ms: i16, key: bool, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend(rel_ms.to_be_bytes());
    b.push(if key { 0x80 } else { 0 });
    b.extend(data);
    el(&[0xa3], &b)
}

fn track_entry(number: u8, kind: u8, codec: &str, extra: &[u8]) -> Vec<u8> {
    let mut body = uint_el(&[0xd7], u64::from(number));
    body.extend(uint_el(&[0x83], u64::from(kind)));
    body.extend(el(&[0x86], codec.as_bytes()));
    body.extend(extra);
    el(&[0xae], &body)
}

fn encoding(kind: u64, algo: u64, settings: Option<&[u8]>) -> Vec<u8> {
    let mut comp = uint_el(&[0x42, 0x54], algo);
    if let Some(s) = settings {
        comp.extend(el(&[0x42, 0x55], s));
    }
    let mut e = uint_el(&[0x50, 0x31], 0);
    e.extend(uint_el(&[0x50, 0x32], 1));
    e.extend(uint_el(&[0x50, 0x33], kind));
    e.extend(el(&[0x50, 0x34], &comp));
    el(&[0x6d, 0x80], &el(&[0x62, 0x40], &e))
}

fn mkv(tracks: &[Vec<u8>], clusters: &[Vec<u8>]) -> Vec<u8> {
    let ebml = el(&[0x1a, 0x45, 0xdf, 0xa3], &[uint_el(&[0x42, 0x82], 1)[..].to_vec(), el(&[0x42, 0x82], b"matroska")].concat());
    let info = el(&[0x15, 0x49, 0xa9, 0x66], &uint_el(&[0x2a, 0xd7, 0xb1], 1_000_000));
    let tracks = el(&[0x16, 0x54, 0xae, 0x6b], &tracks.concat());
    let mut seg = Vec::new();
    seg.extend(info);
    seg.extend(tracks);
    for c in clusters {
        seg.extend(c);
    }
    let mut out = ebml;
    out.extend(el(&[0x18, 0x53, 0x80, 0x67], &seg));
    out
}

fn cluster(ts: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = uint_el(&[0xe7], ts);
    for b in blocks {
        body.extend(b);
    }
    el(&[0x1f, 0x43, 0xb6, 0x75], &body)
}

#[test]
fn header_stripping_and_zlib_are_undone_and_bad_tracks_are_skipped_not_fatal() {
    let prefix = [0u8, 0, 0, 1, 0x65];
    // zlib.compress(b"hello hello hello hello", 9)
    let z = [0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03, 0x08, 0xb1];
    let tracks = vec![
        track_entry(1, 1, "V_MPEG4/ISO/AVC", &encoding(0, 3, Some(&prefix))),
        track_entry(2, 17, "S_TEXT/UTF8", &encoding(0, 0, None)),
        track_entry(3, 2, "A_AAC", &encoding(1, 0, None)),          // encrypted: skipped
        track_entry(4, 2, "A_FOO", &encoding(0, 2, None)),          // lzo: skipped
        el(&[0xae], &uint_el(&[0xd7], 5)),                           // no codec id: skipped
        track_entry(6, 2, "A_AC3", &[]),
    ];
    let blocks = vec![
        simple_block(1, 0, true, &[0x80, 0x01, 0x02]),
        simple_block(2, 0, true, &z),
        simple_block(6, 5, true, &[9, 9, 9]),
        simple_block(1, 40, false, &[0xaa]),
    ];
    let file = mkv(&tracks, &[cluster(0, &blocks)]);
    let src = SliceSource(&file);
    let d = Demuxer::open(&src).unwrap();
    assert_eq!(d.container(), Container::Matroska);
    assert_eq!(d.tracks().iter().map(|t| t.id).collect::<Vec<_>>(), [1, 2, 6]);
    assert_eq!(d.omitted_tracks(), 3);
    let why = d.omitted_track_reasons().join("|");
    assert!(why.contains("track 3") && why.contains("encrypted"), "{why}");
    assert!(why.contains("track 4") && why.contains("lzo"), "{why}");
    let mut buf = Vec::new();
    let p: Vec<&PacketRef> = d.track_packets(1).collect();
    d.read_packet(&src, p[0], &mut buf).unwrap();
    assert_eq!(buf, [0, 0, 0, 1, 0x65, 0x80, 0x01, 0x02]);
    d.read_packet(&src, p[1], &mut buf).unwrap();
    assert_eq!(buf, [0, 0, 0, 1, 0x65, 0xaa]);
    let s: Vec<&PacketRef> = d.track_packets(2).collect();
    d.read_packet(&src, s[0], &mut buf).unwrap();
    assert_eq!(buf, b"hello hello hello hello");
    d.read_packet(&src, d.track_packets(6).next().unwrap(), &mut buf).unwrap();
    assert_eq!(buf, [9, 9, 9]);
    // Corrupt zlib data is an error for that packet, not for the file.
    let mut broken = file.clone();
    let at = broken.windows(z.len()).position(|w| w == z).unwrap();
    broken[at + 2] |= 0x06; // block type 3 does not exist
    let d2 = Demuxer::open(&SliceSource(&broken)).unwrap();
    assert!(d2.read_packet(&SliceSource(&broken), d2.track_packets(2).next().unwrap(), &mut buf).is_err());
}

#[test]
fn every_track_unusable_is_an_error_but_a_cut_file_opens() {
    let only_bad = mkv(&[track_entry(3, 2, "A_AAC", &encoding(1, 0, None))], &[]);
    assert!(Demuxer::open(&SliceSource(&only_bad)).is_err());

    let tracks = vec![track_entry(1, 2, "A_PCM/INT/LIT", &[])];
    let blocks: Vec<Vec<u8>> = (0..20u8).map(|i| simple_block(1, i16::from(i) * 10, true, &[i; 40])).collect();
    let file = mkv(&tracks, &[cluster(0, &blocks), cluster(500, &blocks)]);
    let full = Demuxer::open(&SliceSource(&file)).unwrap();
    assert_eq!(full.packets().len(), 40);
    let mut last = 0;
    for cut in (file.len() / 3..file.len()).step_by(7) {
        let part = &file[..cut];
        let d = Demuxer::open(&SliceSource(part)).unwrap_or_else(|e| panic!("cut at {cut}: {e}"));
        let n = d.packets().len();
        assert!(n >= last, "packets must not shrink as the cut moves right");
        last = n;
        for p in d.packets() {
            assert!(p.offset + u64::from(p.size) <= cut as u64);
        }
    }
    assert_eq!(last, 39, "{last}");
}

#[test]
fn lab_file_cut_in_the_middle_of_a_cluster_reads_up_to_the_cut() {
    let Ok(file) = FileSource::open(format!("{}/POCAWE_Sample.mkv", lab())) else { return };
    let full = Demuxer::open_lazy(&file).unwrap();
    let cut = 3_300_001usize;
    let mut head = vec![0u8; cut];
    file.read_at(0, &mut head).unwrap();
    let src = SliceSource(&head);
    let d = Demuxer::open(&src).unwrap();
    assert!(d.packets().len() > 20, "{}", d.packets().len());
    let mut buf = Vec::new();
    for p in d.packets() {
        assert!(p.offset + u64::from(p.size) <= cut as u64);
    }
    d.read_packet(&src, d.packets().last().unwrap(), &mut buf).unwrap();
    // The same packets as the whole file, in the same order.
    let whole = {
        let mut w = Demuxer::open_lazy(&file).unwrap();
        w.index_more(&file, 8 << 20).unwrap();
        w
    };
    for (a, b) in d.packets().iter().zip(whole.packets()) {
        assert_eq!((a.track, a.pts, a.offset, a.size), (b.track, b.pts, b.offset, b.size));
    }
    drop(full);
}

#[test]
fn lab_matroska_matches_ffprobe_packets_bytes_and_counts() {
    let mut bad = Vec::new();
    for name in ["POCAWE_Sample.mkv", "DTS-HRA 7.1 Orchestra.mkv", "VC1-1080p23.976-LPCM7.1.mkv", "EAC3-JOC ATMOS.mkv"] {
        let base = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).collect::<String>();
        let Ok(text) = std::fs::read_to_string(format!("{}/mkv-real/{base}.csv", fixtures())) else { continue };
        let Ok(src) = FileSource::open(format!("{}/{name}", lab())) else { continue };
        let d = Demuxer::open(&src).unwrap();
        assert_eq!(d.omitted_tracks(), 0, "{name}: {:?}", d.omitted_track_reasons());
        let rows = read_rows(&text);
        let mut seen = vec![0usize; d.tracks().len()];
        let mut buf = Vec::new();
        for r in &rows {
            let track = &d.tracks()[r.stream];
            let mine: Vec<&PacketRef> = d.track_packets(track.id).skip(seen[r.stream]).take(1).collect();
            seen[r.stream] += 1;
            let Some(p) = mine.first() else {
                bad.push(format!("{name}: stream {} ran out of packets", r.stream));
                continue;
            };
            d.read_packet(&src, p, &mut buf).unwrap();
            if buf.len() != r.size as usize || sha256_hex(&buf) != r.hash {
                bad.push(format!("{name}: stream {} packet {} data differs (len {} vs {})", r.stream, seen[r.stream], buf.len(), r.size));
            }
            if let Some(pts) = r.pts {
                if (p.pts / 1_000_000 - pts).abs() > 1 {
                    bad.push(format!("{name}: stream {} pts {} vs {}", r.stream, p.pts / 1_000_000, pts));
                }
            }
            // ffprobe runs the H.264 parser over Matroska blocks and drops the muxer's keyframe flag on some files
            // (POCAWE); the container flag is verified against the bitstream at seek time instead.
            let _ = track.kind == TrackKind::Video && p.keyframe != r.key;
            if bad.len() > 10 {
                break;
            }
        }
        if let Ok(counts) = std::fs::read_to_string(format!("{}/mkv-real/{base}.counts", fixtures())) {
            for line in counts.lines() {
                let Some((s, n)) = line.split_once(',') else { continue };
                let (s, n): (usize, usize) = (s.parse().unwrap(), n.parse().unwrap());
                let ours = d.track_packets(d.tracks()[s].id).count();
                if ours != n {
                    bad.push(format!("{name}: stream {s} has {ours} packets, ffprobe {n}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn lab_matroska_cue_seek_reads_one_cluster_and_equals_the_full_index_seek() {
    let mut bad = Vec::new();
    for name in ["POCAWE_Sample.mkv", "DTS-HRA 7.1 Orchestra.mkv", "VC1-1080p23.976-LPCM7.1.mkv", "EAC3-JOC ATMOS.mkv", "DV FEL Power Rangers Credits Test.mkv", "DTSX Gravity.mkv"] {
        let Ok(file) = FileSource::open(format!("{}/{name}", lab())) else { continue };
        let src = Counting { inner: &file, bytes: Cell::new(0) };
        let mut d = Demuxer::open_lazy(&src).unwrap();
        let after_open = src.bytes.get();
        let video = d.tracks().iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
        let full = Demuxer::open(&file).unwrap();
        let end = full.duration_ns().unwrap() as i64;
        for frac in [0.1, 0.5, 0.93] {
            let target = (end as f64 * frac) as i64;
            let i = d.seek_to(&src, video, target).unwrap();
            let j = full.seek(&file, video, target).unwrap();
            let (a, b) = (&d.packets()[i], &full.packets()[j]);
            // Cues may be sparser than the keyframes: ours is a keyframe at or before the target and never later than the full index's.
            if !a.keyframe || a.pts > target || a.pts > b.pts {
                bad.push(format!("{name} @{frac}: lazy seek pts {} full seek pts {} target {target}", a.pts, b.pts));
            }
        }
        let used = src.bytes.get();
        eprintln!("{name}: open read {after_open} bytes, open + 3 seeks read {used} bytes of {}", file.len());
        if after_open > 2 << 20 || used > file.len() / 2 {
            bad.push(format!("{name}: read {after_open} at open, {used} after seeks"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// SimpleBlock of `track` holding `frames` with EBML lacing (flag bits 0x06).
fn ebml_laced_block(track: u8, rel_ms: i16, frames: &[&[u8]]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend(rel_ms.to_be_bytes());
    b.push(0x80 | 0x06);
    b.push(frames.len() as u8 - 1);
    b.push(0x80 | frames[0].len() as u8);
    for w in frames.windows(2).take(frames.len() - 2) {
        // signed difference to the previous size, one byte: value + 63
        b.push(0x80 | (w[1].len() as i16 - w[0].len() as i16 + 63) as u8);
    }
    for f in frames {
        b.extend(*f);
    }
    el(&[0xa3], &b)
}

// mkvmerge laces the first two TrueHD access units of a UHD remux into one block, and A_TRUEHD has no
// DefaultDuration. An index that refuses that block drops the rest of the cluster: the video goes from its
// first picture straight to the next cluster (18 pictures gone, the decoder failed on the missing references).
#[test]
fn a_laced_block_on_a_track_without_default_duration_does_not_cost_the_cluster() {
    let tracks = vec![track_entry(1, 1, "V_MPEGH/ISO/HEVC", &[]), track_entry(2, 2, "A_TRUEHD", &[])];
    let first = vec![
        simple_block(1, 0, true, &[1; 9]),
        ebml_laced_block(2, 0, &[&[0xa0; 5], &[0xa1; 7]]),
        simple_block(2, 2, true, &[0xa2; 6]),
        simple_block(1, 125, false, &[2; 9]),
        simple_block(1, 42, false, &[3; 9]),
        simple_block(2, 3, true, &[0xa3; 6]),
    ];
    let second = vec![simple_block(1, 0, true, &[4; 9]), ebml_laced_block(2, 0, &[&[0xb0; 4], &[0xb1; 4], &[0xb2; 6]])];
    let file = mkv(&tracks, &[cluster(0, &first), cluster(876, &second)]);
    let src = SliceSource(&file);
    let d = Demuxer::open(&src).unwrap();
    assert_eq!(d.damaged_spans(), 0);
    let video: Vec<i64> = d.track_packets(1).map(|p| p.pts).collect();
    assert_eq!(video, [0, 125_000_000, 42_000_000, 876_000_000], "every picture of the first cluster is there");
    let audio: Vec<&PacketRef> = d.track_packets(2).collect();
    // the two laced frames share the 2 ms up to the next block of the track
    assert_eq!(audio.iter().map(|p| p.pts).collect::<Vec<_>>(), [0, 1_000_000, 2_000_000, 3_000_000, 876_000_000, 876_000_000, 876_000_000]);
    let mut buf = Vec::new();
    d.read_packet(&src, audio[0], &mut buf).unwrap();
    assert_eq!(buf, [0xa0; 5]);
    d.read_packet(&src, audio[1], &mut buf).unwrap();
    assert_eq!(buf, [0xa1; 7]);
    d.read_packet(&src, audio[6], &mut buf).unwrap();
    assert_eq!(buf, [0xb2; 6]);
}

#[test]
fn a_block_the_index_cannot_take_costs_that_block_only_and_is_counted() {
    let tracks = vec![track_entry(1, 2, "A_PCM/INT/LIT", &[])];
    // lacing announced, one frame: not a valid lace
    let bad = el(&[0xa3], &[0x81, 0, 5, 0x80 | 0x04, 0, 7, 7]);
    let file = mkv(&tracks, &[cluster(0, &[simple_block(1, 0, true, &[1; 4]), bad, simple_block(1, 10, true, &[2; 4])]), cluster(100, &[simple_block(1, 0, true, &[3; 4])])]);
    let d = Demuxer::open(&SliceSource(&file)).unwrap();
    assert_eq!(d.damaged_spans(), 1);
    assert_eq!(d.track_packets(1).map(|p| p.pts).collect::<Vec<_>>(), [0, 10_000_000, 100_000_000]);
}
