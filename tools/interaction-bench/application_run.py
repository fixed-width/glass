"""Owned application participants sharing one outcome, phase clock and evidence record."""

import json
import time

import application_cases as cases
from android_session import Android
from drivers.glass import Driver as Glass
from evidence import Evidence, EvidenceError
from measurement import (
    canonical,
    digest,
    file_identity,
    phase_totals,
    verify_files,
    write_json,
)
from protocol import Client, ProtocolError
from sessions import Session
from validation import read_channel, validate_totals


def participants(case):
    if case == "cross-application":
        return {"source": "native", "destination": "electron"}
    return {
        "app": {
            "native-form": "native",
            "electron-form": "electron",
            "android-boundary": "android",
        }[case]
    }


class UI(Glass):
    def __init__(self, fleet, name, kind, session, client, evidence):
        super().__init__(fleet.config, session, client, evidence, fleet.deadline - 30)
        self.fleet, self.name, self.kind = fleet, name, kind

    def call(self, *args, **kwargs):
        before = len(self.events)
        try:
            return super().call(*args, **kwargs)
        finally:
            self.fleet.events.extend(
                {**event, "participant": self.name} for event in self.events[before:]
            )

    def target(self, name, role="Button", scope=None):
        target = {"query": name, "role": role}
        if scope:
            target["within"] = scope
        elif self.kind == "electron":
            target["within"] = {"query": "Interaction fixture", "role": "Document"}
        return target

    def click(self, step, name, **kwargs):
        return self.call(
            step,
            "glass_click_element",
            {
                "target": self.target(name),
                "mode": "native" if self.kind == "electron" else "pointer",
                "timeout_ms": self.config["action_timeout_ms"],
            },
        )

    def read(self, step, name, expected=None, scope=None):
        accessible_name = (
            cases.ANDROID_NAMES.get(name, name) if self.kind == "android" else name
        )
        result = super().read(step, accessible_name, expected, scope)
        if result["result"].get("matched") is not True or len(result["nodes"]) != 1:
            raise EvidenceError(f"{step}: required field did not become observable")
        return result

    def label(self, step, name):
        result = self.call(
            step,
            "glass_wait_for_element",
            {
                "name": name,
                "role": "Label",
                "timeout_ms": self.config["action_timeout_ms"],
            },
        )
        if result["result"].get("matched") is not True:
            raise EvidenceError(f"{step}: native label did not become observable")
        return result

    def launch_application(self):
        spec = self.config["applications"][self.kind]
        if self.kind == "android":
            arguments = [
                spec["apk"],
                "tech.fixedwidth.glassrolefixture/.InteractionActivity",
            ]
        else:
            arguments = [spec["executable"]]
            if self.kind == "native" and self.name == "source":
                arguments.append("--source")
            elif self.kind == "electron":
                arguments += [
                    "--no-sandbox",
                    "--force-renderer-accessibility",
                    "--ozone-platform=x11",
                    "--disable-gpu",
                    f"--user-data-dir={self.session.root}/profile",
                ]
        self.call(
            "launch",
            "glass_start",
            {
                "run": arguments,
                "backend": "android" if self.kind == "android" else "x11",
                "sandbox": self.config["sandbox"],
                "a11y": True,
                "timeout_ms": 30000,
            },
        )
        if self.kind == "android":
            self.label("entry", "Native stage: entry")
        else:
            self.read("ready", "Fixture ready", "ready")
            if self.kind == "electron":
                width, height = self.config["viewport"]
                self.read("geometry", "Content geometry", f"{width}x{height}@1")
            else:
                self.call("geometry", "glass_window", {"op": "geometry"})

    def wait_windows(self, present):
        deadline = min(
            self.deadline, time.monotonic() + self.config["action_timeout_ms"] / 1000
        )
        prefix = "dialog_open" if present else "dialog_closed"
        index = 0
        while time.monotonic() < deadline:
            result = self.call(f"{prefix}_poll_{index}", "glass_list_windows", {})
            if (
                any(w.get("title") == "Confirm account" for w in result["windows"])
                == present
            ):
                return self.call(prefix, "glass_list_windows", {})["windows"]
            index += 1
            time.sleep(0.1)
        raise TimeoutError(f"{prefix}: native window transition timed out")

    def confirm_dialog(self):
        dialogs = [
            w for w in self.wait_windows(True) if w.get("title") == "Confirm account"
        ]
        if len(dialogs) != 1:
            raise EvidenceError("native dialog is ambiguous")
        self.call("dialog_select", "glass_select_window", {"id": dialogs[0]["id"]})
        self.call("dialog_focus", "glass_window", {"op": "focus"})
        self.call("dialog_confirm", "glass_key", {"chord": "Return"})
        windows = self.wait_windows(False)
        if len(windows) != 1:
            raise EvidenceError("main application window is ambiguous after dialog")
        self.call("main_select", "glass_select_window", {"id": windows[0]["id"]})


class Fleet:
    def __init__(self, config, cell, directory, deadline):
        self.config, self.cell, self.directory, self.deadline = (
            config,
            cell,
            directory,
            deadline,
        )
        self.members, self.owned, self.records, self.events = {}, {}, {}, []

    def prepare(self):
        command = next(
            d["command"]
            for d in self.config["drivers"]
            if d["id"] == self.cell["driver"]
        )
        for name, kind in participants(self.cell["case"]).items():
            directory = self.directory / name
            directory.mkdir()
            session = Session()
            owned = self.owned[name] = {
                "session": session,
                "android": None,
                "client": None,
            }
            record = self.records[name] = {
                "kind": kind,
                "case": self.cell["case"],
                "files": {},
                "session": {
                    "runtime_root": str(session.root),
                    "ownership_token": session.token,
                },
            }
            if kind == "android":
                owned["android"] = Android(
                    session,
                    self.config["applications"]["android"],
                    directory,
                    self.deadline - 30,
                )
                record["device"] = owned["android"].start()
                record["session"]["serial"] = owned["android"].serial
            else:
                record["session"]["display"] = session.start_display(
                    directory / "xvfb.log", *self.config["display"]
                )
                session.env["GLASS_DISPLAY"] = record["session"]["display"]
            client = owned["client"] = Client(
                command,
                directory / "mcp",
                env=session.env,
                cwd=directory,
                timeout=self.config["attempt_timeout_ms"] / 1000,
                deadline=self.deadline - 30,
                frame_limit=self.config["frame_limit_bytes"],
                evidence_limit=self.config["evidence_limit_bytes"],
            )
            init, inventory = client.initialize()
            record.update(
                server_info=init.get("serverInfo"),
                tool_definitions_bytes=len(canonical(inventory)),
                tool_definitions_sha256=digest(inventory),
                effective_command=command,
            )
            required = Glass.required_tools | {
                "glass_list_windows",
                "glass_select_window",
                "glass_key",
            }
            if required - {tool["name"] for tool in inventory}:
                raise EvidenceError("application participant lacks required MCP tools")
            evidence = Evidence(
                directory / "evidence",
                client,
                limit=self.config["evidence_limit_bytes"],
            )
            self.members[name] = UI(self, name, kind, session, client, evidence)

    def phase(self, name):
        for owned in self.owned.values():
            if owned["client"]:
                owned["client"].phase = name

    def close(self):
        errors = []
        self.phase("cleanup")
        for name, owned in reversed(list(self.owned.items())):
            client = owned["client"]
            if client:
                client.deadline = min(self.deadline, time.monotonic() + 8)
                if name in self.members and not client.poisoned:
                    self.members[name].deadline = client.deadline
                    try:
                        self.members[name].stop()
                    except Exception as exc:
                        errors.append(f"{name}: {exc}")
                try:
                    if not client.close(grace=1):
                        errors.append(f"{name}: MCP reader/process residue")
                except Exception as exc:
                    errors.append(f"{name}: {exc}")
            if owned["android"]:
                try:
                    if not owned["android"].close():
                        errors.append(f"{name}: emulator required forced cleanup")
                except Exception as exc:
                    errors.append(f"{name}: {exc}")
            try:
                cleanup = owned["session"].close()
                self.records[name]["cleanup"] = cleanup
                if not cleanup["ok"]:
                    errors.append(f"{name}: owned session process residue")
            except Exception as exc:
                errors.append(f"{name}: {exc}")
        return errors


def attempt(config, cell, directory):
    directory.mkdir()
    started = time.monotonic()
    fleet = Fleet(
        config, cell, directory, started + config["attempt_timeout_ms"] / 1000
    )
    result = {
        "schema_version": 1,
        **cell,
        "outcome": "harness_error",
        "error": "attempt incomplete",
        "cleanup_ok": False,
        "evidence_ok": False,
        "participants": {},
        "events": [],
        "files": {},
        "phases": {},
        "artifacts": {},
        "file_reads": [],
        "wall_ms": 0,
        "interrupted": False,
    }
    write_json(directory / "result.json", result)
    durations = {}
    current_phase, phase_start = "server_start", started

    def phase(name):
        nonlocal current_phase, phase_start
        now = time.monotonic()
        durations[current_phase] = (now - phase_start) * 1000
        current_phase, phase_start = name, now
        fleet.phase(name)

    try:
        fleet.prepare()
        phase("app_start")
        for name, member in fleet.members.items():
            member.launch_application()
            fleet.records[name]["owned_process_commands"] = (
                member.session.process_commands()
            )
        setup_errors = cases.evaluate_setup(
            cell["case"], fleet.events, config["viewport"]
        )
        if setup_errors:
            raise EvidenceError("; ".join(setup_errors))
        phase("task")
        cases.execute(cell["case"], fleet)
        result["assertion_errors"] = cases.evaluate(cell["case"], fleet.events)
        result["outcome"] = "failed" if result["assertion_errors"] else "task_completed"
        phase("evidence")
        result.update(evidence_ok=True, error=None)
    except BaseException as exc:
        result.update(
            outcome="harness_error"
            if isinstance(exc, (ProtocolError, OSError))
            else "failed",
            error=f"{type(exc).__name__}: {exc}",
            interrupted=isinstance(exc, KeyboardInterrupt),
        )
    finally:
        phase("cleanup")
        result["cleanup_errors"] = fleet.close()
        result["cleanup_ok"] = not result["cleanup_errors"]
        durations["cleanup"] = (time.monotonic() - phase_start) * 1000
        calls = []
        for name, record in fleet.records.items():
            member = fleet.members.get(name)
            record.update(
                events=member.events if member else [],
                artifacts=member.evidence.artifacts if member else {},
                artifact_references=member.evidence.references if member else [],
                file_reads=[],
                evidence_ok=result["evidence_ok"],
            )
            client = fleet.owned[name]["client"]
            if client:
                calls.extend(client.calls)
                if client.fault:
                    result["evidence_ok"] = False
                    record["evidence_error"] = client.fault
                    record["evidence_ok"] = False
            for path in (directory / name).rglob("*"):
                if path.is_file():
                    record["files"][str(path.relative_to(directory / name))] = (
                        file_identity(path)
                    )
        result.update(
            participants=fleet.records,
            events=fleet.events,
            phases=phase_totals(calls, durations),
            wall_ms=(time.monotonic() - started) * 1000,
            tool_definitions_bytes=sum(
                r.get("tool_definitions_bytes", 0) for r in fleet.records.values()
            ),
            tool_definitions_sha256=digest(
                {
                    name: r.get("tool_definitions_sha256")
                    for name, r in fleet.records.items()
                }
            ),
        )
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
            result.update(
                evidence_ok=False,
                error="attempt evidence exceeds configured storage limit",
            )
        write_json(directory / "result.json", result)
    return result


def validate_application(directory, result, config):
    errors, calls, events = verify_files(directory, result["files"]), [], []
    inventory_hashes, inventory_bytes = {}, 0
    for name, record in result["participants"].items():
        if (
            name not in participants(result["case"])
            or record["kind"] != participants(result["case"])[name]
        ):
            errors.append("unexpected application participant")
            continue
        inventory_path = directory / name / "mcp/inventory.json"
        if inventory_path.is_file():
            inventory = json.loads(inventory_path.read_bytes())
            inventory_hashes[name] = digest(inventory)
            inventory_bytes += len(canonical(inventory))
            if (
                record.get("tool_definitions_sha256"),
                record.get("tool_definitions_bytes"),
            ) != (digest(inventory), len(canonical(inventory))):
                errors.append(f"{name}: tool inventory accounting mismatch")
        elif result["outcome"] == "task_completed":
            errors.append(f"{name}: missing tool inventory")
        else:
            inventory_hashes[name] = None
        channel_errors, reconstructed, ledger = read_channel(
            directory / name, record, Glass
        )
        errors += [f"{name}: {error}" for error in channel_errors]
        calls += ledger
        indexed = {c["sequence"]: c for c in ledger}
        for event, raw in zip(record["events"], reconstructed):
            events.append(
                (
                    indexed[event["call"]]["started"],
                    {**raw, "call": event["call"], "participant": name},
                )
            )
    if result.get("tool_definitions_bytes") != inventory_bytes or result.get(
        "tool_definitions_sha256"
    ) != digest(inventory_hashes):
        errors.append("combined inventory accounting mismatch")
    recovered = [event for _, event in sorted(events, key=lambda pair: pair[0])]
    if recovered != result["events"]:
        errors.append("combined events differ from participant wire evidence")
    errors += validate_totals(directory, result, calls)
    if result["outcome"] == "task_completed":
        if set(result["participants"]) != set(participants(result["case"])):
            errors.append("required participant is missing")
        errors += cases.evaluate(result["case"], recovered)
        errors += cases.evaluate_setup(result["case"], recovered, config["viewport"])
    return errors
