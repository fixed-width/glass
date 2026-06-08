#!/usr/bin/env bash
# run-validation.sh — build + run macOS validation steps 1 & 2 in one shot.
#
# MUST be run inside a logged-in GUI (Aqua) session — see README. Over a bare SSH
# shell at the login window this will create the virtual display but it won't attach
# (CGGetActiveDisplayList stays 0) and capture has nothing to grab.
#
#   ./run-validation.sh [width height app-substring]
#   e.g. ./run-validation.sh 1920 1080 TextEdit
set -euo pipefail
cd "$(dirname "$0")"

W="${1:-1920}"; H="${2:-1080}"; APP="${3:-TextEdit}"

echo "== sanity: are we in an Aqua session? =="
if [ "$(launchctl managername 2>/dev/null)" != "Aqua" ]; then
  echo "WARNING: launchd context is '$(launchctl managername 2>/dev/null)', not 'Aqua'."
  echo "         Virtual display likely won't attach. Run inside a GUI login session"
  echo "         (or via: sudo launchctl asuser \$(id -u) $0 $*)."
fi

echo "== build =="
clang -fobjc-arc -framework Foundation -framework CoreGraphics -o virtualdisplay virtual_display.m
swiftc -O -parse-as-library capture_window.swift -o capture_window
echo "build OK"

echo "== step 1: virtual display (${W}x${H}), held open in background =="
./virtualdisplay "$W" "$H" &
VD_PID=$!
trap 'kill "$VD_PID" 2>/dev/null || true' EXIT
sleep 2

echo "== step 2: launch $APP and capture its window =="
open -a "$APP" || true
sleep 2
./capture_window "$APP" shot.png || {
  echo "capture failed — if this is the first run, grant Screen Recording in the"
  echo "VNC session (System Settings > Privacy & Security > Screen Recording) and re-run."
  exit 1
}
echo "== done. inspect shot.png =="
