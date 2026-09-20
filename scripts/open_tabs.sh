#!/usr/bin/env bash
# Open a list of URLs as tabs in a browser window by sending keystrokes.
# Usage: open_tabs.sh <window-class> url1 url2 ...
set -u
WIN="$1"
shift

send() {
  hyprctl dispatch "hl.dsp.send_shortcut({ window = \"$WIN\", mods = \"$1\", key = \"$2\" })" >/dev/null 2>&1
}

type_text() {
  local text=$1 i ch key
  for ((i = 0; i < ${#text}; i++)); do
    ch=${text:i:1}
    case "$ch" in
      '.') key="period" ;;
      '/') key="slash" ;;
      ':') key="colon" ;;
      '-') key="minus" ;;
      '?') key="question" ;;
      '=') key="equal" ;;
      '&') key="ampersand" ;;
      '_') key="underscore" ;;
      *) key="$ch" ;;
    esac
    send "" "$key"
    sleep 0.05
  done
}

hyprctl dispatch "hl.dsp.focus({ window = \"$WIN\" })" >/dev/null 2>&1
sleep 0.6

first=1
for url in "$@"; do
  if [ $first -eq 1 ]; then
    first=0
    send "CTRL" "l"
  else
    send "CTRL" "t"
  fi
  sleep 0.3
  type_text "$url"
  sleep 0.2
  send "" "Return"
  sleep 1.2
done
echo "opened $# tabs in $WIN"
