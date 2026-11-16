use aether_joc::{emdf, oamd};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let first: usize = a[2].parse().unwrap();
    let n: usize = a[3].parse().unwrap();
    let mut st = oamd::OamdState::default();
    let mut clean = 0; let mut total = 0;
    for (i, au) in aether_ac3::access_units(&stream).into_iter().enumerate() {
        let e = emdf::extract_from_access_unit(au);
        let Some(o) = &e.oamd else { continue };
        if std::env::var("HEX").is_ok() && i == first { println!("{}", o.iter().map(|b| format!("{:08b}", b)).collect::<String>()); }
        let f = match oamd::parse_oamd(o, &mut st) { Ok(f) => f, Err(e) => { println!("AU {i}: err {e:?}"); continue } };
        total += 1; if f.clean { clean += 1; }
        if i < first || i >= first + n { continue; }
        println!("AU {i}: ver {} objs {} clean {} bits {}/{} so {} timing {:?}", f.version, f.object_count, f.clean, f.bits_used, f.bits_total, f.sample_offset, f.timing);
        println!("  program {:?}", f.program);
        for (k, b) in f.objects.iter().enumerate() {
            let s = &b[0];
            println!("  obj {k}: label {:?} active {} gain {:.2} pos {:?} snap {} size {:?} blocks {}", s.bed_label.map(|l| l.name()), s.state.active, s.state.gain, s.state.pos, s.state.snap, s.state.size, b.len());
        }
    }
    println!("OAMD frames {total}, clean {clean}");
}
