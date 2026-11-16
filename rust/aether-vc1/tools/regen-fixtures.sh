#!/bin/bash
# Regenerate the git-ignored conformance fixtures from real VC-1 media (read-only access):
#   fixtures/lpcm71.vc1   fixtures/dtshra51.vc1   raw Annex G elementary streams
#   fixtures/*.md5        per-frame MD5 of FFmpeg's decode (yuv420p), whitespace separated
# usage: tools/regen-fixtures.sh [media-dir]
set -eu
HERE="$(cd "$(dirname "$0")/.." && pwd)"
MEDIA="${1:-${VC1_MEDIA_DIR:-${AETHER_LAB:-$(cd "$(dirname "$0")/../../.." && pwd)/test-media}}}"
OUT="$HERE/fixtures"
mkdir -p "$OUT"
extract() { # name file
  ffmpeg -nostdin -v error -y -i "$2" -map 0:v:0 -c copy -f vc1 "$OUT/$1.vc1"
  # -cpuflags 0: canonical C code paths (identical output for conformant streams)
  ffmpeg -nostdin -v error -y -cpuflags 0 -i "$OUT/$1.vc1" -f framemd5 - | grep -v '^#' | awk '{print $NF}' > "$OUT/$1.md5"
  echo "$1: $(wc -l < "$OUT/$1.md5") frames"
}
extract lpcm71 "$MEDIA/VC1-1080p23.976-LPCM7.1.mkv"
extract dtshra51 "$MEDIA/DTS-HRA5.1_VC1-23.976.mkv"
