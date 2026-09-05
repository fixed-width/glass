"""A bounded external stdio client. Every RPC is dispatched at most once."""

import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import threading
import time

from measurement import call_metrics, canonical, write_json


class ProtocolError(RuntimeError):
    pass


class Client:
    def __init__(
        self,
        command,
        directory,
        *,
        env=None,
        cwd=None,
        timeout=30,
        deadline=None,
        frame_limit=64 * 1024 * 1024,
        evidence_limit=512 * 1024 * 1024,
    ):
        self.directory = Path(directory)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.timeout, self.frame_limit, self.evidence_limit = (
            timeout,
            frame_limit,
            evidence_limit,
        )
        self.deadline = deadline
        self.phase, self.step = "server_start", "initialize"
        self.calls, self.sequence, self.received_bytes = [], 0, 0
        self.messages = queue.Queue(maxsize=128)
        self.stopping = threading.Event()
        self.lock = threading.Lock()
        self.fault = None
        self.poisoned = False
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
            start_new_session=os.name == "posix",
        )
        self.job = None
        self.cleanup = {}
        if os.name == "nt":
            from windows_job import Job

            try:
                self.job = Job(self.process)
            except BaseException:
                self.process.kill()
                self.process.wait(timeout=2)
                for stream in (
                    self.process.stdin,
                    self.process.stdout,
                    self.process.stderr,
                ):
                    stream.close()
                raise
        self.readers = [
            threading.Thread(
                target=self._pump,
                args=(self.process.stdout, "stdout.bin", True),
                daemon=True,
            ),
            threading.Thread(
                target=self._pump,
                args=(self.process.stderr, "stderr.bin", False),
                daemon=True,
            ),
        ]
        for thread in self.readers:
            thread.start()

    def _pump(self, stream, filename, messages):
        pending = bytearray()
        try:
            with (self.directory / filename).open("wb") as output:
                while not self.stopping.is_set():
                    chunk = os.read(stream.fileno(), 65536)
                    if not chunk:
                        if pending:
                            raise ProtocolError("stdout ended inside a message")
                        break
                    with self.lock:
                        remaining = self.evidence_limit - self.received_bytes
                        self.received_bytes += len(chunk)
                    output.write(chunk[: max(0, remaining)])
                    output.flush()
                    if len(chunk) > remaining:
                        raise ProtocolError("attempt wire evidence limit exceeded")
                    if not messages:
                        continue
                    pending.extend(chunk)
                    while b"\n" in pending:
                        line, _, rest = pending.partition(b"\n")
                        pending = bytearray(rest)
                        if len(line) > self.frame_limit:
                            raise ProtocolError("MCP frame limit exceeded")
                        self.messages.put_nowait(bytes(line))
                    if len(pending) > self.frame_limit:
                        raise ProtocolError("MCP frame limit exceeded")
        except (OSError, ValueError, ProtocolError, queue.Full) as exc:
            self.fault = str(exc) or "stdout queue limit exceeded"
        finally:
            if messages:
                try:
                    self.messages.put_nowait(None)
                except queue.Full:
                    self.fault = "stdout queue limit exceeded"

    def _send(self, payload, deadline):
        if time.monotonic() >= deadline:
            raise ProtocolError("timeout before MCP request dispatch")
        raw = canonical(payload)
        if len(raw) > self.frame_limit:
            raise ProtocolError("request exceeds frame limit")
        errors = []

        def write():
            try:
                self.process.stdin.write(raw + b"\n")
                self.process.stdin.flush()
            except (OSError, ValueError) as exc:
                errors.append(exc)

        writer = threading.Thread(target=write, daemon=True)
        writer.start()
        writer.join(max(0, deadline - time.monotonic()))
        if writer.is_alive():
            raise ProtocolError("timeout writing MCP request")
        if errors:
            raise ProtocolError(f"MCP request write failed: {errors[0]}")
        return raw

    def notify(self, method, params):
        payload = {"jsonrpc": "2.0", "method": method, "params": params}
        raw = self._send(
            payload, min(time.monotonic() + self.timeout, self.deadline or float("inf"))
        )
        with (self.directory / "notifications.sent.jsonl").open("ab") as stream:
            stream.write(raw + b"\n")

    def rpc(self, method, params, *, timeout=None):
        if self.poisoned:
            raise ProtocolError("MCP stream unusable after a lost or invalid reply")
        self.sequence += 1
        number = self.sequence
        request = {"jsonrpc": "2.0", "id": number, "method": method, "params": params}
        raw_request = canonical(request)
        request_file, response_file = (
            f"{number:04}.request.json",
            f"{number:04}.response.json",
        )
        (self.directory / request_file).write_bytes(raw_request)
        started = time.monotonic()
        budget = self.timeout if timeout is None else min(timeout, self.timeout)
        if self.deadline is not None:
            budget = max(0, min(budget, self.deadline - started))
        deadline = started + budget
        record = {
            "sequence": number,
            "phase": self.phase,
            "step": self.step,
            "method": method,
            "tool": params.get("name") if method == "tools/call" else None,
            "request_file": request_file,
            "request_wire_bytes": len(raw_request),
            "response_file": None,
            "response_wire_bytes": 0,
            "started": started,
            "timeout_ms": budget * 1000,
            "status": "pending",
        }
        record.update(call_metrics(request, {"result": {}}))
        record["request_wire_bytes"] = len(raw_request)
        self.calls.append(record)
        write_json(self.directory / "calls.json", self.calls)
        try:
            self._send(request, deadline)
            while True:
                if self.fault:
                    raise ProtocolError(self.fault)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ProtocolError("timeout waiting for MCP response")
                try:
                    line = self.messages.get(timeout=min(remaining, 0.1))
                except queue.Empty:
                    continue
                if line is None:
                    raise ProtocolError(self.fault or "MCP server closed stdout")
                try:
                    response = json.loads(line.decode("utf-8"))
                except (UnicodeError, ValueError) as exc:
                    raise ProtocolError(f"invalid MCP JSON/UTF-8: {exc}") from exc
                if not isinstance(response, dict) or response.get("jsonrpc") != "2.0":
                    raise ProtocolError("invalid JSON-RPC envelope")
                if response.get("id") != number:
                    continue
                if ("result" in response) == ("error" in response):
                    raise ProtocolError(
                        "MCP response requires exactly one result or error"
                    )
                if not isinstance(response.get("result", response.get("error")), dict):
                    raise ProtocolError("MCP result/error must be an object")
                (self.directory / response_file).write_bytes(line)
                record.update(call_metrics(request, response))
                record.update(
                    request_wire_bytes=len(raw_request),
                    response_wire_bytes=len(line),
                    response_file=response_file,
                    status="received",
                )
                record["is_error"] = "error" in response or response.get(
                    "result", {}
                ).get("isError", False)
                return response
        except KeyboardInterrupt:
            self.poisoned = True
            record["status"] = "interrupted"
            raise
        except (ProtocolError, ValueError, KeyError, TypeError) as exc:
            self.poisoned = True
            record["status"] = "timeout" if "timeout" in str(exc) else "protocol_error"
            record["error"] = str(exc)
            raise ProtocolError(str(exc)) from exc
        finally:
            record["ended"] = time.monotonic()
            record["elapsed_ms"] = (record["ended"] - started) * 1000
            write_json(self.directory / "calls.json", self.calls)

    def initialize(self):
        init = self.rpc(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "interaction-bench", "version": "1"},
            },
        )
        if "error" in init:
            raise ProtocolError(f"initialize failed: {init['error']}")
        self.notify("notifications/initialized", {})
        inventory, cursor, seen = [], None, set()
        while True:
            response = self.rpc("tools/list", {"cursor": cursor} if cursor else {})
            result = response.get("result", {})
            if not isinstance(result.get("tools"), list):
                raise ProtocolError("tools/list did not return an inventory")
            inventory.extend(result["tools"])
            cursor = result.get("nextCursor")
            if cursor is None:
                break
            if not isinstance(cursor, str) or cursor in seen or len(seen) >= 100:
                raise ProtocolError("invalid or repeated tools/list cursor")
            seen.add(cursor)
        names = [
            tool.get("name") if isinstance(tool, dict) else None for tool in inventory
        ]
        if (
            any(not isinstance(name, str) or not name for name in names)
            or len(names) != len(set(names))
            or any(not isinstance(tool.get("inputSchema"), dict) for tool in inventory)
        ):
            raise ProtocolError("invalid or duplicate tool definition")
        write_json(self.directory / "inventory.json", inventory)
        return init["result"], inventory

    def close(self, grace=2):
        process = self.process
        # A blocked writer can hold stdin's lock; killing first releases it.
        if not self.poisoned:
            process.stdin.close()
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)
        clean = True
        if os.name == "posix":
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                pass
            else:
                os.killpg(process.pid, signal.SIGKILL)
                clean = False
        if self.job:
            residue = self.job.close()
            self.cleanup = {"method": "windows_job", "forced_owned_pids": residue}
            clean = clean and not residue
        for thread in self.readers:
            thread.join(timeout=1)
        self.stopping.set()
        for stream in (process.stdin, process.stdout, process.stderr):
            stream.close()
        return (
            clean
            and not self.fault
            and not any(thread.is_alive() for thread in self.readers)
        )
