//! Semantic check on Dolby's channel-ID JOC sample: finds the segments in
//! which exactly one object dominates and reports the OAMD position of that
//! object, the 7.1.4 speaker energies after point panning, and the dominant
//! channel of the decoded 5.1 bed.
use aether_joc::render::{pan_gains, LAYOUT_714};
use aether_joc::ObjectDecoder;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let win: usize = 4; // frames per window = 128 ms
    let mut dec = aether_ac3::Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut od = ObjectDecoder::new();
    let mut pos = vec![[0.5f32, 0.5, 0.0]; 16];
    let mut wins: Vec<(Vec<f64>, Vec<f64>, Vec<[f32; 3]>)> = Vec::new();
    let mut obj_e = vec![0f64; 16];
    let mut bed_e = vec![0f64; 6];
    let mut n = 0;
    for au in aether_ac3::access_units(&stream) {
        let bed = dec.decode(au).unwrap();
        let o = od.process(au, &bed).unwrap();
        for m in &o.metadata { pos[m.object] = m.state.pos; }
        for k in 0..o.num_objects { obj_e[k] += o.object(k).iter().map(|&v| (v as f64).powi(2)).sum::<f64>(); }
        for c in 0..6.min(bed.channels()) { bed_e[c] += bed.channel(c).iter().map(|&v| (v as f64).powi(2)).sum::<f64>(); }
        n += 1;
        if n == win {
            wins.push((obj_e.clone(), bed_e.clone(), pos.clone()));
            obj_e.iter_mut().for_each(|v| *v = 0.0);
            bed_e.iter_mut().for_each(|v| *v = 0.0);
            n = 0;
        }
    }
    let names = ["L", "R", "C", "LFE", "Ls", "Rs"];
    let sp = ["L", "R", "C", "Ls", "Rs", "Lb", "Rb", "Tfl", "Tfr", "Tbl", "Tbr"];
    // Dominant object per window.
    let mut seg: Option<(usize, usize, usize)> = None; // (obj, start, end)
    let mut segs = Vec::new();
    for (w, (oe, _, _)) in wins.iter().enumerate() {
        let mut idx: Vec<usize> = (0..15).collect();
        idx.sort_by(|&x, &y| oe[y].partial_cmp(&oe[x]).unwrap());
        let (top, second) = (oe[idx[0]], oe[idx[1]]);
        let dom = if top / win as f64 / 1536.0 > 1e-8 && top > second * 10.0 { Some(idx[0]) } else { None };
        match (dom, &mut seg) {
            (Some(d), Some(s)) if s.0 == d => s.2 = w,
            (Some(d), _) => { if let Some(s) = seg.take() { segs.push(s); } seg = Some((d, w, w)); }
            (None, _) => { if let Some(s) = seg.take() { segs.push(s); } }
        }
    }
    if let Some(s) = seg.take() { segs.push(s); }
    println!("segments with a single dominant object (>=10 dB, >=3 windows of 128 ms):");
    let (mut total, mut good) = (0, 0);
    for (obj, s, e) in segs.into_iter().filter(|s| s.2 - s.1 + 1 >= 3) {
        let mut bed = vec![0f64; 6];
        let mut spk = vec![0f64; 11];
        let mut p = [0f32; 3];
        for w in s..=e {
            for c in 0..6 { bed[c] += wins[w].1[c]; }
            p = wins[w].2[obj];
            let g = pan_gains(p, &LAYOUT_714);
            for i in 0..11 { spk[i] += wins[w].0[obj] * (g[i] * g[i]) as f64; }
        }
        let tot: f64 = spk.iter().sum();
        let (si, sv) = spk.iter().enumerate().fold((0, 0.0), |m, (i, &v)| if v > m.1 { (i, v) } else { m });
        let bi = (0..6).filter(|&c| c != 3).fold(0, |m, c| if bed[c] > bed[m] { c } else { m });
        total += 1;
        if sv / tot > 0.5 { good += 1; }
        println!("  t {:6.2}-{:6.2}s obj {:2} pos ({:.2},{:.2},{:.2}) -> speaker {:3} {:3.0}% | bed dominant {}", s as f64 * 0.128, (e + 1) as f64 * 0.128, obj, p[0], p[1], p[2], sp[si], 100.0 * sv / tot, names[bi]);
    }
    println!("per object: window where the object is most dominant (energy vs sum of all other objects)");
    for obj in 0..15 {
        let mut best = (f64::MIN, 0usize);
        for (w, (oe, _, _)) in wins.iter().enumerate() {
            let others: f64 = (0..15).filter(|&k| k != obj).map(|k| oe[k]).sum();
            let r = 10.0 * ((oe[obj] + 1e-20) / (others + 1e-20)).log10();
            if oe[obj] / (win as f64 * 1536.0) > 1e-8 && r > best.0 { best = (r, w); }
        }
        let w = best.1;
        let p = wins[w].2[obj];
        let mut bi = 0; for c in 0..6 { if c != 3 && wins[w].1[c] > wins[w].1[bi] { bi = c; } }
        println!("  obj {obj:2} t={:6.2}s dominance {:5.1} dB pos ({:.2},{:.2},{:.2}) bed dominant {}", w as f64 * 0.128, best.0, p[0], p[1], p[2], names[bi]);
    }
    println!("{good}/{total} segments render >50% energy into one speaker");
}
