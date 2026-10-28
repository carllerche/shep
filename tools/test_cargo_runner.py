"""Integration checks for Cargo -> nextest without compiling a Rust fixture.

Run with: python3 -m unittest discover -s tools -p test_cargo_runner.py
Requires cargo-nextest and the repository's Rust toolchain.
"""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "tools/cargo-runner.sh"

# Speak libtest's discovery protocol, then record what nextest actually runs.
# Names deliberately differ from the real io_buffer tests: rebuilding that
# target or running the entire package would fail these assertions.
FIXTURE = """#!/usr/bin/env python3
import json, os, pathlib, sys
names = ['alpha', 'beta', 'ignored']
if '--list' in sys.argv:
    listed = ['ignored'] if '--ignored' in sys.argv else names
    for name in listed:
        print(name + ': test')
else:
    assert os.environ['NEXTEST_EXECUTION_MODE'] == 'process-per-test'
    name = sys.argv[sys.argv.index('--exact') + 1]
    record = {'name': name, 'pid': os.getpid()}
    path = pathlib.Path(os.environ['SHEP_RUNNER_TEST_LOG']) / name
    path.write_text(json.dumps(record))
    sys.exit(1 if name == os.environ.get('SHEP_RUNNER_TEST_FAIL') else 0)
"""


class CargoRunnerTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="shep-runner-test-")
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.binary = self.directory / "io_buffer-0000000000000000"
        self.binary.write_text(FIXTURE)
        self.binary.chmod(0o755)
        self.log = self.directory / "log"
        self.log.mkdir()
        self.env = os.environ.copy()
        self.env.pop("NEXTEST_EXECUTION_MODE", None)
        self.env.pop("NEXTEST_PROFILE", None)
        self.env.update({
            "CARGO_PKG_NAME": "shep-smp2",
            "CARGO_MANIFEST_PATH": str(ROOT / "crates/shep-smp2/Cargo.toml"),
            "SHEP_RUNNER_TEST_LOG": str(self.log),
        })

    def run_tests(self, *arguments):
        result = subprocess.run(
            [str(RUNNER), str(self.binary), *arguments],
            cwd=ROOT, env=self.env, text=True, capture_output=True, timeout=30,
        )
        return result, [json.loads(path.read_text()) for path in self.log.iterdir()]

    def assert_passed(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_each_test_runs_in_a_separate_process(self):
        result, records = self.run_tests()
        self.assert_passed(result)
        self.assertEqual({r["name"] for r in records}, {"alpha", "beta"})
        self.assertEqual(len({r["pid"] for r in records}), 2)

    def test_exact_filter(self):
        result, records = self.run_tests("beta", "--exact")
        self.assert_passed(result)
        self.assertEqual([r["name"] for r in records], ["beta"])

    def test_no_matches_is_successful(self):
        result, records = self.run_tests("missing")
        self.assert_passed(result)
        self.assertEqual(records, [])

    def test_ignored_selection(self):
        result, records = self.run_tests("--ignored")
        self.assert_passed(result)
        self.assertEqual([r["name"] for r in records], ["ignored"])

    def test_skip_quiet_threads_and_color(self):
        result, records = self.run_tests(
            "--skip", "alpha", "--quiet", "--test-threads=1", "--color", "never",
        )
        self.assert_passed(result)
        self.assertEqual([r["name"] for r in records], ["beta"])

    def test_failure_status_is_forwarded(self):
        self.env["SHEP_RUNNER_TEST_FAIL"] = "beta"
        result, records = self.run_tests("beta", "--exact")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([r["name"] for r in records], ["beta"])

    def test_discovery_does_not_launch_tests(self):
        result, records = self.run_tests("--list")
        self.assert_passed(result)
        self.assertIn("alpha: test", result.stdout)
        self.assertEqual(records, [])
