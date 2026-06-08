#!/usr/bin/env bash
# probe_advanced.sh — advanced Sequoia-baseline probes, run from a GUI (Aqua) Terminal.
#
# Tests what the basic kit didn't: capturing a window that lives ON the virtual display
# (the real product topology), the HiDPI/scale signal, and a capture-latency baseline.
# Needs Screen Recording + Accessibility consent (granted to Terminal).
#
#   ./probe_advanced.sh [app-substring] [bench-iters] [vdisplay WxH]
#   e.g. ./probe_advanced.sh TextEdit 30 2560x1440
set -uo pipefail
cd "$(dirname "$0")"
APP="${1:-TextEdit}"
ITERS="${2:-30}"
RES="${3:-1920x1080}"; W="${RES%x*}"; H="${RES#*x}"
trap 'for p in $(pgrep -f "[v]irtualdisplay"); do kill -9 "$p" 2>/dev/null; done' EXIT

if [ "$(launchctl managername 2>/dev/null)" != "Aqua" ]; then
  echo "WARNING: not an Aqua GUI session — capture/AX will be denied. Run from a VNC Terminal."
fi

echo "== build =="
clang -fobjc-arc -framework Foundation -framework CoreGraphics -o virtualdisplay virtual_display.m \
  && swiftc -O -parse-as-library capture_on_vdisplay.swift -o capture_on_vdisplay \
  || { echo "BUILD FAILED"; exit 1; }

echo "== open $APP + a ${W}x${H} virtual display =="
open -a "$APP" 2>/dev/null; sleep 2
for p in $(pgrep -f "[v]irtualdisplay"); do kill -9 "$p" 2>/dev/null; done
./virtualdisplay "$W" "$H" 0 >/tmp/vd_probe.log 2>&1 &
disown 2>/dev/null || true; sleep 2
grep -E 'OK: created|FAIL' /tmp/vd_probe.log || true

echo "== probe: move $APP onto the virtual display, capture there, bench $ITERS =="
./capture_on_vdisplay "$APP" "$ITERS"
echo
echo "Inspect /tmp/shot_vdisplay.png — it should show $APP's real content, captured while"
echo "the window sits on the virtual (secondary) display."
