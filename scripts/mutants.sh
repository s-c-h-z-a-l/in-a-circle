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

common=(
  --jobs "$JOBS"
  --timeout-multiplier "$TIMEOUT_MULTIPLIER"
  --minimum-test-timeout "$MIN_TEST_TIMEOUT"
  --build-timeout-multiplier "$BUILD_TIMEOUT_MULTIPLIER"
)

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
  stage_tree
  cargo mutants -d "$TREE" "${common[@]}" -o "$PWD/$out" "${pre[@]}" "${EXTRA[@]}" "${post[@]}" || rc=$?
  case $rc in 0 | 2 | 3) ;; *) echo "cargo mutants failed ($rc)" >&2; exit "$rc" ;; esac
  summary "$out"
}

case $MODE in
list)
  stage_tree
  cargo mutants -d "$TREE" --list "${file_args[@]}" "${EXTRA[@]}"
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
  run "$OUT_ROOT/diff" --in-diff "$patch" --test-workspace=true
  ;;
*)
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 1
  ;;
esac
