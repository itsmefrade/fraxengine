# aether-joc

Pure Rust decoder for E-AC-3 Joint Object Coding (Dolby Atmos in Dolby Digital
Plus). Member of the `rust/` workspace, depends read-only on `../aether-ac3`. The raw `joc_*` wasm exports
(`src/wasm.rs`) are behind the `standalone-wasm` feature; `aether-audio` uses the crate as a library.

Input: one E-AC-3 access unit plus the bed PCM that `aether_ac3::Decoder`
produced from it. Output: up to 15 object PCM planes (48 kHz, 1536 samples per
frame), the bypassed LFE, and the OAMD metadata timeline.

## What the specification covers and what is implemented

| ETSI TS 103 420 V1.2.1 | status |
| --- | --- |
| 8.2 EMDF container, payload ids 11 (OAMD) and 14 (JOC) | implemented (`emdf.rs`), bit-level container search with length and protection validation |
| 6.2 to 6.3 JOC syntax, annex A.1 Huffman tables | implemented (`joc.rs`) |
| 6.6.2 to 6.6.4 differential decode, dequantisation | implemented, sparse and dense, coarse and fine |
| 6.5 parameter band mapping (table 54) | implemented |
| 6.6.5 temporal interpolation (smooth, steep, 1 or 2 data points) | implemented (`decoder.rs`) |
| 6.6.6 reconstruction, clause 7 QMF (64 bands, QWIN) | implemented, FFT based (`qmf.rs`) |
| 5 OAMD: program assignment, object element, md_update_info, object_info_block, extended_object_element (position precision, divergence skipped over), trim (skipped over) | implemented (`oamd.rs`) |
| 4.2 anchoring (room), 5.2.1.3 screen anchoring, distance factor projection | values parsed and exposed, not applied |
| dmx configs 2 and 4 (5.X + 2 tops), 3 and 4 (90 degree phase shift) | parsed; channel mapping done; the phase shift is not undone (the spec text says nothing about it) |
| Decorrelators | the spec describes none, so none are implemented |

`render.rs` is a small reference point panner for the tests, not an Atmos renderer.

## Validation (which checks are spec-derived, which semantic)

There is no open bit-exact reference decoder. Nothing below is a bit-exact
comparison against Dolby output.

| check | kind | result |
| --- | --- | --- |
| Payload parsing on both real streams | spec-derived | all 2361 + 1518 JOC payloads parse, each consumed to within its byte padding (< 8 bits left), `joc_seq_count` consecutive; all OAMD payloads consume every element exactly with zero padding; JOC objects = OAMD objects minus the LFE object |
| Huffman tables | spec-derived | complete prefix codes (Kraft sum 1), all symbols present |
| QMF perfect reconstruction | spec-derived | delay 577 samples, gain 1.000000, f32 fast path vs float64 direct form of pseudocode 12/14: 4e-7 max. The normative prototype itself is only near-PR: 7e-5 rms and 4e-4 peak on full-scale noise (about -78 dB rms). A 1e-5 bound is not reachable with the specified filter |
| Energy (objects through a downmix reproduce the bed) | semantic | Atmos sample: static least-squares downmix per second, L/R/C residual -78 to -80 dB (objects 0/3/1 with gain 1.00), Ls/Rs about -26 dB, fitted gains are plausible (Ls = side 0.78 + back 0.67 + top-side 0.40). Channel-ID sample (dmx 3, 90 degree surround phase) median -17 to -25 dB |
| Channel-ID: voices in the right objects | semantic | L, R, C, Lb, Rb, Tsl, Tsr voices each appear alone in the matching object (>= 40 dB over all other objects), bed dominant channel as expected (Lb/Rb/Tsl/Tsr fold into Ls/Rs); with point panning to 7.1.4, > 85% of the energy goes to the expected speakers |
| Channel-ID: other 8 objects | not verified | they are only active together in the moving section of the sample, no isolated segment |
| Spoken names | not verified | no speech recognition was run |

## Findings about the spec text

* Pseudocode 14 (synthesis phase) is wrong: `(2*j - 2*n - 1)` does not reconstruct. The printed equation
  `exp(i*pi*(k+1/2)*(j-2n+1/2)/n)`, i.e. `(2*j - 4*n + 1)`, does (`qmf::SYN_D`).
* Array flags such as `obj_render_info[]` are read MSB first with the indices of the syntax listing
  (`[0]` position, `[1]` zone, `[2]` size, `[3]` screen reference). Table 31 lists them the other way
  round and does not parse the real stream.
* With `b_dyn_object_only_program` and `b_lfe_present`, OAMD object 0 is the LFE (in-bed, no render info);
  JOC object k is OAMD object k + 1.
* Pseudocode 2 uses 50 and 100 as the neutral value of non-selected channels in sparse mode, which
  dequantises to +0.4 instead of 0 (the zero point is 48 and 96). Neither sample uses sparse mode, so this
  is unresolved; `SparseOffsetMode::ZeroPoint` is the default.
* Pseudocode 6 uses `sb` where `sb_to_pb(sb)` is meant in the steep branch.

## Speed

15 objects, 5.1 bed, 48 kHz (75.6 s channel-ID sample), best of 5:

| target | E-AC-3 only | E-AC-3 + JOC |
| --- | --- | --- |
| native (aarch64 macOS, release, lto) | 689x | 130x (objects alone 163x) |
| wasm32 simd128 in Node 25 | 521x | 74x (objects alone 87x) |
| wasm32 without simd128 | 468x | 56x |

Native and wasm produce the same checksum (`examples/bench.rs`, `tests/wasm_bench.mjs`).

## Reproduce

```sh
export CARGO_HOME=/tmp/aether-rust-toolchain/cargo RUSTUP_HOME=/tmp/aether-rust-toolchain/rustup CARGO_TARGET_DIR=$PWD/target
cargo test --release
cargo run --release --example channel_id_report -- chid.eac3
cargo run --release --example downmix_fit -- atmos.eac3
RUSTFLAGS="-C target-feature=+simd128" cargo build --release --lib --features standalone-wasm --target wasm32-unknown-unknown
node tests/wasm_bench.mjs target/wasm32-unknown-unknown/release/aether_joc.wasm chid.eac3
```

The real-stream tests read the two fixture paths named in `tests/real_streams.rs`, extract the audio with
`ffmpeg -c copy`, and skip when the media is missing.

## Uncertain

* Sparse mode, two data points, 7-channel downmixes (dmx 1, 2, 4), bed instances in `program_assignment`
  and `bed_channel_assignment` bit order are implemented from the text but exercised by no real stream
  (only a synthetic round trip for sparse).
* Timing of JOC matrices against OAMD updates relative to the QMF delay is assumed (frame N side
  information applies to the QMF slots of frame N PCM); the -79 dB front channel fit supports it for the
  matrices, OAMD `start_sample` is reported on both clocks.
* Behaviour on splices, lost JOC payloads (no objects are produced, matrices held) and `b_joc_obj_present = 0`
  (treated as an all-zero target) is not specified and not tested.
