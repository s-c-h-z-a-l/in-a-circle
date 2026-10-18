#!/usr/bin/env python3
"""Summarize bench.sh output: median per (backend, size, mode) across runs."""
import re
import statistics as st
import sys
from collections import defaultdict


def kv(line):
    return dict(re.findall(r"(\w+)=(\S+)", line))


def num(s):
    try:
        return float(s.rstrip("MiB"))
    except (ValueError, AttributeError):
        return None


runs = defaultdict(list)  # key -> list of per-run dicts
cur = None
for path in sys.argv[1:]:
    for line in open(path):
        line = line.rstrip("\n")
        if line.startswith("=== "):
            parts = line.split()
            backend = parts[2]
            size = "full" if "--full" in parts else "box"
            mode = "lazy" if "--lazy" in parts else "eager"
            cur = {"key": (backend, size, mode), "warm": [], "cpu": []}
            runs[cur["key"]].append(cur)
            continue
        if cur is None or not line.startswith("M "):
            continue
        d = kv(line)
        tag = line.split()[1].split("=")[0]
        if tag.startswith("exec_to_first_frame_us"):
            cur["cold_ms"] = float(d["exec_to_first_frame_us"]) / 1000
        elif tag == "init":
            cur["init_ms"] = float(d["init_ms"])
        elif tag == "cycle":
            if d["i"] != "0":
                cur["warm"].append(float(d["first_frame_ms"]))
            cur["cpu"].append(float(d["anim_cpu_us_per_frame"]))
        elif tag == "vram":
            cur["vram"] = num(d.get("vram", ""))
        elif tag == "idle":
            cur["idle_cs"] = float(d["ctx_switches"]) / float(d["secs"])
            cur["idle_cpu"] = float(d["cpu_us"]) / float(d["secs"])
        elif tag == "idle_end":
            cur["rss"] = float(d["rss_kb"]) / 1024
            cur["pss"] = float(d["pss_kb"]) / 1024
            cur["anon"] = float(d["anon_kb"]) / 1024
            cur["threads"] = float(d["threads"])
            cur["ok"] = True
        elif tag == "shown":
            cur["rss_shown"] = max(cur.get("rss_shown", 0), float(d["rss_kb"]) / 1024)


def med(vals):
    vals = [v for v in vals if v is not None]
    return st.median(vals) if vals else float("nan")


cols = [
    ("idle RSS MB", "rss", "{:.1f}"),
    ("idle PSS MB", "pss", "{:.1f}"),
    ("idle anon MB", "anon", "{:.1f}"),
    ("peak RSS shown", "rss_shown", "{:.1f}"),
    ("VRAM MiB", "vram", "{:.0f}"),
    ("threads", "threads", "{:.0f}"),
    ("idle wakeups/s", "idle_cs", "{:.1f}"),
    ("idle CPU us/s", "idle_cpu", "{:.0f}"),
    ("cold exec->frame ms", "cold_ms", "{:.1f}"),
    ("warm show->frame ms", "warm", "{:.2f}"),
    ("CPU us/frame", "cpu", "{:.0f}"),
]
print("| backend | size | mode | n | " + " | ".join(c[0] for c in cols) + " |")
print("|" + "---|" * (4 + len(cols)))
for key in sorted(runs):
    rs = [r for r in runs[key] if r.get("ok")]
    if not rs:
        continue
    cells = []
    for _, f, fmt in cols:
        if f in ("warm", "cpu"):
            v = med([med(r[f]) for r in rs])
        else:
            v = med([r.get(f) for r in rs])
        cells.append(fmt.format(v))
    print(f"| {key[0]} | {key[1]} | {key[2]} | {len(rs)} | " + " | ".join(cells) + " |")
