use aether_audio::paired::*;
fn selection() -> Selection {
    Selection {
        bed_id: 2,
        elements_id: 3,
        bed_labels: ["L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb"]
            .map(str::to_owned)
            .to_vec(),
        element_channels: 16,
        element_labels: vec!["LFE".into()],
    }
}
#[test]
fn bounds_and_selection() {
    let mut s = selection();
    s.bed_id = 3;
    assert!(matches!(
        PairedDecoder::new(s),
        Err(Error::InvalidSelection)
    ));
    let mut d = PairedDecoder::new(selection()).unwrap();
    assert_eq!(d.push(&vec![0; MAX_PUSH + 1]), Err(Error::PushLimit));
    for _ in 0..16 {
        d.push(&vec![0; MAX_PUSH]).unwrap()
    }
    assert_eq!(d.push(&[0]), Err(Error::PendingLimit));
    assert_eq!(d.pending_bytes(), MAX_PENDING);
    d.reset().unwrap();
    assert_eq!(d.generation(), 1);
    assert_eq!(d.pending_bytes(), 0);
}
#[derive(Debug, PartialEq)]
struct Record {
    au: u64,
    byte: u64,
    frame: u64,
    rate: u32,
    len: usize,
    bed: Vec<i32>,
    elements: Vec<i32>,
    metadata: String,
}
fn run(bytes: &[u8], chunk: usize) -> Vec<Record> {
    let mut d = PairedDecoder::new(selection()).unwrap();
    let mut result = Vec::new();
    for b in bytes.chunks(chunk) {
        d.push(b).unwrap();
        while let Pull::Block(x) = d.pull().unwrap() {
            result.push(Record {
                au: x.access_unit,
                byte: x.input_offset_bytes,
                frame: x.output_frame_offset,
                rate: x.sample_rate,
                len: x.frames,
                bed: x.bed.pcm.clone(),
                elements: x.elements.pcm.clone(),
                metadata: format!("{:?}", x.metadata),
            })
        }
    }
    assert_eq!(d.pending_bytes(), 0);
    d.finish().unwrap();
    assert_eq!(d.push(&[]), Err(Error::Finished));
    assert!(matches!(d.pull().unwrap(), Pull::Eof));
    assert!(matches!(d.pull().unwrap(), Pull::Eof));
    result
}
#[test]
#[ignore = "requires local fixture and prior companion probe output"]
fn real_fixture_pair_and_rejections() {
    let bytes = std::fs::read("../../test-media/prefix.thd").unwrap();
    let reference = std::fs::read("../../test-media/p2.s32le").unwrap();
    let a = run(&bytes, 997);
    let b = run(&bytes, 65536);
    let c = run(&bytes, 1);
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert_eq!(a.len(), 1200);
    let mut pcm = Vec::new();
    let mut meta = 0;
    for (i, x) in a.iter().enumerate() {
        assert_eq!(x.au, i as u64);
        assert_eq!(x.frame, (i * 40) as u64);
        assert_eq!(x.len, 40);
        assert_eq!(x.rate, 48000);
        if x.metadata != "[]" {
            meta += 1
        }
        for v in &x.bed {
            pcm.extend_from_slice(&v.wrapping_shl(8).to_le_bytes())
        }
    }
    assert!(meta > 0);
    assert_eq!(pcm, reference);
    let mut bad = selection();
    bad.bed_labels.swap(0, 1);
    let mut d = PairedDecoder::new(bad).unwrap();
    d.push(&bytes[..997]).unwrap();
    assert_eq!(d.pull().unwrap_err(), Error::InvalidBlock);
    assert_eq!(d.pull().unwrap_err(), Error::Poisoned);
    assert_eq!(d.push(&[]), Err(Error::Poisoned));
    d.reset().unwrap();
    assert_eq!(d.generation(), 1);
    assert_eq!(d.pending_bytes(), 0);
    let mut valid = PairedDecoder::new(selection()).unwrap();
    valid.push(&bytes[..8192]).unwrap();
    let mut held = Vec::new();
    for _ in 0..MAX_OUTSTANDING_BLOCKS {
        held.push(match valid.pull().unwrap() {
            Pull::Block(x) => x,
            _ => panic!("expected block"),
        })
    }
    assert_eq!(valid.pull().unwrap_err(), Error::OutstandingLimit);
    valid.reset().unwrap();
    valid.push(&bytes[..997]).unwrap();
    assert_eq!(valid.pull().unwrap_err(), Error::OutstandingLimit);
    held.pop();
    let fresh = match valid.pull().unwrap() {
        Pull::Block(x) => x,
        _ => panic!("expected block"),
    };
    assert_eq!(fresh.generation, 1);
    assert_eq!(fresh.input_offset_bytes, 0);
    assert_eq!(fresh.output_frame_offset, 0);
    assert_eq!(held[0].bed.pcm, fresh.bed.pcm);
    drop(valid);
    assert_eq!(held[0].bed.pcm.len(), 320);
    let mut bad = selection();
    bad.bed_id = 3;
    bad.elements_id = 2;
    let mut d = PairedDecoder::new(bad).unwrap();
    d.push(&bytes[..997]).unwrap();
    assert_eq!(d.pull().unwrap_err(), Error::UnsupportedMap);
    assert_eq!(d.pull().unwrap_err(), Error::Poisoned);
}

#[test]
fn finish_without_sync_is_sticky_and_reset_reopens() {
    let mut d = PairedDecoder::new(selection()).unwrap();
    d.push(&[0; 5]).unwrap();
    d.finish().unwrap();
    assert_eq!(d.push(&[1]), Err(Error::Finished));
    assert_eq!(d.pull().unwrap_err(), Error::MissingMajorSync);
    assert_eq!(d.pull().unwrap_err(), Error::Poisoned);
    assert_eq!(d.finish(), Err(Error::Poisoned));
    d.reset().unwrap();
    assert_eq!(d.generation(), 1);
    d.push(&[1]).unwrap();
    assert!(matches!(d.pull().unwrap(), Pull::NeedInput));
}
#[test]
#[ignore = "requires local fixture and prior companion probe output"]
fn finish_drains_and_truncation_fails() {
    let bytes = std::fs::read("../../test-media/prefix.thd").unwrap();
    let mut buffered = PairedDecoder::new(selection()).unwrap();
    buffered.push(&bytes[..588]).unwrap();
    buffered.finish().unwrap();
    buffered.finish().unwrap();
    match buffered.pull().unwrap() {
        Pull::Block(x) => {
            assert_eq!(x.access_unit, 0);
            assert_eq!(x.input_offset_bytes, 0);
            assert_eq!(x.output_frame_offset, 0);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(buffered.pull().unwrap(), Pull::Eof));
    assert!(matches!(buffered.pull().unwrap(), Pull::Eof));
    buffered.reset().unwrap();
    buffered.push(&bytes[..588]).unwrap();
    match buffered.pull().unwrap() {
        Pull::Block(x) => assert_eq!(x.generation, 1),
        other => panic!("unexpected {other:?}"),
    }
    let mut first = PairedDecoder::new(selection()).unwrap();
    first.push(&bytes[..100]).unwrap();
    assert!(matches!(first.pull().unwrap(), Pull::NeedInput));
    first.finish().unwrap();
    assert_eq!(first.pull().unwrap_err(), Error::MissingMajorSync);
    assert_eq!(first.pull().unwrap_err(), Error::Poisoned);
    let mut d = PairedDecoder::new(selection()).unwrap();
    let mut count = 0;
    for chunk in bytes[..bytes.len() - 1].chunks(65536) {
        d.push(chunk).unwrap();
        loop {
            match d.pull().unwrap() {
                Pull::Block(x) => {
                    assert_eq!(x.access_unit, count);
                    count += 1;
                }
                Pull::NeedInput => break,
                Pull::Eof => panic!("early eof"),
            }
        }
    }
    assert_eq!(count, 1199);
    d.finish().unwrap();
    assert_eq!(d.pull().unwrap_err(), Error::IncompleteInput);
    assert_eq!(d.pull().unwrap_err(), Error::Poisoned);
    d.reset().unwrap();
    d.push(&bytes[..8192]).unwrap();
    d.finish().unwrap();
    let mut n = 0;
    loop {
        match d.pull() {
            Ok(Pull::Block(x)) => {
                assert_eq!(x.generation, 1);
                assert_eq!(x.access_unit, n);
                n += 1
            }
            Err(Error::IncompleteInput) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(n > 0);
    assert_eq!(d.pull().unwrap_err(), Error::Poisoned);
}
