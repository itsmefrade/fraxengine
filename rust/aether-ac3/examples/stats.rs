//! Developer tool: decode a raw stream and print which coding tools it uses.
use aether_ac3::{access_units, Decoder};
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let stream = std::fs::read(&path).unwrap();
    let mut dec = Decoder::new();
    let (mut n, mut errs, mut joc, mut emdf, mut oamd) = (0, 0, 0, 0, 0);
    let mut cx = None;
    let mut first = String::new();
    for au in access_units(&stream) {
        match dec.decode(au) {
            Ok(f) => {
                if first.is_empty() {
                    first = format!(
                        "{} Hz {} ({})",
                        f.sample_rate,
                        f.layout.describe(),
                        f.bitstream_id
                    );
                }
                n += 1;
                joc += f.joc.extension_type_a as u32;
                emdf += f.joc.emdf_joc_payload as u32;
                oamd += f.joc.emdf_oamd_payload as u32;
                cx = cx.or(f.joc.complexity_index);
            }
            Err(e) => {
                errs += 1;
                if errs < 3 {
                    println!("  error: {e}");
                }
            }
        }
    }
    println!("{path}: {n} AUs ok, {errs} errors, ext-type-a {joc}, emdf-joc {emdf}, emdf-oamd {oamd}, complexity {cx:?}; {first}\n  {:?}", dec.stats());
}
