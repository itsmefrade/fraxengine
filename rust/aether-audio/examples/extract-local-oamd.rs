//! Copies only the first OAMD metadata payload for a small metadata parser test.
use truehd::process::{extract::Extractor, parse::Parser};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).unwrap();
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    parser.set_check_fifo(false);
    parser.set_required_presentations(&[false, false, false, true]);
    for chunk in bytes.chunks(65536) {
        extractor.push_bytes(chunk);
        while let Some(Ok(frame)) = extractor.next() {
            let au = parser.parse(&frame).unwrap();
            if let Some(evo) = au.extra_data.and_then(|d| d.evo_frame) {
                for p in evo.evo_payloads {
                    if p.evo_payload_id == 11 {
                        assert!(p.evo_payload_byte.len() < 4096);
                        std::fs::write(&args[2], &p.evo_payload_byte).unwrap();
                        println!("OAMD metadata bytes {}", p.evo_payload_byte.len());
                        return;
                    }
                }
            }
        }
    }
    panic!("no OAMD payload");
}
