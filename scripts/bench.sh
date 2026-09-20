#!/usr/bin/env bash
# Benchmark a browser: startup time, then PSS while tabs load.
# Usage: bench.sh <process-name> <window-class> [--tabs "url1 url2"] <launch-command...>
#
# Examples:
#   ./scripts/bench.sh br0x org.br0x.Browser ./target/release/br0x
#   ./scripts/bench.sh firefox firefox firefox --profile /tmp/ffprof --new-window about:blank
#   ./scripts/bench.sh brave brave-browser brave --user-data-dir=/tmp/braveprof --no-first-run --new-window about:blank
#
# The script opens tabs with keystrokes after launch, like a user would.
# Keep the machine otherwise idle. On a 7 GB box, nine heavy sites cause
# swapping and every browser will look slow; use --tabs for a smaller set.
set -u
here=$(dirname "$0")
PROC="$1"
WIN="$2"
shift 2

URLS="youtube.com github.com wikipedia.org news.ycombinator.com reddit.com"
if [ "${1:-}" = "--tabs" ]; then
  URLS="$2"
  shift 2
fi

if [ "$#" -eq 0 ]; then
  echo "usage: bench.sh <process-name> <window-class> [--tabs \"urls\"] <launch-command...>"
  exit 1
fi

mem_avail() {
  awk '/MemAvailable/{printf "%.0f MB", $2/1024}' /proc/meminfo
}

window_visible() {
  hyprctl clients -j 2>/dev/null | python3 -c "
import json, sys
want = sys.argv[1].lower()
try:
    clients = json.load(sys.stdin)
except Exception:
    sys.exit(1)
sys.exit(0 if any(want in c['class'].lower() for c in clients) else 1)
" "$WIN"
}

echo "launching: $*"
echo "mem before: $(mem_avail)"
start=$(date +%s%3N)
setsid "$@" >/tmp/bench-launch.log 2>&1 &
for _ in $(seq 1 600); do
  if window_visible; then
    break
  fi
  sleep 0.05
done
end=$(date +%s%3N)
echo "startup to window: $((end - start)) ms"

if ! pgrep -x "$PROC" >/dev/null; then
  echo "launch failed, log:"
  cat /tmp/bench-launch.log
  exit 1
fi

sleep 6
echo "opening tabs: $URLS"
"$here/open_tabs.sh" "class:$WIN" $URLS

"$here/measure.py" "$PROC"
for wait in 15 30 60; do
  sleep "$wait"
  echo "--- +${wait}s ---"
  "$here/measure.py" "$PROC"
  echo "mem available: $(mem_avail)"
done

echo "done. close the browser yourself when finished."
