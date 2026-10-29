#!/usr/bin/env bash
# Fails when a tracked public file carries an internal label or a private
# reference: decision numbers (D-12), plan task numbers (T9, T13a), open
# item numbers (U-01), the old parameter-table names (G0-P, g0p), review
# tags (T16 A-I1) and private paths (inacircle-teszt, notes/). A comment must
# state the reason a value is what it is, not point at a private note.
#
#   scripts/check-public-hygiene.sh            check the repository
#   scripts/check-public-hygiene.sh --self-test   prove the patterns bite
#
# Allowlist: ALLOW lines of `path:fixed substring` below, each with its
# reason. Keep it short.
set -euo pipefail
cd "$(dirname "$0")/.."

# Decision and open-item numbers of any length, plan task numbers, old
# parameter-table names, private paths, absolute home paths, and standalone
# review tags (A-I1, B-M3).
PATTERN='\bD-[0-9]+\b|\bU-[0-9]+\b|G0-P|g0p|inacircle-teszt|\bT[0-9]+[ab]?\b|notes/(plans|specs)|parameters\.json|/home/[A-Za-z_][A-Za-z0-9_-]*/|\b[A-F]-[IMCHL][0-9]+\b'
# Every tracked file except fuzz corpora (binary seeds) and lockfiles.
PATHS=(. ':!fuzz/corpus' ':!*Cargo.lock')

# path:substring on the matching line, with the reason.
ALLOW=(
  # A trace-tag prefix in a test fixture, not a plan label.
  'bins/inacircle/tests/wheel.rs:starts_with("T1")'
  # `/home/u/` is a made-up user in URI and path test fixtures.
  'bins/inacircle/src/notice.rs:/home/u/'
  'bins/inacircle/src/uri.rs:/home/u/'
  # T0 is the start time of a timer in two probe scripts, not a plan task.
  'spikes/nautilus/inacircle_probe.py:T0'
  'spikes/observe/observe.py:T0'
  # This script and its self-test carry the patterns by definition.
  'scripts/check-public-hygiene.sh:'
  'scripts/tests/test_public_hygiene.py:'
  # The contributor rules name the private notes folder on purpose.
  'CLAUDE.md:notes/'
)

scan() { # stdin: grep -n lines "path:line:text"
  local line path text a sub
  while IFS= read -r line; do
    path=${line%%:*}
    text=${line#*:}
    for a in "${ALLOW[@]}"; do
      [ "$path" = "${a%%:*}" ] || continue
      sub=${a#*:}
      [ -z "$sub" ] && continue 2
      case $text in *"$sub"*) continue 2 ;; esac
    done
    printf '%s\n' "$line"
  done
}

if [ "${1:-}" = --self-test ]; then
  for sample in "x D-12 y" "D-123" "see T13a" "T123" "U-01 open" "U-123" "G0-P rows" \
    "~/inacircle-teszt/x" "T16 A-I1" "A-I1" "B-M3" "/home/someone/x"; do
    printf '%s\n' "$sample" | grep -qE "$PATTERN" || { echo "pattern misses: $sample" >&2; exit 1; }
  done
  for sample in "UTF-8" "D-Bus" "AT-SPI" "x86-64" "/home/" "~/.config" "A-Z" "SHA-256"; do
    ! printf '%s\n' "$sample" | grep -qE "$PATTERN" || { echo "pattern is too wide: $sample" >&2; exit 1; }
  done
  echo "self-test ok"
  exit 0
fi

hits=$(git grep -nIE "$PATTERN" -- "${PATHS[@]}" | scan || true)
if [ -n "$hits" ]; then
  echo "internal labels or private references in tracked public files:" >&2
  printf '%s\n' "$hits" >&2
  exit 1
fi
echo "public hygiene ok"
