#!/usr/bin/env bash
# Renders docs/assets/mission.svg to the light and dark PNGs the README
# shows. Run it on a Mac: the SVG names macOS fonts (Iowan Old Style,
# Bradley Hand), and a phone without them draws the SVG badly, so the
# README shows these PNGs rather than the SVG.
set -euo pipefail

chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
assets="$(cd "$(dirname "$0")/../docs/assets" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

for theme in light dark; do
  {
    echo "<!doctype html><html><body class=\"$theme\" style=\"margin:0\">"
    cat "$assets/mission.svg"
    echo "</body></html>"
  } >"$work/$theme.html"
  "$chrome" --headless --disable-gpu --hide-scrollbars \
    --force-device-scale-factor=2 --window-size=640,310 \
    --default-background-color=00000000 \
    --screenshot="$assets/mission-$theme.png" "file://$work/$theme.html" 2>/dev/null
done
