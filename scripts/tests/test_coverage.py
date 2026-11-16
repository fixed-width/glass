"""Exercise coverage failure diagnostics with controlled tools and harnesses."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parents[1]
STUB = r'''#!/usr/bin/env python3
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
if name == "cargo":
    if args[:2] == ["llvm-cov", "report"]:
        print("coverage report generated")
        sys.exit(0)
    if args[:2] != ["nextest", "run"]:
        sys.exit(0)
    if "--workspace" in args:
        suite = "unit"
    else:
        package = args[args.index("-p") + 1]
        suite = package.removeprefix("glass-") + "-unit"
elif name == "sway":
    print("sway version 1.12")
    sys.exit(0)
elif name == "Xvfb":
    sys.exit(0)
else:
    suite = Path(name).stem.removeprefix("test-")

if suite != os.environ.get("COVERAGE_TEST_FAIL_SUITE"):
    print("captured success output: " + suite)
    sys.exit(0)

code = int(os.environ["COVERAGE_TEST_EXIT_CODE"])
if code == 4:
    print("error: no tests to run", file=sys.stderr)
else:
    print("FAIL " + suite + "::fake_adb", flush=True)
    print("assertion failed: expected 100% of fixture output", file=sys.stderr, flush=True)
    for i in range(80):
        print("later test output " + str(i))
sys.exit(code)
'''


class CoverageDiagnosticsTests(unittest.TestCase):
    def run_wrapper(self, *, failing_suite="", exit_code=100):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "scripts/lib").mkdir(parents=True)
            (root / "bin").mkdir()
            shutil.copyfile(SCRIPTS / "coverage.sh", root / "scripts/coverage.sh")
            shutil.copyfile(SCRIPTS / "lib/have-sway.sh", root / "scripts/lib/have-sway.sh")
            for relative in (
                "bin/cargo", "bin/Xvfb", "bin/sway",
                "scripts/test-x11.sh", "scripts/test-wayland.sh", "scripts/test-a11y.sh",
            ):
                tool = root / relative
                tool.write_text(STUB)
                tool.chmod(0o755)
            env = dict(os.environ)
            env.update(
                PATH=str(root / "bin") + os.pathsep + env["PATH"],
                COVERAGE_TEST_FAIL_SUITE=failing_suite,
                COVERAGE_TEST_EXIT_CODE=str(exit_code),
            )
            return subprocess.run(
                ["bash", str(root / "scripts/coverage.sh")],
                cwd=root, env=env, text=True, capture_output=True, check=False,
            )

    def test_nextest_failure_preserves_test_names_and_assertions(self):
        for suite in ("unit", "x11-unit", "wayland-unit"):
            with self.subTest(suite=suite):
                result = self.run_wrapper(failing_suite=suite)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, int(suite == "unit"), output)
                self.assertIn("FAIL " + suite + "::fake_adb", output)
                self.assertIn("assertion failed: expected 100% of fixture output", output)
                self.assertIn("later test output 79", output)
                self.assertIn("coverage report generated", output)
                self.assertIn(
                    "coverage: suites that failed "
                    "(a failing suite still contributes its coverage): " + suite,
                    output,
                )

    def test_harness_failure_preserves_test_names_and_assertions(self):
        for suite in ("x11", "wayland", "a11y"):
            with self.subTest(suite=suite):
                result = self.run_wrapper(failing_suite=suite)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, 0, output)
                self.assertIn("FAIL " + suite + "::fake_adb", output)
                self.assertIn("assertion failed: expected 100% of fixture output", output)
                self.assertIn("later test output 79", output)
                self.assertIn("coverage report generated", output)
                self.assertIn(
                    "coverage: suites that failed "
                    "(a failing suite still contributes its coverage): " + suite,
                    output,
                )

    def test_successful_suites_keep_their_output_quiet(self):
        result = self.run_wrapper()
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output)
        self.assertNotIn("captured success output:", output)
        self.assertIn(
            "coverage: suites run:     unit x11-unit x11 wayland-unit wayland a11y",
            output,
        )
        self.assertIn("coverage report generated", output)

    def test_no_tests_remains_a_unit_failure(self):
        result = self.run_wrapper(failing_suite="unit", exit_code=4)
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1, output)
        self.assertIn("error: no tests to run", output)
        self.assertIn("unit (ran no tests)", output)
        self.assertIn("coverage report generated", output)


if __name__ == "__main__":
    unittest.main()
