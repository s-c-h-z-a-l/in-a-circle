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

PATTERN='\bD-[0-9]{2}\b|\bU-0[0-9]\b|G0-P|g0p|inacircle-teszt|\bT1[0-9][ab]?\b|\bT[0-9][ab]?\b|\bT16\b|notes/(plans|specs)|parameters\.json'
PATHS=(docs CLAUDE.md README.md bins crates scripts fuzz .github)

# path:substring on the matching line, with the reason.
ALLOW=(
  # A trace-tag prefix in a test fixture, not a plan label.
  'bins/inacircle/tests/wheel.rs:starts_with("T1")'
  # This script and its self-test carry the patterns by definition.
  'scripts/check-public-hygiene.sh:'
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
  for sample in "x D-12 y" "see T13a" "U-01 open" "G0-P rows" "~/inacircle-teszt/x" "T16 A-I1"; do
    printf '%s\n' "$sample" | grep -qE "$PATTERN" || { echo "pattern misses: $sample" >&2; exit 1; }
  done
  for sample in "UTF-8" "D-Bus" "AT-SPI" "x86-64"; do
    ! printf '%s\n' "$sample" | grep -qE "$PATTERN" || { echo "pattern is too wide: $sample" >&2; exit 1; }
  done
  echo "self-test ok"
  exit 0
fi

hits=$(git grep -nIE "$PATTERN" -- "${PATHS[@]}" ':!fuzz/corpus' | scan || true)
if [ -n "$hits" ]; then
  echo "internal labels or private references in tracked public files:" >&2
  printf '%s\n' "$hits" >&2
  exit 1
fi
echo "public hygiene ok"
