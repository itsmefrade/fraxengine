//! Test helpers: SHA-256, ffprobe oracle rows, and a tiny MP4 box writer.
#![allow(dead_code)]

pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
        0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
        0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
        0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
        0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend(((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// One `ffprobe -show_packets` CSV row: stream_index,pts,dts,duration,size,pos,flags,data_hash.
#[derive(Debug, Clone)]
pub struct Row {
    pub stream: usize,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
    pub size: u32,
    pub pos: u64,
    pub key: bool,
    pub hash: String,
}

pub fn read_rows(csv: &str) -> Vec<Row> {
    csv.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(',').collect();
            if f.len() < 8 {
                return None;
            }
            let n = |s: &str| s.parse::<i64>().ok();
            Some(Row {
                stream: f[0].parse().ok()?,
                pts: n(f[1]),
                dts: n(f[2]),
                duration: n(f[3]),
                size: f[4].parse().ok()?,
                pos: f[5].parse().unwrap_or(u64::MAX),
                key: f[6].starts_with('K'),
                hash: f[7].trim_start_matches("SHA256:").to_ascii_lowercase(),
            })
        })
        .collect()
}

pub fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    v.extend(kind);
    v.extend(body);
    v
}
pub fn bx64(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = 1u32.to_be_bytes().to_vec();
    v.extend(kind);
    v.extend(((16 + body.len()) as u64).to_be_bytes());
    v.extend(body);
    v
}
pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}
pub fn be32(v: u32) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}
pub fn be16(v: u16) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}
