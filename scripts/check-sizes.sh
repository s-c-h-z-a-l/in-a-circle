#!/usr/bin/env bash
# Size guard for the shipped binaries (stripped release builds).
#
#   scripts/check-sizes.sh                  check against the limits + baseline
#   scripts/check-sizes.sh --write-baseline record the current sizes
#
# One baseline per architecture (`uname -m`): the code differs, so one
# machine's sizes say nothing about the other's growth. x86_64 keeps
# scripts/sizes.baseline; any other uses scripts/sizes.baseline.<arch>.
# Lines starting with `#` are provenance (where and from what the numbers
# were measured), kept by hand and skipped here.
#
# Exit 0: within limits (warnings go to stderr). Exit 1: a limit is exceeded.
set -euo pipefail
cd "$(dirname "$0")/.."

# CLAUDE.md budget: `inacircle` <= 1.5 MB (the whole file; it carries the
# embedded glyph atlas, which is why this is the file size).
ENGINE_BIN=inacircle
ENGINE_FILE_LIMIT=$((1536 * 1024)) # 1 572 864 B
# CLAUDE.md budget: each family binary <= 1 MB of code. Measured as the
# `.text` section so embedded data (`.rodata`, e.g. fonts) is excluded, as the
# budget says it is justified and measured separately.
FAMILY_TEXT_LIMIT=$((1024 * 1024)) # 1 048 576 B
# CLAUDE.md: "A plan that grows a binary by more than 100 KB explains why."
# Growth past this over the baseline (file or .text) is a warning, not a
# failure: the plan must justify it and refresh the baseline.
GROWTH_WARN=$((100 * 1024)) # 102 400 B

ARCH=$(uname -m)
if [ "$ARCH" = x86_64 ]; then
    BASELINE=scripts/sizes.baseline
else
    BASELINE=scripts/sizes.baseline.$ARCH
fi
BINS=$(cat scripts/shipped-bins)

# shellcheck disable=SC2086
cargo build --release $(for b in $BINS; do printf -- '-p %s ' "$b"; done) >&2

measure() { # <bin>: prints "<file_bytes> <text_bytes>"
    local path=target/release/$1
    local file text
    file=$(stat -c %s "$path")
    text=$(size -A "$path" | awk '$1 == ".text" {print $2}')
    echo "$file $text"
}

if [ "${1:-}" = "--write-baseline" ]; then
    {
        # The provenance lines of the old baseline stay at the top.
        grep -E '^#' "$BASELINE" 2>/dev/null || true
        for b in $BINS; do echo "$b $(measure "$b")"; done
    } >"$BASELINE.tmp"
    mv "$BASELINE.tmp" "$BASELINE"
    echo "wrote $BASELINE ($ARCH)"
    exit 0
fi

bad=0
for b in $BINS; do
    read -r file text < <(measure "$b")
    echo "$b ($ARCH): file $file B, .text $text B"
    if [ "$b" = "$ENGINE_BIN" ]; then
        if [ "$file" -gt "$ENGINE_FILE_LIMIT" ]; then
            echo "error: $b file is $file B, limit $ENGINE_FILE_LIMIT B" >&2
            bad=1
        fi
    elif [ "$text" -gt "$FAMILY_TEXT_LIMIT" ]; then
        echo "error: $b .text is $text B, limit $FAMILY_TEXT_LIMIT B" >&2
        bad=1
    fi
    if ! line=$(grep -E "^$b " "$BASELINE" 2>/dev/null); then
        echo "warning: $b has no line in $BASELINE" >&2
        continue
    fi
    read -r _ bfile btext <<<"$line"
    if [ "$file" -gt $((bfile + GROWTH_WARN)) ]; then
        echo "warning: $b file grew $((file - bfile)) B over the baseline (> $GROWTH_WARN B)" >&2
    fi
    if [ "$text" -gt $((btext + GROWTH_WARN)) ]; then
        echo "warning: $b .text grew $((text - btext)) B over the baseline (> $GROWTH_WARN B)" >&2
    fi
done
# Baseline entries for binaries that are no longer shipped.
if [ -e "$BASELINE" ]; then
    while read -r name _; do
        case $name in '#'* | '') continue ;; esac
        grep -qxF "$name" scripts/shipped-bins ||
            echo "warning: $BASELINE lists unknown binary $name" >&2
    done <"$BASELINE"
else
    echo "warning: no baseline for $ARCH ($BASELINE)" >&2
fi
exit $bad
