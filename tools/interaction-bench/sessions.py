"""Owned Linux processes, short socket paths and fresh application profiles."""

import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import time
import uuid

GECKO_PREFS = """user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.migrate.content-modal.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.startup.upgradeDialog.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
"""


class Session:
    def __init__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gi-", dir="/tmp")
        self.root = Path(self.temporary.name)
        self.token = uuid.uuid4().hex
        self.env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith("GLASS_")
            and k
            not in (
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "DBUS_SESSION_BUS_ADDRESS",
                "AT_SPI_BUS_ADDRESS",
            )
        }
        for name in ("runtime", "tmp", "profile"):
            (self.root / name).mkdir(mode=0o700)
        self.env.update(
            XDG_RUNTIME_DIR=str(self.root / "runtime"),
            TMPDIR=str(self.root / "tmp"),
            GLASS_BENCH_RUN_ID=self.token,
            LIBGL_ALWAYS_SOFTWARE="1",
            LANG="C.UTF-8",
            LC_ALL="C.UTF-8",
            TZ="UTC",
            MOZ_ENABLE_WAYLAND="0",
            PATH="/usr/bin:/bin:" + self.env.get("PATH", ""),
        )
        self.display = None
        self.display_log = None
        self.display_fd = None
        self.bus = None
        try:
            self.bus = subprocess.Popen(
                [
                    "dbus-daemon",
                    "--session",
                    "--nofork",
                    "--print-address=1",
                    f"--address=unix:path={self.root}/session-bus",
                ],
                env=self.env,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            if not select.select([self.bus.stdout], [], [], 10)[0]:
                raise RuntimeError("private session bus did not start")
            address = self.bus.stdout.readline().decode().strip()
            if not address.startswith("unix:"):
                raise RuntimeError("private session bus returned no address")
            self.env["DBUS_SESSION_BUS_ADDRESS"] = address
        except BaseException:
            if self.bus:
                if self.bus.poll() is None:
                    self.bus.kill()
                    self.bus.wait(timeout=2)
                self.bus.stdout.close()
            self.temporary.cleanup()
            raise

    def start_display(self, output, width=1280, height=900):
        read_fd, write_fd = os.pipe()
        self.display_fd = read_fd
        self.display_log = Path(output).open("wb")
        try:
            self.display = subprocess.Popen(
                [
                    "Xvfb",
                    "-displayfd",
                    str(write_fd),
                    "-screen",
                    "0",
                    f"{width}x{height}x24",
                    "-nolisten",
                    "tcp",
                    "-noreset",
                    "-ac",
                ],
                pass_fds=(write_fd,),
                env=self.env,
                stderr=self.display_log,
                stdout=self.display_log,
                start_new_session=True,
            )
        finally:
            os.close(write_fd)
        try:
            if not select.select([read_fd], [], [], 10)[0]:
                raise RuntimeError("Xvfb did not publish a display within 10 seconds")
            number = os.read(read_fd, 32).decode().strip()
            if not number.isdigit():
                raise RuntimeError("Xvfb did not publish a valid display")
            self.env["DISPLAY"] = ":" + number
            return ":" + number
        except BaseException:
            os.close(read_fd)
            self.display_fd = None
            raise

    def browser_args(self, browser, family, url, extra=()):
        profile = self.root / "profile"
        if family == "firefox":
            (profile / "user.js").write_text(GECKO_PREFS)
            flags = ["--no-remote", "--new-instance", "--profile", str(profile)]
        elif family == "chromium":
            flags = [
                f"--user-data-dir={profile}",
                "--no-first-run",
                "--no-default-browser-check",
                "--force-renderer-accessibility",
                "--ozone-platform=x11",
            ]
        else:
            raise ValueError(f"unknown browser family {family}")
        return [browser, *flags, *extra, url]

    def remaining_processes(self):
        result = []
        needle = f"GLASS_BENCH_RUN_ID={self.token}".encode()
        for path in Path("/proc").iterdir():
            if not path.name.isdigit():
                continue
            try:
                if needle in (path / "environ").read_bytes().split(b"\0"):
                    result.append(int(path.name))
            except (OSError, PermissionError):
                continue
        return result

    def process_commands(self):
        commands = []
        for pid in self.remaining_processes():
            try:
                commands.append(
                    {
                        "pid": pid,
                        "argv": (Path("/proc") / str(pid) / "cmdline")
                        .read_bytes()
                        .decode("utf-8", errors="replace")
                        .rstrip("\0")
                        .split("\0"),
                    }
                )
            except OSError:
                continue
        return commands

    def close(self):
        self.bus.terminate()
        try:
            self.bus.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.bus.kill()
            self.bus.wait(timeout=2)
        self.bus.stdout.close()
        if self.display:
            self.display.terminate()
            try:
                self.display.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.display.kill()
                self.display.wait(timeout=2)
        if self.display_log:
            self.display_log.close()
        if self.display_fd is not None:
            os.close(self.display_fd)
            self.display_fd = None
        deadline = time.monotonic() + 2
        residue = self.remaining_processes()
        while residue and time.monotonic() < deadline:
            time.sleep(0.05)
            residue = self.remaining_processes()
        details = []
        for pid in residue:
            try:
                # Recheck the ownership token immediately before signalling.
                if pid in self.remaining_processes():
                    details.append(
                        {
                            "pid": pid,
                            "command": (Path("/proc") / str(pid) / "comm")
                            .read_text()
                            .strip(),
                        }
                    )
                    os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.temporary.cleanup()
        return {"ok": not residue, "forced_owned_pids": residue, "residue": details}
