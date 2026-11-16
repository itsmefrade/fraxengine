# Notice

FraxEngine is a browser port of **AetherEngine**.

| | |
| --- | --- |
| Original project | [AetherEngine](https://github.com/superuser404notfound/AetherEngine), a media player engine for Apple platforms |
| Original author | Vincent Herbst ([@superuser404notfound](https://github.com/superuser404notfound)) |
| Original licence | LGPL-3.0 with the App Store / DRM exception (the `LICENSE` file in this repository, kept unchanged) |
| This project | Copyright (C) 2026 the FraxEngine contributors, under the same licence |

What changed relative to AetherEngine is listed in [CHANGES.md](CHANGES.md).

## Code and data from other projects

The full inventory, with versions, licences and open questions, is [web/LICENSES.md](web/LICENSES.md). In short:

| Component | Origin | Licence |
| --- | --- | --- |
| `rust/aether-ac3` | Rust translation of FFmpeg's AC-3 / E-AC-3 decoder | LGPL-2.1-or-later |
| `rust/aether-vc1` | derived from FFmpeg 7.1's VC-1 decoder | LGPL-2.1-or-later |
| `rust/aether-audio/vendor/dca` | translation of FFmpeg 8.0's DCA decoder (via harletty-bridge) | LGPL-2.1-or-later |
| `rust/aether-hevc/vendor/rust_h265` | [rust_h265](https://github.com/roticv/rust_h265), modified | MIT OR Apache-2.0 |
| `truehd` crate | fork of [truehdd](https://github.com/truehdd/truehdd) maintained by the AetherEngine author | Apache-2.0 |
| `oamd` crate | [truehdd/oamd](https://github.com/truehdd/oamd) | Apache-2.0 |
| `dolby_vision` crate | [quietvoid/dovi_tool](https://github.com/quietvoid/dovi_tool) | MIT |
| KEMAR HRTF data | MIT Media Lab (Gardner and Martin, 1995) | free with citation |
| ETSI TS 103 420 tables in `rust/aether-joc` | ETSI / EBU | see the open question in `web/LICENSES.md` |

Each vendored crate keeps its own `LICENSE` and `NOTICE` files.

## Trademarks

Dolby, Dolby Vision, Dolby Atmos, TrueHD, AC-3, E-AC-3, DTS, DTS:X and DTS-HD are trademarks of their owners. This
project is not affiliated with or endorsed by them. Codec patents are a separate matter from the copyright licences
above and are not assessed here.
