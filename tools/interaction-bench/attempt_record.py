"""Attempt timing and persisted results, independent of resource ownership."""

from contextlib import contextmanager
import time

from measurement import file_identity, phase_totals, write_json
from protocol import ProtocolError


class AttemptRecord:
    def __init__(self, config, cell, directory, **fields):
        directory.mkdir()
        self.started = time.monotonic()
        self.deadline = self.started + config["attempt_timeout_ms"] / 1000
        self.directory = directory
        self.evidence_limit = config["evidence_limit_bytes"]
        self.durations = {}
        self.current_phase, self.phase_start = "server_start", self.started
        self.result = {
            "schema_version": 1,
            **cell,
            "outcome": "harness_error",
            "error": "attempt incomplete",
            "cleanup_ok": False,
            "evidence_ok": False,
            "events": [],
            "files": {},
            "phases": {},
            "artifacts": {},
            "wall_ms": 0,
            **fields,
        }
        write_json(directory / "result.json", self.result)

    def advance(self, name, clients=()):
        now = time.monotonic()
        if self.current_phase is not None:
            self.durations[self.current_phase] = (now - self.phase_start) * 1000
        self.current_phase, self.phase_start = name, now
        for client in clients:
            client.phase = name

    @contextmanager
    def phase(self, name, clients=()):
        self.advance(name, clients)
        try:
            yield
        finally:
            self.advance(None)

    def fail(self, exc):
        self.result.update(
            outcome=(
                "harness_error"
                if isinstance(exc, (ProtocolError, OSError))
                else "failed"
            ),
            error=f"{type(exc).__name__}: {exc}",
            interrupted=isinstance(exc, KeyboardInterrupt),
        )

    def capture_wall(self):
        # Callers retain their measured boundary relative to participant hashing.
        self.result["wall_ms"] = (time.monotonic() - self.started) * 1000

    def record_calls(self, calls):
        self.result["phases"] = phase_totals(
            calls + self.result.get("file_reads", []), self.durations
        )

    def finalize(self):
        result = self.result
        result.setdefault("interrupted", False)
        write_json(
            self.directory / "timing.json",
            {"wall_ms": result["wall_ms"], "durations": self.durations},
        )
        for path in self.directory.rglob("*"):
            if path.is_file() and path.name != "result.json":
                result["files"][path.relative_to(self.directory).as_posix()] = (
                    file_identity(path)
                )
        if sum(v["bytes"] for v in result["files"].values()) > self.evidence_limit:
            result.update(
                evidence_ok=False,
                error="attempt evidence exceeds configured storage limit",
            )
        write_json(self.directory / "result.json", result)
        return result
