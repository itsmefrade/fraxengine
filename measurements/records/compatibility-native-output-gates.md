# Native output in Chrome on macOS

| Field | Value |
| --- | --- |
| Date | 2026-11-05 to 2026-11-06 |
| Group | Compatibility |
| Type | `Compatibility` |
| Section | Native output in Chrome (audio pass-through, Dolby Vision) |
| Environment | macOS, machine model Unknown, macOS version Unknown. Built-in MacBook Pro speakers (default output 2 channels, 44.1 kHz) and built-in XDR display only. No external display or AV receiver listed by the OS. Chrome 156 for the 2026-11-05 file and MSE tests, with Chromium source pinned to tag 156.0.8078.80 (source reading only, installed binary not measured). Chrome 157 headless for the 2026-11-06 file test. Chrome version for the Dolby Vision file test Unknown. Engine commit Unknown. |
| Status | `Current` |

## Purpose

Which audio and video formats does Chrome on macOS pass through to the system natively (Dolby Atmos, E-AC-3, TrueHD,
multichannel, DTS, Dolby Vision)? And why does the engine decode them in WebAssembly instead?

## Implementation

| Field | Value |
| --- | --- |
| Test asset | Dolby Atmos files: E-AC-3 JOC (`ec-3`) and TrueHD (`mlpa`). AAC file as the positive control, through the Blob file path and the MSE path. Matroska samples: TrueHD + Atmos, DTS-HD MA + DTS:X, a 12-channel TrueHD file, and the original 4K MKV with its audio. A Dolby Vision `dvh1` file (profile 8.1). An official Dolby sample with sound. Fixtures were made offline by FFmpeg stream-copy muxing, with no runtime transcoding. Container, duration and bitrate Unknown. |
| Baseline | Chrome's own answers for the same codecs: `MediaSource.isTypeSupported`, `addSourceBuffer`, MediaCapabilities (file and media-source queries), WebCodecs `AudioDecoder` config. The AAC file is the positive control. For channels, Web Audio `maxChannelCount`. |
| Methodology | Real Chrome runs with silent playback, measuring media clock progress. Checks of the codec strings, the MSE and file load results, and the demux and decoder stage. A Chromium source reading of the Mac audio and video paths. A read-only macOS device inventory. A W3C registry check. The result file for the Atmos gate is named chrome-atmos-mse-gate-20261105.json. Repeat count Unknown. |
| Observations | AAC played in both the file and MSE paths. In both, the clock advanced about 0.25 s. Matroska rejections happened at demux, before any decoder was chosen. The original MKV played its 4K video through VideoToolbox while its TrueHD and DTS:X tracks reported zero decoded audio bytes. The same HEVC packets muxed with AAC decoded both tracks. |
| Margin of error | Unknown. |

## Results

### Audio pass-through in Chrome

| Path | Codec | Result | Chrome |
| --- | --- | --- | --- |
| File (Blob) | AAC | Plays. Clock advances about 0.25 s. | 156 |
| MSE | AAC | Plays. Clock advances about 0.25 s. | 156 |
| File and MSE | E-AC-3 JOC (`ec-3`) | Rejected. `isTypeSupported` false, `addSourceBuffer` throws `NotSupportedError`, `DEMUXER_ERROR_NO_SUPPORTED_STREAMS` at `file-loadeddata`. MediaCapabilities `supported: false`. | 156 |
| File and MSE | TrueHD (`mlpa`) | Rejected in the same way as E-AC-3. | 156 |
| WebCodecs `AudioDecoder` | E-AC-3 (`ec-3`) | Config unsupported. | Unknown |
| Native media path | Official sound Dolby sample | Rejected. | Unknown |
| Matroska file | TrueHD + Atmos, DTS-HD MA + DTS:X | Rejected at demux. Zero decoded audio bytes. | 157 |
| Matroska file | AAC (same HEVC packets) | Both tracks decode. | 157 |

Neither result shows that the codec can never play. They show what this Chrome build did on this machine.
The 156 and 157 results are headless or silent runs. Full sample playback was not checked.

### Channels and output route

| Measure | Value |
| --- | --- |
| Web Audio default output rate | 44.1 kHz |
| `maxChannelCount` | 2 |
| 10 and 12 channel assignments | Rejected by the `[1, 2]` limit |
| 12-channel TrueHD to PCM in Rust/WASM, short silent Chrome runs | Passed |
| Full 63.5 s source, 5.1.4 and 7.1.4 PCM regression (software render) | Passed |

An offline 12-channel render does not prove a 12-channel physical output. The OS device inventory showed the built-in display
and 2-channel built-in speakers. No external display or AV receiver was listed.

### Chromium source reading (156.0.8078.80)

- The AC3/EAC3 build flag is off in the default desktop Mac build.
- The DTS flag is off. The DTS passthrough factory is conditional on Windows.
- The Mac audio path studied has no TrueHD or Apple APAC decoder. FFmpeg's `apac` name is a different codec.

### Dolby Vision

| Check | Result |
| --- | --- |
| `dvh1` file, file path (2026-11-06 update) | Played 18 frames. Shows file admission only. |
| Same codec string, MSE `addSourceBuffer` | `NotSupportedError` |
| Injected RPU, earlier Chrome test | RPU metadata did not come back. |
| Native DV flag default on Mac | Does not cover Mac. |
| Mac VideoToolbox HEVC path | No RPU delivery adapter found. |
| HEVC with RPU or dvvC in the same source | Does not make the HEVC/PQ fallback native DV. The MSE parser can explicitly select cross-compatible HEVC. |
| Browser re-encode, Main10 | Main10 config unsupported. The 8-bit HEVC positive control did produce an encode. |
| W3C VideoFrame metadata registry (checked 2026-11-05) | Lists no Dolby RPU member. |
| Physical display Dolby Vision | Not measured. |

### Which source is newer

The sources agree on the codec results. Where they differ, the newer one is used:

- Dolby Vision: the 2026-11-05 gate table says native DV is still open. The 2026-11-06 update (newer) adds that the file path
  accepted the `dvh1` file (18 frames), while MSE rejected the same codec string. The 2026-11-06 result is used.
- Chrome version: 156 for the 2026-11-05 tests, 157 for the 2026-11-06 headless file test.

### Why the engine decodes in WebAssembly

- Rust/WASM can compute the image and the audio. Format decoding, FEL reconstruction and PCM rendering are separate code
  paths that run in the browser. The full FEL decode matched the gold reference. Two small PQ and encode regions and a full
  TrueHD PCM transport passed.
- The engine owns its own decoder, renderer, queue and clock. No web API path was found that changes Chrome's build flags
  or delivers OS metadata. So native pass-through is not used.
- P7 FEL computation: a paired check (its label is garbled in the source) and 12 independent full-image checks passed.
  Real-time running, a wider corpus and full display management are still open.
- These results do not replace native system Dolby output. A helper app, a custom Chromium or a silent quality reduction
  was not part of the target.

## Limitations

- Chrome 156 and 157 only. Re-verify on newer Chrome. A later Chrome may change the audio and Dolby Vision results.
- Silent runs only. No sample was played audibly, and full sample playback was not verified.
- One machine. Built-in speakers and built-in display only. No external display or AV receiver, so object output,
  physical Atmos height channels and physical Dolby Vision were not measured.
- The Chromium result is a source reading for tag 156.0.8078.80. The installed binary's build arguments and its actual decoder
  choice were not measured.
- These checks do not prove that a codec is impossible on every Chrome version or platform. The Windows DTS path was not tested.
- The Dolby Vision file test shows file admission only. RPU handling and the effect on the display need separate checks.
- The metadata registry check is a point-in-time check (2026-11-05).
- The 10 and 12 channel rejection was seen only through the Web Audio channel assignment, with the limit at 2.

## Fixes

None. These are measurements. They changed no code. The decision to decode in WebAssembly follows from these results.
