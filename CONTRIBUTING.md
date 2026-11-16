# Contributing to FraxEngine

Thanks for your interest. Most of the work here is format- and browser-specific, so a precise bug report or a focused
pull request with a clear test plan helps a lot.

## Reporting bugs

Open an issue. For a playback bug, include:

- the media: container, video codec and profile, audio codec, HDR or Dolby Vision profile (`ffprobe` output is ideal),
- the browser and its version, the operating system and the display (SDR or HDR),
- the route the session took and any warnings. The player page shows both in its statistics panel.

## Building and testing

You need Rust (with the `wasm32-unknown-unknown` target), Node.js 22 and Python 3. Details are in
[docs/build.md](docs/build.md).

```sh
sh scripts/build.sh        # every WebAssembly module the player loads
sh scripts/test.sh         # cargo and node tests; no GPU and no media needed
python3 web/server.py --port 8790
# open http://127.0.0.1:8790/player/index.html
```

Tests that need real media skip with a message when it is missing. To run them, put files in `test-media/` at the
repository root or point `AETHER_LAB` at a folder (see [docs/testing.md](docs/testing.md)). Test media is never
committed.

Browser checks (`*.test.html`, WebGPU pages) run in Chrome on macOS by hand; CI runs only what works without a GPU.

## Pull requests

- Keep each pull request to one change.
- Write a test plan: browser and version, OS, display, and the exact media. Playback differs by all of them, so
  "Chrome 157, macOS 27, XDR display, Dolby Vision 8.1 MP4" says far more than "works for me".
- Update the docs and `CHANGELOG.md` in the same commit as the code.
- Follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat(fel):`, `fix(demux):`, `docs:`, `test:`).
- A performance or quality claim needs a measurement record in `measurements/`, written as described in
  [measurements/METHODOLOGY.md](measurements/METHODOLOGY.md).

## Style

- Plain words, short sentences, the answer first. Tables and lists over long paragraphs.
- No em-dashes anywhere (code, comments, commits, docs).
- Comments explain why, not what. Keep the measurement when a comment cites one.
- Everything in English.

## Licence

FraxEngine is [LGPL-3.0 with the App Store / DRM exception](LICENSE), inherited from AetherEngine. By contributing you
agree that your contribution is licensed under the same terms.
