//! Parses every JOC payload of an E-AC-3 stream and prints statistics.
use aether_joc::{emdf, joc};
use std::collections::BTreeMap;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&a[1]).unwrap();
    let (mut aus, mut with_joc, mut ok, mut exact, mut err, mut seq_ok) = (0, 0, 0, 0, 0, 0);
    let mut hist: BTreeMap<String, usize> = BTreeMap::new();
    let mut last_seq = 0u16;
    for au in aether_ac3::access_units(&stream) {
        aus += 1;
        let e = emdf::extract_from_access_unit(au);
        let Some(j) = &e.joc else { continue };
        with_joc += 1;
        match joc::parse_joc(j, joc::SparseOffsetMode::ZeroPoint) {
            Ok(f) => {
                ok += 1;
                if f.bits_total - f.bits_used < 8 && j[f.bits_used / 8..].iter().all(|&b| b == 0) || f.bits_total - f.bits_used < 8 { exact += 1; }
                if f.seq_count == last_seq + 1 || (last_seq == 1023 && f.seq_count == 1) { seq_ok += 1; }
                last_seq = f.seq_count;
                for o in &f.objects {
                    *hist.entry(format!("present={} bands={} sparse={} quant={} slope={} dp={}", o.present, o.num_bands, o.sparse, o.quant_idx, o.slope_idx, o.num_dpoints)).or_default() += 1;
                }
                *hist.entry(format!("dmx={} nobj={}", f.dmx_config_idx, f.num_objects)).or_default() += 1;
            }
            Err(_) => err += 1,
        }
    }
    println!("AUs {aus} with_joc {with_joc} parsed {ok} fully_consumed(<8 bits left) {exact} errors {err} seq_consecutive {seq_ok}");
    for (k, v) in hist { println!("  {k}: {v}"); }
}
