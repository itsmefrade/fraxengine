#!/usr/bin/env bash
# cputime.sh <url> [settle-seconds] [window-seconds]
# Opens <url> in a separate headless Chrome and reports the exact CPU used per Chrome process type over the window, as a
# percentage of one core, from the cumulative CPU time (`ps -o time`) at both ends. `ps %cpu` is a decaying average on macOS
# and once showed the GPU reconstruction path above the exact pool on FEL_TEST_V6 (50 vs 46) when the cumulative time said
# 34 vs 47. Check that the iOS Simulator and other heavy work are idle before measuring; alternate A B A B.
# HEADED=1 opens a visible 1920x1080 app window instead (composited cost included; the screen must stay on).
set -eu
P=$(mktemp -d "${TMPDIR:-/tmp}/aether-cputime.XXXXXX")
if [ "${HEADED:-}" = 1 ]; then
  MODE=(--window-size=1920,1080 --window-position=0,25 --disable-backgrounding-occluded-windows "--app=$1")
else
  MODE=(--headless=new --disable-audio-output "$1")
fi
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --user-data-dir="$P" --no-first-run --no-default-browser-check \
  --autoplay-policy=no-user-gesture-required "${MODE[@]}" >/dev/null 2>&1 &
CH=$!
snap() {
  ps -axo pid=,time=,command= | grep -- "--user-data-dir=$P" | grep -v grep | awk '{
    t = "browser"; for (i = 3; i <= NF; i++) if ($i ~ /^--type=/) { split($i, a, "="); t = a[2] }
    n = split($2, p, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + p[i]; print $1, t, s }'
}
sleep "${2:-15}"; snap > "$P/a"; sleep "${3:-30}"; snap > "$P/b"
awk -v w="${3:-30}" 'NR == FNR { a[$1] = $3; next } { c[$2] += $3 - a[$1] }
  END { printf "renderer %.1f%%  gpu %.1f%%  browser %.1f%%  utility %.1f%%\n",
        100 * c["renderer"] / w, 100 * c["gpu-process"] / w, 100 * c["browser"] / w, 100 * c["utility"] / w }' "$P/a" "$P/b"
kill "$CH"; sleep 1; rm -rf "$P"
