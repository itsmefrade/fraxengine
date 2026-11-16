//! Check (c): can the decoded objects explain the decoded bed? For every
//! window of `W` frames a static least-squares downmix (bed channel = sum of
//! object k times gain k, bed delayed to the object clock) is fitted and the
//! residual energy relative to the bed energy is reported per bed channel.
use aether_joc::analysis::fit_static_downmix;
use aether_joc::{ObjectDecoder, LATENCY_SAMPLES};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let w: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(32);
    let mut dec = aether_ac3::Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut od = ObjectDecoder::new();
    let mut bed: Vec<Vec<f32>> = vec![Vec::new(); 6];
    let mut objs: Vec<Vec<f32>> = vec![Vec::new(); 15];
    for au in aether_ac3::access_units(&stream) {
        let b = dec.decode(au).unwrap();
        let o = od.process(au, &b).unwrap();
        for c in 0..6 {
            bed[c].extend_from_slice(b.channel(c));
        }
        for k in 0..o.num_objects {
            objs[k].extend_from_slice(o.object(k));
        }
    }
    let n = bed[0].len();
    let win = w * 1536;
    let names = ["L", "R", "C", "LFE", "Ls", "Rs"];
    let chans = [0usize, 1, 2, 4, 5];
    let mut resid: Vec<Vec<f64>> = vec![Vec::new(); 6];
    let mut sum_m = vec![vec![0f64; objs.len()]; 6];
    let mut start = 4 * 1536;
    while start + win + LATENCY_SAMPLES <= n {
        for &c in &chans {
            if let Some((x, r)) = fit_static_downmix(&bed[c], &objs, start, win, LATENCY_SAMPLES) {
                resid[c].push(r);
                for k in 0..objs.len() {
                    sum_m[c][k] += x[k];
                }
            }
        }
        start += win;
    }
    for &c in &chans {
        let mut v = resid[c].clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() {
            continue;
        }
        println!(
            "bed {:3}: residual (dB re bed) median {:6.1}  p10 {:6.1}  p90 {:6.1}  worst {:6.1}  (n={})",
            names[c], v[v.len() / 2], v[v.len() / 10], v[v.len() * 9 / 10], v[v.len() - 1], v.len()
        );
    }
    println!("mean fitted gains (rows: bed channel, columns: objects 0..14):");
    for &c in &chans {
        let k = resid[c].len().max(1) as f64;
        println!("  {:3}: {}", names[c], sum_m[c].iter().map(|v| format!("{:5.2}", v / k)).collect::<Vec<_>>().join(" "));
    }
}
