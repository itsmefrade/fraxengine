//! Decode a raw Annex G VC-1 stream and print one MD5 per output frame (ffmpeg `framemd5` compatible
//! hashes of the packed yuv420p frame), or dump raw yuv.
//!
//! usage: vc1md5 <file.vc1> [max_frames] [--yuv out.yuv] [--time]
use aether_vc1::Decoder;
use std::io::Write;

fn md5(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11,
        16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = (0..16).map(|i| u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (mut f, g);
            if i < 16 {
                f = (b & c) | (!b & d);
                g = i;
            } else if i < 32 {
                f = (d & b) | (!d & c);
                g = (5 * i + 1) % 16;
            } else if i < 48 {
                f = b ^ c ^ d;
                g = (3 * i + 5) % 16;
            } else {
                f = c ^ (b | !d);
                g = (7 * i) % 16;
            }
            f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = String::new();
    for v in [a0, b0, c0, d0] {
        for b in v.to_le_bytes() {
            out.push_str(&format!("{:02x}", b));
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let mut max = usize::MAX;
    let mut yuv: Option<std::fs::File> = None;
    let mut timing = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--yuv" => {
                yuv = Some(std::fs::File::create(&args[i + 1]).unwrap());
                i += 1;
            }
            "--time" => timing = true,
            s => max = s.parse().unwrap_or(usize::MAX),
        }
        i += 1;
    }
    let data = std::fs::read(path).unwrap();
    let mut dec = Decoder::new();
    let aus = aether_vc1::decoder::split_access_units(&data);
    let t0 = std::time::Instant::now();
    let mut n = 0usize;
    let mut buf = Vec::new();
    let mut hashes = Vec::new();
    'outer: for (idx, (a, b)) in aus.iter().enumerate() {
        if let Err(e) = dec.decode(&data[*a..*b], idx as i64) {
            eprintln!("au {} error {:?}", idx, e);
        }
        while let Some(f) = dec.take_frame() {
            buf.clear();
            f.write_i420(&mut buf);
            if let Some(y) = yuv.as_mut() {
                y.write_all(&buf).unwrap();
            }
            if !timing {
                hashes.push(format!("{} {} {}{}", n, f.pts, md5(&buf), f.pict_type));
            }
            n += 1;
            if n >= max {
                break 'outer;
            }
        }
    }
    if n < max {
        dec.flush();
        while let Some(f) = dec.take_frame() {
            buf.clear();
            f.write_i420(&mut buf);
            if let Some(y) = yuv.as_mut() {
                y.write_all(&buf).unwrap();
            }
            if !timing {
                hashes.push(format!("{} {} {}{}", n, f.pts, md5(&buf), f.pict_type));
            }
            n += 1;
        }
    }
    let el = t0.elapsed().as_secs_f64();
    for h in &hashes {
        println!("{}", h);
    }
    #[cfg(feature = "diag")]
    eprintln!("overflows {}", aether_vc1::dsp::OVERFLOWS.load(std::sync::atomic::Ordering::Relaxed));
    eprintln!("{} frames in {:.3}s ({:.1} fps, errors {})", n, el, n as f64 / el, dec.errors);
}
