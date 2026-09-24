#!/bin/bash
# Headless chrome review: run br0x under Xvfb, drive it with xdotool, capture
# the whole screen (popovers included) as PNG.
#
#   ui-shot.sh <out.png> <appearance> [delay_ms] [xdotool args...]
#
# Examples:
#   ui-shot.sh /tmp/a.png Dark 1500
#   ui-shot.sh /tmp/menu.png Dark 1500 -- click 1150 31
#
# The app is a debug build from this repo; the toolchain lives in /tmp/xvfb
# (apt packages extracted without root).
set -u
OUT="$1"; APPEARANCE="${2:-Light}"; DELAY="${3:-1500}"; shift 3 || true

XVFB_ROOT="${XVFB_ROOT:-/tmp/xvfb/root}"
LIB="$XVFB_ROOT/usr/lib/x86_64-linux-gnu"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="${BR0X_BIN:-$REPO/target/debug/br0x}"
HOME_DIR=/tmp/br0x-home
DISPLAY_NUM=:99

export DISPLAY=$DISPLAY_NUM
export XDG_DATA_HOME=$HOME_DIR
export XDG_RUNTIME_DIR=/tmp/fake-runtime
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/fake-runtime/nob.us
export GSK_RENDERER=cairo
export LD_LIBRARY_PATH="$LIB"

mkdir -p "$HOME_DIR/br0x"
printf '{"appearance":"%s","sidebar_visible":true}' "$APPEARANCE" > "$HOME_DIR/br0x/prefs.json"

if ! xdpyinfo_check=$(ls /tmp/.X11-unix/X99 2>/dev/null); then
  setsid env LD_LIBRARY_PATH="$LIB" XKB_CONFIG_ROOT="$XVFB_ROOT/usr/share/X11/xkb" \
    "$XVFB_ROOT/usr/bin/Xvfb" $DISPLAY_NUM -screen 0 1400x900x24 -nolisten tcp \
    > /tmp/xvfb/xvfb.log 2>&1 &
  sleep 3
fi

"$APP" > /tmp/br0x-run.log 2>&1 &
APP_PID=$!
sleep "$(python3 -c "print($DELAY/1000)")"

if [ "$#" -gt 0 ]; then
  if [ "$1" = "--" ]; then shift; fi
  "$XVFB_ROOT/usr/bin/xdotool" "$@" >/dev/null 2>&1
  sleep 1
fi

rm -f "$OUT" /tmp/shot.xwd
"$XVFB_ROOT/usr/bin/xwd" -root -silent > /tmp/shot.xwd 2>/dev/null
python3 "$REPO/scripts/xwd2png.py" /tmp/shot.xwd "$OUT"
kill $APP_PID 2>/dev/null
wait $APP_PID 2>/dev/null
[ -f "$OUT" ] && echo "OK $OUT" || echo "MISSING $OUT"
