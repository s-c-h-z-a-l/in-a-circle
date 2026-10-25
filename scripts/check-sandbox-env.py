#!/usr/bin/env python3
"""Qualify native Linux sandbox capabilities without restricting the caller.

This installation smoke test is not a validation of the product sandbox policy.
Only the disposable child installs no_new_privs and a seccomp filter.
"""

import argparse
import ctypes
import errno
import json
import os
from pathlib import Path
import platform
import subprocess
import sys


# Linux UAPI: asm/unistd*.h, linux/audit.h, linux/filter.h, linux/seccomp.h.
ARCHITECTURES = {
    "x86_64": {"audit_arch": 0xC000003E, "getpid": 39, "getppid": 110},
    "aarch64": {"audit_arch": 0xC00000B7, "getpid": 172, "getppid": 173},
}
CONTROLS = ("no_new_privs", "filter_installed", "denied_syscall", "positive_control")


def kernel_error(number):
    return {
        "errno": number,
        "name": errno.errorcode.get(number, "UNKNOWN"),
        "message": os.strerror(number),
    }


def probe_landlock(libc):
    # LANDLOCK_CREATE_RULESET_VERSION queries the ABI without changing policy.
    ctypes.set_errno(0)
    abi = libc.syscall(444, None, 0, 1)
    if abi > 0:
        return {"abi": int(abi), "error": None}
    return {"abi": None, "error": kernel_error(ctypes.get_errno())}


def seccomp_child():
    result = {"ok": False, **{control: False for control in CONTROLS}, "error": None}
    try:
        architecture = ARCHITECTURES.get(platform.machine())
        if platform.system() != "Linux" or architecture is None:
            raise RuntimeError("Unsupported Linux architecture for seccomp installation")
        libc = ctypes.CDLL(None, use_errno=True)
        libc.syscall.restype = ctypes.c_long

        class SockFilter(ctypes.Structure):
            _fields_ = [("code", ctypes.c_ushort), ("jt", ctypes.c_ubyte),
                        ("jf", ctypes.c_ubyte), ("k", ctypes.c_uint32)]

        class SockFprog(ctypes.Structure):
            _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.POINTER(SockFilter))]

        # Reject an unexpected audit architecture, then deny getppid with EPERM.
        # getpid is a positive control. The smoke filter allows other calls so
        # Python can report the result and terminate normally.
        instructions = (SockFilter * 7)(
            SockFilter(0x20, 0, 0, 4),
            SockFilter(0x15, 1, 0, architecture["audit_arch"]),
            SockFilter(0x06, 0, 0, 0x80000000),
            SockFilter(0x20, 0, 0, 0),
            SockFilter(0x15, 0, 1, architecture["getppid"]),
            SockFilter(0x06, 0, 0, 0x00050000 | errno.EPERM),
            SockFilter(0x06, 0, 0, 0x7FFF0000),
        )
        program = SockFprog(len(instructions), instructions)

        def prctl(option, argument, operation):
            ctypes.set_errno(0)
            if libc.prctl(option, argument, 0, 0, 0) == -1:
                error = kernel_error(ctypes.get_errno())
                raise RuntimeError(f"{operation}: {error['name']} ({error['errno']}): {error['message']}")

        prctl(38, 1, "PR_SET_NO_NEW_PRIVS")
        result["no_new_privs"] = libc.prctl(39, 0, 0, 0, 0) == 1
        ctypes.set_errno(0)
        if libc.prctl(22, 2, ctypes.byref(program), 0, 0) == -1:
            error = kernel_error(ctypes.get_errno())
            raise RuntimeError(f"PR_SET_SECCOMP: {error['name']} ({error['errno']}): {error['message']}")
        result["filter_installed"] = libc.prctl(21, 0, 0, 0, 0) == 2
        ctypes.set_errno(0)
        denied = libc.syscall(architecture["getppid"])
        deny_errno = ctypes.get_errno()
        result["denied_syscall"] = denied == -1 and deny_errno == errno.EPERM
        result["positive_control"] = libc.syscall(architecture["getpid"]) == os.getpid()
        result["ok"] = all(result[control] for control in CONTROLS)
        if not result["ok"]:
            result["error"] = "seccomp verification failed: " + ", ".join(
                control for control in CONTROLS if not result[control]
            )
    except (OSError, RuntimeError) as error:
        result["error"] = str(error)
    return result


def probe_seccomp():
    failed = {"ok": False, **{control: False for control in CONTROLS}}
    try:
        child = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--_seccomp-child"],
            capture_output=True, text=True, timeout=5, check=False,
        )
    except subprocess.TimeoutExpired:
        return {**failed, "error": "seccomp child timed out after 5 seconds"}
    except OSError as error:
        return {**failed, "error": f"seccomp child could not start: {error}"}
    if child.returncode != 0:
        reason = child.stderr.strip()
        try:
            result = json.loads(child.stdout)
            if isinstance(result, dict) and isinstance(result.get("error"), str):
                reason = result["error"]
        except (ValueError, TypeError):
            pass
        return {**failed, "error": f"seccomp child exited {child.returncode}: {reason}"}
    try:
        result = json.loads(child.stdout)
        if not isinstance(result, dict) or any(
            type(result.get(field)) is not bool for field in ("ok", *CONTROLS)
        ):
            raise ValueError("missing boolean verification fields")
        if result["ok"] and not all(result[control] for control in CONTROLS):
            raise ValueError("inconsistent verification fields")
        if not result["ok"] and not isinstance(result.get("error"), str):
            raise ValueError("missing failure reason")
    except (ValueError, TypeError) as error:
        return {**failed, "error": f"invalid seccomp child result: {error}"}
    return result


def collect_probe():
    observation = {
        "system": platform.system(),
        "architecture": platform.machine(),
        "kernel": platform.release(),
        "landlock": {"abi": None, "error": None},
        "seccomp": {"ok": False, "error": "not probed on unsupported platform"},
    }
    if observation["system"] == "Linux" and observation["architecture"] in ARCHITECTURES:
        libc = ctypes.CDLL(None, use_errno=True)
        libc.syscall.restype = ctypes.c_long
        observation["landlock"] = probe_landlock(libc)
        observation["seccomp"] = probe_seccomp()
    return observation


def main(argv=None, *, probe=collect_probe):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--min-landlock-abi", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.min_landlock_abi < 1:
        parser.error("--min-landlock-abi must be positive")
    report = probe()
    failures = []
    if report["system"] != "Linux":
        failures.append(f"Unsupported operating system: {report['system']}")
    if report["architecture"] not in ARCHITECTURES:
        failures.append(f"Unsupported architecture: {report['architecture']}")
    abi = report["landlock"]["abi"]
    if type(abi) is not int or abi < 1:
        error = report["landlock"].get("error")
        reason = (f"{error['name']} ({error['errno']}): {error['message']}"
                  if error else "ABI query did not return an ABI")
        failures.append(f"Landlock unavailable: {reason}")
    elif abi < args.min_landlock_abi:
        failures.append(f"Landlock ABI {abi} is below required ABI {args.min_landlock_abi}")
    seccomp = report["seccomp"]
    if seccomp.get("ok") is not True or any(seccomp.get(control) is not True for control in CONTROLS):
        failures.append(f"Seccomp qualification failed: {seccomp.get('error') or 'incomplete child verification'}")
    report.update(schema_version=1, minimum_landlock_abi=args.min_landlock_abi,
                  qualified=not failures, failures=failures)
    serialized = json.dumps(report, indent=2, sort_keys=True) + "\n"
    try:
        args.output.write_text(serialized, encoding="utf-8")
    except OSError as error:
        print(f"Cannot write qualification report: {error}", file=sys.stderr)
        return 1
    print(serialized, end="")
    return 0 if report["qualified"] else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--_seccomp-child"]:
        child_result = seccomp_child()
        print(json.dumps(child_result))
        sys.exit(0 if child_result["ok"] else 1)
    sys.exit(main())
