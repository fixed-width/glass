"""Native desktop ownership, independent of Linux displays and session buses."""

import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time
import uuid

from sessions import Session


class NativeSession:
    browser_args = Session.browser_args

    def __init__(self, backend):
        self.backend = backend
        self.temporary = tempfile.TemporaryDirectory(
            prefix="gi-", dir="/tmp" if os.name == "posix" else None
        )
        self.root = Path(self.temporary.name)
        self.token = uuid.uuid4().hex
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GLASS_")
            and key
            not in (
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "DBUS_SESSION_BUS_ADDRESS",
                "AT_SPI_BUS_ADDRESS",
            )
        }
        for name in ("profile", "tmp", "runtime"):
            (self.root / name).mkdir(mode=0o700)
        self.env.update(
            GLASS_BENCH_RUN_ID=self.token,
            TMPDIR=str(self.root / "tmp"),
            TEMP=str(self.root / "tmp"),
            TMP=str(self.root / "tmp"),
        )

    def process_commands(self):
        if os.name == "nt":
            return (
                []
            )  # The MCP client's Windows Job owns and enumerates its process tree.
        output = subprocess.check_output(
            ["ps", "eww", "-axo", "pid=,command="], timeout=5
        ).decode(errors="replace")
        marker = re.compile(
            r"(?:^|\s)GLASS_BENCH_RUN_ID=" + re.escape(self.token) + r"(?:\s|$)"
        )
        return [
            {"pid": int(line.split(None, 1)[0])}
            for line in output.splitlines()
            if marker.search(line)
        ]

    def close(self):
        deadline = time.monotonic() + 2
        residue = self.process_commands()
        while residue and time.monotonic() < deadline:
            time.sleep(0.05)
            residue = self.process_commands()
        forced = []
        for process in residue:
            if process in self.process_commands():
                try:
                    os.kill(process["pid"], signal.SIGKILL)
                    forced.append(process["pid"])
                except ProcessLookupError:
                    pass
        self.temporary.cleanup()
        return {"ok": not residue, "forced_owned_pids": forced, "residue": residue}


def create_session(backend):
    if backend in ("macos", "windows", "ios"):
        return NativeSession(backend)
    session = Session()
    session.backend = backend
    if backend == "wayland":
        session.env["MOZ_ENABLE_WAYLAND"] = "1"
    return session


def prepare_display(session, config, directory):
    if config.get("backend", "x11") == "x11":
        display = session.start_display(directory / "xvfb.log", *config["display"])
        session.env["GLASS_DISPLAY"] = display
        return display
    if config.get("backend") == "wayland":
        session.env["GLASS_SWAY"] = config["sway"]
        width, height = config["display"]
        session.env["GLASS_WAYLAND_SCREEN"] = f"{width}x{height}"
    return None
