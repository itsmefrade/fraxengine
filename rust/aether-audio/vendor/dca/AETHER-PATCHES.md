Vendored from harletty/harletty-bridge @ 424c1a72c7764a22ae57f3b9871450d571a5c88e (`dca/`, src only).
Local changes (search for `AETHER`):
1. `core.rs` `decode_xxch`: after a successful XXCH set, `extension_base` advances to `nchannels`, so XXCH
   channels map to their speakers and the embedded downmix is undone even when no DTS:X extension follows.
2. `hd.rs` `decode_lossy`: an asset with an X96 component over a 48 kHz core is synthesised through the
   64-band bank (96 kHz output, matching FFmpeg's rate and frame count), also when the X96 payload fails to parse.
3. `core.rs` `decode_xbr`, `decode_x96_exss` (ports of FFmpeg 7.1 `parse_xbr_frame`, `parse_xbr_subframe`,
   `parse_x96_frame_exss`, `parse_x96_frame_data`, `parse_x96_coding_header`, `parse_x96_subframe_header`,
   `parse_x96_subframe_audio`, `rand_x96`; CRC checks left out as in FFmpeg's default). XBR adds a residual to
   the base subbands; X96 fills a second 64-band sample set (`x96_bands`, read through `x96_subband`). The
   per-band arrays (`prediction_*`, `bit_allocation`, `joint_scale_factors`) are 64 wide (`Wide<T>`) because the
   X96 header reuses them, as FFmpeg does. `exss.rs` gains `has_xbr` / `has_x96`; `exss_kind` reports `Lossy`
   for XBR or X96 assets. The core-embedded X96 and XCH (no EXSS) are still not handled.
4. `hd.rs` `decode_lossy` runs XBR then X96 after XXCH (FFmpeg's `ff_dca_core_parse_exss` order); a failing
   component leaves the bed without it.
5. `synth.rs` `synthesize_fixed_by_speaker(.., lossy_x96, ..)`: the lossy route (FFmpeg's float output, not the
   fixed path it forces for XLL) sums the X96 samples into the 64-band bank (`bank_64_x96`: bands 0..32 base plus
   residual, 32..64 high band; always the portable code so the delay lines keep one layout, with a reset when
   the route changes) and interpolates the LFE with the float filter (`lfe_synth`, `lfe_x96_float`) instead of the
   fixed one, whose passband differs. Measured against `ffmpeg -f f32le` on the two HRA files: max abs diff
   8.0e-5 (7.1 Orchestra) and 5.1e-5 (5.1 VC1), was 0.114 and 0.110.
