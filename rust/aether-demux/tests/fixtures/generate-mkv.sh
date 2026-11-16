#!/bin/sh
# ffprobe oracles for Matroska lab files (dev-time only): mkv-real/NAME.csv (first 300 packets per stream,
# columns stream_index,pts,dts,duration,size,pos,flags,data_hash) and NAME.counts (packets per stream).
set -u
cd "$(dirname "$0")"
FFPROBE=${FFPROBE:-/opt/homebrew/bin/ffprobe}
LAB=${LAB:-${AETHER_LAB:-$(cd "$(dirname "$0")/../../../.." && pwd)/test-media}}
mkdir -p mkv-real
SHOW="-show_packets -show_data_hash sha256 -of csv=p=0 -show_entries packet=stream_index,pts,dts,duration,size,pos,flags,data_hash"
for f in "POCAWE_Sample.mkv" "DTS-HRA 7.1 Orchestra.mkv" "VC1-1080p23.976-LPCM7.1.mkv" "EAC3-JOC ATMOS.mkv"; do
  [ -f "$LAB/$f" ] || { echo "no $f"; continue; }
  base=$(printf '%s' "$f" | tr -c 'A-Za-z0-9.' '_')
  "$FFPROBE" -v error -read_intervals "%+#2500" $SHOW "$LAB/$f" | awk -F, '{ n[$1]++; if (n[$1] <= 300) print }' > "mkv-real/$base.csv"
  "$FFPROBE" -v error -count_packets -show_entries stream=index,nb_read_packets -of csv=p=0 "$LAB/$f" > "mkv-real/$base.counts"
done
