"""Fresh Android emulator and ADB ownership for application-boundary attempts."""

import socket
import subprocess
import time
from pathlib import Path

from measurement import write_json


def available_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def emulator_port():
    for port in range(5554, 5586, 2):
        try:
            with socket.socket() as console, socket.socket() as adb:
                console.bind(("127.0.0.1", port))
                adb.bind(("127.0.0.1", port + 1))
                return port
        except OSError:
            continue
    raise RuntimeError("no free owned emulator port pair")


class Android:
    def __init__(self, session, config, directory, deadline):
        self.session, self.config, self.directory, self.deadline = (
            session,
            config,
            directory,
            deadline,
        )
        self.process = self.server = None
        self.streams, self.commands = [], []
        self.sdk = Path(config["sdk"])
        self.adb = str(self.sdk / "platform-tools/adb")
        self.port = emulator_port()
        self.serial = f"emulator-{self.port}"
        server_port = available_port()
        session.env.update(
            ANDROID_HOME=str(self.sdk),
            ANDROID_SDK_ROOT=str(self.sdk),
            ANDROID_AVD_HOME=str(session.root / "avd"),
            ANDROID_EMULATOR_HOME=str(session.root / "android"),
            ANDROID_ADB_SERVER_PORT=str(server_port),
            ADB_SERVER_SOCKET=f"tcp:127.0.0.1:{server_port}",
            GLASS_ADB=self.adb,
            GLASS_ANDROID_SERIAL=self.serial,
            GLASS_ANDROID_LIFECYCLE="attach",
        )
        for name in ("avd", "android"):
            (session.root / name).mkdir()
        for key, value in (
            ("agent_jar", "GLASS_ANDROID_AGENT_JAR"),
            ("a11y_apk", "GLASS_ANDROID_A11Y_APK"),
        ):
            if config.get(key):
                session.env[value] = config[key]

    def command(self, arguments, *, input=None, timeout=15, check=True):
        remaining = max(0.01, min(timeout, self.deadline - time.monotonic()))
        started = time.monotonic()
        record = {"argv": arguments, "exit_code": None}
        try:
            result = subprocess.run(
                arguments,
                input=input,
                capture_output=True,
                env=self.session.env,
                timeout=remaining,
            )
            record.update(
                exit_code=result.returncode,
                stdout=result.stdout.decode(errors="replace"),
                stderr=result.stderr.decode(errors="replace"),
            )
        except subprocess.TimeoutExpired as exc:
            record.update(
                error="timeout",
                stdout=(exc.stdout or b"").decode(errors="replace"),
                stderr=(exc.stderr or b"").decode(errors="replace"),
            )
            raise
        finally:
            record["elapsed_ms"] = (time.monotonic() - started) * 1000
            self.commands.append(record)
            write_json(self.directory / "android-preparation.json", self.commands)
        if check and result.returncode:
            raise RuntimeError(f"Android preparation failed: {record['stderr'][:500]}")
        return result.stdout.decode(errors="replace").strip()

    def spawn(self, arguments, name):
        stream = (self.directory / name).open("wb")
        self.streams.append(stream)
        return subprocess.Popen(
            arguments,
            env=self.session.env,
            cwd=self.directory,
            stdout=stream,
            stderr=stream,
            start_new_session=True,
        )

    def start(self):
        self.command(
            [
                str(self.sdk / "cmdline-tools/latest/bin/avdmanager"),
                "create",
                "avd",
                "--force",
                "--name",
                "interaction",
                "--package",
                self.config["image"],
                "--device",
                "pixel_6",
                "--path",
                str(self.session.root / "avd/interaction.avd"),
            ],
            input=b"no\n",
            timeout=30,
        )
        self.server = self.spawn(
            [
                self.adb,
                "-L",
                "tcp:" + self.session.env["ANDROID_ADB_SERVER_PORT"],
                "server",
                "nodaemon",
            ],
            "adb-server.log",
        )
        ready_deadline = min(self.deadline, time.monotonic() + 10)
        while True:
            if self.server.poll() is not None:
                raise RuntimeError("owned ADB server exited before listening")
            try:
                with socket.create_connection(
                    ("127.0.0.1", int(self.session.env["ANDROID_ADB_SERVER_PORT"])),
                    timeout=0.2,
                ):
                    break
            except OSError:
                if time.monotonic() >= ready_deadline:
                    raise TimeoutError("owned ADB server did not start")
                time.sleep(0.05)
        self.process = self.spawn(
            [
                str(self.sdk / "emulator/emulator"),
                "-avd",
                "interaction",
                "-port",
                str(self.port),
                "-no-window",
                "-no-audio",
                "-no-snapshot",
                "-no-boot-anim",
                "-gpu",
                "swiftshader_indirect",
                "-cores",
                "2",
                "-memory",
                "2048",
            ],
            "emulator.log",
        )
        while time.monotonic() < self.deadline:
            if self.process.poll() is not None:
                raise RuntimeError("owned Android emulator exited before boot")
            if (
                self.command(
                    [
                        self.adb,
                        "-s",
                        self.serial,
                        "shell",
                        "getprop",
                        "sys.boot_completed",
                    ],
                    check=False,
                )
                == "1"
            ):
                break
            time.sleep(0.25)
        else:
            raise TimeoutError("owned Android emulator did not boot before deadline")
        for setting in (
            "window_animation_scale",
            "transition_animation_scale",
            "animator_duration_scale",
        ):
            self.command(
                [
                    self.adb,
                    "-s",
                    self.serial,
                    "shell",
                    "settings",
                    "put",
                    "global",
                    setting,
                    "0",
                ]
            )
        properties = {}
        for name in ("ro.build.fingerprint", "ro.build.version.sdk"):
            properties[name] = self.command(
                [self.adb, "-s", self.serial, "shell", "getprop", name]
            )
        properties["display"] = self.command(
            [self.adb, "-s", self.serial, "shell", "wm", "size"]
        )
        properties["webview"] = self.command(
            [self.adb, "-s", self.serial, "shell", "dumpsys", "webviewupdate"]
        )
        write_json(self.directory / "android-device.json", properties)
        return properties

    def close(self):
        clean = True
        if self.process and self.process.poll() is None:
            try:
                self.command([self.adb, "-s", self.serial, "emu", "kill"], timeout=5)
            except (OSError, subprocess.TimeoutExpired, RuntimeError):
                clean = False
                self.process.terminate()
        for process in (self.process, self.server):
            if not process:
                continue
            if process is self.server:
                process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                clean = False
        for stream in self.streams:
            stream.close()
        return clean
