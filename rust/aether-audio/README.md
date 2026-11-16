# aether-audio

Compressed TrueHD decoding in Rust, plus the speaker and binaural renderers. Runtime media stays in the browser. No C codec, native application,
localhost decoder or media-transcoding server is used.

The decoder dependency is the same exact source pin used by upstream LibTrueHD:

- `truehd` 0.7.2, `superuser404notfound/truehdd@33bae1657ceb25b5fe0347f42c544c3414e2760e`.
- Apache-2.0 dependency; this adapter follows the repository's LGPL-3.0 license.
- Requires Rust 1.89 or later.
- `matrix-interpolation` is enabled for the exact decoder path. This differs from
  upstream AetherEngine's current approximate object-statistics configuration.
- `perf` and `evo-protection` are disabled. No decoder profiling clock is required
  in WASM, and no evolution-protection authentication claim is made.

## Build and verify

```sh
cargo test --manifest-path rust/Cargo.toml -p aether-audio
```

The crate is linked into `aether_wasm.wasm` by `scripts/build.sh`.

The WASM build reserves an 8 MiB stack for fixed decoder DSP structures and caps
linear memory at 256 MiB. It is not a measured production memory budget.

## Browser API

```js
import { loadAetherAudio } from './wasm/aether-audio.mjs';
const audio = await loadAetherAudio('./wasm/aether_audio.wasm');
const result = audio.decode(elementaryTrueHdBytes, {
  chunkSize: 4096,
  presentation: 3,
  // Optional: renderLayout: "7.1.4",
});
```

Instantiate and execute in a dedicated Worker. The synchronous `decode` convenience
method blocks its calling thread. A WASM trap or timeout requires abandoning the
instance and creating a new one; Rust panic recovery is not available with the
current WASM panic-abort build. The browser harness should enforce a worker deadline.

`result` includes:

- `pcm`: interleaved Int32Array, signed 24-bit samples in i32, without left alignment.
- `normalizationDivisor: 8388608`, for conversion to Web Audio float PCM.
- `sampleRate`, `channels`, `channelLabels`, `frames`, `accessUnits`.
- `metadataPayloadCount`, `maxObjects` and `metadata` parsed from OAMD where present.
- `matrixInterpolation:true`, `nativeAtmosOutputVerified:false`.
- `elementRoles` identifies the first known OAMD assignment, for example one
  `bed:LFE` followed by fifteen `object:N` elements. `sourceElementChannels`
  retains the decoder element count when a rendered bed has a different count.

For the synthetic stereo control the order is `L,R`. For the 5.1 control it is
`L,R,C,LFE,Ls,Rs`, matching FFmpeg's reference order `FL,FR,FC,LFE,SL,SR`.
The adapter retains each stream's labels; it does not presume every TrueHD stream
uses this order. Sixteen-bit source values are promoted by a left shift of eight
bits into the signed-24-bit PCM domain; dividing by 8388608 yields the original
signed-16-bit sample divided by 32768.

`presentation` selects the dependency's indexed presentation 0..3; unavailable
ones resolve to the highest independent presentation by the source's presentation
map. Presentation 3 may contain object-element PCM on an Atmos source, not an
already rendered 16-speaker bed. Use the separate optional renderer described below to produce a chosen speaker bed.

For streaming, call `audio.reset(presentation)`, bounded `audio.push(bytes)`, then
`audio.pull()` until null after each push. Each pull returns one decoded access
unit with PCM, labels and metadata. `pendingBytes()` distinguishes a partial
trailing access unit from an empty input queue. The one-shot convenience refuses
mid-stream rate/layout changes and any nonempty tail at end of input.

## Bounds and errors

- Each push: at most 64 KiB.
- Unconsumed input: at most 1 MiB.
- TrueHD access units emitted by the checked extractor: 8..8190 bytes.
- Each decoded block: at most 160 frames and 16 channels.
- One-shot input: at most 16 MiB; output at most 8,388,608 PCM samples.
- A framing, parsing or decode error poisons the decoder. A fresh reset is required.
- Initial garbage can be skipped while the extractor searches for a validated
  major sync. This prototype does not expose a complete skipped-byte recovery report.

Those limits bound adapter storage. They do not prove the third-party parser is
safe against every malicious, CRC-valid stream. It has not undergone a security
audit or exhaustive fuzzing here. A Worker can terminate a hung/trapped decoder;
that is an isolation boundary, not validation of a corrupted file.

## Metadata scope

The dependency parses OAMD payloads. Each one-shot metadata entry has an absolute
`outputFrameOffset` string, `blockFrames`, and source `presentation`. Each streaming
pull has `outputFrameOffset: BigInt`. Payload summaries include `programAssignment`
(bed indices/counts, ISF/dynamic counts, distribution flag), explicit `elementRoles`,
version, declared object count, evolution sample offset and bounded updates including
activity, gain, position, size, elevation/screen/snap flags and timing fields.
It keeps up to eight payloads and 256 object-update records per payload per block;
per-payload `truncated` marks an update cap. Compare the returned payload list length
with `metadataPayloadCount` to detect a payload-list cap. Timing and gain fields
retain the parser's semantics and feed the optional sample-accurate point-source renderer below; unsupported spatial features remain explicitly excluded.

The genuine user-provided local Amaze clip has now exercised this metadata path,
with the findings below. An object count, parsed metadata or correct PCM does not establish native Atmos,
height-speaker output, Dolby MAT, bitstream passthrough or decoder certification.

## Reproducible controls

`tests/data/` contains short synthetic TrueHD streams and raw reference PCM,
produced with FFmpeg 9.0.2 (the generator script is not part of this repository).
The generator creates deterministic integer waveforms, channel-ID impulses and
silence; these assets contain no third-party programme and no Atmos objects.
Both controls decode to 25,600 frames at 48 kHz. Native Rust and actual WASM output
are compared sample by sample with the independent FFmpeg/reference PCM. Native
stereo decoding is also tested with 1-, 7-, 4096- and 65536-byte input chunks.
These short tests establish channel-bed correctness for the controls, not full
format conformance, real-time long-film playback or actual browser speaker output.


## Genuine local Atmos evidence

The user-provided first-five-second Amaze elementary clip is read only at runtime;
no compressed programme audio or PCM is embedded in the repository. Its SHA-256 is
`c235096029ac827c598fe0c3bd0abbfd9e299ef36977d233b48809e0a15b49d9`.

- 1,684,964 input bytes, 6000 access units, 240,000 output frames, 48 kHz.
- Presentation 3 yields sixteen **element** signals: one LFE bed element and
  fifteen dynamic objects. Raw decoder `channelLabels` is only `["LFE"]`; it is
  not a sixteen-speaker channel layout.
- 157 parsed OAMD payloads, 2512 per-element update records. First update begins
  at output frame zero; last payload begins at frame 238,600.
- Native and actual WASM exact-interpolation element PCM match byte for byte:
  PCM24 SHA-256 `6926e794845b403f78e21283bc1768e775e763beeb3a8ca34c0fa90cd2153d4a`.
  This is architecture parity of the same decoder, not an independent Dolby oracle.
- Presentation 2's lossless eight-channel bed independently matches FFmpeg PCM24
  after explicit remapping from `L,R,C,LFE,Ls,Rs,Lb,Rb` to FFmpeg's
  `FL,FR,FC,LFE,BL,BR,SL,SR` using `[0,1,2,3,6,7,4,5]`. All 1,920,000 samples match.
- Only a 67-byte OAMD metadata snippet is retained in `tests/data/` for regression
  tests. It contains no audio/video frames or PCM; provenance is documented there.

Run local programme checks explicitly:

```sh
AETHER_LOCAL_ATMOS=/path/to/five-second-amaze.thd \
AETHER_LOCAL_ATMOS_BED_PCM24=/path/to/ffmpeg-bed.s24le \
cargo test --manifest-path rust/Cargo.toml -p aether-audio --test local_atmos -- --ignored
```

`examples/inspect-local.rs` and `examples/render-local.rs` produce local
measurements from supplied paths. No genuine programme fixture is required by
ordinary default tests.

## Optional point-source renderer

`src/render.rs` ports upstream `AllocentricPanner`, fixed bed/LFE routing and
`ObjectAudioRenderer`'s per-sample linear gain ramps. It mixes the exact-interpolation
object-element PCM directly. There is **no bed regression**.

```js
const bed = audio.decode(bytes, {
  presentation: 3,
  renderLayout: '7.1.4', // or '5.1.4'
});
```

The rendered `pcm` is a Float32Array with `normalizationDivisor:1`,
`sampleFormat:'f32-rendered'`, and ten or twelve explicit speaker labels.
`sourceElementChannels`, source `elementRoles`, metadata and source presentation
remain available separately. Streaming setup uses `audio.reset(3, '7.1.4')`.
`appliedMetadataUpdates` is cumulative since reset. `rendering` explicitly records
`bedRegression:false`, `dolbyReferenceEquivalent:false` and `limiter:false`.

Metadata scheduling follows the upstream sample axis:
`outputFrameOffset + evoSampleOffset + elementSampleOffset + 32 * blockOffsetFactor`.
The first state is a jump; later changes ramp across exactly the declared number
of samples, continuing across access-unit boundaries. Repeated unchanged state
never restarts an in-flight ramp. LFE stays discrete. Dynamic point gains balance
between speaker planes, rows and columns at constant power; ceiling speaker
positions use the upstream quarter-room targets.

Renderer limits and scope:

- At most sixteen input elements and sixty-four pending timed updates.
- ISF and bed-channel-distribution programmes are refused.
- Initial object metadata is required at the initial block; startup holding and
  arbitrary seek recovery before the first metadata are not implemented.
- Timeline discontinuity requires a fresh renderer/reset. No concealment is implied.
- Extended precision positions are included; negative height is clamped to the
  home speaker plane. Active dynamic metadata requesting size/extent, snap,
  nonzero horizontal zones, elevation exclusion, screen anchoring, distance or
  nonzero divergence is refused with `UnsupportedSpatialFeature`. Trim payloads
  are also refused. These controls cannot silently degrade to point rendering.
  Admission is checked across the whole block before updates are scheduled.
  New WASM statuses208..217 respectively identify extent, snap, horizontal zone,
  elevation exclusion, screen reference, distance, divergence, trims, bed metadata
  and alternate object data. Existing
  statuses200..207 retain their meanings. The JS source adapter exposes the new
  reason as `error.unsupportedSpatialFeature` and the code as `error.status`.
  A streaming caller must reset after a render error: that compressed block has
  already been consumed by the decoder, and resuming would skip its timeline.
  Bed metadata and alternative object-data selection are refused even without an
  object element. Recorded bed element IDs also refuse rendering if the parser
  skipped the bed body. Repeated object-element IDs are invalid metadata because
  the parser retains only the final object state; earlier controls cannot be checked.
  Extended object ID5 may occur only once and after ID1; duplicate or premature
  extensions return invalid metadata202, preventing overwritten or unparsed
  divergence/precision controls from bypassing admission. Supported XYZ precision
  offsets remain added before position clamping and panning.
- No peak limiter is included. Float values may exceed full scale on other mixes;
  any physical-output integration must make its gain/limiter policy explicit.
- Genuine Amaze 7.1.4 rendering produced all 240,000 frames and applied all 157
  updates. All four height outputs had nonzero measured energy. Native/WASM
  rendered samples matched exactly for the measured clip, but an independent
  Dolby reference render and physical height-speaker output remain unmeasured.
- Unit tests cover panning power, discrete speaker/LFE routes, exact sample timing,
  ramp completion, unchanged metadata, bounded configuration and the real snippet.

Node timings are development measurements; they are not Chrome real-time or
whole-film benchmarks. Correct object decode and a point-source bed are progress
towards the goal, not proof of native Dolby Atmos output or renderer certification.

### Short object-mixing comparison

On macOS arm64 with Node v25.5.0, the same PCM-only adapter decoded and rendered the first 64 KiB of the local Amaze stream to 15,680 frames of 7.1.4 PCM. Release WASM from `3b4e3eb4` took 7.36 ms per run at the median (7.27–7.48 ms interquartile range). The per-sample ramp mix in `9f805846` took 6.46 ms (6.39–6.55 ms), about 12% less time. Each build had 12 warm-up runs and 32 measured runs, alternating execution order. The measurement includes reset, decode, render and PCM copying. Both builds produced PCM SHA-256 `f218e2b27e7063d12c11d06f93ab9816870eff32a136a3c3919ad1a5af3ef68f`. The measurement covers this input in Node WASM.
