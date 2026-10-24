#!/usr/bin/env bash
# Fuzz driver (libFuzzer through cargo-fuzz, pinned nightly).
#
#   scripts/fuzz.sh smoke    [target...]   30 s per target, on a temporary copy
#                                          of the corpus (never written back)
#   scripts/fuzz.sh close    [target...]   600 s per target, in parallel
#   scripts/fuzz.sh baseline [target...]   3600 s per target, in parallel
#   scripts/fuzz.sh cmin     [target...]   minimize the committed corpus
#
# No target given means all of them. Exit 1 if any target left a crash,
# timeout or OOM artifact in fuzz/artifacts/<target>/.
set -euo pipefail
cd "$(dirname "$0")/.."

# Pinned toolchain and tool. cargo-fuzz and libFuzzer need a nightly; the
# repo's root rust-toolchain.toml (stable) would otherwise apply, so every
# cargo call names the nightly explicitly. The date is 2026-09-01: more than
# two weeks old when chosen (2026-10-24), so it is a settled nightly.
# Install: rustup toolchain install nightly-2026-09-01 --profile minimal -c rust-src
#          cargo install --locked cargo-fuzz@0.13.2
NIGHTLY=nightly-2026-09-01
FUZZ_VERSION=0.13.2

ALL_TARGETS="manifest markdown"

# Wall-clock budget per target, per mode (the three-tier frame):
#   smoke    30 s  CI, every push: only catches an obvious regression.
#   close   600 s  end of every plan that touches a parser: libFuzzer finds
#                  most bugs in the first minutes and small text parsers
#                  saturate in ten.
#   baseline 3600 s once, on the existing code: the most findings are expected
#                  here, and the corpus grows from it.
SMOKE_SECS=30
CLOSE_SECS=600
BASELINE_SECS=3600

# Per-target libFuzzer limits: <max_len> <timeout_s> <rss_limit_mb>.
#   max_len    tied to the input limit the real code already has.
#   timeout    per input; the parsers are linear and finish in ms, so a
#              multi-second input is super-linear behaviour (or a hang),
#              which is what this catches. Kept well above the ASan slowdown.
#   rss_limit  per process; above linear memory growth. ASan itself costs
#              ~100-200 MB, so the limit leaves room for it.
limits() {
    case "$1" in
        # 64 KiB = MAX_OUTPUT in crates/engine/src/catalog.rs, the most a
        # plugin's --manifest output can be; cache and history files are
        # our own writes of the same grammar. 1 s: 64 KiB in a line parser.
        # 512 MB: ~8000x the input.
        manifest) echo "65536 1 512" ;;
        # The document readers have no size limit; the spec examples are
        # under 1 KiB and the deepest nesting (MAX_DEPTH 256 in
        # crates/doc/src/md/inline.rs) fits in a few KiB, so 16 KiB reaches
        # every limit and keeps a quadratic case visible. 2 s: a block
        # parser plus three writers under ASan. 512 MB: ~32000x the input.
        markdown) echo "16384 2 512" ;;
        *) echo "unknown fuzz target: $1" >&2; return 1 ;;
    esac
}

mode=${1:-}
[ -n "$mode" ] || { sed -n '2,10p' "$0" >&2; exit 2; }
shift
TARGETS=${*:-$ALL_TARGETS}
for t in $TARGETS; do limits "$t" >/dev/null; done

if ! cargo +"$NIGHTLY" fuzz --version 2>/dev/null | grep -q "cargo-fuzz $FUZZ_VERSION"; then
    echo "error: need cargo-fuzz $FUZZ_VERSION on $NIGHTLY (see the header)" >&2
    exit 2
fi

# Cores: the run uses all of them. Targets run side by side, each with
# nproc / <targets> libFuzzer workers (at least 1) sharing one corpus, so the
# whole machine is busy for the whole budget and the wall clock equals the
# per-target budget.
NPROC=$(nproc)
count=$(wc -w <<<"$TARGETS")
WORKERS=$((NPROC / count))
[ "$WORKERS" -ge 1 ] || WORKERS=1

cargo +"$NIGHTLY" fuzz build >&2
BIN_DIR=fuzz/target/x86_64-unknown-linux-gnu/release

# run_one <target> <secs> <workers> <corpus-dir>
run_one() {
    local t=$1 secs=$2 workers=$3 corpus=$4 max_len timeout rss
    read -r max_len timeout rss <<<"$(limits "$t")"
    mkdir -p "fuzz/artifacts/$t"
    local work
    work=$(mktemp -d) # libFuzzer's -jobs mode writes fuzz-N.log into the cwd
    local args=("-max_total_time=$secs" "-max_len=$max_len" "-timeout=$timeout"
        "-rss_limit_mb=$rss" "-artifact_prefix=$PWD/fuzz/artifacts/$t/")
    if [ "$workers" -gt 1 ]; then
        args+=("-jobs=$workers" "-workers=$workers")
    fi
    (cd "$work" && "$OLDPWD/$BIN_DIR/$t" "${args[@]}" "$corpus") >"$work/out.log" 2>&1 || true
    tail -n 3 "$work/out.log" | sed "s/^/[$t] /" >&2
    cat "$work"/fuzz-*.log 2>/dev/null | grep -E "ERROR|SUMMARY|crash-|timeout-|oom-" | sed "s/^/[$t] /" >&2 || true
    rm -rf "$work"
}

# Artifacts are the verdict: -jobs mode does not reliably turn a worker's
# crash into a non-zero exit.
artifacts_of() { find "fuzz/artifacts/$1" -type f 2>/dev/null; }

case "$mode" in
smoke)
    bad=0
    for t in $TARGETS; do
        # A temporary copy: the smoke run must never grow the committed corpus.
        tmp=$(mktemp -d)
        cp -r "fuzz/corpus/$t/." "$tmp/" 2>/dev/null || true
        run_one "$t" "$SMOKE_SECS" 1 "$tmp"
        rm -rf "$tmp"
        if [ -n "$(artifacts_of "$t")" ]; then
            echo "FAIL $t:" >&2
            artifacts_of "$t" >&2
            bad=1
        fi
    done
    exit $bad
    ;;
close | baseline)
    secs=$CLOSE_SECS
    [ "$mode" = baseline ] && secs=$BASELINE_SECS
    pids=()
    for t in $TARGETS; do
        mkdir -p "fuzz/corpus/$t"
        run_one "$t" "$secs" "$WORKERS" "$PWD/fuzz/corpus/$t" &
        pids+=($!)
    done
    wait "${pids[@]}"
    bad=0
    for t in $TARGETS; do
        if [ -n "$(artifacts_of "$t")" ]; then
            echo "FAIL $t:" >&2
            artifacts_of "$t" >&2
            bad=1
        fi
    done
    echo "corpus grew unminimized; run: scripts/fuzz.sh cmin" >&2
    exit $bad
    ;;
cmin)
    for t in $TARGETS; do
        cargo +"$NIGHTLY" fuzz cmin "$t" >&2
    done
    ;;
*)
    echo "unknown mode: $mode" >&2
    exit 2
    ;;
esac
