//! End to end test case for the intra stage from a real picture of G0's reconstruction stream.
//!
//! Input: a `reconstream dump <file.hevc> <dir>` directory (web/gpu-recon/FORMAT-STREAM.md): `stream.rcs` plus the
//! stage pictures `picNNN.residual.i16` and `picNNN.pre_deblock.u16` (Y, Cb, Cr back to back, stride = plane width).
//! Output (`<out>/<name>.*`, read by web/gpu-recon/test/intra.html):
//!   sintra.bin  the picture's intra section (6-word records, as the stream carries them; the page converts them with
//!               intra.mjs `fromStreamIntra`, so the conversion is under test too)
//!   stus.bin    the TU section (6-word FORMAT-idct.md records; word 4 = resOffset)
//!   res.bin     i32 residual buffer at those resOffsets, cut from the residual stage picture
//!   init.bin    the pre-deblocking picture with every intra block overwritten by noise (inter, PCM samples final)
//!   expect.bin  the pre-deblocking picture (the decoder's reconstruction before the loop filters)
//!   imap.bin    intra 4x4 luma block bits (intra records' luma blocks)
//!   json        meta
//!
//! Usage: from_dump <dump-dir> <picId> <out-dir> <name>

use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dir, pic_id, out, name) = (&a[1], a[2].parse::<u32>().unwrap(), &a[3], &a[4]);
    let raw = std::fs::read(format!("{dir}/stream.rcs")).unwrap();
    let w: Vec<u32> = raw.chunks(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    assert_eq!(w[0], 0x31534352, "not a reconstruction stream");
    let mut at = 2;
    let pic = loop {
        assert!(at < w.len(), "picture {pic_id} not in the stream");
        let (ty, n) = (w[at], w[at + 1] as usize);
        let p = &w[at + 2..at + 2 + n];
        if ty == 1 && p[0] == pic_id {
            break p;
        }
        at += 2 + n;
    };
    let (width, height) = (pic[3] as usize, pic[4] as usize);
    let bd = pic[5] & 0xff;
    assert_eq!(bd, (pic[5] >> 8) & 0xff, "luma and chroma bit depth differ");
    assert_eq!((pic[5] >> 16) & 0xff, 1, "4:2:0 only");
    let log2_ctb = pic[6] & 0xff;
    let flags = pic[7];
    let sec = |i: usize| {
        let (off, cnt) = (pic[24 + 2 * i] as usize, pic[24 + 2 * i + 1] as usize);
        (off, cnt)
    };
    let (tu_off, tu_n) = sec(0);
    let (in_off, in_n) = sec(4);
    let stus = &pic[tu_off..tu_off + 6 * tu_n];
    let sintra = &pic[in_off..in_off + 6 * in_n];

    let read16 = |ext: &str| -> Vec<u16> {
        let b = std::fs::read(format!("{dir}/pic{pic_id:03}.{ext}")).unwrap();
        b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
    };
    let resid: Vec<i16> = read16("residual.i16").into_iter().map(|v| v as i16).collect();
    let pre = read16("pre_deblock.u16");
    let (wc, hc) = (width / 2, height / 2);
    let plane_base = [0, width * height, width * height + wc * hc];
    let plane_w = [width, wc, wc];

    // Residual buffer at G0's offsets.
    let mut res = Vec::new();
    for t in stus.chunks(6) {
        let (x, y) = ((t[0] & 0xffff) as usize, (t[0] >> 16) as usize);
        let (n, c) = (4usize << (t[1] & 3), ((t[1] >> 2) & 3) as usize);
        assert_eq!(res.len(), t[4] as usize, "resOffset is not the running sum");
        for yy in 0..n {
            for xx in 0..n {
                res.push(resid[plane_base[c] + (y + yy) * plane_w[c] + x + xx] as i32);
            }
        }
    }

    // GPU layout planes from the pre-deblocking picture; noise over every intra block in `init`.
    let align64 = |v: usize| (v + 63) & !63;
    let strides = [align64(width), align64(wc), align64(wc)];
    let heights = [height, hc, hc];
    let mut planes: Vec<Vec<u16>> = (0..3)
        .map(|c| {
            let mut p = vec![0u16; strides[c] * heights[c]];
            for y in 0..heights[c] {
                for x in 0..plane_w[c] {
                    p[y * strides[c] + x] = pre[plane_base[c] + y * plane_w[c] + x];
                }
            }
            p
        })
        .collect();
    let expect = pack(&planes);
    let mut seed = 0x2545F4914F6CDD1Du64 ^ pic_id as u64;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % (1 << bd)) as u16
    };
    let mut imap = vec![0u32; ((width / 4) * (height / 4)).div_ceil(32)];
    let (mut intra_samples, mut max_wave) = (0usize, 0u32);
    for r in sintra.chunks(6) {
        let (x, y) = ((r[0] & 0xffff) as usize, (r[0] >> 16) as usize);
        let (c, n) = ((r[1] & 3) as usize, 1usize << ((r[1] >> 2) & 7));
        max_wave = max_wave.max(r[2]);
        let planes_of: &[usize] = if c == 0 { &[0] } else { &[1, 2] };
        for &p in planes_of {
            intra_samples += n * n;
            for yy in 0..n {
                for xx in 0..n {
                    planes[p][(y + yy) * strides[p] + x + xx] = noise();
                }
            }
        }
        if c == 0 {
            for by in (y / 4)..((y + n) / 4) {
                for bx in (x / 4)..((x + n) / 4) {
                    let i = by * (width / 4) + bx;
                    imap[i >> 5] |= 1 << (i & 31);
                }
            }
        }
    }
    let init = pack(&planes);
    let base = format!("{out}/{name}");
    write_u32(&format!("{base}.init.bin"), &init);
    write_u32(&format!("{base}.expect.bin"), &expect);
    write_u32(&format!("{base}.sintra.bin"), sintra);
    write_u32(&format!("{base}.stus.bin"), stus);
    write_u32(&format!("{base}.imap.bin"), &imap);
    write_u32(&format!("{base}.res.bin"), &res.iter().map(|&v| v as u32).collect::<Vec<_>>());
    let json = format!(
        concat!(
            "{{\"source\":\"stream\",\"dump\":\"{}\",\"picId\":{},\"width\":{},\"height\":{},\"bitDepth\":{},",
            "\"log2Ctb\":{},\"cip\":null,\"strongIntraSmoothing\":{},\"intraSmoothingDisabled\":{},\"intraRecords\":{},",
            "\"tuCount\":{},\"intraSamples\":{},\"maxWave\":{},\"intraFraction\":null}}\n"
        ),
        dir,
        pic_id,
        width,
        height,
        bd,
        log2_ctb,
        flags & 2 != 0,
        flags & 4 != 0,
        in_n,
        tu_n,
        intra_samples,
        max_wave,
    );
    std::fs::write(format!("{base}.json"), &json).unwrap();
    print!("{json}");
}

fn pack(planes: &[Vec<u16>]) -> Vec<u32> {
    planes.iter().flat_map(|p| p.chunks(2).map(|c| c[0] as u32 | (c[1] as u32) << 16)).collect()
}

fn write_u32(path: &str, d: &[u32]) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    for &x in d {
        f.write_all(&x.to_le_bytes()).unwrap();
    }
}
