//! Runs against the local DV P7 fixtures when present (they are git-ignored).
use aether_dovi::vui::{Colour, SDR_CARRIER, read_sps_colour, relabel_annexb};

fn sps_nals(stream: &[u8]) -> Vec<&[u8]> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            if (stream[i + 3] >> 1) & 63 == 33 {
                let end = (i + 3..stream.len() - 3)
                    .find(|&j| stream[j..j + 3] == [0, 0, 1] || stream[j..j + 3] == [0, 0, 0])
                    .unwrap_or(stream.len());
                found.push(&stream[i + 3..end]);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

#[test]
fn relabels_real_fel_fixture_sps_in_both_layers() {
    for name in ["fel-p7-bl.hevc", "fel-p7-el.hevc"] {
        let path = format!("{}/../../web/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let Ok(mut data) = std::fs::read(&path) else {
            eprintln!("skipping {name}: fixture absent");
            continue;
        };
        data.truncate(64 * 1024);
        let before = sps_nals(&data);
        assert!(!before.is_empty(), "{name} has an SPS in its first access unit");
        let original = read_sps_colour(before[0]).unwrap();
        let (patched, originals) = relabel_annexb(&data, SDR_CARRIER).unwrap();
        assert_eq!(originals[0], original);
        let after = sps_nals(&patched);
        assert_eq!(
            read_sps_colour(after[0]).unwrap(),
            Colour { primaries: 1, transfer: 13, matrix: original.matrix }
        );
    }
}
