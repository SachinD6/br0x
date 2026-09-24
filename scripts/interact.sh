#!/bin/bash
# Drive the running br0x window with xdotool and capture the whole screen or a
# specific popup window. Popovers live on their own X surface, so a root dump
# misses them; this script finds the popup and dumps it by id.
#
#   interact.sh start [appearance]
#   interact.sh click <x> <y> [out.png]     # click, then capture
#   interact.sh key <keysym> [out.png]
#   interact.sh shot [out.png]              # full screen
#   interact.sh stop
set -u
TOOLS="${XVFB_ROOT:-/tmp/xvfb/root}/usr/bin"
export DISPLAY=:99
export LD_LIBRARY_PATH=${XVFB_ROOT:-/tmp/xvfb/root}/usr/lib/x86_64-linux-gnu
export XDG_DATA_HOME=/tmp/br0x-home
export XDG_RUNTIME_DIR=/tmp/fake-runtime
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/fake-runtime/nob.us
export GSK_RENDERER=cairo
REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="${BR0X_BIN:-$REPO/target/debug/br0x}"
CONVERT="python3 $REPO/scripts/xwd2png.py"

capture_screen() {
  $TOOLS/xwd -root -silent > /tmp/interact.xwd && $CONVERT /tmp/interact.xwd "$1" >/dev/null
}

capture_popup() {
  # Any mapped window that is not the 1200x800 main window and not 1x1.
  for id in $($TOOLS/xdotool search --onlyvisible --name '.*' 2>/dev/null); do
    geom=$($TOOLS/xdotool getwindowgeometry "$id" | grep Geometry: | awk '{print $2}')
    case "$geom" in
      1200x800|1600x1000|1x1|*x0*|0x*) continue ;;
    esac
    $TOOLS/xwd -id "$id" -silent > /tmp/interact.xwd 2>/dev/null && \
      $CONVERT /tmp/interact.xwd "$1" >/dev/null && echo "popup window $id ($geom)"
  done
}

case "${1:-}" in
  start)
    printf '{"appearance":"%s","sidebar_visible":true}' "${2:-Light}" > /tmp/br0x-home/br0x/prefs.json
    pgrep -x br0x | xargs -r kill
    sleep 1
    setsid "$APP" > /tmp/br0x-run.log 2>&1 &
    sleep 4
    echo "started $(pgrep -x br0x | head -1)"
    ;;
  click)
    x="$2"; y="$3"; out="${4:-/tmp/shots/interact.png}"
    $TOOLS/xdotool mousemove "$x" "$y" sleep 0.3 click 1
    sleep 1.2
    capture_screen "$out"
    echo "screen -> $out"
    capture_popup "${out%.png}-popup.png"
    ;;
  key)
    keys="$2"; out="${3:-/tmp/shots/interact.png}"
    $TOOLS/xdotool key "$keys"
    sleep 1.0
    capture_screen "$out"
    echo "screen -> $out"
    capture_popup "${out%.png}-popup.png"
    ;;
  shot)
    capture_screen "${2:-/tmp/shots/interact.png}"
    echo "screen -> ${2:-/tmp/shots/interact.png}"
    capture_popup "${2:-/tmp/shots/interact.png}"
    ;;
  stop)
    pgrep -x br0x | xargs -r kill
    echo stopped
    ;;
  *)
    echo "usage: interact.sh start|click|key|shot|stop" >&2
    exit 2
    ;;
esac
