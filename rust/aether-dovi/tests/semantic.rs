#![cfg(feature = "semantic-rpu")]
use aether_dovi::{
    rpu,
    semantic::{self, EnhancementType, SemanticError},
};
fn template() -> Vec<u8> {
    let raw = include_bytes!("data/fel-template.bin");
    assert_eq!(&raw[..4], &[0, 0, 0, 1]);
    [&[0x7c, 1], &raw[4..]].concat()
}
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}
fn recalculate_and_escape(mut bytes: Vec<u8>) -> Vec<u8> {
    let crc_offset = bytes.len() - 5;
    let crc = rpu::mpeg2_crc32(&bytes[3..crc_offset]);
    bytes[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_be_bytes());
    let mut out = Vec::new();
    let mut zeros = 0;
    for b in bytes {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}
#[test]
fn upstream_synthetic_template_has_profile7_fel_semantics() {
    let nal = template();
    assert!(rpu::crc_is_valid(&nal));
    let summary = semantic::inspect_rpu(&nal).unwrap();
    assert_eq!(summary.profile, 7);
    assert_eq!(summary.enhancement, EnhancementType::Fel);
    assert!(!summary.disable_residual);
    assert!(!summary.use_previous_mapping);
    assert!(summary.has_mapping && summary.has_display_metadata);
    assert_eq!(
        (
            summary.base_bit_depth,
            summary.enhancement_bit_depth,
            summary.vdr_bit_depth
        ),
        (10, 10, 12)
    );
}
#[test]
fn crc_failures_and_oversized_input_never_reach_semantic_parser() {
    let mut nal = template();
    nal[30] ^= 1;
    assert_eq!(
        semantic::inspect_rpu(&nal),
        Err(SemanticError::CrcOrEnvelope)
    );
    assert_eq!(
        semantic::inspect_rpu(&vec![0; 65537]),
        Err(SemanticError::TooLarge)
    );
    for len in 0..25 {
        assert!(semantic::inspect_rpu(&template()[..len]).is_err());
    }
}
#[test]
fn crc_valid_template_mutations_exercise_guard_and_parser_rejections() {
    let raw = unescape(&template());
    let mut guard_rejected = 0;
    let mut parser_rejected = 0;
    for pos in 3..raw.len() - 5 {
        for bit in [0, 3, 7] {
            let mut mutated = raw.clone();
            mutated[pos] ^= 1 << bit;
            let escaped = recalculate_and_escape(mutated);
            assert!(rpu::crc_is_valid(&escaped));
            match semantic::inspect_rpu(&escaped) {
                Err(SemanticError::UnsupportedOrHostileSyntax) => guard_rejected += 1,
                Err(SemanticError::ParseRejected) => parser_rejected += 1,
                Ok(_) => {}
                other => panic!("unexpected result {other:?}"),
            }
        }
    }
    assert!(guard_rejected > 0 && parser_rejected > 0);
}
#[test]
fn mel_conversion_of_trusted_template_is_not_misclassified_as_fel() {
    use dolby_vision::rpu::{ConversionMode, dovi_rpu::DoviRpu};
    let mut trusted = DoviRpu::parse_unspec62_nalu(&template()).unwrap();
    trusted.convert_with_mode(ConversionMode::ToMel).unwrap();
    let mel = trusted.write_hevc_unspec62_nalu().unwrap();
    let summary = semantic::inspect_rpu(&mel).unwrap();
    assert_eq!(summary.enhancement, EnhancementType::Mel);
    assert_eq!(summary.profile, 7);
}

#[derive(Default)]
struct Writer {
    bits: Vec<bool>,
}
impl Writer {
    fn bits(&mut self, value: u64, width: usize) {
        for i in (0..width).rev() {
            self.bits.push((value >> i) & 1 != 0);
        }
    }
    fn ue(&mut self, value: u64) {
        let x = value + 1;
        let len = (64 - x.leading_zeros()) as usize;
        self.bits(0, len - 1);
        self.bits(x, len);
    }
    fn coefficient(&mut self, value: u64) {
        self.ue(if value == 0 { 0 } else { 2 * value - 1 });
        self.bits(0, 23);
    }
    fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0; self.bits.len().div_ceil(8)];
        for (i, b) in self.bits.iter().enumerate() {
            if *b {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }
}
// A small source-derived syntax writer, not a production RPU generator.
fn crafted_mapping(linear: bool, oversized_pivots: bool) -> Vec<u8> {
    let mut w = Writer::default();
    w.bits(2, 6);
    w.bits(18, 11);
    w.bits(1, 4);
    w.bits(0, 4);
    w.bits(1, 1);
    w.bits(0, 1);
    w.bits(0, 2);
    w.ue(23);
    w.bits(0, 2);
    w.bits(0, 1);
    w.ue(2);
    w.ue(2);
    w.ue(4);
    w.bits(0, 1);
    w.bits(0, 3);
    w.bits(0, 1);
    w.bits(1, 1);
    w.bits(0, 1);
    w.bits(0, 1);
    w.ue(0);
    w.ue(0);
    w.ue(0);
    for component in 0..3 {
        w.ue(if component == 0 && oversized_pivots {
            1 << 30
        } else {
            0
        });
        w.bits(0, 10);
        w.bits(1023, 10);
    }
    w.ue(0);
    w.ue(0);
    for component in 0..3 {
        w.ue(0);
        w.ue(0);
        w.bits(u64::from(linear && component == 0), 1);
        if !(linear && component == 0) {
            w.coefficient(0);
            w.coefficient(1);
        }
    }
    let mut raw = vec![0x7c, 1, 0x19];
    raw.extend_from_slice(&w.bytes());
    raw.extend_from_slice(&[0, 0, 0, 0, 0x80]);
    recalculate_and_escape(raw)
}
#[test]
fn crc_valid_known_panic_and_allocation_shapes_are_refused_before_parser() {
    for nal in [crafted_mapping(true, false), crafted_mapping(false, true)] {
        assert!(rpu::crc_is_valid(&nal));
        assert_eq!(
            semantic::inspect_rpu(&nal),
            Err(SemanticError::UnsupportedOrHostileSyntax)
        );
    }
}
#[test]
fn display_json_of_the_synthetic_fel_template_is_guarded_and_complete() {
    let nal = template();
    let json = aether_dovi::display::display_json(&nal).unwrap();
    for key in ["\"profile\":7", "\"yccToRgb\":[", "\"rgbToLms\":[", "\"signal\":{", "\"source\":{", "\"l1\":", "\"l2\":[", "\"l6\":", "\"cm\":["] {
        assert!(json.contains(key), "{key} missing in {json}");
    }
    let mut bad = nal.clone();
    let n = bad.len();
    bad[n - 8] ^= 1;
    assert_eq!(aether_dovi::display::display_json(&bad), Err(SemanticError::CrcOrEnvelope));
    assert_eq!(aether_dovi::display::display_json(&vec![0u8; 70_000]), Err(SemanticError::TooLarge));
}
