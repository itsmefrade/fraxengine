# Changelog

All notable changes to FraxEngine are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses semantic versioning.

## [Unreleased]

## [0.1.0]

First public release. The development history before this release is not published; the state it reached:

- **Playback in Chrome on macOS** of Matroska, MP4 and MPEG-TS with H.264, HEVC, AV1, VP9 and VC-1, through a
  `<video>` fed by Media Source where Chrome can decode the stream natively, otherwise through WebCodecs and a WebGPU
  canvas. The route is decided once per session.
- **Dolby Vision** Profile 5, 8.x and 7 (MEL and FEL), composed on the GPU from the RPU. Both layers decode on the
  hardware decoder by default (about 60 dB from the bit-exact result); `high` and `reference` decode one or both layers
  bit exact in WebAssembly.
- **HDR10, HDR10+ and HLG** on an HDR canvas, with BT.2390 tone mapping and an experimental ST 2094-10 style display
  mapping.
- **Audio** decoders in WebAssembly for AC-3, E-AC-3 with JOC, TrueHD with Atmos objects, DTS core, DTS-HD MA / HRA,
  DTS:X and LPCM, rendered to the speaker layout or binaural through an AudioWorklet.
- **Subtitles**: PGS, ASS / SSA, SRT and WebVTT.
- **Remote media** read as one long Range request into a 64 MiB ring, with seeks close to a plain `<video>`.
- **Safari** on macOS plays most sources; on WebKit the compositor runs on the main thread. On the iOS simulator 16 of
  20 test files play; nothing was measured on a physical iPhone.
- A reference player page, a small Jellyfin client and a static-site build with a licence inventory and the LGPL source
  archive.
