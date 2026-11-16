#!/bin/sh
# Regenerates tests/data/real/*.rpu (git-ignored) from the manual test lab media.
# Needs ffmpeg and dovi_tool. Usage: regen-real-rpus.sh [media-dir] [frames]
set -eu
media=${1:-${AETHER_LAB:-$(cd "$(dirname "$0")/../../../.." && pwd)/test-media}}
frames=${2:-12}
out=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/real
mkdir -p "$out"
extract() {
  ffmpeg -v error -y -i "$media/$1" -map 0:v:0 -c:v copy -bsf:v hevc_mp4toannexb \
    -frames:v "$frames" -f hevc - | dovi_tool extract-rpu -i - -o "$out/$2.rpu" >/dev/null
}
extract "Test Jellyfin 4K DV P5.mp4" p5
extract "awaken-girl.4K.HDR.DV.mkv" p7-mel
extract "Test Jellyfin 4K DV P8.1.mp4" p81
ls -l "$out"
