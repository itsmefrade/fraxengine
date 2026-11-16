#!/bin/sh
# Regenerates the MP4/MOV fixtures and the ffprobe oracles (dev-time only; needs ffmpeg and ffprobe).
#   sh generate.sh            synthetic fixtures + oracles for the real files that exist on this machine
# Each fixture NAME.mp4 gets NAME.csv: one line per packet from
#   ffprobe -show_packets -show_data_hash sha256
# with columns stream_index,pts,dts,duration,size,pos,flags,data_hash.
set -u
cd "$(dirname "$0")"
FFMPEG=${FFMPEG:-/opt/homebrew/bin/ffmpeg}
FFPROBE=${FFPROBE:-/opt/homebrew/bin/ffprobe}
ROOT=$(cd ../../../.. && pwd)

probe() { # file out.csv [max packets, 0 = all]
  if [ "${3:-0}" -gt 0 ]; then
    "$FFPROBE" -v error -read_intervals "%+#$3" -show_packets -show_data_hash sha256 -of csv=p=0 \
      -show_entries packet=stream_index,pts,dts,duration,size,pos,flags,data_hash "$1" > "$2"
  else
    "$FFPROBE" -v error -show_packets -show_data_hash sha256 -of csv=p=0 \
      -show_entries packet=stream_index,pts,dts,duration,size,pos,flags,data_hash "$1" > "$2"
  fi
}

V="-f lavfi -i testsrc2=size=128x72:rate=24:duration=2"
A="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=2"
A6="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=2"
PROG="-movflags +faststart"
FRAG="-movflags frag_keyframe+empty_moov+default_base_moof+delay_moov"
FRAG2="-movflags frag_keyframe+empty_moov+negative_cts_offsets"
X264="-c:v libx264 -g 12 -bf 2 -pix_fmt yuv420p"
X265="-c:v libx265 -x265-params log-level=none:keyint=12:bframes=2 -pix_fmt yuv420p -tag:v hvc1"

make() { # name, then ffmpeg args
  name=$1; shift
  if "$FFMPEG" -v error -y "$@" "$name" 2>"$name.err"; then
    probe "$name" "${name%.*}.csv"; rm -f "$name.err"
  else
    echo "skipped $name: $(head -1 "$name.err")"; rm -f "$name" "$name.err"
  fi
}

for kind in prog frag; do
  if [ $kind = prog ]; then F="$PROG"; else F="$FRAG"; fi
  make h264-aac-$kind.mp4   $V $A $X264 -c:a aac -b:a 96k -shortest $F
  make hevc-opus-$kind.mp4  $V $A $X265 -c:a libopus -b:a 96k -shortest $F
  make hevc-flac-$kind.mp4  $V $A $X265 -c:a flac -strict -2 -shortest $F
  make h264-ac3-$kind.mp4   $V $A $X264 -c:a ac3 -b:a 192k -shortest $F
  make h264-eac3-$kind.mp4  $V $A $X264 -c:a eac3 -b:a 192k -shortest $F
  make h264-truehd-$kind.mp4 $V $A $X264 -c:a truehd -strict -2 -shortest $F
  make h264-dts-$kind.mp4   $V $A $X264 -c:a dca -strict -2 -ar 48000 -shortest $F
  make audio-aac-$kind.mp4  $A -c:a aac -b:a 96k $F
  make pcm-s16-$kind.mp4    $A -c:a pcm_s16le -strict -2 $F
  make vp9-$kind.mp4        $V -c:v libvpx-vp9 -b:v 200k $F
  make av1-$kind.mp4        $V -c:v libsvtav1 -g 12 $F
done
make hevc-aac-frag-negcts.mp4 $V $A $X265 -c:a aac -shortest $FRAG2
make pcm-s16.mov          $A -c:a pcm_s16le
make pcm-s24.mov          $A -c:a pcm_s24le
make h264-aac-pcm.mov     $V $A $X264 -c:a pcm_s24le -shortest
# ffmpeg writes duplicate tfdt values here and ffprobe stops early; the tests compare it with the progressive file.
make h264-aac-frag-everyframe.mp4 $V $A $X264 -c:a aac -shortest -movflags frag_every_frame+empty_moov+default_base_moof
make h264-aac-dash.mp4    $V $A $X264 -c:a aac -shortest -f mp4 -movflags dash+frag_keyframe+empty_moov
make h264-aac-sidx.mp4    $V $A $X264 -c:a aac -shortest -movflags frag_keyframe+empty_moov+default_base_moof+global_sidx
# Fragmented file with an mfra (random access) box at the end.
make h264-aac-mfra.mp4    $V $A $X264 -c:a aac -shortest -movflags frag_keyframe+empty_moov+default_base_moof -write_tmcd 0

# Real files. Oracles for the large ones are limited to the first packets.
mkdir -p real
M=${AETHER_LAB:-$(cd "$(dirname "$0")/../../../.." && pwd)/test-media}
i=0
while IFS= read -r f; do
  [ -f "$f" ] || continue
  i=$((i+1)); base=$(basename "$f" | tr -c 'A-Za-z0-9._\n-' '_')
  probe "$f" "real/$base.csv" 900
done <<LIST
$ROOT/test-media/full-original-source.mp4
$ROOT/test-media/audio-official-joc-channel-id.mp4
/private/tmp/aether-fel-truehd-frag.mp4
/private/tmp/aether-fel-truehd-signed-cts.mp4
/private/tmp/aether-fel-truehd-audio-first.mp4
/private/tmp/aether-fel-truehd-extended-mdat.mp4
$M/Test Jellyfin 4K DV P5.mp4
$M/Test Jellyfin 4K DV P8.1.mp4
$M/Test Jellyfin 1080p DV P8.4.mp4
$M/DV8_TEST.mp4
$M/Test Jellyfin 4K HEVC HDR10 40M.mp4
$M/Test Jellyfin 4K AV1 HDR10 40M.mp4
$M/ChID-BLITS-EBU-Narration441AOT2.mp4
$M/DTS-UHD DTSX P2 Sample.mp4
$M/iPhone11_4K-recorder_59.940DV+HLG.mov
$M/Audio_Video_Sync_23,98_HEVC_1080p-by_PhotoJoseph.mov
$M/Audio_Video_Sync_24,00_HEVC_1080p-by_PhotoJoseph.mov
$M/Audio_Video_Sync_29,97_HEVC_1080p-by_PhotoJoseph.mov
LIST
ls
