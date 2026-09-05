"""Validate application inputs and enumerate immutable runtime dependencies."""

import os
import json
from pathlib import Path
import shutil
import subprocess

from application_cases import CASES
from application_run import participants


def configure(config):
    selected = set(config["cases"]) & CASES.keys()
    if not selected:
        return
    if any(d["adapter"] != "glass" for d in config["drivers"]):
        raise ValueError(
            "application cases require an eligible application MCP adapter (glass)"
        )
    required = {kind for case in selected for kind in participants(case).values()}
    applications = config.get("applications", {})
    if set(applications) != required:
        raise ValueError(f"applications must configure exactly {sorted(required)}")
    for kind, spec in applications.items():
        keys = (
            {"sdk", "image", "apk", "agent_jar", "a11y_apk"}
            if kind == "android"
            else (
                {"app", "runtime", "device_type", "companion"}
                if kind == "ios"
                else {"executable"}
            )
        )
        if kind == "electron":
            keys.add("bundle")
        if set(spec) != keys:
            raise ValueError(f"{kind} requires exactly {sorted(keys)}")
        for key in keys - {"image", "runtime", "device_type"}:
            spec[key] = str(Path(spec[key]).expanduser().resolve(strict=True))
        if kind == "electron":
            if config["viewport"] != [1000, 700]:
                raise ValueError(
                    "packaged Electron fixture requires viewport [1000, 700]"
                )
            if not Path(spec["executable"]).is_relative_to(spec["bundle"]):
                raise ValueError(
                    "Electron executable must belong to its packaged bundle"
                )
        if kind == "android":
            parts = spec["image"].split(";")
            if (
                len(parts) != 4
                or parts[0] != "system-images"
                or any(not part or "/" in part or part in (".", "..") for part in parts)
            ):
                raise ValueError("invalid Android system image package")


def prerequisites(config):
    errors = []
    for kind, spec in config.get("applications", {}).items():
        if kind == "ios":
            if not shutil.which("xcrun") or not os.access(spec["companion"], os.X_OK):
                errors.append(
                    "iOS publication requires Xcode and an executable companion"
                )
            if not (Path(spec["app"]) / "Info.plist").is_file():
                errors.append("iOS app bundle is missing Info.plist")
        elif kind != "android":
            if not os.access(spec["executable"], os.X_OK):
                errors.append(f"{kind} application is not executable")
            if (
                kind == "electron"
                and not (Path(spec["bundle"]) / "fixture-build.json").is_file()
            ):
                errors.append("Electron packaged build manifest is missing")
        else:
            sdk = Path(spec["sdk"])
            if not shutil.which("java"):
                errors.append("Android avdmanager requires Java")
            for command in (
                "platform-tools/adb",
                "emulator/emulator",
                "cmdline-tools/latest/bin/avdmanager",
            ):
                if not os.access(sdk / command, os.X_OK):
                    errors.append(f"Android prerequisite missing: {command}")
            if not os.access("/dev/kvm", os.R_OK | os.W_OK):
                errors.append("Android execution requires accessible KVM")
            if not (sdk.joinpath(*spec["image"].split(";")) / "system.img").is_file():
                errors.append("Android system image is not installed")
    return errors


def runtime_metadata(config):
    result = {}
    for kind, spec in config.get("applications", {}).items():
        if kind == "electron":
            result[kind] = json.loads(
                (Path(spec["bundle"]) / "fixture-build.json").read_bytes()
            )
        elif kind == "ios":
            runtimes = json.loads(
                subprocess.check_output(
                    ["xcrun", "simctl", "list", "runtimes", "--json"], timeout=15
                )
            )
            runtime = next(
                (r for r in runtimes["runtimes"] if r["identifier"] == spec["runtime"]),
                None,
            )
            if not runtime or not runtime.get("isAvailable"):
                raise ValueError("configured iOS Simulator runtime is unavailable")
            result[kind] = {
                "runtime": runtime,
                "sdk_version": subprocess.check_output(
                    ["xcrun", "--sdk", "iphonesimulator", "--show-sdk-version"],
                    timeout=15,
                )
                .decode()
                .strip(),
                "xcode_version": subprocess.check_output(
                    ["xcodebuild", "-version"], timeout=15
                )
                .decode()
                .strip(),
            }
        elif kind == "android":
            sdk = Path(spec["sdk"])
            result[kind] = {
                path.relative_to(sdk).as_posix(): path.read_text()
                for path in (
                    sdk / "platform-tools/source.properties",
                    sdk / "emulator/source.properties",
                    sdk.joinpath(*spec["image"].split(";")) / "source.properties",
                )
                if path.is_file()
            }
    return result


def frozen_paths(config):
    paths = set()
    for kind, spec in config.get("applications", {}).items():
        if kind == "electron":
            paths.update(p for p in Path(spec["bundle"]).rglob("*") if p.is_file())
        elif kind == "ios":
            paths.update(p for p in Path(spec["app"]).rglob("*") if p.is_file())
            paths.add(Path(spec["companion"]))
            frameworks = Path(spec["companion"]).parent.parent / "Frameworks"
            paths.update(p for p in frameworks.rglob("*") if p.is_file())
        elif kind == "native":
            paths.add(Path(spec["executable"]))
        else:
            paths.update(Path(spec[key]) for key in ("apk", "agent_jar", "a11y_apk"))
            sdk = Path(spec["sdk"])
            for root in (
                sdk / "platform-tools",
                sdk / "emulator",
                sdk / "cmdline-tools/latest",
                sdk.joinpath(*spec["image"].split(";")),
            ):
                paths.update(p for p in root.rglob("*") if p.is_file())
    return paths
