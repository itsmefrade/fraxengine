<h1 align="center">FraxEngine</h1>

<p align="center">
  <b>A media engine for the browser that plays Dolby Vision, HDR and lossless object audio in stock Chrome, with no plugin and no server-side transcoding.</b>
</p>

<p align="center">
  <a href="https://github.com/itsmefrade/fraxengine/actions/workflows/ci.yml"><img alt="build" src="https://github.com/itsmefrade/fraxengine/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/itsmefrade/fraxengine/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/itsmefrade/fraxengine?label=release&color=blue&include_prereleases"></a>
  <img alt="license" src="https://img.shields.io/badge/license-LGPL--3.0%20%2B%20App%20Store%20exception-lightgrey">
  <img alt="platform" src="https://img.shields.io/badge/platform-Chrome%20on%20macOS-4285F4?logo=googlechrome&logoColor=white">
  <img alt="rust" src="https://img.shields.io/badge/Rust-WebAssembly-B7410E?logo=rust&logoColor=white">
</p>

---

## Origin

FraxEngine is a browser port of [AetherEngine](https://github.com/superuser404notfound/AetherEngine) by Vincent Herbst,
a media player engine for Apple platforms. The goal is the same; the stack is not: Rust compiled to WebAssembly,
WebCodecs, Media Source and WebGPU instead of FFmpeg, VideoToolbox and AVPlayer. What came from where is in
[NOTICE.md](NOTICE.md); what is different is in [CHANGES.md](CHANGES.md).

Status: research-grade, version 0.1.0.

**Broad objective.** Deliver high-end formats such as Dolby Vision, HDR and DTS in Chrome and Safari, as far as the
hardware of macOS, iOS, Windows and other platforms supports them. Playback should be smooth, without lossy
translation, and use as little CPU and RAM as possible. Today the engine is measured on Chrome for macOS only; the
other browsers and platforms are the target, not the current state.

## Measurements

Every number below has a record in [measurements/](measurements/README.md), written to one
[methodology](measurements/METHODOLOGY.md). MacBook Pro M4 Pro, macOS 27.2, Chrome 157, FraxEngine `v0.1.0`. CPU in %
of one core for the whole Chrome process tree, median of 5 runs with the lowest and highest run in brackets
([record](measurements/records/comparison-video-element-5-repeats.md)).

| File | FraxEngine | Chrome `<video>` |
| --- | --- | --- |
| 1080p H.264 | 9.1 % (8.2 to 9.4) | 12.2 % (10.3 to 12.6) |
| 4K HEVC HDR10 | 12.4 % (9.3 to 12.8) | 14.8 % (11.1 to 15.2) |
| 4K HEVC HDR10, 90 Mbit/s | 12.0 % (10.0 to 12.9) | 16.0 % (12.6 to 16.4) |
| VP9 | 10.0 % (8.9 to 10.2) | 12.4 % (10.4 to 12.5) |
| Dolby Vision 8.1, processed on the GPU (default) | 28.4 % (25.8 to 28.8) | 13.1 % (12.6 to 13.1), HDR10 base layer only |
| Dolby Vision 8.1, base layer only (`?dvbase=1`) | 13.6 % (13.1 to 13.8) | same as above |
| Dolby Vision 7 FEL (hardware, renderer only, [record](measurements/records/dolby-vision-profile-7-fel-cpu.md)) | 7 to 10 % | cannot play it |

- On the 4K HDR10 file the two ranges overlap, so that gap is within the run-to-run spread.
- Dolby Vision 7 FEL on the hardware path is about 60 dB from the bit-exact result; an optional mode is bit-identical.
- Memory is 1.0 to 1.45 GB (RSS, whole Chrome tree), 0.2 to 0.45 GB more than a plain `<video>`.
- Profile 8.1 with processing costs about twice `<video>` because the picture goes through a WebGPU canvas; the
  analysis is in [this record](measurements/records/dolby-vision-profile-8-1-canvas-cost.md).

Comparison with native players and the method: [player engines on one Mac](measurements/records/comparison-player-engines-m4-pro.md).

## Features

### What it can do

| Area | Supported |
| --- | --- |
| Containers | Matroska, MP4 / fMP4 / MOV, MPEG-TS / M2TS, raw audio (AC-3, E-AC-3, FLAC, DTS, TrueHD, WAV, ADTS) |
| Video | H.264, HEVC, AV1, VP9 on the browser's decoders (hardware where available); VC-1 Advanced Profile in WebAssembly, bit exact with FFmpeg |
| HDR | HDR10, HDR10+ (scene metadata applied), HLG |
| Dolby Vision | Profile 5, 8.x, 7 MEL and 7 FEL, composed on the GPU from the RPU; optional bit-exact decode of either layer |
| Audio | AAC, Opus, FLAC; AC-3, E-AC-3 with JOC (Atmos objects), TrueHD with Atmos objects, DTS core, DTS-HD MA / HRA, DTS:X, LPCM; rendered to the speaker layout or binaural for headphones |
| Subtitles | PGS, ASS / SSA, SRT, WebVTT |
| Sources | local files (read in slices) and HTTP(S) URLs with Range |
| Extras | a reference player page and a small Jellyfin client |

### What it cannot do

- **No native Dolby Vision display mode.** The output is an HDR canvas; the display is not switched into Dolby Vision.
- **No bitstream pass-through.** Atmos and DTS:X reach the speakers as decoded PCM, not as signalled objects to a receiver.
- **No DTS-UHD**, VobSub, DASH or HLS manifests, disc structures (ISO / BD folders), DRM.
- **Chrome on macOS is the tested target.** Safari works in part; other browsers and platforms are untested.
- Real-time 4K FEL depends on the machine; bit-exact modes need several times the CPU of the default.

## Installation

Requirements: Rust with the `wasm32-unknown-unknown` target, Node.js 22, Python 3.

```sh
git clone https://github.com/itsmefrade/fraxengine.git
cd fraxengine
rustup target add wasm32-unknown-unknown
sh scripts/build.sh          # builds every WebAssembly module into web/
sh scripts/test.sh           # optional: cargo and node tests
```

Toolchain details and build outputs: [docs/build.md](docs/build.md).

## Quick start

Run the reference player locally:

```sh
python3 web/server.py --port 8790
# open http://127.0.0.1:8790/player/index.html and pick a file, or add ?src=<url>
```

Embed the engine in your own page (served with the headers under Configuration):

```js
import { Session } from './player/js/session.js';

const session = new Session({
  url: 'https://media.example.org/movie.mkv',
  host: document.getElementById('player'),   // the engine adds its <video> or <canvas> here
  wantPlay: true,
  onEvent: (ev) => {
    if (ev.type === 'state') console.log('state', ev.state);
    if (ev.type === 'error') console.error(ev.message);
  },
});
await session.open();
// session.pause(), session.play(), session.seek(timeUs), session.switchAudio(index), session.stop()
```

## Configuration

**Required headers.** The page must be cross-origin isolated:

| Header | Value |
| --- | --- |
| `Cross-Origin-Opener-Policy` | `same-origin` |
| `Cross-Origin-Embedder-Policy` | `require-corp` or `credentialless` |

A media server on another origin must allow CORS and Range: `Access-Control-Allow-Origin`,
`Access-Control-Expose-Headers: Content-Range, Content-Length, Accept-Ranges`, and `206` answers to `Range`. Hosts
that cannot set headers can use the bundled service worker. Details: [docs/deploy.md](docs/deploy.md).

**Session options.** `url` or `file`, `host`, `onEvent`, `wantPlay`, `startUs`, `audioIndex`, `subtitle`,
`headers` (extra request headers, so a token stays out of the URL), `size`, `requireRange`, and `quality`:

| `quality` | Dolby Vision behaviour |
| --- | --- |
| `null` (auto) | Fast for Dolby Vision and HDR10+, native for other streams |
| `native` | the base layer as HDR10 through the browser |
| `fast` | both layers on the hardware decoder, composed on the GPU |
| `high` | base layer decoded bit exact in WebAssembly |
| `reference` | both layers bit exact |

**Page parameters** for testing, read from the player page's URL: `?q=` (the quality above), `?mse=0` (never use the
`<video>` route), `?dvbase=1` (Dolby Vision base layer through the `<video>` route: lower CPU, a battery-saving option, but no Dolby
Vision processing; the default keeps it), `?peak=<nits>` and
`?sdrwhite=<nits>` (display assumptions), `?maxw=<px>` (cap the canvas width).

**Environment variables** (build and tests only): `AETHER_LAB` (folder with local test media),
`AETHER_RUST_TOOLCHAIN` (an isolated Rust toolchain), `AETHER_OFFLINE=1` (cargo without network).

## Documentation

| Page | Contents |
| --- | --- |
| [docs/architecture.md](docs/architecture.md) | workers, routes, the Dolby Vision pipeline, repository layout |
| [docs/build.md](docs/build.md) | toolchain and build outputs |
| [docs/testing.md](docs/testing.md) | test suites and local test media |
| [docs/deploy.md](docs/deploy.md) | static deployment, headers, hosts |
| [docs/jellyfin.md](docs/jellyfin.md) | the Jellyfin client |
| [web/fel/README.md](web/fel/README.md) | the WebGPU Dolby Vision compositor |
| [measurements/](measurements/README.md) | measurement records |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security issues: [SECURITY.md](SECURITY.md).

## License

[LGPL-3.0 with the App Store / DRM exception](LICENSE), inherited from AetherEngine. Third-party code and its licences
are listed in [NOTICE.md](NOTICE.md) and [web/LICENSES.md](web/LICENSES.md).

**Patents and trademarks.** The licences cover copyright only. HEVC, VC-1, AC-3, E-AC-3, DTS, TrueHD and Dolby Vision
may be covered by patents; this project grants no patent licence, and anyone who ships it is responsible for the
licences their use needs. Dolby, Dolby Vision, Dolby Atmos, DTS and the other format names are trademarks of their
owners. FraxEngine is not affiliated with or endorsed by them.
