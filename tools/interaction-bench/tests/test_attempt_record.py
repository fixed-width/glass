import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from attempt_record import AttemptRecord
from evidence import EvidenceError
from measurement import verify_files
from protocol import ProtocolError
from validation import validate_totals


class AttemptRecordTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name) / "attempt"
        clock_patch = patch("attempt_record.time.monotonic", return_value=100)
        self.clock = clock_patch.start()
        self.addCleanup(clock_patch.stop)
        self.record = AttemptRecord(
            {"attempt_timeout_ms": 60000, "evidence_limit_bytes": 4096},
            {"case": "disabled", "driver": "glass", "iteration": 0, "warmup": False},
            self.directory,
            artifact_references=[],
        )

    def test_initial_record_is_incomplete_and_existing_attempt_is_not_overwritten(self):
        saved = (self.directory / "result.json").read_bytes()
        result = json.loads(saved)
        self.assertEqual(result["outcome"], "harness_error")
        self.assertEqual(result["error"], "attempt incomplete")
        self.assertFalse(result["cleanup_ok"])
        self.assertFalse(result["evidence_ok"])
        self.assertEqual(self.record.deadline, 160)
        with self.assertRaises(FileExistsError):
            AttemptRecord({}, {}, self.directory)
        self.assertEqual((self.directory / "result.json").read_bytes(), saved)

    def test_failure_classification_preserves_interruption_and_timeout_semantics(self):
        for exc, outcome in [
            (ProtocolError("wire"), "harness_error"),
            (OSError("storage"), "harness_error"),
            (TimeoutError("deadline"), "harness_error"),
            (EvidenceError("missing body"), "failed"),
            (ValueError("assertion"), "failed"),
            (KeyboardInterrupt(), "failed"),
            (SystemExit(2), "failed"),
        ]:
            with self.subTest(exception=type(exc).__name__):
                self.record.fail(exc)
                self.assertEqual(self.record.result["outcome"], outcome)
                self.assertEqual(
                    self.record.result["error"], f"{type(exc).__name__}: {exc}"
                )
                self.assertEqual(
                    self.record.result["interrupted"],
                    isinstance(exc, KeyboardInterrupt),
                )

    def test_phase_clock_excludes_gaps_and_overwrites_repeated_evidence(self):
        client = SimpleNamespace(phase="server_start")
        self.clock.return_value = 102
        self.record.advance(None)
        self.clock.return_value = 107
        with self.assertRaises(EvidenceError):
            with self.record.phase("evidence", (client,)):
                self.assertEqual(client.phase, "evidence")
                self.clock.return_value = 110
                raise EvidenceError("first collection failed")
        self.clock.return_value = 120
        with self.record.phase("evidence", (client,)):
            self.clock.return_value = 121
        self.assertEqual(
            self.record.durations, {"server_start": 2000, "evidence": 1000}
        )

    def test_continuous_phases_share_one_clock_and_sum_all_channels_and_reads(self):
        clients = [SimpleNamespace(phase="server_start") for _ in range(2)]
        self.clock.return_value = 103
        self.record.advance("task", clients)
        self.assertEqual([client.phase for client in clients], ["task", "task"])
        calls = [{"phase": "task", "rpc_calls": 1, "tool_calls": 1} for _ in clients]
        self.record.result["file_reads"] = [{"phase": "evidence", "file_reads": 1}]
        self.clock.return_value = 108
        self.record.advance("cleanup", clients)
        self.clock.return_value = 109
        self.record.advance(None)
        self.record.record_calls(calls)
        self.record.capture_wall()
        self.clock.return_value = 200
        self.record.finalize()
        result = self.record.result
        self.assertEqual(result["wall_ms"], 9000)
        self.assertEqual(result["phases"]["task"]["elapsed_ms"], 5000)
        self.assertEqual(result["phases"]["task"]["tool_calls"], 2)
        self.assertEqual(result["phases"]["evidence"]["file_reads"], 1)
        self.assertEqual(validate_totals(self.directory, result, calls), [])

    def test_early_failure_keeps_startup_time_and_zero_unentered_phases(self):
        self.clock.return_value = 103
        self.record.fail(ProtocolError("initialize"))
        self.record.advance("cleanup")
        self.clock.return_value = 104
        self.record.advance(None)
        self.record.record_calls([])
        self.record.capture_wall()
        self.record.finalize()
        phases = self.record.result["phases"]
        self.assertEqual(phases["server_start"]["elapsed_ms"], 3000)
        self.assertEqual(phases["cleanup"]["elapsed_ms"], 1000)
        self.assertEqual(phases["task"]["elapsed_ms"], 0)

    def test_finalization_inventories_timing_and_rejects_aggregate_overflow(self):
        body = self.directory / "evidence.bin"
        body.write_bytes(b"x" * 4096)
        self.record.result.update(
            outcome="task_completed", evidence_ok=True, cleanup_ok=True
        )
        self.record.capture_wall()
        self.record.record_calls([])
        result = self.record.finalize()
        self.assertEqual(set(result["files"]), {"evidence.bin", "timing.json"})
        self.assertEqual(verify_files(self.directory, result["files"]), [])
        self.assertFalse(result["evidence_ok"])
        self.assertTrue(result["cleanup_ok"])
        self.assertEqual(result["outcome"], "task_completed")
        self.assertEqual(
            result["error"], "attempt evidence exceeds configured storage limit"
        )
        self.assertEqual(
            json.loads((self.directory / "result.json").read_bytes()), result
        )


if __name__ == "__main__":
    unittest.main()
