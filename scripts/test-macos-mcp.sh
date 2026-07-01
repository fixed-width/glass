#!/usr/bin/env bash
# Headless glass-mcp smoke: proves the stdio MCP server boots and lists its tools with
# NEITHER a TCC grant NOR a WindowServer session — the macos-14 CI runner has neither, and
# tools are registered before any backend is constructed, so `initialize` + `tools/list`
# alone proves the macOS build links and the server starts, without touching capture/input
# (glass_start, which builds a live MacosPlatform, is deliberately not called here).
#
# Skips (exit 0) when not on macOS, mirroring scripts/test-macos.sh, so it's safe to call
# from any CI matrix leg.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "test-macos-mcp.sh: not macOS (uname=$(uname -s)) — skipping."
  exit 0
fi

# Run from the repo root so `cargo -p` resolves regardless of the caller's cwd (mirrors
# scripts/test-macos.sh / test-x11.sh / test-wayland.sh / test-windows.sh).
cd "$(dirname "$0")/.."

# The JSON-RPC round trip needs a real request/response exchange over stdin/stdout, which
# is awkward in portable bash (no `coproc`/GNU `timeout` guarantee on macOS's default
# bash/coreutils) — python3 ships on every GitHub-hosted macOS runner, so do it there. A
# SIGALRM bounds the whole exchange so a wedged server fails the job instead of hanging it.
python3 - <<'PY'
import json
import signal
import subprocess
import sys


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode())
    proc.stdin.flush()


def recv(proc):
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("glass-mcp closed stdout unexpectedly")
    return json.loads(line)


def alarm(signum, frame):
    raise TimeoutError("glass-mcp smoke timed out after 30s")


signal.signal(signal.SIGALRM, alarm)
signal.alarm(30)

proc = subprocess.Popen(
    ["cargo", "run", "-p", "glass-mcp", "--locked", "--"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
)
try:
    send(proc, {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "ci-macos-smoke", "version": "0"},
        },
    })
    init = recv(proc)
    assert "result" in init, f"initialize failed: {init}"

    send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})

    send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    tools = recv(proc)
    names = [t["name"] for t in tools["result"]["tools"]]
    assert "glass_start" in names, f"glass_start missing from tools/list: {names}"
    print(f"OK: glass-mcp stdio server listed {len(names)} tools incl. glass_start")
finally:
    signal.alarm(0)
    proc.stdin.close()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    stderr = proc.stderr.read().decode(errors="replace")
    if stderr.strip():
        print("---- glass-mcp stderr ----", file=sys.stderr)
        print(stderr, file=sys.stderr)
PY
