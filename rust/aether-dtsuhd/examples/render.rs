//! Decodes a DTS-UHD file and writes a WAV. Usage: render <file.mp4|pkts> <out.wav> [channel-label|all] [max_frames]
#[path = "support/mp4lite.rs"]
mod mp4lite;
use aether_dtsuhd::{ChannelStatus, Decoder};
use std::collections::BTreeMap;
use std::io::Write;

fn load(path: &str) -> Vec<Vec<u8>> {
    if path.ends_with(".mp4") {
        mp4lite::read_audio_samples(path).1
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

fn write_wav(path: &str, rate: u32, chans: &[Vec<f32>]) {
    let n = chans.iter().map(|c| c.len()).min().unwrap_or(0);
    let nc = chans.len() as u16;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let data_len = (n * nc as usize * 2) as u32;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&nc.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * nc as u32 * 2).to_le_bytes()).unwrap();
    f.write_all(&(nc * 2).to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for i in 0..n {
        for c in chans {
            let v = (c[i].clamp(-1.0, 1.0) * 32767.0).round() as i16;
            f.write_all(&v.to_le_bytes()).unwrap();
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let frames = load(&a[1]);
    let which = a.get(3).cloned().unwrap_or_else(|| "all".into());
    let max: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let mut dec = Decoder::new();
    let mut labels: Vec<&'static str> = vec![];
    let mut out: Vec<Vec<f32>> = vec![];
    let mut stat: BTreeMap<(usize, String), usize> = BTreeMap::new();
    for (i, f) in frames.iter().enumerate().take(max) {
        match dec.decode_frame(f) {
            Ok(d) => {
                if out.is_empty() {
                    labels = d.info.channel_labels.clone();
                    out = vec![vec![]; d.pcm.len()];
                }
                for (c, p) in d.pcm.iter().enumerate() {
                    out[c].extend_from_slice(p);
                    let s = match &d.status[c] {
                        ChannelStatus::Unsupported(_) => "Unsupported".to_string(),
                        s => format!("{:?}", s),
                    };
                    *stat.entry((c, s)).or_default() += 1;
                }
            }
            Err(e) => eprintln!("frame {i}: {e}"),
        }
    }
    for ((c, s), n) in &stat {
        println!(
            "channel {:>2} {:<5} {:<14} {}",
            c,
            labels.get(*c).copied().unwrap_or("?"),
            s,
            n
        );
    }
    for (c, o) in out.iter().enumerate() {
        let peak = o.iter().fold(0f32, |m, v| m.max(v.abs()));
        let rms =
            (o.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / o.len().max(1) as f64).sqrt();
        println!(
            "channel {:>2} {:<5} peak {:.5} rms {:.6}",
            c,
            labels.get(c).copied().unwrap_or("?"),
            peak,
            rms
        );
    }
    let sel: Vec<Vec<f32>> = if which == "all" {
        out.clone()
    } else {
        out.iter()
            .enumerate()
            .filter(|(c, _)| labels.get(*c).copied() == Some(which.as_str()))
            .map(|(_, v)| v.clone())
            .collect()
    };
    write_wav(&a[2], 48000, &sel);
}
