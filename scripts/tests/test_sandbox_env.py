"""Capability qualification must never turn an unavailable sandbox into PASS."""

import contextlib
import ctypes
import errno
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import Mock, patch
import subprocess


SCRIPT = Path(__file__).resolve().parents[1] / "check-sandbox-env.py"


class QualificationTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.is_file(), "sandbox capability probe implementation is missing")
        spec = importlib.util.spec_from_file_location("sandbox_env", SCRIPT)
        self.probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.probe)

    def qualify(self, *, abi=6, arch="x86_64", seccomp_ok=True, landlock_error=None,
                seccomp_changes=None):
        observation = {
            "system": "Linux",
            "architecture": arch,
            "kernel": "test-kernel",
            "landlock": {"abi": abi, "error": landlock_error},
            "seccomp": {
                "ok": seccomp_ok,
                "no_new_privs": seccomp_ok,
                "filter_installed": seccomp_ok,
                "denied_syscall": seccomp_ok,
                "positive_control": seccomp_ok,
                "error": None if seccomp_ok else "seccomp installation failed: EPERM",
            },
        }
        observation["seccomp"].update(seccomp_changes or {})
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "qualification.json"
            with contextlib.redirect_stdout(io.StringIO()):
                status = self.probe.main(
                    ["--min-landlock-abi", "6", "--output", str(output)],
                    probe=lambda: observation,
                )
            return status, json.loads(output.read_text())

    def test_abi_five_fails_with_observed_and_required_abi(self):
        status, report = self.qualify(abi=5)
        self.assertNotEqual(status, 0)
        self.assertFalse(report["qualified"])
        self.assertIn("Landlock ABI 5 is below required ABI 6", report["failures"])

    def test_missing_abi_fails_and_preserves_kernel_error(self):
        error = {"errno": 38, "name": "ENOSYS", "message": "Function not implemented"}
        status, report = self.qualify(abi=None, landlock_error=error)
        self.assertNotEqual(status, 0)
        self.assertEqual(report["landlock"]["error"], error)
        self.assertTrue(any("ENOSYS" in failure for failure in report["failures"]))

    def test_seccomp_child_failure_is_not_a_pass(self):
        status, report = self.qualify(seccomp_ok=False)
        self.assertNotEqual(status, 0)
        self.assertFalse(report["qualified"])
        self.assertTrue(any("EPERM" in failure for failure in report["failures"]))

    def test_unknown_architecture_is_not_a_pass(self):
        status, report = self.qualify(arch="riscv64")
        self.assertNotEqual(status, 0)
        self.assertIn("Unsupported architecture: riscv64", report["failures"])

    def test_abi_six_and_successful_child_qualify_both_supported_architectures(self):
        for arch in ("x86_64", "aarch64"):
            with self.subTest(arch=arch):
                status, report = self.qualify(arch=arch)
                self.assertEqual(status, 0)
                self.assertTrue(report["qualified"])
                self.assertEqual(report["failures"], [])

    def test_claimed_success_without_each_verified_control_is_rejected(self):
        for control in ("no_new_privs", "filter_installed", "denied_syscall", "positive_control"):
            with self.subTest(control=control):
                status, report = self.qualify(seccomp_changes={control: False})
                self.assertNotEqual(status, 0)
                self.assertFalse(report["qualified"])

    def test_landlock_errors_keep_the_actual_errno(self):
        class FailingKernel:
            def __init__(self, error_number):
                self.error_number = error_number

            def syscall(self, *args):
                ctypes.set_errno(self.error_number)
                return -1

        for number, name in ((38, "ENOSYS"), (95, "ENOTSUP"), (1, "EPERM")):
            with self.subTest(number=number):
                observed = self.probe.probe_landlock(FailingKernel(number))
                self.assertIsNone(observed["abi"])
                self.assertEqual(observed["error"]["errno"], number)
                # errno aliases can use the Linux EOPNOTSUPP spelling.
                self.assertIn(observed["error"]["name"], (name, "EOPNOTSUPP") if number == 95 else (name,))

    def test_abnormal_child_exit_cannot_be_qualified_by_its_stdout(self):
        child = subprocess.CompletedProcess([], 7, stdout='{"ok": true}', stderr="child failed")
        with patch.object(self.probe.subprocess, "run", return_value=child):
            observed = self.probe.probe_seccomp()
        self.assertFalse(observed["ok"])
        self.assertIn("7", observed["error"])

    def test_failed_child_preserves_the_actual_installation_error(self):
        child = subprocess.CompletedProcess(
            [], 1, stdout=json.dumps({"ok": False, "error": "PR_SET_SECCOMP: EACCES (13)"}),
            stderr="",
        )
        with patch.object(self.probe.subprocess, "run", return_value=child):
            observed = self.probe.probe_seccomp()
        self.assertFalse(observed["ok"])
        self.assertIn("PR_SET_SECCOMP: EACCES (13)", observed["error"])

    def run_seccomp_child(self, overrides):
        responses = {
            "PR_SET_NO_NEW_PRIVS": (0, errno.EINVAL),
            "PR_GET_NO_NEW_PRIVS": (1, errno.EINVAL),
            "PR_SET_SECCOMP": (0, errno.EINVAL),
            "PR_GET_SECCOMP": (2, errno.EINVAL),
            "getppid": (-1, errno.EPERM),
            "getpid": (4242, errno.EINVAL),
        }
        responses.update(overrides)

        def respond(operation):
            value, error_number = responses[operation]
            ctypes.set_errno(error_number)
            return value

        libc = SimpleNamespace(
            prctl=Mock(side_effect=lambda option, *args: respond({
                38: "PR_SET_NO_NEW_PRIVS", 39: "PR_GET_NO_NEW_PRIVS",
                22: "PR_SET_SECCOMP", 21: "PR_GET_SECCOMP",
            }[option])),
            syscall=Mock(side_effect=lambda number: respond({110: "getppid", 39: "getpid"}[number])),
        )
        with patch.object(self.probe.ctypes, "CDLL", return_value=libc), \
                patch.object(self.probe.platform, "system", return_value="Linux"), \
                patch.object(self.probe.platform, "machine", return_value="x86_64"), \
                patch.object(self.probe.os, "getpid", return_value=4242):
            return self.probe.seccomp_child()

    def test_failed_postinstall_controls_preserve_immediate_results_and_errno(self):
        cases = (
            ("no_new_privs", "PR_GET_NO_NEW_PRIVS", 13, "EACCES"),
            ("filter_installed", "PR_GET_SECCOMP", 1, "EPERM"),
            ("denied_syscall", "getppid", 13, "EACCES"),
            ("positive_control", "getpid", 38, "ENOSYS"),
        )
        for control, operation, number, name in cases:
            with self.subTest(control=control):
                observed = self.run_seccomp_child({operation: (-1, number)})
                self.assertFalse(observed["ok"])
                self.assertFalse(observed[control])
                self.assertIn(f"{name} ({number})", observed["error"])
                self.assertIn(operation, observed["error"])
                self.assertEqual(observed["checks"][control]["return_value"], -1)
                self.assertEqual(observed["checks"][control]["errno"], number)

    def test_unexpected_success_values_are_reported_without_stale_errno(self):
        cases = (
            ("no_new_privs", "PR_GET_NO_NEW_PRIVS", 0),
            ("filter_installed", "PR_GET_SECCOMP", 1),
            ("denied_syscall", "getppid", 1234),
            ("positive_control", "getpid", 4243),
        )
        for control, operation, returned in cases:
            with self.subTest(control=control):
                observed = self.run_seccomp_child({operation: (returned, errno.EACCES)})
                self.assertFalse(observed["ok"])
                self.assertIn(f"{operation} returned {returned}", observed["error"])
                self.assertEqual(observed["checks"][control]["return_value"], returned)
                self.assertEqual(observed["checks"][control]["errno"], 0)

    def test_failed_child_forwards_postinstall_observations_to_parent_report(self):
        checks = {"denied_syscall": {"operation": "getppid", "return_value": -1, "errno": 13}}
        child = subprocess.CompletedProcess(
            [], 1, stdout=json.dumps({"ok": False, "checks": checks,
                                      "error": "getppid returned -1: EACCES (13)"}), stderr="",
        )
        with patch.object(self.probe.subprocess, "run", return_value=child):
            observed = self.probe.probe_seccomp()
        self.assertFalse(observed["ok"])
        self.assertIn("checks", observed)
        self.assertEqual(observed["checks"], checks)
        self.assertIn("EACCES (13)", observed["error"])

    def test_malformed_child_result_is_a_failure(self):
        for body in ("not json", "[]", '{"ok": true}'):
            child = subprocess.CompletedProcess([], 0, stdout=body, stderr="")
            with self.subTest(body=body), patch.object(self.probe.subprocess, "run", return_value=child):
                observed = self.probe.probe_seccomp()
                self.assertFalse(observed["ok"])

    def test_child_timeout_is_a_failure(self):
        with patch.object(self.probe.subprocess, "run", side_effect=subprocess.TimeoutExpired([], 5)):
            observed = self.probe.probe_seccomp()
        self.assertFalse(observed["ok"])
        self.assertIn("timed out", observed["error"])

    def test_real_child_does_not_change_parent_security_state(self):
        import platform

        if platform.system() != "Linux" or platform.machine() not in ("x86_64", "aarch64"):
            self.skipTest("real child isolation requires a supported Linux architecture")
        libc = ctypes.CDLL(None, use_errno=True)
        before = (libc.prctl(39, 0, 0, 0, 0), libc.prctl(21, 0, 0, 0, 0))
        observed = self.probe.probe_seccomp()
        after = (libc.prctl(39, 0, 0, 0, 0), libc.prctl(21, 0, 0, 0, 0))
        self.assertEqual(after, before)
        self.assertIs(type(observed["ok"]), bool)
        if observed["ok"]:
            for control in ("no_new_privs", "filter_installed", "denied_syscall", "positive_control"):
                self.assertIs(observed[control], True)
        else:
            self.assertTrue(observed["error"])


if __name__ == "__main__":
    unittest.main()
