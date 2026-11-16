//! `cargo run -p aether-demux --example dump -- FILE`: tracks and packet summary.
//! `LAZY=1` opens lazily and prints the bytes read; `PK=n` prints the first n packets per track.
use aether_demux::{ByteSource, Demuxer, FileSource};
use std::cell::Cell;

struct Counting<'a> {
    inner: &'a FileSource,
    bytes: Cell<u64>,
    reads: Cell<u64>,
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

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump FILE");
    let file = FileSource::open(&path).expect("open");
    let src = Counting { inner: &file, bytes: Cell::new(0), reads: Cell::new(0) };
    let t = std::time::Instant::now();
    let lazy = std::env::var("LAZY").is_ok();
    let mut d = if lazy { Demuxer::open_lazy(&src) } else { Demuxer::open(&src) }.expect("demux");
    println!("opened in {:?}: read {} bytes in {} reads (file {})", t.elapsed(), src.bytes.get(), src.reads.get(), file.len());
    if let Ok(t) = std::env::var("SEEK") {
        let track: u64 = std::env::var("TRACK").ok().and_then(|v| v.parse().ok()).unwrap_or(d.tracks()[0].id);
        let ts = d.tracks().iter().find(|t| t.id == track).unwrap().timescale as f64;
        let before = src.bytes.get();
        let i = d.seek_to(&src, track, (t.parse::<f64>().unwrap() * ts) as i64).expect("seek");
        println!("seek_to -> index {i} {:?} (read {} more bytes)", d.packets()[i], src.bytes.get() - before);
    }
    if std::env::var("ALL").is_ok() {
        d.index_all(&src).unwrap();
    }
    println!(
        "{:?} duration_ns={:?} fragmented={} omitted={} fragidx={} complete={} breaks={}",
        d.container(),
        d.duration_ns(),
        d.fragmented(),
        d.omitted_tracks(),
        d.fragment_index().len(),
        d.is_complete(),
        d.timeline_breaks().len()
    );
    for t in d.tracks() {
        let n = d.track_packets(t.id).count();
        let k = d.track_packets(t.id).filter(|p| p.keyframe).count();
        println!(
            "track {} {:?} {} string={:?} fb={:?} ts={} ch={:?} rate={:?} depth={:?} {:?}x{:?} cp={}B joc={:?} descs={} el={} packets={n} key={k}",
            t.id, t.kind, t.codec, t.codec_string, t.fallback_codec_string, t.timescale, t.channels, t.sample_rate, t.bit_depth, t.width, t.height, t.codec_private.len(), t.joc_complexity, t.sample_descriptions, t.enhancement_layer
        );
        if let Ok(n) = std::env::var("PK") {
            for p in d.track_packets(t.id).take(n.parse().unwrap_or(1)) {
                println!("   {} pts={} dts={} dur={} off={} inner={} size={} k={} pres={}", p.track, p.pts, p.dts, p.duration, p.offset, p.inner, p.size, p.keyframe, p.presentation);
            }
        } else if let Some(p) = d.track_packets(t.id).next() {
            println!("   first {p:?}");
        }
    }
}
