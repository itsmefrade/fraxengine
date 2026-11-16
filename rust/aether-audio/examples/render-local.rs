//! Local renderer measurement tool; does not embed programme PCM in source.
use aether_audio::{
    TrueHdDecoder,
    render::{Layout, ObjectRenderer},
};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = std::fs::read(&args[1]).unwrap();
    let layout = if args.get(2).is_some_and(|s| s == "5.1.4") {
        Layout::FiveOneFour
    } else {
        Layout::SevenOneFour
    };
    let mut decoder = TrueHdDecoder::new(3).unwrap();
    let mut renderer = ObjectRenderer::new(layout);
    let mut frames = 0u64;
    let mut energy = vec![0f64; layout.channels()];
    let mut peaks = vec![0f32; layout.channels()];
    let mut output = Vec::new();
    let mut updates = 0;
    for chunk in input.chunks(65536) {
        decoder.push(chunk).unwrap();
        while let Some(block) = decoder.pull().unwrap() {
            let rendered = renderer.render_block(&block).unwrap();
            updates = rendered.applied_updates;
            frames += block.frames as u64;
            for row in rendered.pcm.chunks(layout.channels()) {
                for (c, &sample) in row.iter().enumerate() {
                    assert!(sample.is_finite());
                    energy[c] += (sample as f64).powi(2);
                    peaks[c] = peaks[c].max(sample.abs());
                    if args.len() > 3 {
                        output.extend_from_slice(&sample.to_le_bytes());
                    }
                }
            }
        }
    }
    if let Some(path) = args.get(3) {
        std::fs::write(path, output).unwrap();
    }
    println!(
        "layout={}; frames={frames}; channels={}; applied_updates={updates}; labels={:?}; peaks={peaks:?}; rms={:?}",
        layout.name(),
        layout.channels(),
        layout.labels(),
        energy
            .iter()
            .map(|v| (v / frames as f64).sqrt())
            .collect::<Vec<_>>()
    );
}
