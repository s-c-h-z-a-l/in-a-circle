#!/usr/bin/env bash
# Fuzz driver (libFuzzer through cargo-fuzz, pinned nightly).
#
#   scripts/fuzz.sh smoke    [target...]   30 s per target, on a temporary copy
#                                          of the corpus (never written back)
#   scripts/fuzz.sh close    [target...]   600 s per target, in parallel
#   scripts/fuzz.sh baseline [target...]   3600 s per target, in parallel
#   scripts/fuzz.sh cmin     [target...]   minimize the committed corpus
#
# No target given means all of them. Exit 1 if libFuzzer did not really run for a
# target, or left a crash, timeout or OOM artifact in this run. Logs and
# artifacts: fuzz/runs/<mode>-<time>/<target>/ (gitignored).
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

ALL_TARGETS="manifest markdown dbus_message uri_list catalog_cache history wire_events"

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
        # 64 KiB = MAX_MESSAGE in crates/dbus/src/message.rs: frame_len
        # rejects anything larger before decode runs, so longer inputs only
        # test that rejection. 1 s: a message that small decodes in us.
        # 512 MB: ~8000x the input.
        dbus_message) echo "65536 1 512" ;;
        # 64 KiB = MAX_BYTES in bins/inacircle/src/uri.rs, the most read
        # from a drop. 1 s: a line splitter plus percent-decoding is linear.
        # 512 MB: ~8000x the input.
        uri_list) echo "65536 1 512" ;;
        # The cache has no size cap in the code (read_cache reads the whole
        # file). 64 KiB = MAX_OUTPUT: an entry is one binary's manifest
        # output (capped at that) plus a bin line, so a cache of a few
        # binaries is a few KiB (the real one for 2 binaries is 0.8 KiB) and
        # 64 KiB holds one maximal entry. 1 s: a line parser, linear.
        # 512 MB: ~8000x the input.
        catalog_cache) echo "65536 1 512" ;;
        # The history has no line or size cap in the code (it only grows;
        # undo streams it and holds one line at a time). A record is ~200
        # bytes, so 64 KiB (the same bound as the other line grammars) is
        # ~300 records and reaches every branch. 1 s: linear scan plus a
        # substring search per line. 512 MB: ~8000x the input.
        history) echo "65536 1 512" ;;
        # 8192 = MAX_INB in crates/wire/src/conn.rs (2 * MAX_MESSAGE, 4096):
        # the most unframed bytes a Conn holds, and fuzz_events cuts to it.
        # 1 s: framing and the readers are linear in the input. 512 MB:
        # ~64000x the input.
        wire_events) echo "8192 1 512" ;;
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

# Every run gets its own directory, fuzz/runs/<mode>-<time>/<target>/, holding
# the libFuzzer logs and the artifacts (crash-*, timeout-*, oom-*) of that run
# only. It is gitignored and kept, so crash details survive the run; an old
# run's artifacts can never fail a later one.
RUN_DIR=fuzz/runs/$mode-$(date +%Y%m%d-%H%M%S)

# run_one <target> <secs> <workers> <corpus-dir>
# Returns 0 only if libFuzzer really ran (a "Done N runs" line in its log) and
# left no artifact in this run's directory. A missing or non-executable
# binary, a startup failure or a killed process leave no such line and fail.
run_one() {
    local t=$1 secs=$2 workers=$3 corpus=$4 max_len timeout rss
    read -r max_len timeout rss <<<"$(limits "$t")"
    local dir=$PWD/$RUN_DIR/$t
    mkdir -p "$dir/artifacts"
    local args=("-max_total_time=$secs" "-max_len=$max_len" "-timeout=$timeout"
        "-rss_limit_mb=$rss" "-artifact_prefix=$dir/artifacts/")
    if [ "$workers" -gt 1 ]; then
        args+=("-jobs=$workers" "-workers=$workers") # writes fuzz-N.log into the cwd
    fi
    (cd "$dir" && "$PWD/../../../../$BIN_DIR/$t" "${args[@]}" "$corpus") >"$dir/out.log" 2>&1 || true
    local ran=0 crashed=0
    # (grep on the files, not a pipe: grep -q closing early would fail a pipefail pipeline)
    if grep -qE '^Done [0-9]+ runs' "$dir"/out.log "$dir"/fuzz-*.log 2>/dev/null; then ran=1; fi
    if [ -n "$(find "$dir/artifacts" -type f)" ]; then crashed=1; fi
    tail -n 2 "$dir/out.log" | sed "s/^/[$t] /" >&2
    echo "[$t] logs: $RUN_DIR/$t" >&2
    if [ "$crashed" = 1 ]; then
        echo "FAIL $t: artifacts:" >&2
        find "$dir/artifacts" -type f >&2
        return 1
    fi
    if [ "$ran" = 0 ]; then
        echo "FAIL $t: libFuzzer did not complete a run; log tail:" >&2
        tail -n 20 "$dir/out.log" >&2
        return 1
    fi
}

case "$mode" in
smoke)
    bad=0
    for t in $TARGETS; do
        # A temporary copy: the smoke run must never grow the committed corpus.
        tmp=$(mktemp -d)
        cp -r "fuzz/corpus/$t/." "$tmp/" 2>/dev/null || true
        run_one "$t" "$SMOKE_SECS" 1 "$tmp" || bad=1
        rm -rf "$tmp"
    done
    exit $bad
    ;;
close | baseline)
    secs=$CLOSE_SECS
    [ "$mode" = baseline ] && secs=$BASELINE_SECS
    declare -A pid before
    for t in $TARGETS; do
        mkdir -p "fuzz/corpus/$t"
        before[$t]=$(find "fuzz/corpus/$t" -type f | wc -l)
        run_one "$t" "$secs" "$WORKERS" "$PWD/fuzz/corpus/$t" &
        pid[$t]=$!
    done
    bad=0
    for t in $TARGETS; do
        wait "${pid[$t]}" || bad=1
        after=$(find "fuzz/corpus/$t" -type f | wc -l)
        [ "$after" -gt "${before[$t]}" ] &&
            echo "[$t] corpus grew ${before[$t]} -> $after files; run: scripts/fuzz.sh cmin $t" >&2
    done
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
