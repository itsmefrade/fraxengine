#!/bin/sh
# Headless Chrome run of web/exact/bench-ref.html (own profile, server on 8790 serving web/ and accepting POST /results).
# Optional env EXTRA, e.g. EXTRA='&pace=60&pre=4&budget=1024&qmax=12' for a paced playback simulation (counts missed ticks).
# Usage: tools/chrome-bench.sh <variant base|ref> <stream fel|awaken|p5> <n workers> <frames> <hash 0|1> [timeout s]
cd "$(dirname "$0")/../../../web"
V=$1; S=$2; N=$3; F=$4; H=$5; T=${6:-400}
NAME="hevc-$V-$S-n$N-f$F-h$H-$$"
PROFILE=$(mktemp -d /tmp/aether-hevc-chrome.XXXXXX)
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --user-data-dir="$PROFILE" --no-first-run \
  --no-default-browser-check --disable-background-timer-throttling --disable-renderer-backgrounding \
  --disable-backgrounding-occluded-windows --window-size=800,600 \
  "http://127.0.0.1:8790/exact/bench-ref.html?variant=$V&stream=$S&n=$N&frames=$F&hash=$H&name=$NAME$EXTRA" >/dev/null 2>&1 &
PID=$!
i=0
while [ $i -lt $T ]; do
  f=$(grep -l "\"name\": \"$NAME\"" results/*.json 2>/dev/null | head -1)
  if [ -n "$f" ]; then cat "$f" | python3 -c "import json,sys; d=json.load(sys.stdin); d.pop('ua',None); print(json.dumps(d))"; break; fi
  sleep 2; i=$((i+2))
done
[ $i -ge $T ] && echo "TIMEOUT $NAME"
kill $PID 2>/dev/null; sleep 1; pkill -f "$PROFILE" 2>/dev/null; rm -rf "$PROFILE"
