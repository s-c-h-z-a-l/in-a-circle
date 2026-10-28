#!/usr/bin/env bash
# T13b: the A/A and A/B-B/A startup series on the user's own niri desktop.
#
#   scripts/measure-desktop-t13b.sh --i-am-present <A-bin-dir> <B-bin-dir> <out-dir>
#
# INJECTS INPUT (Escape via wtype) and opens the wheel ~600 times in the
# running session: run it only when the owner sits at the machine, has
# agreed, and leaves keyboard and mouse alone for the ~35 minutes it takes.
# It never starts on its own; the flag is the explicit go-ahead.
# Plan: two independent A/A series, then two A/B-B/A runs, each until both
# builds have 50 valid observations (cap 100 attempts per build and run).
# A-bin-dir/B-bin-dir hold `inacircle` (and `inacircle-text`) of the A build
# (~/inacircle-teszt/A-169163b) and the candidate release. Raw results:
# <out-dir>/{attempts.jsonl,runs.json,environment.json,wheel-*.log}; then
# summarize per series with scripts/measure-wheel.py (--build, one series
# at a time) and compare A against B.
set -euo pipefail
[ "${1:-}" = --i-am-present ] || { sed -n '2,13p' "$0" >&2; exit 2; }
A=${2:?A-bin-dir}; B=${3:?B-bin-dir}; OUT=${4:?out-dir}
for d in "$A" "$B"; do [ -x "$d/inacircle" ] || { echo "no $d/inacircle" >&2; exit 1; }; done
[ -z "$(ls -A "$OUT" 2>/dev/null)" ] || { echo "$OUT is not empty: raw data is never overwritten" >&2; exit 1; }
timeout 5 niri msg version >/dev/null || { echo "no niri session" >&2; exit 1; }
timeout 5 wtype --version >/dev/null 2>&1 || command -v wtype >/dev/null || { echo "no wtype" >&2; exit 1; }
exec python3 "$(dirname "$0")/measure-ab.py" --out "$OUT" --a-bin "$A" --label-a A --b-bin "$B" \
  --label-b B --plan aa,aa,ab,ab --valid 50
