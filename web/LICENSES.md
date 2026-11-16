# Licence inventory of the browser player (web/dist)

Status: inventory prepared by the maintainers' tooling from the Cargo manifests, the vendored crates' own
notice files and `cargo metadata` for `wasm32-unknown-unknown` (locked, offline). It is not legal advice.
Items marked **FLAG** are unclear or need a decision by the copyright holder or a lawyer before a public
release. The build copies this file, the licence texts and every notice listed here into `dist/legal/`
(`dist/legal/third-party/` holds the licence files of the crates.io and git crates, `third-party.json` is the index).

## What is in web/dist

| File in dist/assets/&lt;build&gt;/ | Contains |
| --- | --- |
| `wasm/aether_wasm.wasm` | demux (MKV, MP4, TS), Dolby Vision RPU handling, TrueHD, DTS (core, HD, X), AC-3, E-AC-3 (JOC), VC-1, subtitles, session logic, binaural renderer with HRTF data |
| `exact/wasm/aether_hevc_simd.wasm` | bit-exact HEVC Main/Main10 decoder (aether-hevc, a fork of rust_h265) used for Dolby Vision FEL/MEL reconstruction |
| `player/`, `engine/`, `fel/`, `subs/`, `exact/*.js` | JavaScript and WGSL shaders, shipped unminified and unmodified from `web/` |

Not shipped, although present in the repository: `rust/aether-dtsuhd` (DTS-UHD is unsupported), the vendored
`rust/aether-demux/vendor/mp4parse` (not a dependency of any crate in the build), `web/exact/wasm/aether_video_simd.wasm`
(the older decoder variant, only used by benchmarks), all test media and fixtures. No fonts, images or icons
are shipped. The browser plays everything through WebCodecs and Web Audio; no codec library comes from the
browser vendor's licence terms except through those APIs.

## Our code

| Component | Licence | Notes |
| --- | --- | --- |
| Repository root `LICENSE` | LGPL-3.0 plus the "Apple Store / DRM exception" | Copyright (C) 2026 Vincent Herbst. The exception is about app stores and does not matter for a website. |
| `aether-demux`, `aether-dovi`, `aether-player`, `aether-subs`, `aether-audio`, `aether-wasm` (and the `aether-core` shim) | `LGPL-3.0-only` (Cargo.toml) | Consistent with the root licence. |
| JavaScript under `web/` (engine, fel, exact, subs, player/js) | `LGPL-3.0-only` via `// SPDX-License-Identifier: LGPL-3.0-only` on line 1 | Chosen to match the root `LICENSE` and the Cargo manifests of our crates. **Owner to confirm** (see Owner decisions needed). |

## Derived from FFmpeg (LGPL-2.1-or-later)

| Component | Origin | Licence | Source in repo |
| --- | --- | --- | --- |
| `aether-ac3` 0.1.0 | Rust translation of FFmpeg libavcodec ac3dec, eac3dec, ac3, ac3dsp, kbdwin and libavutil lfg | `LGPL-2.1-or-later` | `rust/aether-ac3` (NOTICE lists the copyright holders) |
| `aether-vc1` 0.1.0 | derivative of FFmpeg 7.1 VC-1 decoder (vc1*.c, vc1dsp, hpel, h264chroma) | `LGPL-2.1-or-later` | `rust/aether-vc1` (NOTICE, LICENSE) |
| `dca` 0.9.0 | vendored from harletty/harletty-bridge @ 424c1a72, translation of FFmpeg 8.0 DCA decoder, with local patches (`AETHER-PATCHES.md`) | `LGPL-2.1-or-later` | `rust/aether-audio/vendor/dca` (NOTICE, LICENSE). Releases up to 0.9.0 upstream were labelled Apache-2.0 by mistake, the crate's NOTICE says so. |

## Original code under LGPL-2.1-or-later

| Component | Licence | Notes |
| --- | --- | --- |
| `aether-joc` 0.1.0 (E-AC-3 Joint Object Coding, Dolby Atmos) | `LGPL-2.1-or-later` | Written from ETSI TS 103 420 and TS 102 366. Contains normative tables (Huffman codes A.1, QMF prototype filter) from the ETSI zip. Notice: "(C) ETSI 2018, (C) European Broadcasting Union 2018". **FLAG**: whether ETSI's terms allow redistribution of those tables inside an LGPL work is not settled in the repository; the NOTICE only states they are needed for interoperability. Needs review. |

## HEVC

| Component | Licence | Notes |
| --- | --- | --- |
| `aether-hevc` 0.1.0 (cdylib to `aether_hevc_simd.wasm`) | `MIT OR Apache-2.0` (its Cargo.toml) | Optimised fork of rust_h265 f582ce89. Its session wrapper (`src/session.rs`, `src/session_abi.rs`) derives from an earlier prototype (hevc-rust) written in this project (recorded in `rust/aether-hevc/NOTICE`), so the wrapper has the project licence (LGPL-3.0-only). **Owner to confirm**: the crate's Cargo.toml still says `MIT OR Apache-2.0`. |
| `rust_h265` 0.4.1 | `MIT OR Apache-2.0` | https://github.com/roticv/rust_h265, vendored in `rust/aether-hevc/vendor/rust_h265` with modifications (SIMD paths, allocation removal). Licence texts: `LICENSE-MIT`, `LICENSE-APACHE`. **FLAG**: upstream's own provenance (whether it is an independent implementation or derived from another decoder) was not audited here. |

## Third-party crates linked into aether_wasm.wasm

Normal dependencies of `aether-wasm` for `wasm32-unknown-unknown`; versions from `rust/Cargo.lock`.

| Crate | Version | Licence | Source |
| --- | --- | --- | --- |
| truehd | 0.7.2 | Apache-2.0 | git fork https://github.com/superuser404notfound/truehdd, rev 33bae1657ceb25b5fe0347f42c544c3414e2760e (Cargo.lock; Cargo.toml repository field https://github.com/truehdd/truehdd/tree/main/truehd). `LICENSE` at the repo root and in `truehd/` is the Apache License 2.0 text. |
| oamd | 0.1.0 | Apache-2.0 | crates.io, https://github.com/truehdd/oamd |
| dolby_vision | 3.4.0 | MIT | crates.io, https://github.com/quietvoid/dovi_tool/tree/main/dolby_vision. `LICENSE` is the MIT License, "Copyright (c) 2026 quietvoid". |
| bitvec_helpers | 4.0.2 | MIT | https://github.com/quietvoid/bitvec_helpers |
| bitvec, funty, radium, tap, wyz | 1.1.1, 2.0.0, 0.7.0, 1.0.1, 0.5.1 | MIT | https://github.com/bitvecto-rs |
| bitstream-io | 4.10.0 | MIT/Apache-2.0 | https://github.com/tuffy/bitstream-io |
| rustfft, primal-check, strength_reduce, transpose | 6.4.1, 0.3.4, 0.2.4, 0.2.3 | MIT OR Apache-2.0 | https://github.com/ejmahler, https://github.com/huonw/primal |
| num-complex, num-integer, num-traits | 0.4.6, 0.1.47, 0.2.19 | MIT OR Apache-2.0 | https://github.com/rust-num |
| crc, crc-catalog | 3.4.0, 2.5.0 | MIT OR Apache-2.0 | https://github.com/mrhooray/crc-rs, https://github.com/akhilles/crc-catalog (the catalog crate ships no licence file, the id is recorded in third-party.json) |
| anyhow, log, thiserror (+ thiserror-impl), proc-macro2, quote, syn | see Cargo.lock | MIT OR Apache-2.0 | https://github.com/dtolnay, https://github.com/rust-lang/log (the last four are compile-time proc-macro helpers, not linked) |
| memchr | 2.8.3 | Unlicense OR MIT | https://github.com/BurntSushi/memchr |
| no_std_io2 | 0.9.4 | Apache-2.0 OR MIT | https://github.com/wcampbell0x2a/no-std-io2 |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/tinyvec |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident |

`aether-hevc` has no dependency besides `rust_h265`. The `truehd` crate is a pinned git revision of a fork; its
Cargo.toml says Apache-2.0 and the crate ships a LICENSE file. **FLAG**: the provenance of the TrueHD/MLP
implementation (independent from the specification, or derived from other code) is taken from the crate's claim
and was not audited.

## Data

| Data | Licence | Notes |
| --- | --- | --- |
| KEMAR HRTF (14 directions, `aether-audio/src/output/hrtf_data.rs`) | MIT Media Lab terms: free, no restrictions, authors must be cited | Gardner, W. G. and Martin, K. D., "HRTF measurements of a KEMAR", J. Acoust. Soc. Am. 97(6), 3907-3908, 1995 (https://sound.media.mit.edu/resources/KEMAR.html). Full text in `legal/NOTICE-hrtf-kemar.txt`. The player page has an "About / licenses" panel with the citation and a link to this file. |
| ETSI TS 103 420 tables | see aether-joc above | **FLAG** |
| Dolby Vision, Dolby Atmos, TrueHD, AC-3, E-AC-3, DTS, DTS:X, DTS-HD | trademarks of their owners | Not affiliated or endorsed (per the crates' notices). **FLAG**: codec and format patents are a separate matter from the copyright licences above (HEVC, VC-1, AC-3, DTS, TrueHD and Dolby Vision are covered by patent pools or licensors). This inventory does not assess them. |

## LGPL obligations for a static web distribution

LGPL-2.1-or-later parts can be used under LGPL-3.0, so the shipped WASM is treated as one work under
LGPL-3.0-or-later terms; the MIT/Apache parts are compatible with that. What this means in practice, and how the
build meets it (an engineering reading, not legal advice):

1. **Notices and licence text.** Convey the licence texts and copyright notices. `dist/legal/` carries the root
   `LICENSE` (LGPL-3.0 plus exception, which includes the GPL-3 incorporation), the NOTICE/LICENSE files of ac3, vc1,
   joc, dca, hevc, rust_h265, the HRTF notice, and the licence files of all third-party crates. The entry page links
   to `legal/LICENSES.md`.
2. **Corresponding source.** The JavaScript is shipped as written (no minification, no bundling), so it is its own
   source. For the WASM binaries the corresponding source is the Rust workspace at the commit named in
   `dist/SOURCE.txt`. The build writes `dist/source/aether-web-source-<build>.tar.gz` (rust/, web/, LICENSE, docs) next
   to the site, so the source is offered from the same place as the binary. The archive is made with `git archive HEAD`
   (committed tree only; `assemble.py` warns when `rust/` or `web/` is dirty and `SOURCE.txt` and `manifest.json` record the
   commit and the dirty flag). Anything untracked is therefore not in the archive, so commit all crates before a release build.
3. **Rebuild and replacement ("relinking").** `sh web/dist-tools/build.sh` rebuilds the WASM from the source with the
   locked dependencies (`Cargo.lock`, `--locked`), and the site is plain static files, so anyone can host a modified
   build or swap `aether_wasm.wasm` and the JavaScript in their own copy. The build needs the Rust toolchain named in
   `rust/build.sh` (wasm32-unknown-unknown) and, for the HEVC module, `rust/aether-hevc/build-wasm.sh`.
   LGPL-3.0 section 4 also asks for "Installation Information" only for "User Products" (consumer devices that
   block modified code). A website is probably not that, but whether a browser app counts is a legal question; the
   design (no signing, no integrity lock beyond the build manifest) is meant to leave replacement open.
4. **No restriction on reverse engineering for debugging modifications.** Nothing in the site forbids it; the
   `manifest.json` hashes are for cache and integrity checking only.
5. **If you modify** an LGPL part and host it, you must offer your modified source the same way.

## Owner decisions needed

Open questions, recorded and not resolved here:

1. **ETSI table redistribution.** Whether ETSI's terms allow redistributing the normative tables (Huffman codes A.1, QMF
   prototype filter, from ETSI TS 103 420) inside an LGPL work (`aether-joc`).
2. **Codec patents.** HEVC, VC-1, AC-3, E-AC-3, DTS, TrueHD and Dolby Vision patent pools or licensors; separate from copyright.
3. **LGPL-3 Installation Information for web apps.** Whether a browser app counts as a "User Product" under section 4.
4. **Project licence of our JavaScript.** `LGPL-3.0-only` was chosen from the root `LICENSE`; confirm.
5. **Licence of `aether-hevc` as a whole.** Cargo.toml says `MIT OR Apache-2.0`; the session wrapper originates in this project.

## Checking the inventory

```sh
cd rust
cargo metadata --format-version 1 --locked --offline --filter-platform wasm32-unknown-unknown \
  | python3 -c "import json,sys; m=json.load(sys.stdin); [print(p['name'],p['version'],p['license']) for p in m['packages']]"
```

The build repeats this scan and writes `dist/legal/third-party.json`; compare it with the tables above when
`Cargo.lock` changes.
