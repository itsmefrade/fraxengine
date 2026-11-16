# FraxEngine player

A static web app that plays local files and HTTP(S) URLs in Chrome with no install and no server-side media
processing. Demuxing, Dolby Vision RPU handling and the audio decoders are Rust compiled to WebAssembly
(`rust/`, built into `wasm/aether_wasm.wasm`); video decode uses WebCodecs (hardware where Chrome has it),
Dolby Vision FEL/HDR composition runs in WebGPU, audio goes through an AudioWorklet. Plain ES modules, no bundler.

Architecture: [docs/architecture.md](../docs/architecture.md). Deployment: [docs/deploy.md](../docs/deploy.md).
Licences: [LICENSES.md](LICENSES.md).

## Requirements

- **Chrome on macOS first** (developed and measured on Chrome 157, Apple silicon). Other Chromium browsers and
  platforms may work but are not tested.
- WebCodecs, WebGPU, AudioWorklet, module workers, `SharedArrayBuffer`. The last one needs **cross-origin
  isolation**: the page must be served with `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`, or through the bundled service worker (see Deploy).
- A secure context: HTTPS or `http://localhost` / `127.0.0.1`.
- HDR output needs an HDR-capable display and Chrome's HDR canvas path; on an SDR display the player tone maps.

## Run locally (development)

```sh
python3 web/server.py --port 8790        # serves web/ with COOP/COEP, no cache
# open http://127.0.0.1:8790/player/index.html?src=/player/fixtures/<file>
```

`player/index.html` has a file picker, drag and drop onto the video area, and an address field for a URL. `?src=URL`
opens a URL directly. Local test media goes in `web/player/fixtures` (git-ignored, see [docs/testing.md](../docs/testing.md)).

## Build and deploy

```sh
sh web/dist-tools/build.sh            # tests the Rust workspace, builds the WASM, assembles web/dist, checks it in headless Chrome
sh web/dist-tools/build.sh --skip-rust --no-verify     # re-assemble from the WASM already in web/wasm (seconds)
python3 web/dist-tools/serve.py --port 8800            # preview web/dist with the production headers and Range support
```

`build.sh` options: `--skip-rust`, `--no-source` (leave out the LGPL source archive), `--no-verify`, `--media FILE`.
It fails if any import or worker URL in the runtime graph resolves to a missing file, and again if the built
site, run in headless Chrome, requests anything that returns 4xx/5xx. Chrome is expected at
`/Applications/Google Chrome.app` (override with `CHROME=...`); the verification needs `pip install websockets`.

Result: `web/dist/` is a self-contained static site (about 3.7 MiB of runtime files, 1.2 MiB gzip, plus legal files
and the source archive). `index.html` and `coi*.js` sit at the root; everything else is under `assets/<build id>/`,
where the build id is a hash of the contents, so those URLs are immutable and can be cached for a year. Upload the
contents of `web/dist` to any static host (see [docs/deploy.md](../docs/deploy.md)). Host header configs for Netlify, Cloudflare Pages, Vercel, nginx and
Caddy are in `dist-tools/hosts/`; GitHub Pages and other hosts that cannot set headers use the service worker
fallback automatically.

## Opening media

| Route | How | Limits |
| --- | --- | --- |
| Local file | file picker or drag and drop; read in slices (`FileReaderSync`), never loaded whole | the file picker API `showOpenFilePicker` is not used, so there is no remembered handle; reselect after a reload |
| HTTP(S) URL | `?src=` or the address field; one continuous download into a 64 MiB read-ahead ring (16 MiB kept behind the reader), a seek outside it restarts the download; a page that is not cross-origin isolated reads 4 MiB Range blocks | the server must allow CORS and Range (below); without Range the whole file is downloaded first |

A media server on another origin must send `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers: Content-Range,
Content-Length, Accept-Ranges` and answer `Range` with `206` and `Content-Range`. Details: [docs/deploy.md](../docs/deploy.md).

## Feature matrix

| Area | Works | Honest limits |
| --- | --- | --- |
| Containers | Matroska, MP4/fMP4/MOV, MPEG-TS/M2TS, raw audio (AC-3, E-AC-3, FLAC, DTS, TrueHD, WAV, ADTS) | no ISO/BD disc structures, no DASH/HLS manifests |
| Video | H.264, HEVC, AV1, VP9 through WebCodecs when Chrome can decode them; VC-1 Advanced Profile in WASM (bit exact with FFmpeg) | what hardware lacks falls back to Chrome's software path or is reported as unsupported; MPEG-2, VP8 and interlaced-specific handling are not claimed |
| HDR | HDR10, HDR10+, HLG on an HDR canvas | the display is not switched into a Dolby Vision mode |
| Dolby Vision | P5, P8.x, P7 MEL and FEL are composed in the app (RPU applied on the GPU; FEL uses the enhancement layer). Base and enhancement layer decode on Chrome's hardware HEVC decoder by default; profile 5 falls back to the WASM decoder where no hardware decoder takes the stream | **no native DV display mode**; Chrome gives no way to hand RPU to the system. Output is an HDR canvas. Real-time FEL at 4K depends on the machine |
| Quality modes (HDR select) | Auto, Native (BL as HDR10), Fast (hardware BL/EL), High (bit-exact BL in WASM), Reference (bit-exact BL and EL) | Exact modes decode HEVC in WASM at several times the CPU of Fast (4K FEL: 24 to 38.5 % of a renderer core against 7.1 to 8.8 %); Auto runs Dolby Vision and HDR10+ in Fast and other HEVC natively (High on Safari) |
| Audio | AAC, Opus, FLAC through WebCodecs, LPCM in WASM; AC-3, E-AC-3 (+ JOC Atmos objects rendered to 7.1.4), TrueHD (+ Atmos objects), DTS core, DTS-HD MA/HRA, DTS:X (7.1.4) decoded in WASM | Rendering to the output device layout (speakers) or binaural (headphones) with downmix; no bitstream passthrough to a receiver (Chrome has no API), so Atmos/DTS:X reach the speakers as decoded PCM, not as signalled objects |
| DTS-UHD | **not supported** (parser exists in the repo but is not in the build) | the track is reported unsupported |
| Subtitles | PGS, ASS/SSA, SRT, WebVTT (drawn on a canvas overlay; forced-only option for PGS) | VobSub is not decoded; WebVTT regions and `::cue` CSS are ignored |
| Seek, audio track switch | Verified-keyframe seek, track switch inside the file | Matroska cue points are not trusted, so seeks may land a few seconds earlier |
| Controls | play, pause, seek, keyboard (space, arrows, `i` stats), stats overlay, normalize and limiter | no playlists, no chapters UI, no casting |

