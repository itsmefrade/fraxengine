//! Developer tool: print decoded vs reference samples of one frame.
//! Usage: dump <stream> <reference.f32le> <au index> <channel> <from> <count>
use aether_ac3::{access_units, Decoder};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let reference: Vec<f32> = std::fs::read(&a[2])
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let (au_i, chan, from, n): (usize, usize, usize, usize) = (
        a[3].parse().unwrap(),
        a[4].parse().unwrap(),
        a[5].parse().unwrap(),
        a[6].parse().unwrap(),
    );
    let mut dec = Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut off = 0;
    for (i, au) in access_units(&stream).into_iter().enumerate() {
        let f = dec.decode(au).unwrap();
        let ch = f.channels();
        if i == au_i {
            for s in from..from + n {
                let r = reference[off + s * ch + chan];
                let m = f.channel(chan)[s];
                println!("{s}: mine {m:+.5} ref {r:+.5} diff {:+.5}", m - r);
            }
        }
        off += f.frames * ch;
    }
}
