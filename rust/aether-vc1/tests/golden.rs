//! Pinned regression vectors: deterministic random-payload streams (see `testgen`) whose decoded
//! output was cross-checked against FFmpeg (`-cpuflags 0`) when the vectors were recorded.
//! Covers progressive, interlaced frame and interlaced field pictures, slices, intensity
//! compensation, overlap, loop filter, all B modes and the DQUANT profiles.
mod common;
use aether_vc1::testgen::{generate, GenConfig};
use aether_vc1::Decoder;
use common::md5_hex;

// (seed, width, height, frames, interlace, payload, fcm, slices, expected output frames, md5 of the concatenated per-frame md5 hex strings)
#[allow(clippy::type_complexity)]
const VECTORS: &[(u64, usize, usize, usize, i32, usize, i32, i32, usize, &str)] = &[
    (11, 128, 96, 6, 0, 6000, 0, 0, 5, "e3b96d82ef370476e1e7f236a70e0be6"),
    (12, 128, 96, 6, 0, 6000, 0, 1, 6, "83bfe7744ebda4ff9052687526a2f4ce"),
    (13, 128, 96, 6, 1, 6000, 1, 0, 5, "acad00ef700ebb285546bd0992d8006b"),
    (14, 128, 96, 6, 1, 6000, 1, 1, 5, "13653c4318723fbd8e449f6572ef7f3b"),
    (15, 128, 96, 6, 1, 6000, 2, 0, 5, "5cf6ce985cc9f866bdcbe50c90c44216"),
    (16, 128, 96, 6, 1, 6000, 2, 1, 6, "05ca8e4f64eb569f546cfd788ea875a4"),
    (17, 98, 90, 8, 1, 6000, 1, 0, 6, "008a1c2c791297b55c1d9fe5bf71e1dd"),
    (18, 98, 90, 8, 1, 6000, 2, 0, 7, "93ec0df33eb83a4431a04fb359d0a9b2"),
    (19, 176, 144, 6, 0, 16000, 0, 0, 5, "e853713ce33ee7bdc45701e829cce574"),
    (20, 80, 48, 8, 1, 5000, 1, 1, 8, "40ee6b8021e7f5218511050db392576f"),
    (21, 80, 48, 8, 1, 5000, 2, 1, 8, "5e2f7a207e48444e7fbc892d65d20409"),
    (22, 130, 66, 8, 1, 6000, 2, 0, 8, "b71fa755aeb5a18228de4f0fd6ea0478"),
];

#[test]
fn pinned_vectors_match_ffmpeg_recording() {
    for &(seed, w, h, frames, il, payload, fcm, sl, nframes, expect) in VECTORS {
        let cfg = GenConfig { width: w, height: h, frames, interlace: il, payload_bytes: payload, fcm, slices: sl != 0 };
        let stream = generate(seed, &cfg);
        let mut dec = Decoder::new();
        let mut hashes = String::new();
        let mut n = 0;
        let mut collect = |dec: &mut Decoder| {
            while let Some(f) = dec.take_frame() {
                let mut v = Vec::new();
                f.write_i420(&mut v);
                hashes.push_str(&md5_hex(&v));
                n += 1;
            }
        };
        for (i, (a, b)) in aether_vc1::decoder::split_access_units(&stream).into_iter().enumerate() {
            dec.decode(&stream[a..b], i as i64).unwrap();
            collect(&mut dec);
        }
        dec.flush();
        collect(&mut dec);
        assert_eq!(n, nframes, "seed {seed}: frame count");
        assert_eq!(md5_hex(hashes.as_bytes()), expect, "seed {seed}: output differs from the FFmpeg recording");
    }
}

/// Container packets (no start code in front of the frame data, both fields in one packet divided
/// by a field marker, headers in CodecPrivate) must decode exactly like the Annex G form.
#[test]
fn container_style_equals_annex_g() {
    for &(seed, w, h, frames, il, payload, fcm, sl, _n, _h) in VECTORS {
        if sl != 0 {
            continue; // slice units need their start codes inside the packet; FFmpeg has the same limit
        }
        let cfg = GenConfig { width: w, height: h, frames, interlace: il, payload_bytes: payload, fcm, slices: false };
        let stream = generate(seed, &cfg);
        let aus = aether_vc1::decoder::split_access_units(&stream);
        let run = |container: bool| -> Vec<String> {
            let mut dec = Decoder::new();
            let mut out = Vec::new();
            for (i, (a, b)) in aus.iter().enumerate() {
                let au = &stream[*a..*b];
                let fpos = au.windows(4).position(|x| x == [0, 0, 1, 0x0d]).unwrap();
                if container {
                    if i == 0 {
                        dec.set_extradata(&au[..fpos]).unwrap();
                    }
                    dec.decode(&au[fpos + 4..], i as i64).unwrap();
                } else {
                    dec.decode(au, i as i64).unwrap();
                }
                while let Some(f) = dec.take_frame() {
                    let mut v = Vec::new();
                    f.write_i420(&mut v);
                    out.push(md5_hex(&v));
                }
            }
            dec.flush();
            while let Some(f) = dec.take_frame() {
                let mut v = Vec::new();
                f.write_i420(&mut v);
                out.push(md5_hex(&v));
            }
            out
        };
        assert_eq!(run(true), run(false), "seed {seed}");
    }
}
