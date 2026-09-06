"""Owned application participants sharing one outcome, phase clock and evidence record."""

import json
import time

import application_cases as cases
from android_session import Android
from attempt_record import AttemptRecord
import ios_publication
from drivers.glass import Driver as Glass
from evidence import Evidence, EvidenceError
from measurement import (
    canonical,
    digest,
    file_identity,
    verify_files,
)
from protocol import Client
from native_sessions import create_session, prepare_display
from validation import read_channel, validate_totals


def participants(case):
    if case == "cross-application":
        return {"source": "native", "destination": "electron"}
    return {
        "app": {
            "native-form": "native",
            "electron-form": "electron",
            "android-boundary": "android",
            "ios-publication": "ios",
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
        if (
            self.kind == "native"
            and self.config.get("backend") == "macos"
            and step == "empty"
            and name == "Account name"
            and expected == ""
        ):
            return self.call(
                step,
                "glass_set_value",
                {
                    "target": self.target(name, "TextField"),
                    "text": "",
                    "timeout_ms": self.config["action_timeout_ms"],
                },
            )
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
        if self.kind == "ios":
            arguments = [spec["app"], "--tab=controls"]
        elif self.kind == "android":
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
                "backend": (
                    self.kind
                    if self.kind in ("android", "ios")
                    else self.config.get("backend", "x11")
                ),
                "sandbox": self.config["sandbox"],
                "a11y": True,
                "timeout_ms": 30000,
            },
        )
        if self.kind == "ios":
            return
        if self.kind == "android":
            self.label("entry", "Native stage: entry")
        else:
            self.read("ready", "Fixture ready", "ready")
            if self.kind == "electron":
                width, height = self.config["viewport"]
                self.read("geometry", "Content geometry", f"{width}x{height}@1")
            else:
                if self.config.get("backend", "x11") in ("macos", "windows"):
                    self.call(
                        "resize_native",
                        "glass_window",
                        {"op": "resize", "width": 600, "height": 500},
                    )
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
            session = create_session(self.config.get("backend", "x11"))
            owned = self.owned[name] = {
                "session": session,
                "android": None,
                "ios": None,
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
            if kind == "ios":
                owned["ios"] = ios_publication.IOS(
                    session,
                    self.config["applications"]["ios"],
                    directory,
                    self.deadline - 30,
                )
                record["device"] = owned["ios"].start()
                record["session"]["udid"] = owned["ios"].udid
            elif kind == "android":
                owned["android"] = Android(
                    session,
                    self.config["applications"]["android"],
                    directory,
                    self.deadline - 30,
                )
                record["device"] = owned["android"].start()
                record["session"]["serial"] = owned["android"].serial
            else:
                record["session"]["display"] = prepare_display(
                    session, self.config, directory
                )
                record["session"]["backend"] = self.config.get("backend", "x11")
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
            if kind == "ios":
                required |= {"glass_screenshot"}
            if kind == "native" and self.config.get("backend") == "macos":
                required |= {"glass_set_value"}
            if required - {tool["name"] for tool in inventory}:
                raise EvidenceError("application participant lacks required MCP tools")
            evidence = Evidence(
                directory / "evidence",
                client,
                limit=self.config["evidence_limit_bytes"],
            )
            self.members[name] = UI(self, name, kind, session, client, evidence)

    @property
    def clients(self):
        return [owned["client"] for owned in self.owned.values() if owned["client"]]

    def close(self):
        errors = []
        for client in self.clients:
            client.phase = "cleanup"
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
            if owned.get("ios"):
                try:
                    if not owned["ios"].close():
                        errors.append(f"{name}: simulator cleanup failed")
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
    record = AttemptRecord(
        config, cell, directory, participants={}, file_reads=[], interrupted=False
    )
    result = record.result
    fleet = Fleet(config, cell, directory, record.deadline)

    try:
        fleet.prepare()
        record.advance("app_start", fleet.clients)
        for name, member in fleet.members.items():
            member.launch_application()
            fleet.records[name][
                "owned_process_commands"
            ] = member.session.process_commands()
        setup_errors = cases.evaluate_setup(
            cell["case"], fleet.events, config["viewport"]
        )
        if setup_errors:
            raise EvidenceError("; ".join(setup_errors))
        record.advance("task", fleet.clients)
        cases.execute(cell["case"], fleet)
        if cell["case"] == "ios-publication":
            result["outcome"], result["assertion_errors"] = ios_publication.evaluate(
                fleet.events
            )
        else:
            result["assertion_errors"] = cases.evaluate(cell["case"], fleet.events)
            result["outcome"] = (
                "failed" if result["assertion_errors"] else "task_completed"
            )
        record.advance("evidence", fleet.clients)
        result.update(evidence_ok=True, error=None)
    except BaseException as exc:
        record.fail(exc)
    finally:
        record.advance("cleanup", fleet.clients)
        result["cleanup_errors"] = fleet.close()
        result["cleanup_ok"] = not result["cleanup_errors"]
        record.advance(None)
        calls = []
        for name, participant in fleet.records.items():
            member = fleet.members.get(name)
            participant.update(
                events=member.events if member else [],
                artifacts=member.evidence.artifacts if member else {},
                artifact_references=member.evidence.references if member else [],
                file_reads=[],
                evidence_ok=result["evidence_ok"],
            )
            client = fleet.owned[name]["client"]
            if client:
                calls.extend(client.calls)
                participant["process_cleanup"] = client.cleanup
                if client.fault:
                    result["evidence_ok"] = False
                    participant["evidence_error"] = client.fault
                    participant["evidence_ok"] = False
            for path in (directory / name).rglob("*"):
                if path.is_file():
                    participant["files"][
                        path.relative_to(directory / name).as_posix()
                    ] = file_identity(path)
        record.record_calls(calls)
        record.capture_wall()
        result.update(
            participants=fleet.records,
            events=fleet.events,
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
        record.finalize()
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
    if result["case"] == "ios-publication" and result["outcome"] in (
        "task_completed",
        "unsupported",
    ):
        errors += ios_publication.validate_probe(
            directory / "app",
            result["participants"]["app"],
            config["applications"]["ios"],
        )
        outcome, assertions = ios_publication.evaluate(recovered)
        errors += assertions
        if result["outcome"] != outcome:
            errors.append("publication outcome is misclassified")
    if result["outcome"] == "task_completed" and result["case"] != "ios-publication":
        if set(result["participants"]) != set(participants(result["case"])):
            errors.append("required participant is missing")
        errors += cases.evaluate(result["case"], recovered)
        errors += cases.evaluate_setup(result["case"], recovered, config["viewport"])
    return errors
