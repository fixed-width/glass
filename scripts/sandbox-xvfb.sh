#!/usr/bin/env bash
# Manage the glass sandbox X display — an Xvfb that glass-mcp drives instead of
# your real :0, so the agent's clicks/keystrokes never touch your live desktop.
#
#   scripts/sandbox-xvfb.sh [start|stop|status|restart]
#
# Defaults to display :42 (matches the glass MCP server registration). Override
# with GLASS_DISPLAY / GLASS_XVFB_SCREEN, e.g.
#   GLASS_DISPLAY=77 scripts/sandbox-xvfb.sh start
#
# The glass MCP server connects to its DISPLAY at startup, so start this BEFORE
# the MCP client launches the server (and it must stay up while in use).
set -euo pipefail

DPY="${GLASS_DISPLAY:-42}"
SCREEN="${GLASS_XVFB_SCREEN:-1280x800x24}"
LOG="/tmp/glass-xvfb-${DPY}.log"

# Reliable liveness check: can we actually open the display?
is_up() { xdpyinfo -display ":${DPY}" >/dev/null 2>&1; }

start() {
    if is_up; then
        echo "sandbox display :${DPY} already up"
        return 0
    fi
    command -v Xvfb >/dev/null 2>&1 || { echo "Xvfb not installed (try: sudo apt-get install -y xvfb)"; exit 1; }
    # Clear a stale lock/socket left behind by a killed Xvfb.
    rm -f "/tmp/.X${DPY}-lock" "/tmp/.X11-unix/X${DPY}" 2>/dev/null || true
    nohup Xvfb ":${DPY}" -screen 0 "${SCREEN}" >"${LOG}" 2>&1 &
    for _ in $(seq 1 40); do is_up && break; sleep 0.1; done
    if is_up; then
        echo "sandbox display :${DPY} up (${SCREEN}); point clients at DISPLAY=:${DPY}"
    else
        echo "failed to start Xvfb :${DPY}; see ${LOG}"; exit 1
    fi
}

stop() {
    # Trailing space pins the match to the real Xvfb process (not :420 etc.).
    if pkill -f "Xvfb :${DPY} " 2>/dev/null; then
        echo "stopped Xvfb :${DPY}"
    else
        echo "no Xvfb :${DPY} to stop"
    fi
    rm -f "/tmp/.X${DPY}-lock" "/tmp/.X11-unix/X${DPY}" 2>/dev/null || true
}

case "${1:-start}" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; start ;;
    status)  is_up && echo "sandbox display :${DPY} is UP" || echo "sandbox display :${DPY} is DOWN" ;;
    *)       echo "usage: $0 {start|stop|status|restart}  (display :${DPY})"; exit 2 ;;
esac
