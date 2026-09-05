from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from measurement import canonical, digest, schedule, statistics, summarize, verify_files


class MeasurementTests(unittest.TestCase):
    def test_unicode_and_key_order_are_canonical(self):
        self.assertEqual(canonical({"z": "雪", "a": 1}), b'{"a":1,"z":"\xe9\x9b\xaa"}')
        self.assertEqual(digest({"a": 1, "z": 2}), digest({"z": 2, "a": 1}))

    def test_schedule_alternates_and_retains_warmups(self):
        rows = schedule(["form", "delay"], ["a", "b"], 3, 1, 41)
        self.assertEqual(rows, schedule(["form", "delay"], ["a", "b"], 3, 1, 41))
        self.assertEqual(len(rows), 16)
        self.assertEqual(sum(r["warmup"] for r in rows), 4)
        for case in ("form", "delay"):
            pairs = [
                [r["driver"] for r in rows if r["case"] == case and r["iteration"] == i]
                for i in range(4)
            ]
            self.assertEqual(pairs[0], pairs[2])
            self.assertEqual(pairs[1], pairs[0][::-1])

    def test_small_sample_has_no_p95(self):
        self.assertEqual(
            statistics([2, 8]), {"n": 2, "min": 2, "median": 5.0, "max": 8, "p95": None}
        )
        self.assertEqual(statistics(list(range(1, 21)))["p95"], 19)
        self.assertEqual(statistics([])["median"], None)

    def test_warmups_failures_and_refusals_have_separate_denominators(self):
        rows = []
        for outcome, warmup, elapsed in [
            ("task_completed", True, 999),
            ("task_completed", False, 2),
            ("failed", False, 180),
            ("expected_refusal", False, 1),
            ("skipped", False, 0),
        ]:
            rows.append(
                {
                    "case": "form",
                    "driver": "a",
                    "warmup": warmup,
                    "outcome": outcome,
                    "wall_ms": elapsed,
                    "cleanup_ok": True,
                    "evidence_ok": True,
                    "phases": {"task": {"elapsed_ms": elapsed}},
                }
            )
        report = summarize(rows)[0]
        self.assertEqual(report["measured"], 4)
        self.assertEqual(report["counts"]["failed"], 1)
        self.assertEqual(report["successful_task_ms"]["median"], 2)
        self.assertEqual(report["failed_wall_ms"]["median"], 180)

    def test_hash_validation_rejects_mutation_and_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "call.json").write_bytes(b"{}")
            files = {"call.json": {"sha256": digest(b"{}"), "bytes": 2}}
            self.assertEqual(verify_files(root, files), [])
            (root / "call.json").write_bytes(b"[]")
            self.assertTrue(verify_files(root, files))
            self.assertTrue(
                verify_files(root, {"../escape": {"sha256": "", "bytes": 0}})
            )


if __name__ == "__main__":
    unittest.main()
