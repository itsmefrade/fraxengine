# FraxEngine Rust workspace

The workspace `aether_wasm.wasm` is built from. Architecture: [docs/architecture.md](../docs/architecture.md).

```sh
./build.sh      # cargo test --workspace, then aether-wasm release for wasm32 into web/wasm/
```

`build.sh` picks `/tmp/aether-rust-toolchain` when present (a Homebrew Rust may lack libLLVM);
`AETHER_RUST_TOOLCHAIN` overrides it and `AETHER_OFFLINE=1` adds `--offline`. Pinned git
revisions (`truehd`, `dca`) and the vendored `mp4parse 0.17.0` are unchanged from the experiments.

## Crates

| Crate | Contents | Moved from |
| --- | --- | --- |
| `aether-demux` | `Demuxer`, `ByteSource`, Matroska, MP4, MPEG-TS, raw audio files | `mkv-rust`, `mp4-dv-entries` (superset of `mp4-rust`, with DV entries), vendored `mp4parse` |
| `aether-dovi` | HEVC framing, RPU guard and parse, BL/EL split, POC pairing, FEL kernel and GPU tables, FEL colour maths, SPS VUI relabelling | `rust/aether-core`, `fel-rust`, `fel-color-rust` |
| `aether-audio` | `AudioDecoder` trait over TrueHD, DTS family, AC-3/E-AC-3, LPCM, canonical channel layouts, object renderer, bed regression, paired TrueHD block, output renderer | `rust/aether-audio`, `dts-rust`, `upstream-bed-regression-rust`, `paired-truehd-block` |
| `aether-ac3` | pure Rust AC-3 / E-AC-3 decoder (FFmpeg `-drc_scale 0` parity), wrapped by `aether-audio` | |
| `aether-joc` | E-AC-3 Joint Object Coding (Atmos objects) decoder, used by `aether-audio` | |
| `aether-vc1` | VC-1 Advanced Profile decoder | |
| `aether-subs` | PGS bitmaps and SRT, WebVTT, ASS/SSA text as one styled-cue model | |
| `aether-player` | session generations, rational clock, bounded PTS scheduler | `rust/aether-core`, `player-scheduler` |
| `aether-wasm` | raw C ABI cdylib (no wasm-bindgen) | `player-core-wasm`, `aether-core` exports |
| `aether-core` | shim re-exporting `aether-dovi` and `aether-player` so archived experiments that path-depend on it still build | |

## Public API in brief

**aether-demux**
- `trait ByteSource { fn len(&self) -> u64; fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String>; }`
  with `SliceSource`, `FileSource` (unix) and `BufferedSource` (read-ahead window for remote sources).
- `Demuxer::open(&dyn ByteSource)`, `.tracks() -> &[TrackInfo]` (id, kind, codec, `codec_private` = hvcC or CodecPrivate,
  `dolby_vision` dvcC/dvvC record, timescale, language, channels, sample rate, size), `.packets() -> &[PacketRef]`
  (track, pts, dts, duration, keyframe, presentation, offset, size; storage order), `.read_packet`,
  `.seek(src, track, pts) -> index`, `verify_keyframe`.
- Raw audio files (`Container::Raw`, one audio track with id 1, timescale = sample rate; auto-detected after MKV / MP4 / TS, an ID3v2 prefix is skipped): AC-3 and E-AC-3 (`0B 77`, the independent frame plus its
  dependent substreams is one packet), native FLAC (`fLaC`; frame ends confirmed by the next header CRC-8, its number and the frame CRC-16; `codec_private` is `fLaC` + STREAMINFO block; the SEEKTABLE is used when present, else a
  bisection on the sample numbers in the frame headers), DTS and DTS-HD (`7F FE 80 01` core, the EXSS extension substreams stay in the core's packet; `.dtshd` `STRMDATA` is unwrapped), TrueHD and MLP (major sync `F8 72 6F BA`/`BB`,
  one access unit per packet, only major syncs are key), WAV / RF64 / BW64 / Wave64 (`codec_private` is the `fmt ` chunk, packets are 4096 sample frames) and ADTS AAC (packets keep their ADTS header, no description).
  Codec names: `codec` is `ac3 eac3 flac dts truehd mlp wav aac`, `codec_string` `ac-3 ec-3 flac dtsc|dtsh|dtse|dtsl mlpa pcm-s24le|pcm-f32le|... mp4a.40.N`. The packet list equals `ffprobe -show_packets` (`tests/raw.rs`).
  `open_lazy` indexes 128 KB and, for variable rate streams, samples six 32 KB windows to estimate the duration; a seek jumps to the modelled byte position, resynchronises and indexes a region there (exact for PCM and FLAC,
  else the first timestamp of the region comes from the model and regions are re-timed when indexing joins them). `Demuxer::is_complete` then means "the region being read reaches the end of the file".
  Fixtures: `aether-demux/tests/rawfixtures/generate.sh` (ffmpeg; outputs are git-ignored).
- Matroska clusters are walked block header by block header (payloads stay unread); Cues are used only to land a `seek_to` on the block (cluster position + CueRelativePosition). `seek` takes the block keyframe flag and re-checks HEVC/AVC payloads for an IRAP/IDR NAL.
  Matroska times are nanoseconds (timescale 1e9) and `dts == pts`.

**aether-dovi**
- `hevc::{inspect, split}`, `semantic::inspect_rpu` (guarded by `rpu_guard`), `display::display_json` (display metadata JSON, same guard), `pairing`, `picture_identity`, `source_association`.
- `fel::gpu_tables_for_rpu(&mut MappingHistory, epoch, rpu_nal) -> GpuFrameTables` (6832 i32 words: pivots, polynomial/MMR
  coefficients, prediction and NLQ tables), `fel::compose` (scalar reference), `fel::color` (PQ/RPU colour maths).
- `vui::relabel_sps(nal, ColourPatch) -> Relabelled`, `vui::SDR_CARRIER` (primaries 1, transfer 13, matrix kept),
  `relabel_annexb`, `relabel_hvcc`, `read_sps_colour`. The patch is made on the unescaped RBSP and the NAL is escaped again,
  so `length_delta` reports any change caused by emulation prevention bytes.

**aether-audio**
- `decoder::open(Codec) -> Box<dyn AudioDecoder>`; `push_packet(&[u8])`, `pull() -> Option<PcmBlock>` (planar `I32` for lossless
  24-bit, `F32` otherwise, channel labels, sample rate). `Codec::{TrueHd{presentation}, Dts, DtsX, Ac3, Eac3, Pcm(PcmParams)}`; `PcmBlock.lossless` / `.joc` / `.joc_objects` / `.joc_rendered`; `AudioDecoder::set_joc(bool)`.
- `render::ObjectRenderer`, `bed_regression::BedRegression`, `paired::*`, `dts::DtsSession`.

**aether-player**: `session::Session`, `clock::MediaClock`, `scheduler::Scheduler`.

**aether-wasm** exports (see `src/lib.rs` for conventions: inputs via `aether_alloc`, variable results via
`aether_result_ptr/len`): `aether_demux_{open,info,packets,read,seek,close,next,index_more,complete,revision}` (reads call the imported
`env.aether_host_read(offset: u64, ptr, len) -> i32`; see "Demuxer info" for the lazy-indexing calls), `aether_dovi_{inspect,split,inspect_rpu,rpu_display,relabel_sps,relabel_hvcc}`, `aether_demux_verify_keyframe`,
`aether_fel_{reset,gpu_tables}`, `aether_audio_{open,open_pcm,push,pull,block_flags,layout_table,close}`, `aether_output_*` (downmix, binaural,
limiter), `aether_hdr10plus_*`, `aether_subs_*`, `aether_vc1_*`, `aether_last_error`, `aether_clear_error`, `aether_player_*`.

## Errors

Every audio and demux export that returns a negative status also records a UTF-8 message:
`aether_last_error(ptr, cap) -> len` copies up to `cap` bytes and returns the full length (call with
`cap = 0` to size the buffer), `aether_clear_error()` forgets it. `AetherError.detail` in `web/engine`
carries it and is the headline of the error message. Audio statuses: -1 not open / bad id, -2 open
failed, -100-n TrueHD error n, -200 DTS, -300 PCM, -400 AC-3/E-AC-3.

## Audio decoders

| Codec id (`aether_audio_open`) | Decoder | Notes |
| --- | --- | --- |
| 0 | TrueHD | `arg` = presentation; lossless |
| 1 | DTS / DTS-HD MA / HRA | core, XXCH, X96 (see limits), XLL lossless |
| 2 | DTS:X 7.1.4 | needs an XLL asset |
| 3, 4 | AC-3, E-AC-3 | `aether-ac3`, DRC off, FFmpeg bug compat off; `aether_audio_block_flags` bit 1 = Atmos JOC signalled, bit 2 = the block is the JOC objects rendered to 7.1.4 by `aether-joc` + `aether-audio::joc` (`aether_audio_block_joc_objects`; `aether_audio_set_joc(0)` returns the 5.1/7.1 bed instead) |
| `aether_audio_open_pcm` | LPCM | int LE/BE 8/16/24/32, float 32/64, unsigned 8, Blu-ray HDMV LPCM (4 byte header per packet, 16/24 bit; 20 bit refused) |

`aether_audio_pull` result header is five u32: `rate, frames, channels, label_bytes, lossless`, then the labels,
then planar f32. `aether_audio_block_flags()` after a pull: bit 0 lossless, bit 1 JOC, bit 2 JOC objects rendered.

DTS-HD High Resolution Audio: the core and the XXCH channels (7.1) decode, an X96 asset over a 48 kHz core is
synthesised at 96 kHz (FFmpeg's rate and frame count). The EXSS X96 and XBR extensions are ported from FFmpeg
7.1: against `ffmpeg -f f32le` the max abs diff is 8.0e-5 on `DTS-HRA 7.1 Orchestra.mkv` and 5.1e-5 on
`DTS-HRA5.1_VC1-23.976.mkv` (ignored test `tests/dts_hra.rs`, needs `AETHER_DTS_HRA`, `AETHER_DTS_HRA_REF`,
`AETHER_DTS_HRA_CHANNELS`). XBR has only synthetic coverage (no XBR media found); core-embedded X96 and XCH are
not handled.
The `dca` crate is vendored in `aether-audio/vendor/dca` (see `AETHER-PATCHES.md`): the pinned revision left the
XXCH channels without speakers when no DTS:X extension followed, so HRA failed with "missing bed plane".

## Channel layouts

One naming for every decoder, defined in `aether-audio/src/layout.rs` and exported as JSON by
`aether_audio_layout_table` (mirrored by `AetherWasm.audioLayoutTable()`). A block carries explicit labels, one per
plane, in the decoder's native plane order; consumers permute with `layout::order(labels, Order::Wave|Smpte)`.

Names: `FL FR FC LFE BL BR FLC FRC BC SL SR TC TFL TFC TFR TBL TBC TBR` (bits 0..17 of a WAVE channel mask), then
`WL WR TSL TSR SDL SDR LFE2`. `S*` side surround, `B*` rear (back) surround, `W*` front wide, `T*` heights.

| Order | 5.1 | 7.1 | 7.1.4 |
| --- | --- | --- | --- |
| WAVE (ffmpeg, WAVEFORMATEXTENSIBLE) | FL FR FC LFE BL BR (or SL SR) | FL FR FC LFE BL BR SL SR | 7.1, then TFL TFR TBL TBR |
| SMPTE (cinema, Blu-ray, DTS) | same | FL FR FC LFE SL SR BL BR | 7.1, then TFL TFR TBL TBR |

Native plane orders: TrueHD `L R C LFE Ls Rs Lb Rb` (reported as `FL FR FC LFE SL SR BL BR`); DTS `FL FR FC LFE BL BR SL SR`
(XLL) or the core order `FC FL FR SL SR LFE` (5.1 core); AC-3 family ascending AV_CH bit (`FL FR FC LFE BL BR ... SL SR`);
LPCM with no mask: WAVE default for the channel count (`default_for_channels`: 6 = `FL FR FC LFE BL BR`, 8 = 7.1), or a WAVE
mask; Blu-ray LPCM: the assignment code's stream order (`FL FR FC LFE SL SR BL BR` for code 11). Alias names accepted by
`layout::canonical`: TrueHD (`L R C Ls Rs Lb Rb Cb Lsc Rsc ...`) and SMPTE (`Lss Rss Lrs Rrs Cs ...`).

## Demuxer info

`aether_demux_open(len)` uses `Demuxer::open_lazy`: headers only (measured: Matroska under 10 KB, MP4 under 83 KB,
M2TS 0.46 to 0.86 MB). Packets are indexed on demand:

- `aether_demux_next() -> 1|0|<0`: sequential delivery in storage order from the cursor; result = the 48 byte packet record
  followed by the payload. Its `size` field is the payload length actually returned (a Matroska content-encoded packet is
  longer or shorter than the stored size). It indexes 1 MiB further whenever it runs out, calls `Demuxer::index_after` when the
  next listed packet lies far beyond the last one (a span a seek skipped), and holds back nothing: a packet the indexer lists
  late (a PES whose end was seen in a later step) is delivered after packets of other tracks with higher offsets, never twice.
- `aether_demux_seek(track, pts)` runs `Demuxer::seek_to` and moves the `next` cursor to the keyframe it returns.
- `aether_demux_index_more(max_bytes)`, `aether_demux_complete()`, `aether_demux_revision()` (changes when the track list or
  timeline breaks changed; MPEG-TS discovers both while indexing). `aether_demux_packets(start, count)` and
  `aether_demux_read(index)` still work on the indices of the packets listed so far, which indexing invalidates.

`aether_demux_info` JSON: container level `container` (`mp4`|`matroska`|`mpegts`), `fragmented`, `complete`, `durationNs`,
`omittedTracks` (unusable tracks), `omittedTrackReasons` (strings, Matroska), `timelineBreaks` (`[{offset, shiftTicks}]`, MPEG-TS,
90 kHz); per track `codec` (container id or sample entry; TS: `h264 hevc mpeg2video vc1 aac pcm_bluray ac3 eac3 truehd dts
hdmv_pgs_subtitle ...`), `codecString`, `fallbackCodecString` (Dolby Vision base layer), `jocComplexity`, `dolbyVisionEl` (hex `hvcE`),
`sampleDescriptions`, `sourcePid` (TS PID; the AC-3 core split out of a TrueHD PID has id `0x10000 | pid`), `enhancementLayer`
(UHD Blu-ray Dolby Vision EL, PID 0x1015), `codecDelayNs` (Matroska, Opus pre-skip), plus the existing fields.
`web/engine` uses `codecString` as `track.codec` when present. DTS tracks in TS report the channel count of the extension
substream header (7.1 = 8) instead of the 5.1 of the core.

## Known gaps

- `aether-audio/src/wasm.rs` (old `aether_audio_*` render/checkpoint ABI) is kept behind the default
  `legacy-wasm-exports` feature but is not part of `aether-wasm`, whose output exports are `aether_output_*` (`aether-wasm/src/output.rs`).
  The `render` checkpoint helpers show as dead code in the wasm32 build for that reason.
- Vendored `mp4parse` has its `[dev-dependencies]` and benches removed so Cargo can resolve offline; sources are unchanged.
  `build.sh` excludes it from `cargo test` (its own test suite needs the removed dev-dependencies).
- No Node-side WASM tests for demux ABI errors beyond `web/engine/test`; ABI behaviour is covered natively in `aether-wasm/tests/abi.rs`.
