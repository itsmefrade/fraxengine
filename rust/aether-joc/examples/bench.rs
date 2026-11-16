//! Native speed: x realtime of the E-AC-3 decode alone, the JOC object decode
//! alone (bed pre-decoded) and both together.
use aether_joc::ObjectDecoder;
use std::time::Instant;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let iters: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let aus = aether_ac3::access_units(&stream);
    let mut dec = aether_ac3::Decoder::new();
    let beds: Vec<_> = aus.iter().map(|au| dec.decode(au).unwrap()).collect();
    let seconds: f64 = beds.iter().map(|b| b.frames as f64).sum::<f64>() / 48000.0;
    let best = |f: &mut dyn FnMut() -> usize| -> (f64, usize) {
        let mut b = f64::MAX; let mut r = 0;
        for _ in 0..iters { let t = Instant::now(); r = f(); b = b.min(t.elapsed().as_secs_f64()); }
        (b, r)
    };
    let (t_ac3, _) = best(&mut || { let mut d = aether_ac3::Decoder::new(); aus.iter().map(|au| d.decode(au).unwrap().frames).sum() });
    let (t_joc, n) = best(&mut || { let mut o = ObjectDecoder::new(); aus.iter().zip(&beds).map(|(au, b)| o.process(au, b).unwrap().num_objects).sum() });
    let (t_both, _) = best(&mut || { let mut d = aether_ac3::Decoder::new(); let mut o = ObjectDecoder::new(); aus.iter().map(|au| { let b = d.decode(au).unwrap(); o.process(au, &b).unwrap().num_objects }).sum() });
    let mut o = ObjectDecoder::new();
    let mut sum = 0f64;
    for (au, b) in aus.iter().zip(&beds) {
        let f = o.process(au, b).unwrap();
        let r = f.num_objects;
        sum += f.object(0)[3] as f64 + f.object(1)[5] as f64 + f.object(r - 1)[7] as f64;
    }
    println!("checksum {sum:.6} (the wasm bench prints the same figure)");
    println!("audio {seconds:.1} s, {} AUs, objects/frame {}", aus.len(), n / aus.len());
    println!("E-AC-3 decode only : {:7.1}x realtime ({:.1} ms)", seconds / t_ac3, t_ac3 * 1e3);
    println!("JOC objects only   : {:7.1}x realtime ({:.1} ms)", seconds / t_joc, t_joc * 1e3);
    println!("both               : {:7.1}x realtime ({:.1} ms)", seconds / t_both, t_both * 1e3);
}
