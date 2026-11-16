#!/usr/bin/env bash
# Regenerates the decoder test fixtures under tests/fixtures/ with FFmpeg.
# FFmpeg is used at development time only; nothing here ships with the crate.
#
# Every fixture is a raw elementary stream (<name>.ac3 / <name>.eac3) plus the
# reference decode produced by FFmpeg's float decoder with dynamic range
# compression disabled (<name>.ref.f32le, interleaved f32le in FFmpeg's native
# channel order for the stream).
#
# Usage: tests/gen_fixtures.sh [path/to/ffmpeg]
#   AETHER_REPO_FIXTURES  directory holding the real-world samples
#                         (default: <repo>/test-media)
#   AETHER_FIXTURE_SECS   length of the real-world excerpts (default 20)
set -euo pipefail

FFMPEG="${1:-/opt/homebrew/bin/ffmpeg}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/fixtures"
REPO="$(cd "$HERE/../../.." && pwd)"
REAL="${AETHER_REPO_FIXTURES:-$REPO/test-media}"
SECS="${AETHER_FIXTURE_SECS:-20}"
mkdir -p "$OUT"

ff() { "$FFMPEG" -hide_banner -loglevel error -y "$@"; }

# Decode a raw stream with FFmpeg's float decoder, DRC off, native layout.
reference() { # <codec> <in> <out>
  ff -drc_scale 0 -c:a "$1" -i "$2" -f f32le "$3"
}

# Deterministic multi-channel test signal: per-channel tones, a slow tremolo,
# noise, and a shared burst component so that coupling and rematrixing have
# something to chew on. $1 = channel count, $2 = seconds.
signal() {
  local n="$1" dur="$2" exprs="" k
  for ((k = 0; k < n; k++)); do
    local e="0.18*sin(2*PI*$((110 * (k + 1)))*t)"
    e+="+0.10*sin(2*PI*$((1000 + 330 * k))*t)*(0.5+0.5*sin(2*PI*$(echo "0.5 + 0.13 * $k" | bc -l)*t))"
    e+="+0.06*(random($k)-0.5)"
    e+="+0.15*sin(2*PI*440*t)*gt(mod(t,1),0.9)"
    e+="+0.12*sin(2*PI*5200*t)*gt(mod(t,0.7),0.65)"
    exprs+="${exprs:+|}$e"
  done
  echo "aevalsrc=exprs='$exprs':s=48000:d=$dur"
}

make() { # <name> <codec> <channels> <seconds> [encoder args...]
  local name="$1" codec="$2" ch="$3" dur="$4"
  shift 4
  local ext="$codec"
  local layout
  case "$ch" in
    1) layout=mono ;;
    2) layout=stereo ;;
    6) layout="5.1(side)" ;;
    *) layout="$ch" ;;
  esac
  ff -f lavfi -i "$(signal "$ch" "$dur")" -af "aformat=channel_layouts=$layout" \
    -c:a "$codec" "$@" -f "$ext" "$OUT/$name.$ext"
  reference "$codec" "$OUT/$name.$ext" "$OUT/$name.ref.f32le"
  echo "fixture $name: $(stat -f %z "$OUT/$name.$ext" 2>/dev/null || stat -c %s "$OUT/$name.$ext") bytes"
}

make ac3-mono ac3 1 6 -b:a 96k
make ac3-stereo ac3 2 8 -b:a 192k
make ac3-stereo-lowrate ac3 2 8 -b:a 96k -channel_coupling 1
make ac3-51 ac3 6 8 -b:a 448k
make ac3-51-coupled ac3 6 8 -b:a 192k -channel_coupling 1
make eac3-stereo eac3 2 8 -b:a 192k
make eac3-51 eac3 6 8 -b:a 640k
make eac3-51-coupled eac3 6 8 -b:a 192k -channel_coupling 1

# Synthetic spectral-extension streams (no encoder or sample media uses SPX).
python3 -I "$HERE/gen_spx_fixture.py" "$OUT"
reference eac3 "$OUT/spx-synth-mono.eac3" "$OUT/spx-synth-mono.ref.f32le"
reference eac3 "$OUT/spx-synth-stereo.eac3" "$OUT/spx-synth-stereo.ref.f32le"

# Real-world material.
if [[ -f "$REAL/audio-official-joc-channel-id.mp4" ]]; then
  ff -i "$REAL/audio-official-joc-channel-id.mp4" -map 0:a:0 -c copy -t "$SECS" -f eac3 "$OUT/joc-51.eac3"
  reference eac3 "$OUT/joc-51.eac3" "$OUT/joc-51.ref.f32le"
fi
if [[ -f "$REAL/full-original-source.mp4" ]]; then
  ff -i "$REAL/full-original-source.mp4" -map 0:a:0 -c copy -t "$SECS" -f ac3 "$OUT/film-51.ac3"
  reference ac3 "$OUT/film-51.ac3" "$OUT/film-51.ref.f32le"
fi
if [[ -f "$REAL/eac3-joc-signalling-silence.mp4" ]]; then
  ff -i "$REAL/eac3-joc-signalling-silence.mp4" -map 0:a:0 -c copy -f eac3 "$OUT/joc-signalling.eac3"
  reference eac3 "$OUT/joc-signalling.eac3" "$OUT/joc-signalling.ref.f32le"
fi

# Extra real-world media (optional): a directory of larger test files. Each
# call stream-copies a few seconds of the first audio stream of a codec
# (extract <fixture name> <file> <codec> <seconds> [start seconds]).
EXTRA="${AETHER_EXTRA_MEDIA:-${AETHER_LAB:-$(cd "$(dirname "$0")/../../.." && pwd)/test-media}}"
extract() { # <name> <file> <codec> <secs> [skip secs]
  local name="$1" file="$EXTRA/$2" codec="$3" secs="$4" skip="${5:-0}"
  [[ -f "$file" ]] || { echo "skip $name (no $file)"; return 0; }
  local idx
  idx=$("${FFMPEG%ffmpeg}ffprobe" -v error -select_streams a -show_entries stream=index,codec_name \
    -of csv=p=0 "$file" | awk -F, -v c="$codec" '$2==c{print $1; exit}')
  [[ -n "$idx" ]] || { echo "skip $name (no $codec stream)"; return 0; }
  ff -ss "$skip" -i "$file" -map "0:$idx" -c copy -t "$secs" -f "$codec" "$OUT/$name.$codec"
  reference "$codec" "$OUT/$name.$codec" "$OUT/$name.ref.f32le"
  echo "fixture $name"
}
extract real-71-dd-plus 7_pt_1.eac3 eac3 12 20
extract real-71-channel-check "Dolby Digital Plus 7.1 Channel Check.mkv" eac3 12
extract real-joc-atmos-mkv "EAC3-JOC ATMOS.mkv" eac3 12 60
extract real-joc-hdr10plus HDR10Plus_PB_EAC3JOC.mkv eac3 12 60
extract real-joc-71-amaze "Dolby Amaze.ATMOS.m2ts" eac3 12 20
extract real-ac3-amaze "Dolby Amaze.ATMOS.m2ts" ac3 12 20
extract real-ac3-canyon Canyon-5.1-48khz-448kbit.ac3 ac3 12 30

echo "fixtures written to $OUT"
