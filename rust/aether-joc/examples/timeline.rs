//! Prints per-window energies of bed channels and decoded objects.
use aether_joc::ObjectDecoder;
fn db(e: f64) -> f64 { 10.0 * (e + 1e-12).log10() }
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let win_frames: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(8); // 8 frames = 0.256 s
    let mut dec = aether_ac3::Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut od = ObjectDecoder::new();
    let mut bed_e = vec![0f64; 6];
    let mut obj_e = vec![0f64; 16];
    let mut n = 0usize;
    let mut positions: Vec<String> = vec![String::new(); 16];
    println!("win   t(s) | bed L R C LFE Ls Rs (dB) | objects 0..14 (dB)");
    for (i, au) in aether_ac3::access_units(&stream).into_iter().enumerate() {
        let bed = dec.decode(au).unwrap();
        let o = od.process(au, &bed).unwrap();
        for c in 0..bed.channels().min(6) { bed_e[c] += bed.channel(c).iter().map(|&v| (v as f64).powi(2)).sum::<f64>(); }
        for k in 0..o.num_objects { obj_e[k] += o.object(k).iter().map(|&v| (v as f64).powi(2)).sum::<f64>(); }
        for m in &o.metadata { positions[m.object] = format!("({:.2},{:.2},{:.2})", m.state.pos[0], m.state.pos[1], m.state.pos[2]); }
        n += 1;
        if n == win_frames {
            let t = (i + 1 - win_frames) as f64 * 1536.0 / 48000.0;
            let denom = (win_frames * 1536) as f64;
            let b: Vec<String> = bed_e.iter().map(|&e| format!("{:5.0}", db(e / denom))).collect();
            let ob: Vec<String> = obj_e.iter().take(15).map(|&e| format!("{:4.0}", db(e / denom))).collect();
            println!("{:5} {:6.2} | {} | {}", (i + 1) / win_frames, t, b.join(" "), ob.join(" "));
            bed_e.iter_mut().for_each(|v| *v = 0.0); obj_e.iter_mut().for_each(|v| *v = 0.0); n = 0;
        }
    }
    println!("last positions: {positions:?}");
}
