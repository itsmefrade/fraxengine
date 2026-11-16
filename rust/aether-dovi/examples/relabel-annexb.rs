//! `relabel-annexb in.hevc out.hevc`: rewrite every SPS to BT.709 primaries / sRGB transfer.
use aether_dovi::vui::{SDR_CARRIER, relabel_annexb};

fn main() {
    let mut args = std::env::args().skip(1);
    let (input, output) = (args.next().expect("input"), args.next().expect("output"));
    let data = std::fs::read(&input).expect("read input");
    let (out, originals) = relabel_annexb(&data, SDR_CARRIER).expect("relabel");
    std::fs::write(&output, &out).expect("write output");
    eprintln!("{} SPS rewritten, first original {:?}, size {} -> {}", originals.len(), originals.first(), data.len(), out.len());
}
