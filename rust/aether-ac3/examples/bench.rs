//! Developer tool: decode speed in multiples of realtime.
//! Usage: bench <stream> [iterations]

use aether_ac3::{access_units, Decoder};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&args[1]).unwrap();
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let aus = access_units(&stream);
    let mut best = f64::MAX;
    let mut audio_secs = 0.0;
    let mut desc = String::new();
    for _ in 0..iters {
        let mut dec = Decoder::new();
        let mut samples = 0usize;
        let mut rate = 48000;
        let t = Instant::now();
        let mut out = aether_ac3::DecodedFrame::default();
        for au in &aus {
            dec.decode_into(au, &mut out).unwrap();
            samples += out.frames;
            rate = out.sample_rate;
            if desc.is_empty() && out.channels() > 0 {
                desc = format!("{} ch ({})", out.channels(), out.layout.describe());
            }
        }
        let dt = t.elapsed().as_secs_f64();
        audio_secs = samples as f64 / rate as f64;
        best = best.min(dt);
    }
    println!(
        "{}: {desc}, {:.1} s of audio decoded in {:.1} ms (best of {iters}) = {:.0}x realtime",
        args[1],
        audio_secs,
        best * 1000.0,
        audio_secs / best
    );
}
