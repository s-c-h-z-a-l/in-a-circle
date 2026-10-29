#!/usr/bin/env python3
"""A/A and A/B-B/A startup series inside a running niri session.

Run it ON the machine whose niri it measures, only with the owner's explicit
go-ahead: it spawns the wheel through `niri msg action spawn` and sends
Escape with `wtype`, which is input injected into that session. It is never
run on a desktop someone is using.

  measure-ab.py --out DIR --a-bin DIR --label-a A [--b-bin DIR --label-b B]
                [--plan aa,ab,ab] [--valid 50] [--cap-factor 2]

`--plan` is a comma list run in order: `aa` = two independent series of the
A binary (series A1, A2, 10 s apart), `ab` = one alternating A/B-B/A run
(pair 1 is A,B; pair 2 is B,A; ...) until both builds have `--valid` valid
observations or `valid * cap-factor` attempts each were made. Every attempt
stays in `attempts.jsonl` (measure-wheel.py schema plus `build`); nothing
is replaced silently. Series and runs are never pooled: each plan step has
its own series name (aa1, aa2, ab1, ab2, ...).

Method (docs/benchmarks/wheel-reference.md, "Reproduction"): read the whole
engine file and check page residency with mincore before every attempt,
spawn, wait 1 s, check the process exists, `wtype -k Escape -s 50`, allow 5 s
to exit, pause 0.35 s. State, cache and config are private per build; the
action binary of the same build sits in that build's actions dir.
"""
import argparse
import ctypes
import ctypes.util
import glob
import importlib.util
import json
import mmap
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone

# Reference-method waits (each has a reason in wheel-reference.md):
SETTLE_S = 1.0  # the wheel commits after one compositor round trip (ms)
EXIT_DEADLINE_S = 5.0  # far above a normal close; protects the session
PAUSE_S = 0.35
SERIES_PAUSE_S = 10.0
POLL_S = 0.05
MAP_DEADLINE_S = 2.0  # mapping takes one compositor round trip; 2 s is 2x the settle wait
CMD_TIMEOUT_S = 5  # every helper command (niri msg, wtype, pgrep)
METRICS = ("main_to_commit_cpu_us", "main_to_commit_wall_us", "main_to_flush_cpu_us",
           "main_to_flush_wall_us", "wheel_cpu_us", "pss_bytes", "idle_wakeups")

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("measure_wheel", HERE / "measure-wheel.py")
mw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mw)

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)


def residency(path):
    """(resident pages, pages) of the file after reading it whole; None on error."""
    with open(path, "rb") as f:
        f.read()
        size = os.fstat(f.fileno()).st_size
        pages = (size + mmap.PAGESIZE - 1) // mmap.PAGESIZE
        vec = (ctypes.c_ubyte * pages)()
        libc.mmap.restype = ctypes.c_void_p
        libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int,
                              ctypes.c_int, ctypes.c_int, ctypes.c_long]
        addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, f.fileno(), 0)
        if addr in (None, ctypes.c_void_p(-1).value):
            return None, pages
        rc = libc.mincore(ctypes.c_void_p(addr), ctypes.c_size_t(size), vec)
        libc.munmap(ctypes.c_void_p(addr), ctypes.c_size_t(size))
        return (sum(v & 1 for v in vec) if rc == 0 else None), pages


def pids(exe):
    """Processes running exactly `exe`: the command line starts with the
    escaped path (a path is not a regex), and the process's own executable
    link says the same, so nothing else of the user's is ever matched, let
    alone killed."""
    r = subprocess.run(["pgrep", "-f", f"^{re.escape(exe)}( |$)"], capture_output=True,
                       text=True, timeout=CMD_TIMEOUT_S)
    found = []
    for p in r.stdout.split():
        try:
            if os.readlink(f"/proc/{p}/exe") == exe:
                found.append(int(p))
        except OSError:
            pass  # gone already
    return found


class Build:
    def __init__(self, label, bindir, out):
        self.label = label
        self.exe = str(Path(bindir).resolve() / "inacircle")
        self.root = Path(out).resolve() / f"state-{label}"
        self.log = Path(out).resolve() / f"wheel-{label}.log"
        for sub in ("actions", "cache", "state", "config", "home"):
            (self.root / sub).mkdir(parents=True, exist_ok=True)
        text = Path(bindir) / "inacircle-text"
        if text.exists():
            shutil.copy2(text, self.root / "actions" / "inacircle-text")
        self.log.touch()

    def launch(self):
        r = self.root
        return (f"INACIRCLE_MEASURE=1 INACIRCLE_ACTIONS_DIR={r}/actions XDG_CACHE_HOME={r}/cache "
                f"XDG_STATE_HOME={r}/state XDG_CONFIG_HOME={r}/config HOME={r}/home "
                f"exec {self.exe} 2>>{self.log}")


def wheel_mapped(exe):
    """True once the wheel process is alive and its layer surface is mapped.

    Proof: `niri msg --json layers` lists a layer surface in the "inacircle"
    namespace (the wheel's own, see surface.rs). Polled up to MAP_DEADLINE_S;
    any error or absence counts as not proven.
    """
    deadline = time.monotonic() + MAP_DEADLINE_S
    while True:
        try:
            if pids(exe):
                r = subprocess.run(["niri", "msg", "--json", "layers"], capture_output=True,
                                   text=True, timeout=CMD_TIMEOUT_S)
                if r.returncode == 0 and '"inacircle"' in r.stdout:
                    return True
        except (subprocess.TimeoutExpired, OSError):
            pass
        if time.monotonic() >= deadline:
            return False
        time.sleep(POLL_S)


def attempt(build, series, index):
    row = {"build": build.label, "series": series, "attempt": index,
           "utc": datetime.now(timezone.utc).isoformat(timespec="milliseconds")}
    row["resident_pages"], row["executable_pages"] = residency(build.exe)
    offset = build.log.stat().st_size
    failure = None
    try:
        sp = subprocess.run(["niri", "msg", "action", "spawn", "--", "sh", "-c", build.launch()],
                            capture_output=True, text=True, timeout=CMD_TIMEOUT_S)
        row["spawn_rc"] = sp.returncode
        if sp.returncode != 0:
            failure = f"niri spawn rc={sp.returncode}: {sp.stderr.strip()[:200]}"
        time.sleep(SETTLE_S)
        row["engine_seen"] = bool(pids(build.exe))
        if not row["engine_seen"] and failure is None:
            failure = "no wheel process after the settle time"
        # Input goes into the focused window of the session: send Escape only
        # with proof that OUR wheel is alive and has a mapped layer surface.
        # Otherwise the attempt is invalid and the launched process is killed
        # below (the exit wait finds it alive and SIGKILLs it).
        row["mapped"] = row["engine_seen"] and wheel_mapped(build.exe)
        # The wheel may have closed between the proof and the keystroke: the
        # Escape goes only if it is still ours and alive right now.
        if row["mapped"] and pids(build.exe):
            subprocess.run(["wtype", "-k", "Escape", "-s", "50"], timeout=CMD_TIMEOUT_S)
        elif failure is None:
            failure = "wheel not verified alive and mapped; no input sent"
    except (subprocess.TimeoutExpired, OSError) as error:
        failure = failure or f"harness command failed: {error}"
        row.setdefault("engine_seen", False)
    deadline = time.monotonic() + EXIT_DEADLINE_S
    while pids(build.exe) and time.monotonic() < deadline:
        time.sleep(POLL_S)
    left = pids(build.exe)
    row["exited"] = not left
    for pid in left:
        os.kill(pid, signal.SIGKILL)  # never hold the session
    if left:
        failure = failure or f"wheel alive {EXIT_DEADLINE_S}s after Escape (killed)"
        time.sleep(0.5)
    with open(build.log) as f:
        f.seek(offset)
        text = f.read()
    row["log"] = text
    marks = [l for l in text.splitlines() if "measure wheel " in l]
    row["marker"] = marks[-1] if marks else None
    row["failure"] = failure
    return row


def all_metrics_valid(row):
    """An attempt counts only when every gated metric is valid."""
    return all(x["status"] == "valid" for x in to_ledger(row, CONDITION))


def to_ledger(row, condition):
    note = (f"attempt {row['attempt']} {row['utc']} resident "
            f"{row['resident_pages']}/{row['executable_pages']}")
    return mw.marker_attempts(row["marker"], condition=condition, series=row["series"],
                              build=row["build"], required=METRICS, note=note,
                              failure=row["failure"])


def environment(args, builds):
    def sh(cmd):
        try:
            return subprocess.run(cmd, shell=True, capture_output=True, text=True,
                                  timeout=CMD_TIMEOUT_S).stdout.strip()
        except subprocess.TimeoutExpired:
            return "timeout"

    cpu = "/sys/devices/system/cpu/cpu0/cpufreq/"
    return {
        "utc_start": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "host": platform.node(), "kernel": platform.release(),
        "cpu": sh("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2").strip(),
        "cpus": os.cpu_count(),
        "governor": sh(f"cat {cpu}scaling_governor"),
        "epp": sh(f"cat {cpu}energy_performance_preference"),
        "freq_min_max_khz": sh(f"cat {cpu}scaling_min_freq {cpu}scaling_max_freq").split(),
        "intel_pstate": sh("cat /sys/devices/system/cpu/intel_pstate/status"),
        "niri": sh("niri --version"), "wtype": sh("wtype --version 2>&1 | head -1"),
        "python": platform.python_version(),
        "mitigated_vulnerabilities": sh("grep -l Mitigation /sys/devices/system/cpu/vulnerabilities/* | wc -l"),
        "outputs": sh("niri msg outputs | head -12"),
        "loadavg": sh("cat /proc/loadavg"),
        "mem": sh("grep -E 'MemTotal|MemAvailable' /proc/meminfo"),
        "builds": {b.label: {"exe": b.exe, "sha256": sh(f"sha256sum {b.exe} | cut -d' ' -f1")}
                   for b in builds},
        "plan": args.plan, "valid_target": args.valid,
        "roles": {"a": args.label_a, "b": args.label_b if args.b_bin else None},
        "launcher": "niri msg action spawn -- sh -c <INACIRCLE_MEASURE=1 ... exec exe 2>>log>",
        "condition": CONDITION,
        "waits_s": {"settle": SETTLE_S, "exit_deadline": EXIT_DEADLINE_S, "pause": PAUSE_S,
                    "series_pause": SERIES_PAUSE_S},
    }


CONDITION = "startup-plain-warm-niri-spawn"

# Consecutive attempts without a full set of valid metrics after which the
# run stops. Each attempt holds a full-screen, keyboard-grabbing overlay for
# up to MAP_DEADLINE_S + EXIT_DEADLINE_S on the user's own session, so a
# setup that does not work (the layer listing differs, the binary fails to
# start) must not go on for the ~600 attempts of a full plan: three in a
# row is a pattern, not noise. The raw data already written stays.
MAX_CONSECUTIVE_FAILURES = 3


class Abort(Exception):
    pass


def refuse_installed(bindirs):
    """A measured build directory must not be the one the user's own
    `inacircle` runs from: the run launches and, if one hangs, kills the
    binary there."""
    installed = shutil.which("inacircle")
    if not installed:
        return
    home = Path(installed).resolve().parent
    for d in bindirs:
        if d and Path(d).resolve() == home:
            sys.exit(f"{d} holds the installed inacircle ({installed}); measure copies")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", required=True)
    ap.add_argument("--a-bin", required=True)
    ap.add_argument("--label-a", default="A")
    ap.add_argument("--b-bin")
    ap.add_argument("--label-b", default="B")
    ap.add_argument("--plan", default="aa,aa,ab,ab")
    ap.add_argument("--valid", type=int, default=50)
    ap.add_argument("--cap-factor", type=int, default=2)
    args = ap.parse_args()
    steps = args.plan.split(",")
    if "ab" in steps and not args.b_bin:
        sys.exit("--plan ab needs --b-bin")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    refuse_installed([args.a_bin, args.b_bin])
    a = Build(args.label_a, args.a_bin, out)
    b = Build(args.label_b, args.b_bin, out) if args.b_bin else None
    builds = [x for x in (a, b) if x]
    for x in builds:
        if pids(x.exe):
            sys.exit(f"{x.exe} is already running; refusing to start")
    (out / "environment.json").write_text(json.dumps(environment(args, builds), indent=1))
    rows, counters = [], {}
    streak = [0]
    ledger = open(out / "attempts.jsonl", "a")

    def run(build, series, pair=None):
        counters[(series, build.label)] = counters.get((series, build.label), 0) + 1
        row = attempt(build, series, counters[(series, build.label)])
        row["pair"] = pair
        rows.append(row)
        for rec in to_ledger(row, CONDITION):
            ledger.write(json.dumps(rec) + "\n")
        ledger.flush()
        ok = row["failure"] is None and row["marker"] is not None
        streak[0] = 0 if all_metrics_valid(row) else streak[0] + 1
        print(f"{series} {build.label} #{row['attempt']} {'ok' if ok else 'FAIL ' + str(row['failure'])} "
              f"{(row['marker'] or '')[:0]}", flush=True)
        time.sleep(PAUSE_S)
        if streak[0] >= MAX_CONSECUTIVE_FAILURES:
            raise Abort(f"{streak[0]} attempts in a row without valid metrics; stopping "
                        f"(last: {row['failure'] or 'incomplete marker'})")
        return ok

    def valid_count(series, label):
        return sum(1 for r in rows if r["series"] == series and r["build"] == label
                   and all_metrics_valid(r))

    seen = {}
    aborted = 0
    try:
        for step in steps:
            seen[step] = seen.get(step, 0) + 1
            series = f"{step}{seen[step]}"
            if step == "aa":
                # Two independent series of the same binary are two plan steps
                # ("aa,aa"); each is its own series.
                cap = args.valid * args.cap_factor
                while valid_count(series, a.label) < args.valid and \
                        counters.get((series, a.label), 0) < cap:
                    run(a, series)
            elif step == "ab":
                cap = args.valid * args.cap_factor
                pair = 0
                while (valid_count(series, a.label) < args.valid
                       or valid_count(series, b.label) < args.valid) and pair < cap:
                    pair += 1
                    order = (a, b) if pair % 2 == 1 else (b, a)  # AB, BA, AB, ...
                    for build in order:
                        run(build, series, pair)
            else:
                sys.exit(f"unknown plan step {step!r}")
            time.sleep(SERIES_PAUSE_S)
    except Abort as why:
        print("measure-ab: aborted:", why, file=sys.stderr)
        aborted = 1
    finally:
        for x in builds:
            for pid in pids(x.exe):
                os.kill(pid, signal.SIGKILL)
        ledger.close()
        (out / "runs.json").write_text(json.dumps(rows, indent=1))
    if aborted:
        sys.exit(1)
    print("done:", out)


if __name__ == "__main__":
    main()
