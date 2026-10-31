#!/usr/bin/env bash
# Tests of scripts/git-hooks/pre-commit: the hook checks the STAGED tree,
# not the working tree. Runs the real hook in a throwaway git repo holding a
# tiny cargo workspace.
#
#   bash scripts/tests/test_pre_commit_hook.sh
#
# Exit 0: every case passed. Needs git and cargo. The real repo (its index,
# its target/) and the user's home are never touched: the throwaway repo
# lives in a temporary dir, git reads no user or system config, and the
# hook's cache is the throwaway repo's own target/pre-commit.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fails=0

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
# A git variable inherited from a caller (this test may itself run inside a
# hook) must not point git at another repository.
unset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_PREFIX

r="$work/repo"
mkdir -p "$r/scripts/git-hooks" "$r/app/src"
cp "$repo/scripts/git-hooks/pre-commit" "$r/scripts/git-hooks/pre-commit"
cd "$r"
git init -q -b main
git config user.email t@example.invalid
git config user.name t
git config core.hooksPath scripts/git-hooks
printf 'target/\n' >.gitignore

cat >Cargo.toml <<'T'
[workspace]
members = ["app"]
resolver = "2"
T
cat >app/Cargo.toml <<'T'
[package]
name = "app"
version = "0.0.0"
edition = "2021"
T
cat >app/src/lib.rs <<'T'
pub fn f() -> u32 {
    1
}
T
cat >app/src/main.rs <<'T'
mod helper;

fn main() {
    println!("{}", app::f() + helper::h());
}
T
cat >app/src/helper.rs <<'T'
pub fn h() -> u32 {
    2
}
T
echo "docs" >README.md

out="$work/out"
# commit <label> <expect: pass|fail>: runs `git commit` (so the hook runs
# through git exactly as for a person) and compares the verdict.
commit() {
  local label=$1 expect=$2 got=pass
  git commit -q -m "$label" >"$out" 2>&1 || got=fail
  if [ "$got" = "$expect" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label: expected $expect, got $got"
    sed 's/^/     | /' "$out"
    fails=$((fails + 1))
  fi
}
# said <text>: the last commit's output contains the text.
said() {
  grep -q -- "$1" "$out" || {
    echo "FAIL output lacks: $1"
    sed 's/^/     | /' "$out"
    fails=$((fails + 1))
  }
}

git add -A
commit "baseline builds" pass

# (a) The change touches lib.rs and main.rs; only lib.rs is staged. The
# working tree builds, the commit would not.
cat >app/src/lib.rs <<'T'
pub fn f(x: u32) -> u32 {
    x
}
T
cat >app/src/main.rs <<'T'
mod helper;

fn main() {
    println!("{}", app::f(1) + helper::h());
}
T
git add app/src/lib.rs
commit "partially staged change" fail
said "cargo check"

# (b) Everything staged: passes.
git add app/src/main.rs
commit "fully staged change" pass

# (c) Unstaged broken edit and an untracked broken file do not block a good
# staged commit.
echo 'fn broken( {' >>app/src/main.rs
echo 'fn also_broken( {' >app/src/untracked.rs
cat >app/src/lib.rs <<'T'
pub fn f(x: u32) -> u32 {
    x + 1
}
T
git add app/src/lib.rs
commit "good staged, broken unstaged" pass
git diff --quiet -- app/src/main.rs && {
  echo "FAIL the hook changed the working tree"
  fails=$((fails + 1))
}

# (d) Doc-only commit skips the check: it passes although the working tree
# is broken, and leaves the cache untouched.
before=$(cat target/pre-commit/tree.id)
echo "more docs" >>README.md
git add README.md
commit "doc only" pass
[ "$(cat target/pre-commit/tree.id)" = "$before" ] || {
  echo "FAIL a doc-only commit ran the check"
  fails=$((fails + 1))
}

# (e) A deletion staged with `git rm --cached` is honoured: the file stays
# in the working tree but is absent from the commit, and main.rs needs it.
git checkout -q -- app/src/main.rs 2>/dev/null || git show HEAD:app/src/main.rs >app/src/main.rs
rm -f app/src/untracked.rs
git rm -q --cached app/src/helper.rs
commit "staged deletion" fail
git reset -q
git add -A

# (f) Formatting is judged on the staged content: staged unformatted code is
# rejected although the working tree is formatted.
printf 'pub fn f(x:u32)->u32{x+1}\n' >app/src/lib.rs
git add app/src/lib.rs
cat >app/src/lib.rs <<'T'
pub fn f(x: u32) -> u32 {
    x + 1
}
T
commit "staged unformatted" fail
said "cargo fmt"

if [ "$fails" -ne 0 ]; then
  echo "$fails failure(s)"
  exit 1
fi
echo "all passed"
