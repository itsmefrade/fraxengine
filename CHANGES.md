# Changes from AetherEngine

FraxEngine started as a port of [AetherEngine](https://github.com/superuser404notfound/AetherEngine) to the browser.
AetherEngine is a Swift package for Apple platforms; FraxEngine runs in a web page. The goal is the same (play
almost any file, keep HDR, Dolby Vision and object audio intact), but almost every layer had to be replaced, because a
browser offers none of the native APIs AetherEngine is built on.

This file lists the differences. Day-to-day changes to FraxEngine itself are in [CHANGELOG.md](CHANGELOG.md).

## Platform and language

| | AetherEngine | FraxEngine |
| --- | --- | --- |
| Runs in | an app (iOS, tvOS, macOS, visionOS) | a web page; Chrome on macOS is the main target, Safari partly |
| Language | Swift, with FFmpeg and dav1d binaries | Rust compiled to WebAssembly, plus plain JavaScript ES modules and WGSL shaders |
| Packaging | Swift package | static files; the page must be cross-origin isolated (COOP / COEP) |
| UI | none, the host app draws it | a reference player page (`web/player`) and a small Jellyfin client (`web/app`); the engine itself can be embedded without them |

## Pipeline

| Stage | AetherEngine | FraxEngine |
| --- | --- | --- |
| Demux | FFmpeg | own Rust demuxer for Matroska, MP4 and MPEG-TS (`rust/aether-demux`) |
| Video decode | VideoToolbox, dav1d, libavcodec | the browser's decoders: WebCodecs, or a `<video>` element fed through Media Source; VC-1 in WebAssembly |
| Display | AVPlayer or `AVSampleBufferDisplayLayer` | `<video>` (Media Source route) or a WebGPU canvas (WebCodecs route) |
| Dolby Vision | Apple's native handling; Profile 7 FEL through a software path | browsers do not support Dolby Vision, so the RPU is parsed in WebAssembly and applied in WebGPU shaders (reshaping, NLQ, display mapping); Profile 7 FEL has an optional bit-exact mode with an HEVC decoder in WebAssembly (`rust/aether-hevc`) |
| Audio | stream copy to AVPlayer, or a FLAC / APAC bridge | decoders in WebAssembly (TrueHD, E-AC-3 with JOC, AC-3, DTS core / HD / X, LPCM) into an AudioWorklet, with speaker or binaural output; WebCodecs for AAC, Opus and FLAC; pass-through in the element where the browser supports the codec |
| Subtitles | emitted as cues, the host draws them | rendered by the engine (`web/subs`: PGS, ASS / SSA, SRT, WebVTT) |
| Network | HTTP range reading and a loopback HLS server for AVPlayer | HTTP range reading in a worker into a shared ring buffer; no local server |

## What is not carried over

- Live TV, DVR timeshift and the audio-only Now-Playing path.
- The `aetherctl` command-line harness and the Swift test suite.
- Git history. This repository starts with a fresh history; the development history before the first public commit is
  not published.

## Unknown

Which individual modules follow AetherEngine's code closely and which were written from the specifications is not
recorded per file. The licence (`LICENSE`) is kept unchanged from AetherEngine.
