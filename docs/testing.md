# Testing

## What runs where

| Suite | Command | Needs | On CI |
| --- | --- | --- | --- |
| Rust workspace and stand-alone crates (`aether-hevc`, `aether-dtsuhd`, `gpu-recon-ref*`) | `sh scripts/test.sh --rust` | Rust | yes |
| JavaScript (`web/player/test`, `web/engine/test`, `web/subs/test`) | `sh scripts/test.sh --js` | Node 22 and the WebAssembly from `scripts/build.sh` | yes |
| Browser pages (`*.test.html`, `web/gpu-recon/test/*.html`, `web/player/test.html`) | open them from `python3 web/server.py --port 8790` | Chrome with WebGPU and WebCodecs | no |
| Deployed site smoke test | `python3 web/dist-tools/verify-dist.py --mode smoke` | Chrome, `pip install websockets` | no |
| Jellyfin client smoke test | see [jellyfin.md](jellyfin.md) | Chrome | no |

`sh scripts/test.sh` runs both of the first two. Judge it by its exit code.

## Test media

Test media is never committed. Tests that need it skip with a message when it is missing, so a fresh clone passes.

| Where tests look | Used by |
| --- | --- |
| `$AETHER_LAB`, else `test-media/` at the repository root (git-ignored) | Rust tests, `web/engine/test`, the fixture generator scripts |
| `web/player/fixtures/`, `web/fixtures/`, `web/fel/fixtures/` (git-ignored) | browser pages served by `web/server.py` |

The simplest setup is one folder of media and symlinks to it:

```sh
ln -s /path/to/your/media test-media
ln -s ../../test-media web/player/fixtures
```

File names the tests expect are listed in `web/engine/test/_helpers.mjs` and at the top of each Rust test that reads
media. They are common public test clips (Dolby Vision, Atmos and DTS samples). Some fixtures are generated instead:

| Script | Makes | Needs |
| --- | --- | --- |
| `rust/aether-demux/tests/fixtures/generate*.sh` | small MKV, MP4 and TS cuts | ffmpeg, the media above |
| `rust/aether-demux/tests/rawfixtures/generate.sh` | raw elementary streams | ffmpeg |
| `rust/aether-ac3/tests/gen_fixtures.sh` | AC-3 and E-AC-3 references | ffmpeg |
| `rust/aether-dovi/tests/data/regen-real-rpus.sh` | RPUs cut from real files | `dovi_tool` |

With ffmpeg and ffprobe on `PATH`, several tests also compare against FFmpeg's decode.

## Measuring

Performance and quality claims are recorded in [measurements/](../measurements/README.md) following
[METHODOLOGY.md](../measurements/METHODOLOGY.md). The pages used for the playback numbers are `web/player/perf.html`
(the engine), `web/player/perf-floor.html` (a plain `<video>`) and `web/player/perf-mse.html` (bare Media Source), driven
by `web/player/tools/perfrun.py`. Keep the browser window visible while measuring; Chrome pauses or throttles media in a
hidden page.
