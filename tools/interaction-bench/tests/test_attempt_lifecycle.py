import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import application_run
import attempt_record
import run
from evidence import EvidenceError
from measurement import file_identity
from protocol import ProtocolError


class AttemptLifecycleTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name) / "attempt"
        self.now = 100
        self.closed = []
        self.config = {
            "attempt_timeout_ms": 60000,
            "evidence_limit_bytes": 100000,
            "frame_limit_bytes": 100000,
            "viewport": [1000, 700],
            "backend": "x11",
            "drivers": [{"id": "glass", "adapter": "glass", "command": ["unused"]}],
        }
        self.cell = {
            "driver": "glass",
            "case": "disabled",
            "iteration": 0,
            "warmup": False,
        }
        self.replace(attempt_record.time, "monotonic", side_effect=lambda: self.now)
        self.replace(attempt_record, "file_identity", side_effect=self.hash_file)

    def replace(self, owner, name, **kwargs):
        replacement = patch.object(owner, name, **kwargs)
        self.addCleanup(replacement.stop)
        return replacement.start()

    def tick(self, seconds):
        self.now += seconds

    def hash_file(self, path):
        self.tick(10)
        return file_identity(path)

    def prepare_display(self, session, config, directory):
        self.tick(2)
        (directory / "note.txt").write_text("retained evidence")

    def session(self, name):
        session = Mock(env={}, root=self.directory, token=name)
        session.process_commands.return_value = []

        def close():
            self.tick(1)
            self.closed.append(f"{name}:session")
            return {"ok": True}

        session.close.side_effect = close
        return session

    def client(self, name):
        client = Mock(
            calls=[], phase="server_start", poisoned=False, fault=None, cleanup={}
        )

        def initialize():
            self.tick(1)
            client.calls.append({"phase": client.phase, "rpc_calls": 1})
            required = run.GlassDriver.required_tools | {
                "glass_list_windows",
                "glass_select_window",
                "glass_key",
            }
            return {}, [{"name": name} for name in sorted(required)]

        def close(grace):
            self.assertEqual(grace, 1)
            self.tick(1)
            self.closed.append(f"{name}:client")
            return True

        client.initialize.side_effect = initialize
        client.close.side_effect = close
        return client

    def configure_web(self):
        self.web_client = self.client("web")
        self.web_session = self.session("web")
        self.web_driver = Mock(events=[], file_reads=[])
        self.web_driver.launch.side_effect = lambda url: self.tick(4)
        self.web_driver.collect.side_effect = lambda: self.tick(5)

        def stop():
            self.tick(1)
            self.closed.append("web:stop")

        self.web_driver.stop.side_effect = stop

        def construct(config, session, client, evidence, deadline):
            self.assertEqual(deadline, 150)
            self.tick(2)  # Web driver construction is outside phase durations.
            return self.web_driver

        self.adapter = Mock(side_effect=construct, required_tools=set())
        self.adapter.command.return_value = ["unused"]
        self.replace(run, "create_session", return_value=self.web_session)
        self.replace(run, "prepare_display", side_effect=self.prepare_display)
        self.launch_client = self.replace(run, "Client", return_value=self.web_client)
        self.replace(run, "evaluate_setup", return_value=[])
        self.replace(run, "evaluate", return_value=[])
        self.execute = self.replace(
            run, "execute", side_effect=lambda *args: self.tick(3)
        )

    def web_attempt(self):
        fixtures = Mock()
        fixtures.url.return_value = "http://fixture.invalid/disabled"
        result = run.attempt(
            self.config, self.cell, self.directory, fixtures, self.adapter
        )
        self.assertEqual(
            json.loads((self.directory / "result.json").read_bytes()), result
        )
        return result

    def test_web_preserves_phase_gaps_cleanup_allowances_and_wall_boundary(self):
        self.configure_web()
        result = self.web_attempt()
        self.assertEqual(self.launch_client.call_args.kwargs["deadline"], 150)
        self.assertEqual(self.web_client.deadline, 122)
        self.assertEqual(self.closed, ["web:stop", "web:client", "web:session"])
        self.assertEqual(result["wall_ms"], 20000)
        self.assertEqual(
            {name: phase["elapsed_ms"] for name, phase in result["phases"].items()},
            {
                "server_start": 3000,
                "app_start": 4000,
                "task": 3000,
                "evidence": 5000,
                "cleanup": 3000,
            },
        )
        self.assertTrue(result["cleanup_ok"] and result["evidence_ok"])
        self.assertNotIn("participants", result)
        self.assertGreater(
            self.now, 120
        )  # Final file hashing is excluded from wall time.

    def test_web_partial_startup_failure_still_closes_client_and_session(self):
        self.configure_web()
        self.web_client.initialize.side_effect = ProtocolError("initialize")
        result = self.web_attempt()
        self.assertEqual(result["outcome"], "harness_error")
        self.assertEqual(self.closed, ["web:client", "web:session"])
        self.assertEqual(result["phases"]["server_start"]["elapsed_ms"], 2000)
        self.assertEqual(result["phases"]["task"]["elapsed_ms"], 0)
        self.assertFalse(result["evidence_ok"])

    def test_web_interruption_recovers_evidence_and_preserves_cleanup_failure(self):
        self.configure_web()
        self.execute.side_effect = KeyboardInterrupt()
        self.web_client.poisoned = True
        self.web_session.close.return_value = {"ok": False}
        self.web_session.close.side_effect = None
        result = self.web_attempt()
        self.assertEqual(result["outcome"], "failed")
        self.assertTrue(result["interrupted"] and result["evidence_ok"])
        self.assertFalse(result["cleanup_ok"])
        self.web_driver.collect.assert_called_once()
        self.web_driver.stop.assert_not_called()
        self.web_client.close.assert_called_once()
        self.web_session.close.assert_called_once()
        self.assertEqual(
            result["cleanup_errors"],
            ["owned session processes required forced cleanup"],
        )

    def test_web_repeated_collection_retains_only_last_evidence_duration(self):
        self.configure_web()

        def collect():
            self.tick(5 if self.web_driver.collect.call_count == 1 else 2)
            if self.web_driver.collect.call_count == 1:
                raise EvidenceError("archive")

        self.web_driver.collect.side_effect = collect
        result = self.web_attempt()
        self.assertEqual(self.web_driver.collect.call_count, 2)
        self.assertEqual(result["phases"]["evidence"]["elapsed_ms"], 2000)
        self.assertEqual(result["outcome"], "failed")
        self.assertTrue(result["evidence_ok"])

    def configure_applications(self):
        self.cell["case"] = "cross-application"
        self.app_clients = [self.client(name) for name in ("source", "destination")]
        self.app_sessions = [self.session(name) for name in ("source", "destination")]
        self.replace(application_run, "create_session", side_effect=self.app_sessions)
        self.replace(
            application_run, "prepare_display", side_effect=self.prepare_display
        )
        self.launch_clients = self.replace(
            application_run, "Client", side_effect=self.app_clients
        )
        self.replace(application_run, "file_identity", side_effect=self.hash_file)
        self.replace(
            application_run.UI, "launch_application", side_effect=lambda: self.tick(4)
        )

        def stop(member):
            self.tick(1)
            self.closed.append(f"{member.name}:stop")

        self.replace(application_run.UI, "stop", autospec=True, side_effect=stop)
        self.replace(application_run.cases, "evaluate_setup", return_value=[])
        self.replace(application_run.cases, "evaluate", return_value=[])

        def execute(case, fleet):
            self.tick(3)
            for member in fleet.members.values():
                self.assertEqual(member.deadline, 130)
                member.client.calls.append(
                    {"phase": member.client.phase, "rpc_calls": 1, "tool_calls": 1}
                )

        self.replace(application_run.cases, "execute", side_effect=execute)

    def test_application_wall_includes_participant_hashing_and_sums_both_channels(self):
        self.configure_applications()
        result = application_run.attempt(self.config, self.cell, self.directory)
        self.assertEqual(
            [call.kwargs["deadline"] for call in self.launch_clients.call_args_list],
            [130, 130],
        )
        self.assertEqual(
            self.closed,
            [
                "destination:stop",
                "destination:client",
                "destination:session",
                "source:stop",
                "source:client",
                "source:session",
            ],
        )
        self.assertEqual([client.deadline for client in self.app_clients], [128, 125])
        self.assertEqual(result["wall_ms"], 43000)
        self.assertEqual(result["phases"]["task"]["tool_calls"], 2)
        self.assertEqual(result["phases"]["task"]["elapsed_ms"], 3000)
        self.assertEqual(set(result["participants"]), {"source", "destination"})
        self.assertEqual(
            json.loads((self.directory / "result.json").read_bytes()), result
        )

    def test_application_partial_startup_cleans_all_owned_participants_in_reverse(self):
        self.configure_applications()
        self.app_clients[1].initialize.side_effect = ProtocolError(
            "destination initialize"
        )
        result = application_run.attempt(self.config, self.cell, self.directory)
        self.assertEqual(result["outcome"], "harness_error")
        self.assertTrue(result["cleanup_ok"])
        self.assertFalse(result["evidence_ok"])
        self.assertEqual(
            self.closed,
            [
                "destination:client",
                "destination:session",
                "source:stop",
                "source:client",
                "source:session",
            ],
        )
        self.assertEqual(result["phases"]["task"]["elapsed_ms"], 0)
        self.assertEqual(result["participants"]["destination"]["events"], [])
        self.assertEqual(
            json.loads((self.directory / "result.json").read_bytes()), result
        )


if __name__ == "__main__":
    unittest.main()
