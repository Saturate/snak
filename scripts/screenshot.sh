#!/usr/bin/env bash
# Captures the README screenshot from the demo workspace: no account, no real
# messages. Builds Snak.app, opens it on a still demo (no scripted activity) at a
# fixed size on #backend, captures the window with its shadow and quits it.
# Usage: scripts/screenshot.sh [conversation id] [WxH]
#   scripts/screenshot.sh                  # eng-backend at 1024x704
#   scripts/screenshot.sh dm-maja 1280x820
set -euo pipefail
cd "$(dirname "$0")/.."

conversation="${1:-eng-backend}"
window_size="${2:-1024x704}"
out=docs/screenshot.png

./scripts/bundle.sh >/dev/null
# Other demo instances may be running; only the one started here is ours.
before=" $( (pgrep -f "Snak.app/Contents/MacOS/snak --demo" || true) | tr '\n' ' ')"
# `open` launches through LaunchServices, which gives the new window focus;
# a bare binary would stay behind the terminal and never finish drawing.
open -n target/Snak.app \
  --env SNAK_DEMO_STILL=1 \
  --env SNAK_OPEN="$conversation" \
  --env SNAK_WINDOW_SIZE="$window_size" \
  --args --demo

pid=""
for _ in $(seq 50); do
  for candidate in $(pgrep -f "Snak.app/Contents/MacOS/snak --demo" || true); do
    case "$before" in *" $candidate "*) ;; *) pid=$candidate ;; esac
  done
  [ -n "$pid" ] && break
  sleep 0.2
done
[ -n "$pid" ] || { echo "the demo didn't start" >&2; exit 1; }
trap 'kill "$pid" 2>/dev/null || true' EXIT

window=""
for _ in $(seq 50); do
  window=$(swift scripts/window.swift find "$pid" 2>/dev/null || true)
  [ -n "$window" ] && break
  sleep 0.2
done
[ -n "$window" ] || { echo "no demo window" >&2; exit 1; }
read -r id x y _ _ <<<"$window"

# GPUI only finishes drawing a window that has focus. `open` usually gives it,
# but not while another app is taking keystrokes, so ask once more and check.
frontmost() {
  osascript -e 'tell application "System Events" to get unix id of first process whose frontmost is true'
}
for _ in $(seq 20); do
  [ "$(frontmost)" = "$pid" ] && break
  osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $pid) to true" >/dev/null
  sleep 0.5
done
[ "$(frontmost)" = "$pid" ] || { echo "the demo window never came to the front" >&2; exit 1; }

# Just left of the window, so no row is hovered in the shot.
swift scripts/window.swift park "$((x - 40))" "$((y + 200))"
# Long enough to sign in, load the channel and lay out its messages.
sleep 3

mkdir -p docs
screencapture -x -l"$id" "$out"
# Retina captures of big windows get large; shrink those, never enlarge.
width=$(sips -g pixelWidth "$out" | awk '/pixelWidth/ {print $2}')
[ "$width" -gt 2400 ] && sips -Z 2400 "$out" >/dev/null
echo "Wrote $out"
