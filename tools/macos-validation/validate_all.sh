#!/usr/bin/env bash
# validate_all.sh — one-command macOS validation harness (items 1-4) + summary.
#
# Builds all in-session tools, runs items 1-4 in order, and prints a results table.
# Item 5 (headless unattended reboot) needs a reboot, so it's guided at the end, not run.
#
# RUN FROM A GUI TERMINAL (Aqua session — e.g. Terminal.app inside VNC). Capture (item 2)
# and input (item 3/4) need TCC consent granted to Terminal:
#   - FIRST run: item 2 triggers the Screen Recording prompt; item 3 triggers Accessibility.
#     Grant both (System Settings > Privacy & Security), then RE-RUN for an all-green pass.
#
#   ./validate_all.sh [app-substring]      # default target app: TextEdit
#
# The headline check is item 2: "non-blank" = capture works; "BLANK/UNIFORM" = the
# Tahoe (macOS 26) capture show-stopper. That single line is the buy/no-buy signal.
set -uo pipefail   # deliberately not -e: run every item, report even on failure
cd "$(dirname "$0")"
APP="${1:-TextEdit}"
TMP="$(mktemp -d)"
trap 'for p in $(pgrep -f "[v]irtualdisplay"); do kill -9 "$p" 2>/dev/null; done; rm -rf "$TMP"' EXIT

echo "== macOS validation harness (items 1-4) =="
sw_vers 2>/dev/null | sed 's/^/  /'
echo "  arch: $(uname -m)   launchd context: $(launchctl managername 2>/dev/null)"
if [ "$(launchctl managername 2>/dev/null)" != "Aqua" ]; then
  echo "  WARNING: not an Aqua GUI session — capture/input will fail."
  echo "           Run this from Terminal.app inside the VNC session."
fi
echo

echo "== build =="
clang -fobjc-arc -framework Foundation -framework CoreGraphics -o virtualdisplay virtual_display.m \
  && swiftc -O -parse-as-library capture_window.swift -o capture_window \
  && swiftc -O -parse-as-library inject_input.swift   -o inject_input \
  && swiftc -O window_ops.swift -o window_ops \
  || { echo "BUILD FAILED — fix the error above and re-run."; exit 1; }
echo "build OK"; echo

# Clean any stale holders/processes from previous runs (job control is unavailable here).
pkill -9 -f '[ci]apture_window|[i]nject_input|[w]indow_ops' 2>/dev/null || true
for p in $(pgrep -f "[v]irtualdisplay"); do kill -9 "$p" 2>/dev/null; done
sleep 1

# ---------- item 1: CGVirtualDisplay attaches ----------
./virtualdisplay 1600 1000 >"$TMP/vd.log" 2>&1 &
disown 2>/dev/null || true   # silence bash's "Killed: 9" when the EXIT trap reaps it
sleep 2
before=$(grep -o 'before: [0-9]*' "$TMP/vd.log" | awk '{print $2}' | head -1)
after=$(grep -o 'after: [0-9]*'  "$TMP/vd.log" | awk '{print $2}' | head -1)
if grep -q 'OK: created virtual display' "$TMP/vd.log" && [ "${after:-0}" -gt "${before:-0}" ]; then
  R1="PASS"; N1="virtual display attached (${before:-?}->${after:-?} active)"
else
  R1="FAIL"; N1="$(tail -1 "$TMP/vd.log" | tr -d '\n')"
fi
# Tear the virtual display down NOW — holding it attached through items 2-4 reconfigures
# the display arrangement and can black out the VNC-mirrored base display on Tahoe.
# Items 2-4 capture/input target the app window on the base display; no vdisplay needed.
for p in $(pgrep -f "[v]irtualdisplay"); do kill -9 "$p" 2>/dev/null; done
sleep 1

# ---------- item 2: ScreenCaptureKit real pixels (THE headline) ----------
open -a "$APP" 2>/dev/null; sleep 2
./capture_window "$APP" "$TMP/shot.png" >"$TMP/cap.log" 2>&1
if grep -q 'OK: captured non-blank' "$TMP/cap.log"; then
  R2="PASS"; N2="$(grep -o 'OK: captured non-blank.*' "$TMP/cap.log")  -> $TMP/shot.png"
elif grep -q 'blank/uniform' "$TMP/cap.log"; then
  R2="FAIL-BLANK"; N2="BLANK/UNIFORM FRAME = the Tahoe capture show-stopper"
elif grep -qiE 'CGS_REQUIRE_INIT|SCShareableContent failed|captureImage failed|grant Screen Recording' "$TMP/cap.log"; then
  R2="NEEDS-CONSENT"; N2="grant Screen Recording to Terminal, then re-run | $(grep -iE 'failed|CGS_REQUIRE_INIT' "$TMP/cap.log" | head -1 | tr -d '\n')"
else
  R2="FAIL"; N2="$(tail -1 "$TMP/cap.log" | tr -d '\n')"
fi

# ---------- item 3: CGEvent input ----------
./inject_input "$APP" "glass input ok" >"$TMP/inj.log" 2>&1
trust=$(grep -o 'AXIsProcessTrusted = [a-z]*' "$TMP/inj.log" | awk '{print $3}' | head -1)
if [ "$trust" = "true" ] && grep -q 'done — capture' "$TMP/inj.log"; then
  R3="PASS"; N3="trusted + injected (verify text in $APP / re-capture to eyeball)"
elif [ "$trust" = "false" ]; then
  R3="NEEDS-CONSENT"; N3="grant Accessibility to Terminal, then re-run"
else
  R3="FAIL"; N3="$(tail -1 "$TMP/inj.log" | tr -d '\n')"
fi

# ---------- item 4: AXUIElement window ops ----------
./window_ops "$APP" >"$TMP/win.log" 2>&1
if grep -q 'OK: window moved AND resized' "$TMP/win.log"; then
  R4="PASS"; N4="$(grep -E 'move:|resize:' "$TMP/win.log" | tr '\n' ' ')"
elif grep -qi 'AXIsProcessTrusted = false' "$TMP/win.log"; then
  R4="NEEDS-CONSENT"; N4="grant Accessibility to Terminal, then re-run"
else
  R4="FAIL"; N4="$(tail -1 "$TMP/win.log" | tr -d '\n')"
fi

echo
echo "================ RESULTS ($(sw_vers -productVersion 2>/dev/null)) ================"
printf '  %-14s %-14s %s\n' "1 vdisplay" "$R1" "$N1"
printf '  %-14s %-14s %s\n' "2 capture" "$R2" "$N2"
printf '  %-14s %-14s %s\n' "3 input"   "$R3" "$N3"
printf '  %-14s %-14s %s\n' "4 windowops" "$R4" "$N4"
echo "==================================================="
echo
if [ "$R2" = "PASS" ]; then
  echo ">> Item 2 (capture) is NON-BLANK on this OS — the make-or-break path works."
elif [ "$R2" = "FAIL-BLANK" ]; then
  echo ">> Item 2 is BLANK on this OS — capture path NOT viable here. Note the version, stop."
else
  echo ">> Item 2 inconclusive ($R2) — see note above; grant consent and re-run."
fi
echo
echo "Item 5 (headless unattended reboot), run manually:"
echo "  ./set_autologin.sh \$(id -un)   # enable auto-login from SSH (FileVault must be off)"
echo "  sudo reboot                     # disconnect VNC first to simulate no monitor"
echo "  # reconnect SSH, then: launchctl print gui/\$(id -u) >/dev/null && echo 'session LIVE'"
echo "  # and re-run ./virtualdisplay to confirm it attaches headlessly."
