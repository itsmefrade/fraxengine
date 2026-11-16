#!/bin/sh
# Larger fragmented MP4 files for the lazy indexing tests (git-ignored, dev-time only).
set -u
cd "$(dirname "$0")"
FFMPEG=${FFMPEG:-/opt/homebrew/bin/ffmpeg}
LAB=${LAB:-${AETHER_LAB:-$(cd "$(dirname "$0")/../../../.." && pwd)/test-media}}
mkdir -p lazy
SRC="$LAB/Test Jellyfin 4K HEVC HDR10 40M.mp4"
[ -f "$SRC" ] || { echo "no $SRC"; exit 0; }
# Two second fragments with a global sidx, and the same without any random access box.
"$FFMPEG" -v error -y -i "$SRC" -t 20 -c copy -movflags frag_keyframe+empty_moov+default_base_moof+global_sidx lazy/big-frag-sidx.mp4
"$FFMPEG" -v error -y -i "$SRC" -t 20 -c copy -movflags frag_keyframe+empty_moov+default_base_moof lazy/big-frag-plain.mp4
