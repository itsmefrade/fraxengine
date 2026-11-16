//! Development oracle only. Media bytes remain local and are never embedded.
use aether_audio::TrueHdDecoder;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = std::fs::read(
        args.get(1)
            .expect("usage: inspect-local path [presentation] [pcm24-output]"),
    )
    .unwrap();
    let selection = args.get(2).map_or(3, |s| s.parse().unwrap());
    let mut decoder = TrueHdDecoder::new(selection).unwrap();
    let (
        mut blocks,
        mut frames,
        mut channels,
        mut rate,
        mut payloads,
        mut max_objects,
        mut updates,
        mut active_updates,
    ) = (0u64, 0u64, 0usize, 0u32, 0usize, 0usize, 0usize, 0usize);
    let (mut labels, mut first_payload) = (Vec::new(), None);
    let (mut peaks, mut energy) = (vec![0u32; 16], vec![0f64; 16]);
    let mut pcm = Vec::new();
    for chunk in input.chunks(65536) {
        decoder.push(chunk).unwrap();
        while let Some(block) = decoder.pull().unwrap() {
            blocks += 1;
            frames += block.frames as u64;
            channels = block.channels;
            rate = block.sample_rate;
            labels = block.labels.clone();
            for row in block.pcm.chunks(channels) {
                for (c, &sample) in row.iter().enumerate() {
                    peaks[c] = peaks[c].max(sample.unsigned_abs());
                    energy[c] += (sample as f64 / 8388608.0).powi(2);
                }
            }
            if args.len() > 3 {
                for sample in &block.pcm {
                    pcm.extend_from_slice(&sample.to_le_bytes()[..3]);
                }
            }
            for payload in &block.metadata {
                payloads += 1;
                max_objects = max_objects.max(payload.object_count);
                if first_payload.is_none() {
                    first_payload = Some(format!(
                        "AU {} at output frame {}: {:?}",
                        block.access_unit,
                        frames - block.frames as u64,
                        payload
                    ));
                }
                if let Some(element) = &payload.object_element {
                    for object in &element.object_data {
                        updates += object.len();
                        active_updates += object
                            .iter()
                            .filter(|info| !info.b_object_not_active)
                            .count();
                    }
                }
            }
        }
    }
    if let Some(path) = args.get(3) {
        std::fs::write(path, &pcm).unwrap();
    }
    println!(
        "input_bytes={}; presentation={}; blocks={}; frames={}; channels={}; sample_rate={}; labels={labels:?}; oamd_payloads={}; max_declared_objects={}; object_updates={}; active_updates={}; pending={}; peaks={:?}; rms={:?}",
        input.len(),
        selection,
        blocks,
        frames,
        channels,
        rate,
        payloads,
        max_objects,
        updates,
        active_updates,
        decoder.pending_bytes(),
        &peaks[..channels],
        energy[..channels]
            .iter()
            .map(|v| (v / frames as f64).sqrt())
            .collect::<Vec<_>>()
    );
    if let Some(payload) = first_payload {
        println!("first_payload={payload}");
    }
}
