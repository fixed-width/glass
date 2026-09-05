"""Exercise the mutation wrapper with a controlled cargo-mutants executable."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


WRAPPER = Path(__file__).resolve().parents[1] / "mutants.sh"
TEST_FILE = "crates/glass-core/src/action/tests.rs"
STUB = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
with Path("calls.jsonl").open("a") as calls:
    calls.write(json.dumps(args) + "\n")
files = [args[i + 1] for i, arg in enumerate(args) if arg == "--file"]
owner = os.environ["MUTANTS_TEST_OWNER"]
if "--list" in args:
    if "--in-diff" not in args and owner in files and Path(owner).is_file():
        print(owner + ":1: replace action -> bool with false")
    sys.exit(0)

assert owner in files, "the run must mutate the production module"
assert "--in-diff" not in args, "the test-only diff must not hide the mutation"
assert "--test-tool" in args and "nextest" in args
assert "--shard" in args and "3/8" in args
missed = int(os.environ.get("MUTANTS_TEST_MISSED", "0"))
out = Path(args[args.index("--output") + 1]) / "mutants.out"
out.mkdir(parents=True)
(out / "outcomes.json").write_text(json.dumps({
    "total_mutants": 1, "caught": 1 - missed, "missed": missed,
    "timeout": 0, "unviable": 0,
}))
# Prove the wrapper grades the report even when the command returns success.
'''


class MutationScopeTests(unittest.TestCase):
    def run_wrapper(self, owner, *, create_owner=True, missed=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            test_file = root / TEST_FILE
            test_file.parent.mkdir(parents=True)
            test_file.write_text("#[test]\nfn checks_action() {}\n")
            if create_owner:
                (root / owner).write_text("pub fn action() -> bool { true }\n")
            diff = root / "pr.diff"
            diff.write_text(
                f"diff --git a/{TEST_FILE} b/{TEST_FILE}\n"
                f"--- a/{TEST_FILE}\n+++ b/{TEST_FILE}\n"
                "@@ -1 +1 @@\n-old test\n+new test\n"
            )
            binary_dir = root / "bin"
            binary_dir.mkdir()
            cargo = binary_dir / "cargo"
            cargo.write_text(STUB)
            cargo.chmod(0o755)
            env = dict(os.environ)
            env.update(
                PATH=str(binary_dir) + os.pathsep + env["PATH"],
                MUTANTS_TEST_OWNER=owner,
                MUTANTS_TEST_MISSED=str(int(missed)),
            )
            result = subprocess.run(
                [
                    "bash", str(WRAPPER), str(root / "out"),
                    "--package", "glass-core", "--test-tool", "nextest",
                    "--shard", "3/8", "--in-diff", str(diff),
                ],
                cwd=root, env=env, text=True, capture_output=True, check=False,
            )
            calls = [
                json.loads(line)
                for line in (root / "calls.jsonl").read_text().splitlines()
            ]
            return result, calls

    def test_separate_tests_gate_the_sibling_module(self):
        result, calls = self.run_wrapper("crates/glass-core/src/action.rs")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(any("--output" in call for call in calls))

    def test_separate_tests_gate_a_mod_rs_module(self):
        result, calls = self.run_wrapper("crates/glass-core/src/action/mod.rs")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(any("--output" in call for call in calls))

    def test_a_survivor_in_the_owner_fails_the_gate(self):
        result, _ = self.run_wrapper("crates/glass-core/src/action.rs", missed=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("1 missed", result.stdout)

    def test_an_unresolved_owner_cannot_pass_an_empty_gate(self):
        result, calls = self.run_wrapper(
            "crates/glass-core/src/action.rs", create_owner=False
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("This run would gate nothing", result.stdout)
        self.assertFalse(any("--output" in call for call in calls))


if __name__ == "__main__":
    unittest.main()
