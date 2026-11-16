//! Decodes a raw `.dts` elementary stream (core + EXSS per packet, as `ffmpeg -c copy -f dts`
//! writes it) through `DtsSession` and writes interleaved f32le to stdout-file argv[2].
//! Prints the format. Used to compare against `ffmpeg -f f32le` on DTS-HD HRA files.
use aether_audio::dts::DtsSession;
use dca::parse_header;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut out = args.get(2).map(|p| std::fs::File::create(p).unwrap());
    let mut session = DtsSession::new();
    let mut pos = 0;
    let mut n = 0;
    while pos + 16 < data.len() {
        let info = parse_header(&data[pos..]).expect("core header");
        // The packet is core + zero padding + EXSS; find the end by the next core sync word.
        let mut end = pos + info.frame_size;
        let mut e = end;
        while e + 4 <= data.len() && data[e] == 0 {
            e += 1;
        }
        if data.get(e..e + 4) == Some(&[0x64, 0x58, 0x20, 0x25]) {
            end = e + dca::exss_substream_size(&data[e..]).expect("exss size");
        }
        let packet = &data[pos..end.min(data.len())];
        pos = end;
        if n == 0 {
            let mut e = info.frame_size;
            while e < packet.len() && packet[e] == 0 { e += 1; }
            eprintln!("core {} bytes, exss at {e}: kind {:?} size {:?} head {:02x?}", info.frame_size, dca::exss_kind(&packet[e..]), dca::exss_substream_size(&packet[e..]), &packet[e..(e+64).min(packet.len())]);
        }
        let mut framed = (packet.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(packet);
        match session.push(&framed, 48_000) {
            Ok(d) => {
                if n == 0 {
                    eprintln!("rate {} labels {:?} frames {} hd {} core_rate {}", d.sample_rate, d.channel_labels, d.pcm.len() / d.channel_labels.len().max(1), d.hd_packets, info.sample_rate);
                }
                if let Some(f) = out.as_mut() {
                    for s in &d.pcm {
                        f.write_all(&s.to_le_bytes()).unwrap();
                    }
                }
            }
            Err(e) => {
                eprintln!("packet {n}: {e}");
                return;
            }
        }
        n += 1;
    }
    eprintln!("{n} packets");
}
