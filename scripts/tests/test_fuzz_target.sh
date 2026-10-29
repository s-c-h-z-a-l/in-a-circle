#!/usr/bin/env bash
# Tests of scripts/fuzz.sh's target choice, with mock rustc and cargo: on an
# x86_64 and an aarch64 host the build and the run use the same triple and
# the binary comes from that triple's dir; a target that is unsupported or
# of another architecture is refused before anything is built.
#
#   bash scripts/tests/test_fuzz_target.sh
#
# Exit 0: every case passed. Nothing outside a temporary dir is touched.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fails=0

# A copy of the script in a fake repo with one corpus file.
mkdir -p "$work/repo/scripts" "$work/repo/fuzz/corpus/manifest" "$work/bin"
cp "$repo/scripts/fuzz.sh" "$work/repo/scripts/fuzz.sh"
echo seed >"$work/repo/fuzz/corpus/manifest/seed"

# rustc +<nightly> -vV: the host comes from MOCK_HOST.
cat >"$work/bin/rustc" <<'EOF'
#!/usr/bin/env bash
[ "$2" = -vV ] && { echo "rustc 1.0.0-nightly"; echo "host: $MOCK_HOST"; }
EOF
# cargo +<nightly> fuzz ...: `--version` names the pinned cargo-fuzz;
# `build --target T` makes a fake libFuzzer binary for every target in
# fuzz/target/T/release (or, with MOCK_WRONG_DIR, in another triple's dir,
# as a build that ignored --target would). Every call is logged.
cat >"$work/bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo "cargo $*" >>"$MOCK_LOG"
shift # +nightly
[ "$1" = fuzz ] || exit 1
case "$2" in
--version) echo "cargo-fuzz 0.13.2" ;;
build)
    [ "$3" = --target ] || exit 1
    dir=fuzz/target/${MOCK_WRONG_DIR:-$4}/release
    mkdir -p "$dir"
    for t in manifest markdown dbus_message uri_list catalog_cache history wire_events; do
        printf '#!/bin/sh\necho "ran %s $0" >>"%s"\necho "Done 1 runs in 0 second(s)"\n' \
            "$t" "$MOCK_LOG" >"$dir/$t"
        chmod +x "$dir/$t"
    done
    ;;
*) exit 1 ;;
esac
EOF
chmod +x "$work/bin/rustc" "$work/bin/cargo"

# check <name> <want-exit> <grep-pattern-in-log-or-output> <env...> -- <args...>
check() {
    local name=$1 want=$2 pattern=$3 code=0
    shift 3
    local envs=()
    while [ "$1" != -- ]; do envs+=("$1"); shift; done
    shift
    rm -rf "$work/repo/fuzz/target" "$work/repo/fuzz/runs"
    : >"$work/log"
    env PATH="$work/bin:$PATH" MOCK_LOG="$work/log" "${envs[@]}" \
        bash "$work/repo/scripts/fuzz.sh" "$@" >"$work/out" 2>&1 || code=$?
    if [ "$code" != "$want" ]; then
        echo "FAIL $name: exit $code, want $want"; cat "$work/out"; fails=$((fails + 1)); return
    fi
    if ! grep -qE -- "$pattern" "$work/out" "$work/log"; then
        echo "FAIL $name: no match for: $pattern"; cat "$work/out" "$work/log"; fails=$((fails + 1)); return
    fi
    echo "ok   $name"
}

for host in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    check "target on $host" 0 "^$host fuzz/target/$host/release\$" MOCK_HOST=$host -- target
    # The build gets --target, and the run executes that triple's binary.
    check "build on $host" 0 "fuzz build --target $host" MOCK_HOST=$host -- smoke manifest
    check "run on $host" 0 "ran manifest .*/fuzz/target/$host/release/manifest" \
        MOCK_HOST=$host -- smoke manifest
done
# A build that put its binary in another triple's dir is not run: the
# smoke fails for want of a Done line.
check "binary of another triple not run" 1 "FAIL manifest: libFuzzer did not complete" \
    MOCK_HOST=aarch64-unknown-linux-gnu MOCK_WRONG_DIR=x86_64-unknown-linux-gnu -- smoke manifest
# An explicit target is checked.
check "explicit same-arch target" 0 "^x86_64-unknown-linux-gnu " \
    MOCK_HOST=x86_64-unknown-linux-gnu FUZZ_TARGET=x86_64-unknown-linux-gnu -- target
check "explicit other-arch target refused" 2 "cannot run on host" \
    MOCK_HOST=x86_64-unknown-linux-gnu FUZZ_TARGET=aarch64-unknown-linux-gnu -- target
check "unsupported explicit target refused" 2 "is not one of" \
    MOCK_HOST=x86_64-unknown-linux-gnu FUZZ_TARGET=x86_64-unknown-linux-musl -- target
check "unsupported host refused" 2 "is not one of" \
    MOCK_HOST=riscv64gc-unknown-linux-gnu -- target
check "unreadable host refused" 2 "cannot read the host" MOCK_HOST= -- target
# Nothing was built for a refused target.
check "refusal builds nothing" 2 "is not one of" \
    MOCK_HOST=riscv64gc-unknown-linux-gnu -- smoke manifest
if grep -q "fuzz build" "$work/log"; then
    echo "FAIL refusal builds nothing: a build ran"; fails=$((fails + 1))
fi

[ "$fails" = 0 ] && echo "all fuzz target cases passed"
exit $((fails > 0))
