use aether_joc::{emdf, joc};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let mode = if a.get(3).map(|s| s.as_str()) == Some("printed") { joc::SparseOffsetMode::AsPrinted } else { joc::SparseOffsetMode::ZeroPoint };
    let n: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    for (i, au) in aether_ac3::access_units(&stream).into_iter().enumerate().take(n) {
        let e = emdf::extract_from_access_unit(au);
        println!("AU {i} len {} payloads {:?}", au.len(), e.all);
        if let Some(j) = &e.joc {
            match joc::parse_joc(j, mode) {
                Ok(f) => {
                    println!("  joc dmx {} ch {} obj {} clip {} seq {} bits {}/{}", f.dmx_config_idx, f.num_channels, f.num_objects, f.clip_gain, f.seq_count, f.bits_used, f.bits_total);
                    for (k, o) in f.objects.iter().enumerate() {
                        println!("   obj {k}: present {} bands {} sparse {} q {} slope {} dp {} off {:?}", o.present, o.num_bands, o.sparse, o.quant_idx, o.slope_idx, o.num_dpoints, o.offset_ts);
                    }
                }
                Err(e) => println!("  joc error {e:?}"),
            }
        }
    }
}
