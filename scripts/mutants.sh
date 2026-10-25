#!/usr/bin/env bash
# Two-stage mutation testing (cargo-mutants 27.1.0) of the safety-critical files.
#
#   scripts/mutants.sh list                 mutants of the eight files, no tests run
#   scripts/mutants.sh stage1               each mutant vs its own crate's tests (fast)
#   scripts/mutants.sh stage2               stage 1's survivors vs the whole workspace
#   scripts/mutants.sh diff [base]          only code changed in `git diff <base>...HEAD`
#                                           (default base: main), whole workspace tests
#
# Extra cargo-mutants arguments go in $MUTANTS_ARGS (e.g. "-F event.rs:1[0-9]:"
# to narrow a trial run; no spaces inside an argument). Outputs: target/mutants/<mode>/mutants.out/ (gitignored
# through target/): caught.txt, missed.txt, timeout.txt, unviable.txt, outcomes.json.
# Exit 0 means the run completed; survivors are the result, not a failure.
set -euo pipefail
cd "$(dirname "$0")/.."

MODE=${1:-}
BASE=${2:-main}
OUT_ROOT=target/mutants

# The scope: the safety-critical files (spec section 3). Sandbox set-up,
# output placement and undo, Wayland and D-Bus framing.
FILES=(
  crates/engine/src/sandbox.rs
  crates/engine/src/exec.rs
  crates/engine/src/output.rs
  crates/wire/src/conn.rs
  crates/wire/src/event.rs
  crates/dbus/src/conn.rs
  crates/dbus/src/message.rs
  crates/dbus/src/marshal.rs
)

# Parallel jobs. The machine has 16 threads. Each job is a separate tree copy
# with its own cargo build (itself multi-threaded) and test run, and the tests
# have wall-clock assertions (deadlines, sleeps) that a saturated machine turns
# into false timeouts. 8 = half the threads leaves room for the compiles and
# the test processes inside each job.
JOBS=${MUTANTS_JOBS:-8} # override for a trial run only

# Per-mutant test timeout = max(MIN_TEST_TIMEOUT, baseline time x
# TIMEOUT_MULTIPLIER). A mutant that loops or blocks forever (a removed
# deadline, a flipped loop condition) is reported as a timeout, which counts as
# caught, so it must be cut off quickly. The multiplier 3 covers the slowdown of
# 8 parallel jobs over the unmutated baseline; the minimum 60 s is a floor for
# the very fast crates (wire, dbus), where a baseline of a few seconds times 3
# would trip on a loaded machine's scheduling noise.
TIMEOUT_MULTIPLIER=3
MIN_TEST_TIMEOUT=60

# Build timeout multiplier: a mutant that only fails to compile in the slow
# way (type-check hangs do not happen) needs no tight bound; the default floor
# of cargo-mutants applies, scaled by the baseline build. 5 = its own default,
# written down so the number has a reason: a mutated build is an incremental
# build of one crate plus its dependents, never slower than a few baselines.
BUILD_TIMEOUT_MULTIPLIER=5

# Stage 1 only: tests skipped because they cannot touch the mutated files and
# each takes seconds, which sets the per-mutant time (measured: an engine
# mutant ~22 s, 15 s of it the slowest test).
#   refresh_budget_leaves_no_child (catalog.rs, ~13 s): eight 1.5 s manifests
#     against the 10 s refresh budget; it checks catalog.rs's budget and child
#     reaping. The sandbox code under test is already run by the faster
#     manifest_cannot_write_outside / manifest_cannot_read_outside tests.
#   hanging_manifest_killed_after_deadline (catalog.rs, ~2-3 s): a manifest
#     that sleeps is killed at the 2 s catalog deadline; that is catalog.rs's
#     timer, not the sandbox, exec or output code.
# Stage 2 runs everything, so a mutant that only these tests would kill is
# still caught there; nothing is lost, it is just examined by the slower run.
SKIP_STAGE1=(
  refresh_budget_leaves_no_child
  hanging_manifest_killed_after_deadline
)

# Memory caps (incident: the first stage-1 run, 8 jobs, took the machine to
# 99.5 of 101 GB RAM plus the swap, and systemd-oomd killed the whole terminal
# scope. A mutant turned a bounded loop into an endless one that pushes into a
# Vec, e.g. `+=` to `*=` in marshal.rs parse_type leaves `pos` at 0; nothing
# capped memory and the 60 s timeout came far too late). Two layers:
#
# 1. RLIMIT_DATA per process (hard, so no child can raise it): a runaway test
#    aborts on allocation failure within seconds, and cargo-mutants counts the
#    mutant as caught (it changed behavior). 6 GiB = the 4 GiB top of the
#    sandbox's own data_cap (crates/engine/src/sandbox.rs: RAM/8 clamped to
#    1-4 GiB; the alloc_* tests in bins/inacircle run an action up to that cap,
#    and a child clamps to the inherited hard limit, so the cap must be >= 4 GiB
#    for alloc_over_cap_fails to see its documented limit) plus 2 GiB for the
#    harness. Measured (debug, this repo): the largest process of a clean
#    `cargo test --workspace --no-run` peaks at 386 MiB RSS, the test
#    binaries at <= 66 MiB RSS, and the whole workspace builds and tests under
#    `ulimit -d` 1 GiB (and the inacircle suite passes under 6 GiB), so 2 GiB of
#    margin is 5x what any legitimate process needs.
DATA_CAP_KIB=$((6 * 1024 * 1024))
# 2. A cgroup for the whole run (systemd-run --user --scope): MemoryMax kills
#    only this scope, never the terminal, and MemorySwapMax=0 keeps a runaway
#    from thrashing the swap first. Half of MemTotal (read at run time): jobs
#    are 8 x (rustc 0.4 GiB + tests, or up to 4 GiB + a little in the alloc
#    tests) = ~36 GiB worst legitimate case, which fits; the rest stays for the
#    desktop. A multi-process runaway still dies at DATA_CAP_KIB each.
MEM_TOTAL_KIB=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
SCOPE_MAX_KIB=$((MEM_TOTAL_KIB / 2))

# Landlock must be real in every run: a mutant that survives only without it
# must not exist (the tests would skip the sandbox checks).
export INACIRCLE_REQUIRE_LANDLOCK=1

# cargo-mutants would copy the whole directory (untracked files too) once per
# job. fuzz/ (a live corpus, rewritten while a campaign runs, so the copy even
# fails) and spikes/ (1.4 GB) are outside the workspace (`exclude`), so every
# run works on a staged copy of the tracked and untracked-not-ignored files
# without them. The copy is of the working tree, so uncommitted changes are
# mutation-tested too; stage 2 must see the tree stage 1 saw.
TREE=$PWD/$OUT_ROOT/tree
stage_tree() {
  mkdir -p "$TREE"
  git ls-files -z --cached --others --exclude-standard |
    grep -zvE '^(fuzz|spikes|notes|\.superpowers|\.obsidian)/' |
    rsync -a --delete --from0 --files-from=- --delete-excluded ./ "$TREE/"
}

# shellcheck disable=SC2206
EXTRA=(${MUTANTS_ARGS:-})

file_args=()
for f in "${FILES[@]}"; do file_args+=(-f "$f"); done

# Code under #[cfg(fuzzing)] (the fuzz_* entry points and Event::clone_args) is
# compiled out of every test build, so no test could kill a mutant in it: they
# would all be reported as missed. They are fuzz-only glue, not shipped code.
EXCLUDE_FUZZ_RE='(\bfuzz_[a-z_]+|Event::clone_args)'

common=(
  --exclude-re "$EXCLUDE_FUZZ_RE"
  --jobs "$JOBS"
  --build-timeout-multiplier "$BUILD_TIMEOUT_MULTIPLIER"
)

# Test timeout arguments. Stage 1 (one crate's tests): cargo-mutants' own
# baseline of that crate, x TIMEOUT_MULTIPLIER, at least MIN_TEST_TIMEOUT.
timeout_args=(--timeout-multiplier "$TIMEOUT_MULTIPLIER" --minimum-test-timeout "$MIN_TEST_TIMEOUT")

# Stage 2 and diff run the WHOLE workspace's tests per mutant, but
# cargo-mutants times a baseline of only the packages that hold the selected
# mutants (incident: dbus-only survivors gave a 2 s baseline, so the 60 s
# floor was far below the ~66 s a passing workspace run takes; every mutant
# "timed out", and a timeout counts as caught, which hid real survivors).
# So measure the full workspace suite here, in the staged tree, and give each
# mutant WORKSPACE_TIMEOUT_MULTIPLIER x that (3 = the load of 8 parallel jobs
# over an idle run, as for stage 1). A timeout then means a hang, not a pass.
WORKSPACE_TIMEOUT_MULTIPLIER=${WORKSPACE_TIMEOUT_MULTIPLIER:-3} # override for a trial run only
workspace_timeout() {
  stage_tree
  # Build first, so the measured time is the tests alone.
  (cd "$TREE" && ulimit -S -H -d "$DATA_CAP_KIB" && cargo test --workspace --no-run >/dev/null 2>&1) ||
    { echo "the staged tree does not build" >&2; exit 1; }
  local t0=$SECONDS
  (cd "$TREE" && ulimit -S -H -d "$DATA_CAP_KIB" && cargo test --workspace >/dev/null 2>&1) ||
    { echo "the unmutated workspace suite fails in the staged tree" >&2; exit 1; }
  local base=$((SECONDS - t0)) t
  t=$((base * WORKSPACE_TIMEOUT_MULTIPLIER))
  ((t < MIN_TEST_TIMEOUT)) && t=$MIN_TEST_TIMEOUT
  echo "workspace baseline ${base}s, per-mutant test timeout ${t}s"
  # The baseline was just measured: skip cargo-mutants' per-package one,
  # which would set its own (wrong) timeout.
  timeout_args=(--baseline skip --timeout "$t")
}

count() { # <file>: number of lines, 0 if the file is missing
  if [[ -f $1 ]]; then wc -l <"$1"; else echo 0; fi
}

summary() { # <outdir>
  local d=$1/mutants.out
  echo "== summary ($d)"
  echo "caught:   $(count "$d/caught.txt")"
  echo "missed:   $(count "$d/missed.txt")"
  echo "timeout:  $(count "$d/timeout.txt")  (counted as caught)"
  echo "unviable: $(count "$d/unviable.txt")"
  echo "outcomes: $d/outcomes.json, survivors: $d/missed.txt, logs: $d/log/"
}

run() { # <outdir> <cargo-mutants args...> [-- <test binary args...>]
  local out=$1
  shift
  # $MUTANTS_ARGS must land before any `--`, or cargo would pass it to the tests.
  local pre=() post=() seen=0 a
  for a in "$@"; do
    if [[ $seen == 0 && $a == -- ]]; then seen=1; elif [[ $seen == 1 ]]; then post+=("$a"); else pre+=("$a"); fi
  done
  [[ $seen == 1 ]] && post=(-- "${post[@]}")
  mkdir -p "$out"
  # cargo-mutants exits 2 when mutants were missed and 3 when some timed out;
  # those are results, not failures. Anything else is a real error.
  local rc=0
  # Never run uncapped: without a user systemd scope there is no whole-run cap.
  if ! systemd-run --user --scope --quiet -p MemoryMax=1G -- true 2>/dev/null; then
    echo "systemd-run --user --scope is unavailable; refusing to run mutation tests without a memory cap" >&2
    exit 1
  fi
  local unit=inacircle-mutants-$$-$RANDOM
  stage_tree
  # The subshell sets the hard RLIMIT_DATA only for the run; the scope's
  # processes inherit it from systemd-run.
  (
    ulimit -S -H -d "$DATA_CAP_KIB"
    exec systemd-run --user --scope --quiet --unit "$unit" -p "MemoryMax=${SCOPE_MAX_KIB}K" -p MemorySwapMax=0 -- \
      cargo mutants -d "$TREE" "${common[@]}" "${timeout_args[@]}" -o "$PWD/$out" "${pre[@]}" "${EXTRA[@]}" "${post[@]}"
  ) || rc=$?
  # A timed-out mutant's test helper (a child that called setsid, e.g. the
  # sandbox tests' seccomp helpers) escapes cargo-mutants' process-group kill
  # and outlived the run as an orphan (two were found sleeping for over an
  # hour). A scope lives while any process is in it, so stopping it here
  # kills whatever is left; harmless if it already ended.
  systemctl --user stop "$unit.scope" 2>/dev/null || true
  case $rc in 0 | 2 | 3) ;; *) echo "cargo mutants failed ($rc)" >&2; exit "$rc" ;; esac
  summary "$out"
}

case $MODE in
list)
  stage_tree
  cargo mutants -d "$TREE" --list --exclude-re "$EXCLUDE_FUZZ_RE" "${file_args[@]}" "${EXTRA[@]}"
  ;;
stage1)
  skips=()
  for t in "${SKIP_STAGE1[@]}"; do skips+=(--skip "$t"); done
  # No --test-workspace: each mutant runs the tests of the crate it is in.
  # cargo-mutants hands what follows its own `--` to `cargo test`, so the
  # second `--` is the one that reaches the test binaries.
  run "$OUT_ROOT/stage1" "${file_args[@]}" -- -- "${skips[@]}"
  ;;
stage2)
  missed=$OUT_ROOT/stage1/mutants.out/missed.txt
  [[ -f $missed ]] || { echo "no $missed: run stage1 first" >&2; exit 1; }
  if [[ ! -s $missed ]]; then
    echo "stage 1 left no survivors; nothing to re-run"
    exit 0
  fi
  # cargo-mutants has no "only these mutants" list option, so the survivors
  # become one anchored --re regex each (repeatable; a mutant is examined if it
  # matches any). The names carry file:line:col, so the tree must be the one
  # stage 1 ran on, or the lines point at other code.
  if [[ -n $(git status --porcelain --untracked-files=no -- crates bins Cargo.toml Cargo.lock) ]]; then
    echo "warning: tracked sources differ from HEAD; survivor line numbers must still match stage 1's tree" >&2
  fi
  re=()
  while IFS= read -r name; do
    [[ -n $name ]] || continue
    # Escape regex metacharacters; `/` is not special.
    esc=$(printf '%s' "$name" | sed -e 's/[][\\.^$*+?(){}|]/\\&/g')
    re+=(--re "^${esc}\$")
  done <"$missed"
  # The whole workspace's tests, nothing skipped: bins/inacircle's integration
  # tests carry much of exec/output's coverage.
  workspace_timeout
  run "$OUT_ROOT/stage2" "${file_args[@]}" --test-workspace=true "${re[@]}"
  ;;
diff)
  mkdir -p "$OUT_ROOT"
  patch=$OUT_ROOT/diff-$$.patch
  git diff "$BASE...HEAD" >"$patch"
  trap 'rm -f "$patch"' EXIT
  [[ -s $patch ]] || { echo "no diff against $BASE"; exit 0; }
  # Changed code only (CLAUDE.md), across all crates, whole workspace tests,
  # nothing skipped: the diff is small, so the slow tests cost little.
  workspace_timeout
  run "$OUT_ROOT/diff" --in-diff "$patch" --test-workspace=true
  ;;
*)
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 1
  ;;
esac
