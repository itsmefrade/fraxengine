# web/engine

Browser-side adapter for the Rust/WASM engine (`web/wasm/aether_wasm.wasm`, built by `sh rust/build.sh`).
Plain ES modules, no build step, no dependencies. Runs in Workers, Node 20+ and (for the helpers) the main thread.

| File | Purpose |
| --- | --- |
| `aether-wasm.mjs` | Raw C ABI loader: module cache, `aether_alloc`/`aether_free`, result buffer copies, negative status to `AetherError`. |
| `sources.mjs` | Synchronous byte sources behind `env.aether_host_read`. |
| `demuxer.mjs` | `openDemuxer`, codec strings, Dolby Vision helpers. Containers: Matroska, MP4, MPEG-TS / M2TS and raw audio files. |
| `dovi.mjs` | `createDoviSession` / `parseRpu`: RPU display metadata (libdovi, guarded) and FEL / mapping GPU tables with `use_prev_vdr_rpu` reuse. |
| `vc1.mjs` | `createVc1Decoder(track)`: VC-1 Advanced Profile video decode (rust/aether-vc1 in the same wasm, bit exact with FFmpeg), see below. |
| `audio-plugins.mjs` | `createAudioPlugin` for TrueHD, DTS family, AC-3, E-AC-3 and LPCM; `pcmParams`, `engineCodec`. |

## API

```js
import { openDemuxer, splitDolbyVision, relabelForChrome, relabelPacketSps } from './demuxer.mjs';
import { createAudioPlugin } from './audio-plugins.mjs';

const d = await openDemuxer(fileOrBlob /* or {url, headers?, blockSize?} or {path, maxBytes?} (Node) or Uint8Array */);
d.tracks();        // Track[]  {id, kind, codec, description?, width?, height?, sampleRate?, channels?, timescale, language?, dolbyVision?}
d.seek(timeUs);    // verified keyframe at or before timeUs on the first video track (else first track); timeUs is on the packets' timeline
d.nextPacket();    // Packet {trackId, ptsUs, dtsUs, durationUs, key, data} or null at the end; indexes the file as it goes
d.durationUs; d.startTimeUs; d.container; d.complete; d.count; d.timelineBreaks; d.omittedTracks; d.omittedTrackReasons; d.damaged; d.close();
```

Extra optional Track fields: `name`, `bitDepth`, `containerColor` (video: `{primaries, transfer, matrix, fullRange, mastering, contentLight}` from MP4 `colr` / `mdcv` / `clli` or Matroska `Colour`, a field null when the container is silent; `mastering` is ten numbers in `mdcv` order), `defaultDurationUs`, `containerCodec` (raw Matroska CodecID, MP4 entry or TS stream name),
`sourcePid` (TS), `enhancementLayer: true` (TS Dolby Vision EL, PID 0x1015), `codecDelayUs` (Matroska Opus pre-skip), `annexB: true` (TS H.264/HEVC),
`pcm` (`{format: 'hdmv'}` for Blu-ray LPCM, `{format: 'int'|'float', bits}` for Matroska `A_MS/ACM` PCM; `codec` is then `pcm`).
Extra Packet field: `presentation: false` on MP4 samples outside the edit list (decode, do not display).

Raw audio files (auto-detected, `d.container === 'raw'`; AC-3, E-AC-3, FLAC, DTS / DTS-HD, TrueHD / MLP, WAV / RF64 / Wave64, ADTS AAC) give one audio track with the packets of
`ffprobe -show_packets`: `codec` is `ac3 | eac3 | dts | truehd | pcm` (engine plugins), `flac` (`description` = `fLaC` + STREAMINFO block) or `mp4a.40.<aot>` (ADTS frames with their headers,
no `description`, which is how WebCodecs wants them). A WAV track is `codec 'pcm'` with `track.pcm = {format, bits, layoutMask}` taken from the `fmt ` chunk (extensible channel mask, 8 bit unsigned) and `description` = that chunk.
`startTimeUs` is 0. A lazy open reads about 128 KB (variable rate streams add six 32 KB windows to estimate `durationUs`; it becomes exact once the whole file is indexed); `seek(timeUs)` is exact for WAV and FLAC and
lands within the byte-to-time model's error for the others (CBR AC-3 / E-AC-3 / DTS core: milliseconds; TrueHD, DTS-HD MA, AAC: a few percent), always on a key packet at or before the target (TrueHD: a major sync).
`d.complete` means the region being read reaches the end of the file. Tests: `test/raw.test.mjs`, fixtures from `rust/aether-demux/tests/rawfixtures/generate.sh`.

Subtitle `codec` names are the ones web/subs expects: `S_HDMV/PGS` and TS `hdmv_pgs_subtitle` -> `pgs`, `S_TEXT/ASS|SSA` -> `ass`,
`S_TEXT/UTF8` and MP4 `tx3g` -> `srt`, `D_WEBVTT/SUBTITLES`, `S_TEXT/WEBVTT` and MP4 `wvtt` -> `webvtt` (the raw id stays in `containerCodec`).

MPEG-TS / M2TS (auto-detected, `d.container === 'mpegts'`):
- Time is the stream's own: M2TS usually starts at 600 s or 11.65 s, so `startTimeUs` is the earliest indexed presentation time, `durationUs` is
  end minus start, and `seek(startTimeUs + x)` is how to seek `x` into the file. Timeline jumps are removed by the engine (`timelineBreaks`
  lists them as `{offset, shiftUs}`).
- Video is Annex B (start codes, in-band SPS/PPS): `track.annexB` is true and there is no `description`. Chrome's WebCodecs accepts that
  without a `description`; the codec string is `avc1.PPCCLL` or `hev1.*` from the in-band SPS. We did not build an avcC/hvcC record: if a
  decoder needs length-prefixed NALs, `annexBToLengthPrefixed(packet.data, 4)` converts an access unit.
- TrueHD PIDs carry an AC-3 core as well: it is its own track (`id = 0x10000 | pid`, codec `ac-3`). Track lists can grow while reading
  (`tracks()` refreshes); `enhancementLayer` tracks are never used for `seek`.
- HDMV LPCM: `codec 'pcm'` with `track.pcm.format 'hdmv'`, the 4 byte header is in `description` and at the start of each packet.
- DTS-HD tracks report the channel count of the extension substream (8 for 7.1), not the 5.1 of the core.

Codec strings: `hvc1.<profile>.<compat>.<L|H><level>.<constraints>` from hvcC, `avc1.PPCCLL`, `av01.P.LLT.DD`, `vp09.PP.LL.DD`,
`mp4a.40.<aot>`, `opus`, `flac`, `mp3`, `vorbis`, and `truehd|dts|eac3|ac3|pcm` for engine-decoded audio. Anything else is
passed through as the container codec id (VC-1 reports `V_MS/VFW/FOURCC`). Subtitles: `ass|srt|webvtt|pgs|vobsub`.
`description` is the container CodecPrivate (hvcC, avcC, av1C, ASC, OpusHead, fLaC...). The Matroska HEVC files in the fixtures carry
an hvcC with no parameter sets (SPS/PPS are in band), so relabelling has to happen on the packets.

Dolby Vision:
- `splitDolbyVision(packet, track|lengthSize=4)` returns `{bl, el, rpu, rpus}`: `bl` and `el` are length-prefixed NAL runs (the type-63
  wrapper is removed from EL NALs), `rpu` is the first type-62 NAL without prefix.
- `relabelForChrome(track, patch = SDR_CARRIER)` returns `{track, original}`; `track.description` has every hvcC SPS retagged
  (primaries 1, transfer 13, matrix kept), `original` is the stream's colour (`null` when the hvcC holds no SPS).
- `relabelPacketSps(data, lengthSize, patch)` retags SPS NALs inside a length-prefixed access unit, returns `{data, original}`;
  pass the original colour as `patch` to restore it.
- `initEngine()` compiles the module for helper use without a demuxer.

RPU display metadata and GPU tables (`dovi.mjs`, replaces `web/fel/fel-tables.mjs`):
```js
import { createDoviSession } from './dovi.mjs';
const s = await createDoviSession();      // own wasm instance = one mapping history, one per playback / worker
s.reset();                                // load and every seek (new epoch, forgets use_prev_vdr_rpu history and inherited DM fields)
const { display, tables, tablesError } = s.parseRpu(rpuNal);  // rpuNal: one type-62 NAL (2 byte header, no start code), e.g. splitDolbyVision(p).rpu
```
`tables` is an `Int32Array(GPU_TABLE_WORDS = 6832)` (null with `tablesError` when the table build fails, e.g. a `use_prev_vdr_rpu` RPU right after `reset`); `parseRpu(nal, {tables: false})`
skips it. `s.display(nal)`, `s.tables(nal)` and `s.inspect(nal)` are the parts. Every RPU passes the CRC and `DoviRpuGuard` before libdovi parses it; a rejected RPU throws `AetherError` (`dovi_rpu_display`, status -300 too large,
-301 CRC / envelope, -302 unsupported syntax, -303 parse rejected). `display` is the JSON of `aether_dovi_rpu_display` (layout: `rust/aether-dovi/src/display.rs`): `profile, compressed, hasDm, usePrevMapping,
dmId, sceneRefresh, yccToRgb[9] (/2^13), yccToRgbOffset[3] (/2^28), rgbToLms[9] (/2^14), signal{eotf,eotfParam,bitDepth,colorSpace,chromaFormat,fullRange}, source{minPq,maxPq,diagonal}, l1{minPq,avgPq,maxPq},
l2[{targetMaxPq,slope,offset,power,chromaWeight,saturationGain,msWeight}], l3, l4, l5{left,right,top,bottom}, l6{maxMasteringLuminance,minMasteringLuminance,maxCll,maxFall}, l8[], l9, l10[], l11, cm[29|40]`, plus
`norm` (floats: `yccToRgb`, `yccToRgbOffset`, `rgbToLms`, `sourceMinPq/MaxPq` 0..1, `sourceMinNits/MaxNits`) and `inherited` (a `compressed` RPU takes the matrices / signal / source of the last full RPU of the epoch). PQ values are
12 bit codes; `pq12ToNits(code)` converts. Checked field by field against `dovi_tool export` (libdovi's serde output) on P5, P7 FEL, P7 MEL, P8.1 and P7 CM v4.0 RPUs (`test/dovirpu.test.mjs`).

Dolby Vision codec strings: a track with a dvcC / dvvC record has `dolbyVisionCodecString` (`dvh1.PP.LL`, `dvhe.PP.LL` when the parameter sets are in band, `dva1` / `dvav` / `dav1` for AVC / AV1) and
`fallbackCodecString` (the base layer string from hvcC: `hvc1.2.4.H153.B0`, tier / level / constraint bytes from the record, at least one constraint byte). MP4 `codec` is the Dolby Vision string (a plain `hvc1` entry with a dvcC box
now also reports `dvh1.PP.LL`, never `hvc1.08.09`); a Matroska track keeps the base layer string in `codec`. `dolbyVisionCodecString(dv, {inband, base})` builds one from `track.dolbyVision`.

Matroska keyframes: for H.264 / HEVC `key` is the container flag AND a payload check (IDR / IRAP, `aether_demux_verify_keyframe`), so it equals ffprobe's flag on POCAWE (25 of 1436) where muxers flag non-IDR I pictures
as keys. The first picture of a file can be `key: false`; a decoder still has to start there. Streams with recovery point SEI keyframes (ffprobe marks them) stay non-key here (stricter than the container, as before for `seek`).

Audio: `createAudioPlugin(track, {presentation = 2, dtsx = false})` returns `null` for `mp4a.*`, `opus`, `flac`, `mp3`, `vorbis`
and a plugin for `truehd`, `dts`, `ac3`, `eac3` and `pcm` (MP4 spellings `ac-3`, `ec-3`, `mlpa`, `dtsc|h|e|l`, `pcm-s16le`... are
accepted, see `engineCodec`). Plugin: `configure(track)`, `decode(packet)` ->
`{frames, sampleRate, channels, layout: string[], planar: Float32Array[], lossless, joc}` (zero frames while priming), `flush()`
(reset after a seek), `close()`. Lossless output is 24-bit integers divided by 2^23, exact in f32. TrueHD presentation 2 is the
7.1 bed, 3 the full object presentation (16 channels). `dtsx: true` selects the DTS:X 7.1.4 presentation (12 channels).
`joc` is `false`, or `{objects, rendered}` for E-AC-3 streams that signal Dolby Atmos (JOC). By default the objects are decoded
(`aether-joc`) and rendered with the TrueHD Atmos point panner to **7.1.4** (12 channels `FL FR FC LFE SL SR BL BR TFL TFR TBL TBR`,
`rendered: true`, `objects` = decoded object count), sample aligned with the bed (the 577 sample object delay is absorbed, the last
12 ms of a stream are not produced). `createAudioPlugin(track, {joc: false})` or `plugin.setJoc(false)` (from the next packet on)
returns the 5.1/7.1 bed instead (`rendered: false`, identical to the plain E-AC-3 decode). Differences from the TrueHD renderer
(size, zones, snap, screen anchoring ignored instead of refused) are listed in `rust/aether-audio/src/joc.rs`.
`layout` uses the canonical names of `rust/README.md` ("Channel layouts"): `FL FR FC LFE BL BR SL SR ...`; `AetherWasm.audioLayoutTable()`
returns the WAVE and SMPTE orders. LPCM: `pcmParams(track)` derives the format from Matroska `A_PCM/*` (+ `bitDepth`), MP4 entries
(`sowt twos in24 in32 fl32 fl64 raw ipcm fpcm`, `pcmC` in `description`) or `track.pcm = {format: 'int'|'float'|'hdmv', bigEndian, signed, bits, layoutMask}`
(needed for `lpcm`; `hdmv` = Blu-ray LPCM whose packets start with the 4 byte header). Errors are `AetherError` with `.detail`
(the engine's own message from `aether_last_error`) and `.code`.

## VC-1 video (`vc1.mjs`)

```js
import { createVc1Decoder, isVc1Track } from './vc1.mjs';
const dec = await createVc1Decoder(track);                    // own wasm instance; track.description (BITMAPINFOHEADER + sequence header + entry point) is parsed
for (const f of dec.decode(packet.data, packet.ptsUs)) f;     // display order {ptsUs, width, height, y, u, v (packed 8 bit planes), interlaced, topFieldFirst, repeatFirstField, key, pictType}
dec.reset();                                                  // seek; dec.flush() at the end of the stream; dec.info(); dec.errors; dec.close()
```
Matroska `V_MS/VFW/FOURCC` with FourCC `WVC1` (`isVc1Track`). The CodecPrivate is a 40 byte BITMAPINFOHEADER (its `biSize` field is the size of the whole record in the lab files) followed by the sequence header and entry point units (`vc1Extradata`). Simple and Main profile (`WMV3`) are not implemented and throw. The block timestamps of the lab files are in decode order, the decoder returns pictures in display order: each returned picture takes the smallest timestamp still waiting (I and P pictures end up on their own timestamp, B pictures between), `reorderPts = false` keeps the decoder's own. First 120 frames of both lab files equal `ffmpeg -f framemd5` (`test/vc1.test.mjs`); the Rust decoder matches all 1432 / 1077 frames.

## Threading design

The Rust host read is synchronous (`aether_host_read` returns before the demux call returns), and `nextPacket` / `seek` / `decode`
are synchronous too, so the whole engine lives in one dedicated Worker that owns the demuxer (and audio plugins) and posts packets out.

- `Blob`/`File`: `FileReaderSync` on `blob.slice()` inside the Worker, nothing is copied up front. Off a Worker it falls back to loading the whole blob.
- `{url}`: synchronous `XMLHttpRequest` with a `Range` header inside the Worker (allowed in dedicated Workers) and a 1 MiB block LRU
  cache (32 blocks). `{url, transport(url, range, headers) -> {status, contentRange, body}}` replaces the XHR (used by the tests to count bytes). This was picked over a SharedArrayBuffer + `Atomics.wait` bridge because it needs no second thread and no
  COOP/COEP; it needs a server that answers Range with 206. The main thread cannot use it and `openDemuxer({url})` throws there.
- Each `AetherWasm` instance holds one demuxer and one audio decoder (thread-local state in Rust). The compiled module is shared;
  every `openDemuxer` and every plugin instantiates its own instance (a few MB of memory each).
- `openDemuxer` reads headers only (`Demuxer::open_lazy`); `nextPacket` and `seek` index on demand inside the engine (1 MiB steps, around the
  target for a seek), and no packet table lives in JS. Bytes fetched over HTTP before the first packet (1 MiB blocks, measured with the counting
  tests): M2TS 1.3 to 2.2 MiB, MP4 1.6 MiB, Matroska 2.9 to 9.5 MiB (clusters are walked block header by block header in bounded steps; the rest is the adapter's dts look-ahead of at least 16 video frames, so it grows with the frame size).
  Seeking to the middle fetches 8 to 16 MiB for M2TS (timestamp search), 2.6 MiB for progressive MP4, 2 to 7 MiB for Matroska (a Cue jump lands on the block named by CueClusterPosition + CueRelativePosition; the dts look-ahead is again most of it).
- Matroska stores no DTS (the engine reports dts == pts). For video tracks the adapter reads ahead up to 64 packets and sets
  `dts[k] = k-th smallest pts - constant`, so dts is monotonic and dts <= pts.

## Tests

```sh
node --test web/engine/test/            # Node 22+ resolves the directory to test/index.js, which loads every *.test.mjs
node --test web/engine/test/*.test.mjs  # same
```

`ts.test.mjs` opens the M2TS lab files and POCAWE (tracks, engine audio decode against ffmpeg: lossless exact, AC-3 family within 1e-4, seek, HTTP byte counts); `subtitles.test.mjs` feeds the PGS track to web/subs.

Needs local test media in `test-media/` or `$AETHER_LAB` (see [docs/testing.md](../../docs/testing.md));
missing files skip. Audio tests decode the first 3 s and compare with `ffmpeg -f f32le` (cached in `test/fixtures/`, git-ignored) after mapping
channels by label (`-drc_scale 0` for AC-3 family): lossless codecs and LPCM must match to 1e-6 (they match exactly), AC-3/E-AC-3 to 1e-6
(observed 1.5e-7), lossy DTS core to 1e-5 (observed 5.5e-6). DTS-HRA is asserted on format and a bounded residual only (max 0.11, X96/XBR not decoded). Dolby Vision tests check the BL/EL/RPU split, the SPS relabel round trip and hvcC relabelling.

## Known gaps

- DTS-HRA decodes the core, XXCH and the 96 kHz output rate but not the X96/XBR residual (max abs diff vs ffmpeg 0.11).
- Matroska `ContentEncodings` are undone by the engine (packet `data.length` can differ from the stored size). No VC-1 decoder.
- HDMV LPCM 20 bit is refused.
- `seek` repositions on the video keyframe; audio packets then start at the same storage position, so the player drops audio before its target.
