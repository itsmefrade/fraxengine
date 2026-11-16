//! Prints DTS-UHD stream information. Usage: info <file.mp4 | packets.pkts>
#[path = "support/mp4lite.rs"]
mod mp4lite;
use aether_dtsuhd::Decoder;
use std::collections::BTreeMap;

fn load(path: &str) -> Vec<Vec<u8>> {
    if path.ends_with(".mp4") || path.ends_with(".m4v") || path.ends_with(".mov") {
        let (fourcc, s) = mp4lite::read_audio_samples(path);
        eprintln!("sample entry: {fourcc}, {} samples", s.len());
        s
    } else {
        let pk = std::fs::read(path).unwrap();
        let idx = std::fs::read_to_string(format!("{path}.idx")).unwrap();
        let sizes: Vec<usize> = idx
            .trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace())
            .split(',')
            .map(|s| s.trim().parse().unwrap())
            .collect();
        let mut off = 0;
        sizes
            .iter()
            .map(|&s| {
                let v = pk[off..off + s].to_vec();
                off += s;
                v
            })
            .collect()
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let frames = load(&path);
    let mut dec = Decoder::new();
    let mut sync = 0;
    let mut errs: BTreeMap<String, usize> = BTreeMap::new();
    let mut first = None;
    let mut bytes = 0usize;
    for f in &frames {
        bytes += f.len();
        match dec.parse_frame(f) {
            Ok(info) => {
                if info.is_sync {
                    sync += 1;
                }
                if let Some(a) = &info.ace {
                    if !a.padding_consistent {
                        *errs
                            .entry("padding length mismatch".to_string())
                            .or_default() += 1;
                    }
                }
                if first.is_none() {
                    first = Some(info);
                }
            }
            Err(e) => *errs.entry(e.to_string()).or_default() += 1,
        }
    }
    println!(
        "frames: {} ({} sync), {} bytes, parse errors: {:?}",
        frames.len(),
        sync,
        bytes,
        errs
    );
    if let Some(i) = first {
        println!("{:#?}", i);
    }
}
