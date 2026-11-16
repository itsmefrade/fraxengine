//! Minimal MP4 sample table reader for the first audio track (development helper).
#![allow(dead_code)]

fn be32(b: &[u8], p: usize) -> u32 {
    u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
}
fn be64(b: &[u8], p: usize) -> u64 {
    u64::from_be_bytes(b[p..p + 8].try_into().unwrap())
}

fn boxes(b: &[u8], s: usize, e: usize) -> Vec<(String, usize, usize)> {
    let mut v = vec![];
    let mut p = s;
    while p + 8 <= e {
        let mut sz = be32(b, p) as usize;
        let t = String::from_utf8_lossy(&b[p + 4..p + 8]).to_string();
        let mut hdr = 8;
        if sz == 1 {
            sz = be64(b, p + 8) as usize;
            hdr = 16;
        } else if sz == 0 {
            sz = e - p;
        }
        if sz < hdr || p + sz > e {
            break;
        }
        v.push((t, p + hdr, p + sz));
        p += sz;
    }
    v
}

fn find(b: &[u8], s: usize, e: usize, path: &[&str]) -> Option<(usize, usize)> {
    for (t, a, z) in boxes(b, s, e) {
        if t == path[0] {
            if path.len() == 1 {
                return Some((a, z));
            }
            if let Some(r) = find(b, a, z, &path[1..]) {
                return Some(r);
            }
        }
    }
    None
}

/// Returns (sample entry fourcc, samples).
pub fn read_audio_samples(path: &str) -> (String, Vec<Vec<u8>>) {
    let data = std::fs::read(path).expect("read mp4");
    let (ms, me) = find(&data, 0, data.len(), &["moov"]).expect("moov");
    for (t, a, z) in boxes(&data, ms, me) {
        if t != "trak" {
            continue;
        }
        let Some(mdia) = find(&data, a, z, &["mdia"]) else {
            continue;
        };
        let Some(hd) = find(&data, mdia.0, mdia.1, &["hdlr"]) else {
            continue;
        };
        if &data[hd.0 + 8..hd.0 + 12] != b"soun" {
            continue;
        }
        let minf = find(&data, mdia.0, mdia.1, &["minf"]).unwrap();
        let stbl = find(&data, minf.0, minf.1, &["stbl"]).unwrap();
        let stsd = find(&data, stbl.0, stbl.1, &["stsd"]).unwrap();
        let fourcc = String::from_utf8_lossy(&data[stsd.0 + 12..stsd.0 + 16]).to_string();
        let stsz = find(&data, stbl.0, stbl.1, &["stsz"]).unwrap();
        let stsc = find(&data, stbl.0, stbl.1, &["stsc"]).unwrap();
        let ss = be32(&data, stsz.0 + 4) as usize;
        let cnt = be32(&data, stsz.0 + 8) as usize;
        let sizes: Vec<usize> = if ss != 0 {
            vec![ss; cnt]
        } else {
            (0..cnt)
                .map(|i| be32(&data, stsz.0 + 12 + 4 * i) as usize)
                .collect()
        };
        let n = be32(&data, stsc.0 + 4) as usize;
        let stscs: Vec<(usize, usize)> = (0..n)
            .map(|i| {
                (
                    be32(&data, stsc.0 + 8 + 12 * i) as usize,
                    be32(&data, stsc.0 + 12 + 12 * i) as usize,
                )
            })
            .collect();
        let offs: Vec<usize> = if let Some(c) = find(&data, stbl.0, stbl.1, &["stco"]) {
            let m = be32(&data, c.0 + 4) as usize;
            (0..m)
                .map(|i| be32(&data, c.0 + 8 + 4 * i) as usize)
                .collect()
        } else {
            let c = find(&data, stbl.0, stbl.1, &["co64"]).unwrap();
            let m = be32(&data, c.0 + 4) as usize;
            (0..m)
                .map(|i| be64(&data, c.0 + 8 + 8 * i) as usize)
                .collect()
        };
        let mut out = vec![];
        let mut si = 0;
        for (ci, &off) in offs.iter().enumerate() {
            let spc = stscs.iter().rev().find(|x| x.0 <= ci + 1).unwrap().1;
            let mut o = off;
            for _ in 0..spc {
                if si >= sizes.len() {
                    break;
                }
                out.push(data[o..o + sizes[si]].to_vec());
                o += sizes[si];
                si += 1;
            }
        }
        return (fourcc, out);
    }
    panic!("no audio track");
}
