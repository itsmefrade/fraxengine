#!/bin/sh
# Regenerates the raw elementary stream fixtures used by tests/raw.rs and web/engine/test/raw.test.mjs.
# The outputs are git-ignored (see .gitignore here). Needs ffmpeg; the *-lab files also need the
# manual test lab media and are skipped when it is absent.
set -eu
cd "$(dirname "$0")"
LAB=${AETHER_LAB:-$(cd "$(dirname "$0")/../../../.." && pwd)/test-media}
ff() { ffmpeg -v error -y "$@"; }
SRC="-f lavfi -i aevalsrc=0.3*sin(2*PI*440*t)|0.3*sin(2*PI*660*t)|0.2*sin(2*PI*220*t)|0.1*sin(2*PI*110*t)|0.2*sin(2*PI*880*t)|0.2*sin(2*PI*990*t)|0.2*sin(2*PI*330*t)|0.2*sin(2*PI*550*t):s=48000:d=14"
# shellcheck disable=SC2086
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a dca -strict -2 -b:a 1509k -f dts tone51.dts
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a truehd -strict -2 -f truehd tone51.thd
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a mlp -strict -2 -f mlp tone51.mlp
ff $SRC -c:a pcm_s24le -f wav sine71-s24.wav
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a pcm_f32le -f wav sine51-f32.wav
ff $SRC -af "pan=stereo|c0=c0|c1=c1" -c:a pcm_s16le -f wav stereo-s16.wav
ff $SRC -c:a pcm_s24le -rf64 always -f wav sine71-s24-rf64.wav
ff $SRC -c:a pcm_s32le -f w64 sine71-s32.w64
ff $SRC -af "pan=stereo|c0=c0|c1=c1" -c:a aac -b:a 128k -f adts stereo.aac
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a aac -b:a 384k -f adts tone51.aac
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a ac3 -b:a 448k -f ac3 tone51.ac3
ff $SRC -af "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5" -c:a eac3 -b:a 640k -f eac3 tone51.eac3
ff $SRC -c:a flac -f flac sine71.flac
ff $SRC -af "pan=stereo|c0=c0|c1=c1" -c:a flac -sample_fmt s32 -f flac stereo-s24.flac
if [ -n "$LAB" ] && [ -d "$LAB" ]; then
  [ -f "$LAB/dts_hd_master_audio_sound_check_5-1_lossless.mkv" ] && ff -t 20 -i "$LAB/dts_hd_master_audio_sound_check_5-1_lossless.mkv" -map 0:a:0 -c copy -f dts dtshd-ma-lab.dts || true
  [ -f "$LAB/Dolby TrueHD 7.1 Channel Check.m2ts" ] && ff -t 20 -i "$LAB/Dolby TrueHD 7.1 Channel Check.m2ts" -map 0:a:0 -c copy -f truehd truehd71-lab.thd || true
  [ -f "$LAB/EAC3-JOC ATMOS.mkv" ] && ff -t 20 -i "$LAB/EAC3-JOC ATMOS.mkv" -map 0:a:0 -c copy -f eac3 eac3-joc-lab.eac3 || true
fi
ls -l
