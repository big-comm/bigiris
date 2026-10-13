# SPDX-License-Identifier: GPL-3.0-or-later
"""Verify container restrictions never hide native or unexpected test failures."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "package-check.sh"


class PackageCheckTests(unittest.TestCase):
    def exercise(self, container=False, error=None, cargo_status=0):
        with tempfile.TemporaryDirectory(prefix="bigiris-package-check-") as name:
            root = Path(name)
            (root / "target" / "release").mkdir(parents=True)
            (root / "bin").mkdir()
            executable = root / "target" / "release" / "bigiris"
            executable.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ -n ${PROBE_ERROR:-} ]]; then\n"
                "  printf '%s\\n' \"$PROBE_ERROR\" >&2\n"
                "  exit 1\n"
                "fi\n"
                "printf '%s\\n' 'self-test OK'\n"
            )
            executable.chmod(0o755)
            cargo = root / "bin" / "cargo"
            cargo.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' \"$@\" > \"$CARGO_TRACE\"\n"
                "exit \"$CARGO_STATUS\"\n"
            )
            cargo.chmod(0o755)
            trace = root / "cargo-arguments"
            environment = dict(os.environ)
            environment.update(
                PATH=str(root / "bin") + os.pathsep + environment["PATH"],
                CARGO_TRACE=str(trace),
                CARGO_STATUS=str(cargo_status),
                PROBE_ERROR=error or "",
            )
            # Override only the detection function in the sourced test fixture.
            # The standalone production script always defines its own detection.
            shell = (
                'source "$1"; '
                f'in_container() {{ return {0 if container else 1}; }}; '
                'run_package_checks "$2"'
            )
            result = subprocess.run(
                ["bash", "-c", shell, "fixture", str(SCRIPT), str(root / "target")],
                env=environment, capture_output=True, text=True, check=False,
            )
            arguments = trace.read_text().splitlines() if trace.exists() else []
            return result, arguments

    def test_native_runs_all_tests(self):
        result, arguments = self.exercise()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--all-targets", arguments)
        self.assertNotIn("--skip", arguments)

    def test_supported_container_runs_all_tests(self):
        result, arguments = self.exercise(container=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("--skip", arguments)

    def test_recorded_loopback_restriction_filters_only_three_tests(self):
        result, arguments = self.exercise(
            container=True,
            error="bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("WARNING", result.stderr)
        self.assertEqual(arguments.count("--skip"), 3)
        self.assertEqual(arguments[arguments.index("--") + 1:], [
            "--skip", "self_test_and_help_exit_successfully",
            "--skip", "conversion_transform_and_animation_commands_produce_readable_outputs",
            "--skip", "batch_profile_converts_folder_and_skip_is_idempotent",
        ])

    def test_namespace_restriction_is_recognized_in_container(self):
        result, arguments = self.exercise(
            container=True,
            error="bwrap: Creating new namespace failed: Operation not permitted",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(arguments.count("--skip"), 3)

    def test_same_restriction_on_native_host_fails(self):
        result, arguments = self.exercise(
            error="bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(arguments, [])

    def test_unexpected_container_error_fails(self):
        result, arguments = self.exercise(container=True, error="invalid sandbox attestation")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(arguments, [])

    def test_cargo_failure_is_not_masked(self):
        result, _ = self.exercise(cargo_status=23)
        self.assertEqual(result.returncode, 23)


if __name__ == "__main__":
    unittest.main()
