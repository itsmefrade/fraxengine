# aether-dtsuhd design note

Pure Rust DTS-UHD (ETSI TS 103 491 V1.2.1) parser and ACE audio decoder, written from the
published specification only. Standalone crate (own `[workspace]`), no dependencies, builds for
native and `wasm32-unknown-unknown`.

## Status in one paragraph

The container side is complete and verified on all 6241 frames of the lab sample (FTOC with CRC,
chunk navigation, metadata chunk 0x01, ACE frame and stream set headers, exact byte accounting).
The LFE channel is decoded and plausible (see "Verification"). The nine full bandwidth channels
**cannot be decoded from the published text**: 85 % of the coded ACE stream frames of the lab
sample use a bit allocation model that needs `compute_unmasked_levels()`, a function that
TS 103 491 references (clause 9.9.13.11) but never defines, in V1.2.1 or in V1.1.1. Everything
around that function is implemented; the decoder reports those streams as `Unsupported` and
marks the stream `Desynchronized` until the next sync frame instead of emitting noise.

## Sources

ETSI TS 103 491 V1.2.1 (primary; V1.1.1 only has the lossless XLL tools and was checked for the
missing function), ETSI TS 102 114 V1.6.1 (one table, see below). ETSI publishes no zip with
reference code or tables for either document. See `NOTICE`.

## Stream structure (what a DTS-UHD frame is)

One MP4 sample (`dtsx`) is one DTS-UHD frame = 1024 samples at 48 kHz in the lab sample.

```
FTOC | metadata chunks (0x01 ...) | audio chunks (id 0x01 ...) 
```

* **FTOC** (clause 6.4): sync word `0x40411BF2` (sync frame, random access point) or
  `0x71C442E8` (non-sync, predictive). Then a prefix coded FTOC size, the full-channel-mix flag,
  version, base duration (512/480/384) x multiplier, clock rate (32000/44100/48000) x 2^n,
  optional 36 bit time code, audio presentation table (selectable flag, dependency masks,
  explicit object list masks), metadata chunk sizes (+CRC flag), audio chunk (index, id, size)
  list, peak-bit-rate smoothing parameters, reserved bits and a CRC-16 (poly 0x1021, init 0xFFFF,
  over the FTOC without the CRC). Non-sync frames of full-channel mixes carry no CRC.
* **Metadata chunk 0x01** (clause 7): presentation index, object id list, presentation scaling,
  static loudness and dynamics, then per object: representation type, importance, audio chunk
  index, channel layout index/mask, rendering exception, gain per 512-tick metadata sample point.
  For `bFullChannelBasedMixFlag = 1` (Profile 2) there is one object, id 256, and the chunk exists
  only in sync frames. Object groups, 3D sources, Ambisonics and rendering exception matrices are
  recognised and reported as `objects_complete = false`; they are not decoded.
* **Audio chunk 0x01** = one ACE frame with its own header (clause 9.3-9.5): sync flag,
  de-emphasis flag, duration (1024), sample rate, stream set(s), stream composition code,
  bandwidth modes, payload sizes, then the stream payloads in the order LFE, mono, stereo, then
  padding. Chunk types 0x02 to 0x04 (ACE channel sets, multiple ACE frames per chunk) and the XLL
  chunks 0x40 to 0x42 are not implemented.
* **Coding tools**: ACE is an MDCT codec. Per stream: coding mode (full/off/left/right), long term
  synthesis (lag + 3 tap IIR), 1024-line long or 8 x 128 short MDCT (selectable per band by Haar
  time-frequency reshaping), 22 bands (17 to 22 coded by bandwidth mode), band energies as
  Golomb coded "lognorms" predicted from the previous frame and band, bit allocation models,
  pyramid vector quantisation of the band shape with recursive splitting (bit split coded by a
  quantised log energy ratio), mid/side or left/right stereo per band, spectral/temporal hole
  filling (informative), late lognorm refinement with leftover bits, inverse DCT-IV + TDAC windows
  (Vorbis slopes), long term synthesis and a one-pole de-emphasis. The LFE is a separate
  64x decimated LPC/PCM codec with a 1024 tap interpolation filter. Entropy coding is Golomb,
  unary, uniform and the prefix codes of clause 8.2; there are no Huffman tables.

### Lab sample ("DTS-UHD DTSX P2 Sample.mp4")

Sample entry `dtsx`/`udts`, 6241 samples of 1024 at 48 kHz (133.1 s), 67 sync frames (about every
2 s), 378 kb/s. Full-channel-mix, version 2.0, one selectable presentation, one metadata chunk
(7 bytes, sync frames only) and one audio chunk of id 0x01 per frame. Layout index 14 with the
explicit 16 bit mask `0x802F` = C, L R, Ls Rs, LFE1, Lh Rh, Lhr Rhr = **5.1.4** (10 waveforms),
long term loudness -19 dB (table index 44, measurement type unknown). ACE stream set: 1 LFE + 1 mono + 4 stereo streams
("9p1" composition), bandwidth mode 2 (21 bands), de-emphasis on.
Roughly the first 1200 frames are digital silence.

The mapping of ACE streams to channels is not specified in the text. The decoder assumes mask
order: mono -> C, stereo 0..3 -> (L,R), (Ls,Rs), (Lh,Rh), (Lhr,Rhr), LFE -> LFE1. This could not
be checked because the full band channels cannot be decoded.

## Verification: what is verified, what is plausible

Verified (checked on all 6241 frames unless noted):

* FTOC CRC-16 of all 67 sync frames; chunk sizes: FTOC + metadata + audio chunk == sample size
  for every frame; metadata chunk bit accounting (53 of 56 bits used, rest padding).
* ACE header + LFE/mono/stereo payload sizes + padding == audio chunk size for every frame. The
  padding length code (`ReadVLC {7,31}`) sits **after** the stream payloads and counts its own
  bytes; for all 3815 padded frames it equals the remaining chunk bytes (tests/parse.rs fixtures,
  tools via `examples/info`).
* DCT-IV against its definition, orthogonality of the lapped transform (energy and inner
  products over long/short/mixed block patterns), perfect reconstruction by transposition,
  alias-free basis functions (99 % of the energy within +-2.5 bins) for long and short blocks.
* Pyramid vector enumeration is a bijection; Haar transform is orthonormal and involutory.
* Fuzz style mutation tests (bit flips, truncation, random bytes, random ACE payloads) never panic
  and never produce non-finite PCM (`cargo test`, debug build with overflow checks).
* LFE: decoded output of the lab sample is confined to low frequencies (99 % of the energy
  below 100 Hz, below -59 dB above 300 Hz relative to the total), peaks at -2.4 dBFS, 2571 active frames. The
  reflection coefficient table equals TS 102 114 clause 8.5.4 bit for bit (checked by script).
  The prediction coefficient conversion was chosen by measurement, see errata.

Plausible but not verifiable without a reference decoder or the missing function:

* Parsing of allocation models 1, 4 and 5 and the VQ/band path (bit allocation, K estimation,
  refinement budget). Only 28 of 186 coded sync-frame streams use these models, and the bit
  stream always ends "exactly" because late refinement soaks up leftover bits, so exhaustion of
  the payload is **not** evidence of correct parsing. Lognorm profiles of decoded sync frames are
  smooth and left/right similar (supports the LTS flag bit and the lognorm prediction), nothing
  more.
* Scale of the ACE output (assumed 16 bit units, `ACE_PCM_SCALE`), stream-to-channel mapping,
  the sign convention of mid/side, de-emphasis/LTS history interplay.
* LFE output gain (see errata, x64).

## Missing normative data and specification errata

Missing (blocking):

1. `compute_unmasked_levels()` (9.9.13.11). Needed by allocation models 0, 2 and 3
   (`kUnsignedDeltaFromPrimaryLSQ`, `...ModifiedDelta...`, `kSignedDeltaFromPrimaryLSQ`). Not
   defined in TS 103 491 V1.1.1 or V1.2.1. In the lab sample's sync frames: 153 + 3 + 2 of 186
   coded streams (84.9 %). Without the bit allocation the VQ payloads cannot be parsed, and
   every later predictive frame inherits the allocation, so the stream stays lost until the
   next sync frame. `Frame::compute_unmasked_levels` in `src/ace.rs` is the single place to fill
   in; adjustment scaling, cost estimate, the allocation search and the delta decoding after it
   are already implemented. No attempt to infer it from the bitstream succeeded: the allocations
   of the decodable sync frames are not a simple function of band level, and payload
   exhaustion cannot serve as an oracle (see above).
2. The ACE `num_stream_sets` and `streamset_id` codes. The text says `ReadLimitsVLC()`; the lab
   sample carries a single `1` bit for each (one stream set, id 0). Other values (more than one
   stream set) are rejected as `Unsupported`.

Corrections applied (each is a deviation from the printed text, with the evidence):

* ACE frame header: the sync flag is followed directly by the de-emphasis code (no reserved
  boolean as the prose says); the order observed is sync, de-emphasis, duration, rate, number of
  stream sets, has_padding, stream set id.
* Padding length: after the payloads, not after the header (above).
* Metadata chunk of a full-channel mix: the four presentation scale flags (clause 7.5.1) are
  present even though 7.5.2.1 says they are not transmitted; a bit-exact walk of the 7 byte chunk
  (loudness code 44, 3 DRC pairs, layout index 14, mask 0x802F, no rendering exception, gain
  presence bit 0) only works with them.
* Per stream: a long term synthesis enable flag bit follows the coding mode (the printed
  `read_long_term_synthesis` tests `lts_enabled` without reading it). Without the bit the
  lognorms of sync frames are erratic (values up to 40), with it they are smooth and similar
  between channels.
* LFE predictor: `PREDICTOR_QUANTIZATION_COEFF` is not defined; TS 102 114's 128 entry
  reflection coefficient table (identical index layout) is used. The printed conversion
  `-p0 / (1 + p1)` is wrong; `-p0 * (1 + p1)` is the standard lattice to direct form and is the
  one that is self consistent: the order 2 LPC of the decoded LFE signal matches the decoder's
  own predictor with a median error of 0.027 (0.51 for the printed quotient, 0.22 without
  conversion). The interpolation table sums to 1.0 (DC gain 1/64 per phase); a gain of 64 is
  applied so the 64x interpolator keeps the decimated amplitude (peak -2.4 dBFS instead of
  -38 dBFS). The text does not mention this.
* Inverse MDCT: the printed DCT-IV kernel reverses the input and negates the sum, which mirrors
  the spectrum (coefficient 0 would be Nyquist); the plain DCT-IV is used. The printed window
  butterfly (`v0 = w[N-n-1] a - w[n] b`) gives basis functions that are only 81 % frequency
  concentrated, its transpose gives 99.9 %, which is what is implemented.
* `read_alloc_diff_steps`: `delta_min` must be negative (`-ceil(ref/step)`) for the printed
  `DecodeCenterMap(alpha, -d_min, ...)` to make sense. The refinement subtraction is
  `refinement * channels` bits (the printed text subtracts `refinement`). The
  unallocated bit count uses the bits left after the allocation side info.
* Beta dequantisation table: the printed `BQFP(int_beta) / q - 0.5` cannot be right (centre
  entry 0 would give -0.5); the raw table value is taken as Q11. `table[q/2 - index]` for
  index > q/2 is `table[q - index]`. Quantisation level 1 (no table row) only has the two extreme
  indices.
* Obvious transcription errors in the pseudo code (`ReadUint` for lag where `ReadUniform` is
  meant, loop bounds `< last_effective_channel`, undefined `bits`, `size`, `d`, swapped
  `ReadGolomb` arguments) are read as their evident intent. Unverified switches are collected in
  `ace::Quirks`.

## Performance

Native (Apple M-series, release): synthetic 5.1.4 full-load workload (1 mono + 4 stereo ACE
streams per frame, random accepted payloads, includes rejected retries) 36x real time; whole lab
sample (parse + LFE + whatever decodes) 447x real time. wasm32 in Node 25: 30x and 303x. See
`examples/bench.rs`, `tools/wasm_bench.mjs`. The DCT-IV is an N/2 point FFT (O(N log N)); the
allocation and pyramid code is integer.

## Layout of the source

| file | content |
| --- | --- |
| `bitreader.rs` | MSB-first reader, ACE primitive codes (unary, uniform, Golomb, VLC), never panics |
| `ftoc.rs` | FTOC, chunk navigation, CRC-16 |
| `metadata.rs`, `layout.rs` | metadata chunk 0x01, channel masks and labels |
| `aceframe.rs` | ACE frame/stream set headers |
| `ace.rs` | mono/stereo stream decoder, bit allocation, band reconstruction, state across frames |
| `vq.rs`, `transform.rs` | pyramid VQ, reconditioning, DCT-IV, TDAC, Haar reshaping |
| `lfe.rs` | LFE channel |
| `decoder.rs` | `Decoder::parse_frame` / `decode_frame`, `StreamInfo`, channel status |
| `fixed.rs`, `config.rs`, `tables_gen.rs` | fixed point types, band tables, generated tables |
| `bench.rs`, `wasm.rs` | speed workload, C ABI for wasm32 |

Reproduce: `tools/extract_packets.py file.mp4 out.pkts` (or let the examples read the MP4
directly), `cargo run --release --example info -- file.mp4`, `... --example render -- file.mp4
out.wav LFE1`, `cargo test`, `tools/gen_tables.py` regenerates `tables_gen.rs` from the PDF text
(`tools/pdf2txt.swift` extracts it on macOS).
