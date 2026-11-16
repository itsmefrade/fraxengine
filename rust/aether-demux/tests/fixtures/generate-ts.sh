#!/bin/sh
# Regenerates the MPEG-TS / M2TS fixtures and ffprobe oracles (dev-time only; needs ffmpeg and ffprobe).
#   sh generate-ts.sh     synthetic fixtures (ts/, all git-ignored) + oracles for the lab M2TS files on this machine
# Oracle rows: stream_index,pts,dts,duration,size,pos,flags,data_hash from
#   ffprobe -show_packets -show_data_hash sha256
# Real files: ts-real/NAME.csv has the first 500 packets of each stream, NAME.seek.csv the packets
# after ffprobe seeks to the time in NAME.seek.time, NAME.counts the packet count per stream.
set -u
cd "$(dirname "$0")"
FFMPEG=${FFMPEG:-/opt/homebrew/bin/ffmpeg}
FFPROBE=${FFPROBE:-/opt/homebrew/bin/ffprobe}
LAB=${LAB:-${AETHER_LAB:-$(cd "$(dirname "$0")/../../../.." && pwd)/test-media}}
mkdir -p ts ts-real
SHOW="-show_packets -show_data_hash sha256 -of csv=p=0 -show_entries packet=stream_index,pts,dts,duration,size,pos,flags,data_hash"

probe_all() { "$FFPROBE" -v error $SHOW "$1" > "$2"; }
cap() { # csv, per-stream cap
  awk -F, -v cap="$2" '{ n[$1]++; if (n[$1] <= cap) print }' "$1" > "$1.tmp" && mv "$1.tmp" "$1"
}

# ---- synthetic fixtures (ffmpeg's own sources, or lab files with stream copy)
V="-f lavfi -i testsrc2=size=320x180:rate=24:duration=4"
A="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=4"
X264="-c:v libx264 -g 12 -bf 2 -pix_fmt yuv420p"
mk() { # name, ffmpeg args
  name=$1; shift
  if "$FFMPEG" -v error -y "$@" "ts/$name" 2>"ts/$name.err"; then
    probe_all "ts/$name" "ts/${name%.*}.csv"; rm -f "ts/$name.err"
  else
    echo "skipped $name: $(head -1 "ts/$name.err")"; rm -f "ts/$name" "ts/$name.err"
  fi
}
mk h264-ac3.ts $V $A $X264 -c:a ac3 -f mpegts
mk h264-eac3-aac.ts $V $A $A -map 0:v -map 1:a -map 2:a $X264 -c:a:0 eac3 -c:a:1 aac -f mpegts
mk hevc-ac3.ts $V $A -c:v libx265 -x265-params log-level=none:keyint=12:bframes=2 -pix_fmt yuv420p -c:a ac3 -f mpegts
mk mpeg2-ac3.ts $V $A -c:v mpeg2video -g 12 -bf 2 -c:a ac3 -f mpegts
mk h264-ac3.m2ts $V $A $X264 -c:a ac3 -f mpegts -mpegts_m2ts_mode 1
# 33 bit PTS wrap: the timeline starts two seconds before 2^33 ticks (95443.7 s)
mk h264-ac3-wrap.ts $V $A $X264 -c:a ac3 -output_ts_offset 95441 -f mpegts
# Timeline jump: two segments joined with a timestamp reset.
if "$FFMPEG" -v error -y $V -c:v libx264 -g 12 -bf 0 -pix_fmt yuv420p -f mpegts ts/seg-a.ts 2>/dev/null &&
   "$FFMPEG" -v error -y $V -c:v libx264 -g 12 -bf 0 -pix_fmt yuv420p -output_ts_offset 500 -f mpegts ts/seg-b.ts 2>/dev/null; then
  cat ts/seg-a.ts ts/seg-b.ts > ts/h264-jump.ts
  rm -f ts/seg-a.ts ts/seg-b.ts
  probe_all ts/h264-jump.ts ts/h264-jump.csv
fi
# VC-1 and DTS-HD from lab files, stream copy into transport streams.
if [ -f "$LAB/VC1-1080p23.976-LPCM7.1.mkv" ]; then
  "$FFMPEG" -v error -y -fflags +genpts -t 6 -i "$LAB/VC1-1080p23.976-LPCM7.1.mkv" -map 0:v:0 -c copy -muxdelay 0 -f mpegts ts/vc1.ts 2>/dev/null && probe_all ts/vc1.ts ts/vc1.csv
fi
if [ -f "$LAB/dts_hd_master_audio_sound_check_5-1_lossless.mkv" ]; then
  "$FFMPEG" -v error -y -t 6 -i "$LAB/dts_hd_master_audio_sound_check_5-1_lossless.mkv" -map 0:v:0 -map 0:a:0 -c copy -f mpegts ts/dts-hd.ts 2>/dev/null && probe_all ts/dts-hd.ts ts/dts-hd.csv
fi

# ---- lab M2TS files
for f in "Dolby Amaze.ATMOS.m2ts" "Dolby TrueHD 7.1 Channel Check.m2ts" "DTSX_Demo_2016.m2ts" "DTS-HD MA 7.1 'Dredd' Sound Check.m2ts"; do
  [ -f "$LAB/$f" ] || { echo "no $f"; continue; }
  base=$(printf '%s' "$f" | tr -c 'A-Za-z0-9.' '_')
  "$FFPROBE" -v error -read_intervals "%+#4000" $SHOW "$LAB/$f" > "ts-real/$base.csv"; cap "ts-real/$base.csv" 500
  start=$("$FFPROBE" -v error -show_entries format=start_time -of csv=p=0 "$LAB/$f")
  dur=$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$LAB/$f")
  seek=$(awk -v s="$start" -v d="$dur" 'BEGIN { printf "%.3f", s + d * 0.45 }')
  echo "$seek" > "ts-real/$base.seek.time"
  "$FFPROBE" -v error -read_intervals "$seek%+#3000" $SHOW "$LAB/$f" > "ts-real/$base.seek.csv"; cap "ts-real/$base.seek.csv" 400
  "$FFPROBE" -v error -count_packets -show_entries stream=index,nb_read_packets -of csv=p=0 "$LAB/$f" > "ts-real/$base.counts"
done
