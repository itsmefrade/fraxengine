//! Matroska over a remote source: the first packet and a seek to the middle must touch a few blocks,
//! not a cluster. The counting source models an HTTP Range cache of 1 MiB blocks (each distinct block
//! touched is one fetched block). Lab files are skipped when absent.

use aether_demux::{ByteSource, Demuxer, FileSource, TrackKind};
use std::cell::RefCell;
use std::collections::BTreeSet;

const BLOCK: u64 = 1 << 20;
const BOUND: u64 = 4 * BLOCK;

fn lab() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media")
}

struct Blocks<'a> {
    inner: &'a FileSource,
    seen: RefCell<BTreeSet<u64>>,
}
impl Blocks<'_> {
    fn bytes(&self) -> u64 {
        self.seen.borrow().len() as u64 * BLOCK
    }
}
impl ByteSource for Blocks<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let n = self.inner.read_at(offset, buf)?;
        if n > 0 {
            let mut seen = self.seen.borrow_mut();
            for b in offset / BLOCK..=(offset + n as u64 - 1) / BLOCK {
                seen.insert(b);
            }
        }
        Ok(n)
    }
}

#[test]
fn first_packet_and_middle_seek_touch_a_few_blocks() {
    let names = ["POCAWE_Sample.mkv", "awaken-girl.4K.HDR.DV.mkv", "DV FEL Power Rangers Credits Test.mkv", "DTSX Gravity.mkv"];
    let mut bad = Vec::new();
    for name in names {
        let Ok(file) = FileSource::open(format!("{}/{name}", lab())) else {
            eprintln!("skip {name}");
            continue;
        };
        let src = Blocks { inner: &file, seen: RefCell::new(BTreeSet::new()) };
        let mut d = Demuxer::open_lazy(&src).unwrap();
        let video = d.tracks().iter().find(|t| t.kind == TrackKind::Video).or(d.tracks().first()).unwrap().id;
        // The first packet, the way the wasm adapter delivers it: index a step, read the payload.
        while d.packets().is_empty() {
            d.index_more(&src, 1 << 20).unwrap();
        }
        let mut buf = Vec::new();
        let first = d.packets()[0].clone();
        d.read_packet(&src, &first, &mut buf).unwrap();
        let at_first = src.bytes();
        // Positioning at the start is part of every playback start and must not read the Cues at the end of the file.
        // Counted on a source of its own: the bounds below are about the first packet and one seek.
        let start = Blocks { inner: &file, seen: RefCell::new(BTreeSet::new()) };
        let mut at_start = Demuxer::open_lazy(&start).unwrap();
        at_start.seek_to(&start, video, 0).unwrap();
        let last = (file.len() - 1) / BLOCK;
        if last > 16 && start.seen.borrow().contains(&last) {
            bad.push(format!("{name}: the end of the file was read to position at the start"));
        }
        let target_ns = d.duration_ns().unwrap() as i128 / 2;
        let scale = i128::from(d.tracks().iter().find(|t| t.id == video).unwrap().timescale);
        let before = src.bytes();
        let i = d.seek_to(&src, video, (target_ns * scale / 1_000_000_000) as i64).unwrap();
        let p = d.packets()[i].clone();
        d.read_packet(&src, &p, &mut buf).unwrap();
        let seek = src.bytes() - before;
        eprintln!("   blocks {:?} packet {} size {}", src.seen.borrow(), p.offset, p.size);
        eprintln!("{name}: file {} MiB, duration {:?}, landed pts {} for {}, first packet {} MiB, mid seek {} MiB more ({} MiB total)", file.len() >> 20, d.duration_ns(), p.pts, target_ns * scale / 1_000_000_000, at_first >> 20, seek >> 20, src.bytes() >> 20);
        if at_first > BOUND {
            bad.push(format!("{name}: {} MiB before the first packet", at_first >> 20));
        }
        // The first seek also reads the Cues at the end of the file (one block here); open does not.
        if seek > BOUND + (1 << 20) {
            bad.push(format!("{name}: mid seek touched {} MiB", seek >> 20));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
