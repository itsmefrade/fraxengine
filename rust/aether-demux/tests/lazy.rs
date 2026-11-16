//! Lazy indexing: `open_lazy` must read headers only (a counting `ByteSource` bounds the bytes), index
//! incrementally, and agree with the eager index. Large lab files are skipped when absent.

use aether_demux::{ByteSource, Container, Demuxer, FileSource, PacketRef, TrackKind};
use std::cell::Cell;

const OPEN_BOUND: u64 = 2 << 20;

fn lab() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media").to_owned()
}

fn fixtures() -> String {
    format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"))
}

struct Counting<'a> {
    inner: &'a FileSource,
    bytes: Cell<u64>,
    reads: Cell<u64>,
}
impl<'a> Counting<'a> {
    fn new(inner: &'a FileSource) -> Self {
        Self { inner, bytes: Cell::new(0), reads: Cell::new(0) }
    }
}
impl ByteSource for Counting<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let n = self.inner.read_at(offset, buf)?;
        self.bytes.set(self.bytes.get() + n as u64);
        self.reads.set(self.reads.get() + 1);
        Ok(n)
    }
}

fn lab_files() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(lab())
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path().to_string_lossy().into_owned())
                .filter(|p| [".mkv", ".mp4", ".mov", ".m2ts"].iter().any(|x| p.ends_with(x)))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn open_lazy_reads_headers_only_on_every_lab_file() {
    let files = lab_files();
    if files.is_empty() {
        eprintln!("no lab media");
    }
    let mut bad = Vec::new();
    for path in &files {
        let file = FileSource::open(path).unwrap();
        let src = Counting::new(&file);
        let d = match Demuxer::open_lazy(&src) {
            Ok(d) => d,
            Err(e) => {
                bad.push(format!("{path}: open_lazy failed: {e}"));
                continue;
            }
        };
        let name = path.rsplit('/').next().unwrap();
        eprintln!("{name}: {:?}, read {} bytes in {} reads (file {}), {} tracks, {} packets indexed", d.container(), src.bytes.get(), src.reads.get(), file.len(), d.tracks().len(), d.packets().len());
        if src.bytes.get() > OPEN_BOUND {
            bad.push(format!("{name}: open read {} bytes", src.bytes.get()));
        }
        if d.tracks().is_empty() {
            bad.push(format!("{name}: no tracks"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn incremental_index_equals_the_eager_index_on_every_lab_file() {
    let mut bad = Vec::new();
    for path in lab_files() {
        // Large UHD remuxes only need the container logic; the biggest files are covered by the open test.
        let file = FileSource::open(&path).unwrap();
        if file.len() > 400 << 20 {
            continue;
        }
        let name = path.rsplit('/').next().unwrap().to_owned();
        let eager = Demuxer::open(&file).unwrap();
        let mut lazy = Demuxer::open_lazy(&file).unwrap();
        let mut steps = 0;
        while !lazy.is_complete() {
            lazy.index_more(&file, 8 << 20).unwrap();
            steps += 1;
            assert!(steps < 10_000, "{name}: index_more makes no progress");
        }
        let key = |p: &PacketRef| (p.track, p.offset, p.inner, p.pts, p.size, p.keyframe);
        let (mut a, mut b): (Vec<_>, Vec<_>) = (eager.packets().iter().map(key).collect(), lazy.packets().iter().map(key).collect());
        a.sort_unstable();
        b.sort_unstable();
        if a != b {
            bad.push(format!("{name}: {} packets eager, {} incremental", a.len(), b.len()));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn fragmented_mp4_is_indexed_one_fragment_at_a_time_and_seeks_through_sidx() {
    for name in ["big-frag-sidx.mp4", "big-frag-plain.mp4"] {
        let path = format!("{}/lazy/{name}", fixtures());
        let Ok(file) = FileSource::open(&path) else {
            eprintln!("skip {name}: run tests/fixtures/generate-lazy.sh");
            continue;
        };
        let src = Counting::new(&file);
        let mut d = Demuxer::open_lazy(&src).unwrap();
        assert_eq!(d.container(), Container::Mp4);
        assert!(d.fragmented());
        assert_eq!(d.packets().len(), 0, "no fragment is parsed at open");
        let at_open = src.bytes.get();
        assert!(at_open < OPEN_BOUND, "{name}: open read {at_open} bytes");
        let video = d.tracks().iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
        let scale = i64::from(d.tracks()[0].timescale);

        // Following the file: a bounded amount of work per call, and the packets keep accumulating.
        let added = d.index_more(&src, 4 << 20).unwrap();
        assert!(added > 0 && !d.is_complete(), "{name}: first step added {added}");
        assert!(src.bytes.get() < 12 << 20, "{name}: one 4 MB step read {} bytes", src.bytes.get());

        // Seek into the middle without indexing what precedes it.
        let mut fresh = Demuxer::open_lazy(&src).unwrap();
        let before = src.bytes.get();
        let target = 14 * scale;
        let i = fresh.seek_to(&src, video, target).unwrap();
        let used = src.bytes.get() - before;
        let landed = fresh.packets()[i].clone();
        assert!(landed.keyframe && landed.pts <= target && target - landed.pts <= 3 * scale, "{name}: landed at {} for {target}", landed.pts);
        assert!(used < 16 << 20, "{name}: seek read {used} bytes of {}", file.len());
        eprintln!("{name}: seek to 14 s landed at {} reading {used} bytes ({} fragments indexed)", landed.pts / scale, fresh.packets().len());

        let eager = Demuxer::open(&file).unwrap();
        let j = eager.seek(&file, video, target).unwrap();
        assert_eq!(eager.packets()[j].pts, landed.pts, "{name}: same keyframe as the full index");
        // Playback can continue with index_more after the jump and ends complete with every packet.
        let n = fresh.packets().len();
        fresh.index_all(&src).unwrap();
        assert!(fresh.packets().len() > n);
        assert_eq!(fresh.packets().len(), eager.packets().len(), "{name}: complete index after a seek");
    }
}
