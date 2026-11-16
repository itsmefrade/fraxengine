# CLAUDE.md

Guidance for Claude Code and other coding agents working in this repository.

## What this is

FraxEngine plays media in a web page: Dolby Vision (Profile 5, 8.1 and Profile 7 FEL), HDR10, HDR10+, HLG and
lossless or object audio, in stock Chrome on macOS without a helper app or server-side transcoding. Rust is compiled to
WebAssembly for demuxing, Dolby Vision metadata and audio decoding; JavaScript workers drive WebCodecs, Media Source
and WebGPU. It is a browser port of AetherEngine (see `NOTICE.md` and `CHANGES.md`).

Read before changing anything non-trivial:

- `docs/architecture.md`: workers, routes and the data flow.
- `web/fel/README.md`: the Dolby Vision / FEL pipeline in WebGPU.
- `docs/build.md` and `docs/testing.md`: toolchain, build outputs, test media.
- `measurements/METHODOLOGY.md`: how a measurement is recorded. `measurements/README.md` lists the records.

## Build and test

```sh
sh scripts/build.sh
sh scripts/test.sh            # judge by the exit code
python3 web/server.py --port 8790
```

The page needs cross-origin isolation (COOP `same-origin`, COEP `require-corp` or `credentialless`); `web/server.py`
sends it. Browser pages (`*.test.html`, WebGPU checks) run in Chrome by hand.

## Rules that are load-bearing

- **Two routes, decided once per session.** `mse` (a `<video>` through Media Source) whenever the browser can play the
  stream natively; `canvas` (WebCodecs plus WebGPU) for Dolby Vision processing, HDR10+ and what the element cannot
  play. A buffering pause is not a reason to change route.
- **Dolby Vision is never handed to the browser as Dolby Vision.** The SPS is relabelled so the hardware decoder
  returns frames with only the YCbCr to RGB matrix applied; the shaders do the rest.
- **The hidden-page behaviour is Chrome's.** Chrome pauses video-only media and drops the video track of media with
  sound in a hidden page. Do not "fix" it in the engine, and do not benchmark with a hidden window.
- **Every number in the docs has a measurement record.** Add or update the record in `measurements/` in the same
  commit.
- **Test media is never committed.** Tests find it in `test-media/` or `$AETHER_LAB` and skip when it is missing.

## Conventions

- Conventional Commits (`feat(fel):`, `fix(demux):`, `docs:`, `test:`, `ci:`).
- No em-dashes anywhere. Use commas, periods or parentheses.
- Write for humans: the answer first, short sentences, one idea per paragraph, tables over prose.
- Comments explain why, sparingly. Keep the measurement when touching code whose comment cites one.
- Everything in English.
