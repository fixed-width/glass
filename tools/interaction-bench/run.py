#!/usr/bin/env python3
"""Run and revalidate repeated application interactions through external MCP servers."""

import argparse
from contextlib import contextmanager
import datetime as dt
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

from cases import CASES, REVISION, evaluate, evaluate_setup, execute
from drivers.glass import Driver as GlassDriver, normalize as glass_normalize
from evidence import Evidence, EvidenceError
from fixtures import Fixtures
from measurement import (
    canonical,
    call_metrics,
    digest,
    file_identity,
    owned_path,
    phase_totals,
    schedule,
    summarize,
    verify_files,
    write_json,
)
from protocol import Client, ProtocolError
from sessions import Session

ROOT = Path(__file__).resolve().parents[2]
DEFAULTS = {
    "schema_version": 1,
    "repetitions": 10,
    "warmups": 1,
    "seed": 41,
    "cases": list(CASES),
    "browser_family": "firefox",
    "sandbox": "off",
    "display": [1280, 900],
    "viewport": [1000, 700],
    "action_timeout_ms": 10000,
    "attempt_timeout_ms": 180000,
    "delay_ms": 3000,
    "motion_ms": 3000,
    "frame_limit_bytes": 64 * 1024 * 1024,
    "evidence_limit_bytes": 512 * 1024 * 1024,
    "allow_dirty": False,
    "optional_cases": [],
    "exclusions": [],
}


def load_config(path, registry):
    supplied = json.loads(Path(path).read_text())
    config = {**DEFAULTS, **supplied}
    unknown = (
        set(supplied)
        - set(DEFAULTS)
        - {"browser", "browser_args", "app_env", "drivers", "notes"}
    )
    if unknown:
        raise ValueError(f"unknown configuration options: {sorted(unknown)}")
    if config["schema_version"] != 1:
        raise ValueError("unsupported configuration schema")
    for key, minimum, maximum in (
        ("repetitions", 1, 1000),
        ("warmups", 0, 10),
        ("seed", 0, 2**32),
        ("action_timeout_ms", 1000, 120000),
        ("attempt_timeout_ms", 30000, 600000),
        ("motion_ms", 300, 30000),
        ("delay_ms", 250, 30000),
        ("frame_limit_bytes", 1024, 256 * 1024 * 1024),
        ("evidence_limit_bytes", 4096, 2 * 1024**3),
    ):
        if type(config[key]) is not int or not minimum <= config[key] <= maximum:
            raise ValueError(f"invalid {key}: expected {minimum}..{maximum}")
    if (
        not isinstance(config["cases"], list)
        or not config["cases"]
        or any(c not in CASES for c in config["cases"])
        or len(set(config["cases"])) != len(config["cases"])
    ):
        raise ValueError("cases must be a nonempty unique list of known cases")
    if config["browser_family"] not in ("firefox", "chromium") or config[
        "sandbox"
    ] not in ("off", "default", "strict"):
        raise ValueError("unknown browser family or sandbox")
    for key in ("display", "viewport"):
        if (
            not isinstance(config[key], list)
            or len(config[key]) != 2
            or any(type(n) is not int or not 100 <= n <= 8192 for n in config[key])
        ):
            raise ValueError(f"invalid {key} dimensions")
    if type(config["allow_dirty"]) is not bool:
        raise ValueError("allow_dirty must be boolean")
    config["browser"] = str(Path(config["browser"]).expanduser().resolve(strict=True))
    drivers = config.get(
        "drivers",
        [
            {
                "id": "glass",
                "adapter": "glass",
                "command": [str(ROOT / "target/release/glass-mcp")],
            }
        ],
    )
    ids = set()
    for driver in drivers:
        name = driver["id"]
        if (
            not isinstance(name, str)
            or not name.replace("-", "").replace("_", "").isalnum()
            or name in ids
        ):
            raise ValueError("driver IDs must be unique simple names")
        ids.add(name)
        if driver["adapter"] not in registry:
            raise ValueError(f"unknown driver adapter {driver['adapter']}")
        if (
            not isinstance(driver.get("command"), list)
            or not driver["command"]
            or any(not isinstance(a, str) for a in driver["command"])
        ):
            raise ValueError("driver command must be a nonempty argument array")
        resolved = shutil.which(driver["command"][0])
        if not resolved:
            raise ValueError(f"missing server executable: {driver['command'][0]}")
        driver["command"][0] = str(Path(resolved).resolve())
        identity_names = [Path(p).name for p in driver.get("identity_files", [])]
        if len(identity_names) != len(set(identity_names)):
            raise ValueError("identity_files basenames must be unique per driver")
    config["drivers"] = drivers
    if not drivers:
        raise ValueError("at least one driver is required")
    if not isinstance(config["optional_cases"], list) or any(
        case not in config["cases"] for case in config["optional_cases"]
    ):
        raise ValueError("optional_cases must name scheduled cases")
    excluded = set()
    for exclusion in config["exclusions"]:
        key = (exclusion["driver"], exclusion["case"])
        if (
            key[0] not in ids
            or key[1] not in config["optional_cases"]
            or key in excluded
            or not str(exclusion.get("reason", "")).strip()
        ):
            raise ValueError(
                "exclusions require a unique optional driver/case and reason"
            )
        excluded.add(key)
    return config


def git_output(*args):
    return (
        subprocess.check_output(["git", "-C", str(ROOT), *args]).decode("utf-8").strip()
    )


def preflight(config):
    errors = []
    if platform.system() != "Linux":
        errors.append(
            "execution currently requires Linux; offline validation is portable"
        )
    for command in ("Xvfb", "dbus-daemon"):
        if not shutil.which(command):
            errors.append(f"missing prerequisite {command}")
    if not os.access(config["browser"], os.X_OK):
        errors.append("browser is not executable")
    dirty = git_output("status", "--porcelain")
    if dirty and not config["allow_dirty"]:
        errors.append(
            "source checkout is dirty; commit it or set allow_dirty for labelled development evidence"
        )
    return {
        "ready": not errors,
        "errors": errors,
        "source": {
            "commit": git_output("rev-parse", "HEAD"),
            "tree": git_output("rev-parse", "HEAD^{tree}"),
            "dirty": dirty,
        },
        "browser": {"path": config["browser"], **file_identity(config["browser"])},
        "configuration_sha256": digest(config),
        "scheduled_attempts": len(config["cases"])
        * len(config["drivers"])
        * (config["repetitions"] + config["warmups"]),
        "note": "Checks local prerequisites only; live publication/outcomes require a diagnostic run.",
    }


@contextmanager
def phase(client, durations, name):
    client.phase = name
    started = time.monotonic()
    try:
        yield
    finally:
        durations[name] = (time.monotonic() - started) * 1000


def attempt(config, cell, directory, fixtures, adapter):
    directory.mkdir()
    began = time.monotonic()
    result = {
        "schema_version": 1,
        **cell,
        "outcome": "harness_error",
        "cleanup_ok": False,
        "evidence_ok": False,
        "error": "attempt incomplete",
        "wall_ms": 0,
        "events": [],
        "phases": {},
        "artifacts": {},
        "artifact_references": [],
        "files": {},
    }
    write_json(directory / "result.json", result)
    client = session = driver = evidence = None
    durations = {}
    interrupted = False
    errors = []
    spec = next(d for d in config["drivers"] if d["id"] == cell["driver"])
    total_deadline = began + config["attempt_timeout_ms"] / 1000
    try:
        session = Session()
        display = session.start_display(directory / "xvfb.log", *config["display"])
        if spec["adapter"] == "glass":
            session.env["GLASS_DISPLAY"] = display
        command = (
            adapter.command(spec, config, directory, cell["case"])
            if hasattr(adapter, "command")
            else spec["command"]
        )
        result["effective_command"] = command
        result["session"] = {
            "display": display,
            "runtime_root": str(session.root),
            "ownership_token": session.token,
        }
        client = Client(
            command,
            directory / "mcp",
            env=session.env,
            cwd=directory,
            timeout=config["attempt_timeout_ms"] / 1000,
            deadline=total_deadline - 10,
            frame_limit=config["frame_limit_bytes"],
            evidence_limit=config["evidence_limit_bytes"],
        )
        init, inventory = client.initialize()
        result["server_info"] = init.get("serverInfo")
        result["tool_definitions_bytes"] = len(canonical(inventory))
        result["tool_definitions_sha256"] = digest(inventory)
        missing = adapter.required_tools - {t["name"] for t in inventory}
        if missing:
            raise EvidenceError(f"required tools missing: {sorted(missing)}")
        durations["server_start"] = (time.monotonic() - began) * 1000
        evidence = Evidence(
            directory / "evidence", client, limit=config["evidence_limit_bytes"]
        )
        driver = adapter(config, session, client, evidence, total_deadline - 10)
        url = fixtures.url(cell["case"], config)
        result["fixture_url"] = url
        with phase(client, durations, "app_start"):
            driver.launch(url)
            setup_errors = evaluate_setup(driver.events, config["viewport"])
            if setup_errors:
                raise EvidenceError("; ".join(setup_errors))
            result["owned_process_commands"] = session.process_commands()
        with phase(client, durations, "task"):
            execute(cell["case"], driver)
            errors = evaluate(cell["case"], driver.events, evidence.artifacts)
            result["outcome"] = "failed" if errors else CASES[cell["case"]]
        with phase(client, durations, "evidence"):
            if hasattr(driver, "collect"):
                driver.collect()
        result["evidence_ok"] = True
        result["error"] = None
    except BaseException as exc:
        interrupted = isinstance(exc, KeyboardInterrupt)
        result["outcome"] = (
            "harness_error" if isinstance(exc, (ProtocolError, OSError)) else "failed"
        )
        result["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        if driver and not result["evidence_ok"] and hasattr(driver, "collect"):
            try:
                with phase(client, durations, "evidence"):
                    driver.collect()
                result["evidence_ok"] = True
            except Exception as exc:
                result["evidence_error"] = str(exc)
        cleanup_start = time.monotonic()
        if "server_start" not in durations:
            durations["server_start"] = (cleanup_start - began) * 1000
        cleanup_errors = []
        if client:
            client.phase = "cleanup"
            client.deadline = min(total_deadline, time.monotonic() + 5)
            if driver and not client.poisoned:
                driver.deadline = time.monotonic() + 5
                try:
                    driver.stop()
                except Exception as exc:
                    cleanup_errors.append(str(exc))
            try:
                if not client.close(grace=1):
                    cleanup_errors.append("MCP process group or reader residue")
                if client.fault:
                    result["evidence_ok"] = False
                    result["evidence_error"] = client.fault
            except Exception as exc:
                cleanup_errors.append(str(exc))
        if session:
            try:
                result["cleanup"] = session.close()
                if not result["cleanup"]["ok"]:
                    cleanup_errors.append(
                        "owned session processes required forced cleanup"
                    )
            except Exception as exc:
                cleanup_errors.append(str(exc))
        durations["cleanup"] = (time.monotonic() - cleanup_start) * 1000
        result["cleanup_ok"] = not cleanup_errors
        result["cleanup_errors"] = cleanup_errors
        result["assertion_errors"] = errors
        result["wall_ms"] = (time.monotonic() - began) * 1000
        result["events"] = driver.events if driver else []
        result["artifacts"] = evidence.artifacts if evidence else {}
        result["artifact_references"] = evidence.references if evidence else []
        file_reads = getattr(driver, "file_reads", [])
        result["file_reads"] = file_reads
        result["phases"] = phase_totals(
            (client.calls if client else []) + file_reads, durations
        )
        result["interrupted"] = interrupted
        write_json(
            directory / "timing.json",
            {"wall_ms": result["wall_ms"], "durations": durations},
        )
        for path in directory.rglob("*"):
            if path.is_file() and path.name != "result.json":
                result["files"][str(path.relative_to(directory))] = file_identity(path)
        if (
            sum(v["bytes"] for v in result["files"].values())
            > config["evidence_limit_bytes"]
        ):
            result["evidence_ok"] = False
            result["error"] = "attempt evidence exceeds configured storage limit"
        write_json(directory / "result.json", result)
    return result


class ReplayClient:
    def __init__(self, directory, calls, origin):
        self.directory, self.all_calls = directory, calls
        self.calls = [{"sequence": origin}]
        self.origin = origin

    def rpc(self, method, params, **kwargs):
        for call in self.all_calls:
            if call["sequence"] <= self.origin:
                continue
            if call["method"] == "tools/call":
                break
            request = json.loads(
                owned_path(self.directory, call["request_file"]).read_bytes()
            )
            if (
                request["method"] == method
                and request["params"] == params
                and call["response_file"]
            ):
                self.origin = call["sequence"]
                return json.loads(
                    owned_path(self.directory, call["response_file"]).read_bytes()
                )
        raise EvidenceError("missing recorded resource read")


def validate_attempt(directory, result, adapter, config=None):
    errors = verify_files(directory, result["files"])
    if result["outcome"] in ("skipped", "unsupported"):
        return errors
    calls_path = directory / "mcp/calls.json"
    if not calls_path.is_file():
        return errors + ["missing call ledger"]
    calls = json.loads(calls_path.read_bytes())
    sequences = [c["sequence"] for c in calls]
    if sequences != list(range(1, len(calls) + 1)):
        errors.append("RPC ledger sequences are not contiguous and unique")
    for call in calls:
        try:
            request_path = owned_path(directory / "mcp", call["request_file"])
            raw = request_path.read_bytes()
            request = json.loads(raw)
            if (
                request["id"] != call["sequence"]
                or request["method"] != call["method"]
                or len(raw) != call["request_wire_bytes"]
            ):
                errors.append(f"request accounting mismatch: {call['sequence']}")
            response = {"result": {}}
            if call["response_file"]:
                raw = owned_path(directory / "mcp", call["response_file"]).read_bytes()
                response = json.loads(raw)
                if (
                    response["id"] != call["sequence"]
                    or len(raw) != call["response_wire_bytes"]
                ):
                    errors.append(f"response accounting mismatch: {call['sequence']}")
            for key, expected in call_metrics(request, response).items():
                if (
                    key not in ("request_wire_bytes", "response_wire_bytes")
                    and call.get(key, 0) != expected
                ):
                    errors.append(f"call metric mismatch: {call['sequence']} {key}")
        except (ValueError, KeyError, OSError) as exc:
            errors.append(f"invalid call record: {exc}")
    timing = json.loads((directory / "timing.json").read_bytes())
    if result["wall_ms"] != timing["wall_ms"] or result["phases"] != phase_totals(
        calls + result.get("file_reads", []), timing["durations"]
    ):
        errors.append("phase totals differ from recorded calls and timing")
    indexed = {c["sequence"]: c for c in calls}
    for sha, artifact in result["artifacts"].items():
        try:
            identity = file_identity(
                owned_path(directory / "evidence", artifact["path"])
            )
            if sha != artifact["sha256"] or identity != {
                "bytes": artifact["bytes"],
                "sha256": sha,
            }:
                errors.append("artifact metadata differs from archived body")
        except (ValueError, KeyError, OSError) as exc:
            errors.append(f"invalid archived artifact: {exc}")
    for read in result.get("file_reads", []):
        artifact = result["artifacts"].get(read.get("sha256"), {})
        if (
            read.get("file_reads") != 1
            or read.get("response_text_bytes") != artifact.get("bytes")
            or read.get("origin_call") not in indexed
            or not any(
                ref["sha256"] == read.get("sha256")
                and ref["origin_call"] == read.get("origin_call")
                for ref in result.get("artifact_references", [])
            )
        ):
            errors.append("file read accounting differs from artifact evidence")
    if result["case"] == "large-form":
        task_calls = [c for c in calls if c["phase"] == "task"]
        if any(
            c.get("images")
            or c.get("tool") in ("glass_snapshot", "glass_a11y_snapshot")
            for c in task_calls
        ):
            errors.append("normal form workflow acquired an image or full snapshot")
    reconstructed = []
    with tempfile.TemporaryDirectory(prefix="interaction-validate-") as temporary:
        for event in result["events"]:
            try:
                call = indexed[event["call"]]
                if call["step"] != event["step"] or call["response_file"] is None:
                    raise EvidenceError("event does not match its RPC ledger entry")
                request = json.loads(
                    owned_path(directory / "mcp", call["request_file"]).read_bytes()
                )
                response = json.loads(
                    owned_path(directory / "mcp", call["response_file"]).read_bytes()
                )
                if hasattr(adapter, "replay"):
                    facts = adapter.replay(
                        request["params"], response, directory, result
                    )
                else:
                    evidence = Evidence(
                        Path(temporary),
                        ReplayClient(directory / "mcp", calls, event["call"]),
                    )
                    facts = glass_normalize(
                        request["params"],
                        evidence.decode(request["params"]["name"], response),
                    )
                if facts != event["facts"]:
                    errors.append(f"changed interpreted facts: {event['step']}")
                reconstructed.append({"step": event["step"], "facts": facts})
            except (ValueError, KeyError, OSError, EvidenceError) as exc:
                errors.append(f"{event['step']}: {exc}")
    oracle_errors = evaluate(result["case"], reconstructed, result["artifacts"])
    if result["outcome"] in ("task_completed", "expected_refusal"):
        errors += oracle_errors
        if config:
            errors += evaluate_setup(reconstructed, config["viewport"])
        if result["outcome"] != CASES[result["case"]]:
            errors.append("case outcome is misclassified")
    return errors


def saved_run(directory, registry):
    directory = Path(directory)
    manifest = json.loads((directory / "manifest.json").read_bytes())
    if manifest["configuration_sha256"] != digest(manifest["config"]):
        raise ValueError("configuration digest mismatch")
    config = manifest["config"]
    if manifest["schedule"] != schedule(
        config["cases"],
        [d["id"] for d in config["drivers"]],
        config["repetitions"],
        config["warmups"],
        config["seed"],
    ):
        raise ValueError("schedule differs from frozen configuration")
    rows, errors = [], verify_files(directory, manifest.get("source_files", {}))
    if manifest.get("frozen_files"):
        errors.extend(
            manifest.get(
                "final_identity_errors", ["run did not finish identity verification"]
            )
        )
    inventories = {}
    for index, cell in enumerate(manifest["schedule"]):
        path = directory / f"{index:04}-{cell['driver']}-{cell['case']}"
        if not (path / "result.json").is_file():
            errors.append(f"missing scheduled attempt {path.name}")
            continue
        row = json.loads((path / "result.json").read_bytes())
        if any(row.get(k) != v for k, v in cell.items()):
            errors.append(f"attempt identity mismatch {path.name}")
            continue
        if row.get("optional_exclusion") and not any(
            x["driver"] == cell["driver"]
            and x["case"] == cell["case"]
            and x["reason"] == row.get("reason")
            for x in config.get("exclusions", [])
        ):
            errors.append(f"undeclared optional exclusion {path.name}")
        inventory_path = path / "mcp/inventory.json"
        if inventory_path.is_file():
            inventory = json.loads(inventory_path.read_bytes())
            identity = (digest(inventory), len(canonical(inventory)))
            if identity != (
                row.get("tool_definitions_sha256"),
                row.get("tool_definitions_bytes"),
            ):
                errors.append(f"tool inventory accounting mismatch {path.name}")
            if inventories.setdefault(cell["driver"], identity) != identity:
                errors.append(f"tool inventory changed during cohort {path.name}")
        adapter = registry[
            next(d["adapter"] for d in config["drivers"] if d["id"] == cell["driver"])
        ]
        errors.extend(
            f"{path.name}: {error}"
            for error in validate_attempt(path, row, adapter, config)
        )
        rows.append(row)
    return manifest, rows, errors


def run(config, output, registry):
    started = time.monotonic()
    check = preflight(config)
    if not check["ready"]:
        raise ValueError("; ".join(check["errors"]))
    output.mkdir(parents=True, exist_ok=False)
    rows = schedule(
        config["cases"],
        [d["id"] for d in config["drivers"]],
        config["repetitions"],
        config["warmups"],
        config["seed"],
    )
    manifest = {
        "schema_version": 1,
        "recipe_revision": REVISION,
        "config": config,
        "configuration_sha256": digest(config),
        "schedule": rows,
        "preflight": check,
        "created_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "host": {
            "system": platform.platform(),
            "machine": platform.machine(),
            "node": platform.node(),
        },
        "identities": {},
        "fixture_files": {},
    }
    manifest["browser_version"] = (
        subprocess.check_output(
            [config["browser"], "--version"], timeout=10, stderr=subprocess.STDOUT
        )
        .decode("utf-8")
        .strip()
    )
    manifest["browser_components"] = {
        name: file_identity(Path(config["browser"]).parent / name)
        for name in (
            "application.ini",
            "platform.ini",
            "libxul.so",
            "omni.ja",
            "browser/omni.ja",
        )
        if (Path(config["browser"]).parent / name).is_file()
    }
    for driver in config["drivers"]:
        manifest["identities"][driver["id"]] = {
            "command": driver["command"],
            "executable": file_identity(driver["command"][0]),
        }
        for index, argument in enumerate(driver["command"][1:], 1):
            if Path(argument).is_file():
                manifest["identities"][f"{driver['id']}:argument:{index}"] = (
                    file_identity(argument)
                )
        for identity_path in driver.get("identity_files", []):
            manifest["identities"][f"{driver['id']}:{identity_path}"] = file_identity(
                identity_path
            )
            target = (
                output / "source/adapters" / driver["id"] / Path(identity_path).name
            )
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(identity_path, target)
    for source in (ROOT / "tools/interaction-bench").rglob("*.py"):
        manifest["identities"][str(source.relative_to(ROOT))] = file_identity(source)
        target = output / "source" / source.relative_to(ROOT)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
    for path in (ROOT / "examples/interaction-fixture").iterdir():
        if path.is_file():
            manifest["fixture_files"][path.name] = file_identity(path)
            target = output / "source/examples/interaction-fixture" / path.name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
    manifest["source_files"] = {
        str(path.relative_to(output)): file_identity(path)
        for path in (output / "source").rglob("*")
        if path.is_file()
    }
    frozen_paths = {Path(config["browser"])}
    frozen_paths.update(
        Path(config["browser"]).parent / name for name in manifest["browser_components"]
    )
    for driver in config["drivers"]:
        frozen_paths.update(
            Path(arg) for arg in driver["command"] if Path(arg).is_file()
        )
        frozen_paths.update(Path(arg) for arg in driver.get("identity_files", []))
    manifest["frozen_files"] = {
        str(path): file_identity(path) for path in sorted(frozen_paths)
    }
    if check["source"]["dirty"]:
        (output / "source.patch").write_bytes(
            subprocess.check_output(
                ["git", "-C", str(ROOT), "diff", "HEAD", "--binary"]
            )
        )
    fixtures = Fixtures(ROOT / "examples/interaction-fixture")
    manifest["preparation_ms"] = (time.monotonic() - started) * 1000
    write_json(output / "manifest.json", manifest)
    results, halted = [], None
    try:
        for index, cell in enumerate(rows):
            directory = output / f"{index:04}-{cell['driver']}-{cell['case']}"
            adapter = registry[
                next(
                    d["adapter"] for d in config["drivers"] if d["id"] == cell["driver"]
                )
            ]
            exclusion = next(
                (
                    x
                    for x in config["exclusions"]
                    if x["driver"] == cell["driver"] and x["case"] == cell["case"]
                ),
                None,
            )
            if halted or exclusion:
                directory.mkdir()
                result = {
                    **cell,
                    "outcome": "skipped",
                    "reason": halted or exclusion["reason"],
                    "optional_exclusion": bool(exclusion and not halted),
                    "files": {},
                    "wall_ms": 0,
                }
                write_json(directory / "result.json", result)
            else:
                result = attempt(config, cell, directory, fixtures, adapter)
                if not result["cleanup_ok"] or result["interrupted"]:
                    halted = "previous attempt interrupted or cleanup failed"
            results.append(result)
            print(
                f"{index + 1}/{len(rows)} {cell['driver']} {cell['case']} {'warmup' if cell['warmup'] else 'measured'}: {result['outcome']} ({result['wall_ms'] / 1000:.2f}s)",
                flush=True,
            )
            if result.get("error") or result.get("assertion_errors"):
                print(result.get("error") or result.get("assertion_errors"), flush=True)
            write_json(
                output / "summary.json",
                {"configuration_sha256": digest(config), "groups": summarize(results)},
            )
    finally:
        fixtures.close()
        manifest["final_identity_errors"] = []
        for path, expected in manifest["frozen_files"].items():
            try:
                if file_identity(path) != expected:
                    manifest["final_identity_errors"].append(
                        f"changed frozen input: {path}"
                    )
            except OSError as exc:
                manifest["final_identity_errors"].append(str(exc))
        write_json(output / "manifest.json", manifest)
    return int(
        bool(manifest["final_identity_errors"])
        or any(
            r["outcome"] not in ("task_completed", "expected_refusal")
            or not r.get("cleanup_ok")
            or not r.get("evidence_ok")
            for r in results
            if not r.get("optional_exclusion")
        )
    )


def main(registry=None):
    registry = registry or {"glass": GlassDriver}
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    for name in ("preflight", "run"):
        command = commands.add_parser(name)
        command.add_argument("--config", required=True, type=Path)
        if name == "run":
            command.add_argument("--output", type=Path)
    for name in ("validate", "summarize"):
        commands.add_parser(name).add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "list":
            print(json.dumps(CASES, indent=2))
            return 0
        if args.command in ("validate", "summarize"):
            manifest, rows, errors = saved_run(args.directory, registry)
            if errors:
                print("\n".join(errors), file=sys.stderr)
                return 1
            if args.command == "summarize":
                print(
                    json.dumps(
                        {
                            "configuration_sha256": manifest["configuration_sha256"],
                            "groups": summarize(rows),
                        },
                        indent=2,
                    )
                )
            else:
                print(
                    f"Validated {len(rows)} attempts; unsuccessful attempts remain in the report."
                )
            return 0
        config = load_config(args.config, registry)
        if args.command == "preflight":
            result = preflight(config)
            print(json.dumps(result, indent=2))
            return int(not result["ready"])
        output = args.output or ROOT / "target/interaction-bench" / (
            dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ-")
            + uuid.uuid4().hex[:8]
        )
        print(output.resolve(), flush=True)
        return run(config, output.resolve(), registry)
    except (ValueError, KeyError, OSError) as exc:
        print(f"interaction-bench: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
