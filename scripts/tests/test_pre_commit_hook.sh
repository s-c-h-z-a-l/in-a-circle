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
# Cargo.lock is tracked (the hook runs cargo with --locked).
cargo generate-lockfile --offline -q

out="$work/out"
# commit <label> <expect: pass|fail> [paths]: runs `git commit` (so the hook runs
# through git exactly as for a person) and compares the verdict.
commit() {
  local label=$1 expect=$2 got=pass
  shift 2
  git commit -q -m "$label" "$@" >"$out" 2>&1 || got=fail
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
git show HEAD:app/src/main.rs >app/src/main.rs
rm -f app/src/untracked.rs
git rm -q --cached app/src/helper.rs
commit "staged deletion" fail
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

git add app/src/lib.rs
git diff --cached --quiet || { echo "FAIL setup: index differs from HEAD"; fails=$((fails + 1)); }

pc=target/pre-commit
# restore <path>: the committed content, staged (index == HEAD for it).
restore() {
  git show "HEAD:$1" >"$1"
  git add -- "$1"
}
broken_main() {
  printf 'mod helper;\n\nfn main() {\n    println!("{}", app::f(1) + helper::h() + missing());\n}\n' >app/src/main.rs
}

# (g) A rename to a non-Rust name must still run the check: lib-side `mod`
# declarations (here main.rs's `mod helper;`) no longer resolve.
git mv app/src/helper.rs app/src/helper.md
commit "rename rs to md" fail
said "FAILED step"
git mv app/src/helper.md app/src/helper.rs

# (h) `git commit <paths>` checks the tree of those paths only: a broken
# change staged in another path is not part of the commit...
broken_main
git add app/src/main.rs
printf '// note\n' >>app/src/lib.rs
commit "paths: other staged path broken" pass app/src/lib.rs
restore app/src/main.rs
# ... and a broken listed path is rejected although everything else is fine.
printf 'pub fn f(x: u32) -> u32 {\n    x + "a"\n}\n' >app/src/lib.rs
commit "paths: listed path broken" fail app/src/lib.rs
said "cargo check"
git show HEAD:app/src/lib.rs >app/src/lib.rs

# (i) A mode change is carried into the export.
chmod +x app/src/helper.rs
git add app/src/helper.rs
commit "mode +x" pass
[ -x $pc/tree/app/src/helper.rs ] || { echo "FAIL +x not exported"; fails=$((fails + 1)); }
chmod -x app/src/helper.rs
git add app/src/helper.rs
commit "mode -x" pass
[ ! -x $pc/tree/app/src/helper.rs ] || { echo "FAIL -x not exported"; fails=$((fails + 1)); }

# (j) An unusable stamp forces a full export. A file in the export that is
# not in the index (here extra.rs) would let `mod extra;` build if the
# export were reused.
extra_main() {
  printf 'mod extra;\nmod helper;\n\nfn main() {\n    println!("{}", app::f(1) + helper::h());\n}\n' >app/src/main.rs
  git add app/src/main.rs
}
echo 'garbage' >$pc/tree.id
printf '' >$pc/tree/app/src/extra.rs
extra_main
commit "corrupt stamp" fail
restore app/src/main.rs
# A stamp naming a tree that does not exist.
printf '0123456789012345678901234567890123456789\n' >$pc/tree.id
printf '// n2\n' >>app/src/lib.rs
git add app/src/lib.rs
commit "missing-tree stamp" pass
[ ! -e $pc/tree/app/src/extra.rs ] || { echo "FAIL stale file survived a full export"; fails=$((fails + 1)); }
# Valid stamp, but a stray file in the export: the file-count guard.
printf '' >$pc/tree/app/src/extra.rs
extra_main
commit "stray file in export" fail
restore app/src/main.rs

# (k) A failing `git diff-tree` must abort and leave no stamp, never pass
# on a stale export. The shim fails only that subcommand.
realgit=$(command -v git)
mkdir "$work/shim"
printf '#!/bin/sh\n[ "$1" = diff-tree ] && exit 3\nexec %s "$@"\n' "$realgit" >"$work/shim/git"
chmod +x "$work/shim/git"
printf '// n3\n' >>app/src/lib.rs
git add app/src/lib.rs
commit "warm export" pass
broken_main
git add app/src/main.rs
# git puts its own exec dir before PATH for hooks, so the hook is run
# directly here, as git would (cwd = top level).
rc=0
PATH="$work/shim:$PATH" bash scripts/git-hooks/pre-commit >"$out" 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
  echo "FAIL diff-tree failure let the hook pass"
  fails=$((fails + 1))
else
  echo "ok   diff-tree fails"
fi
said "diff-tree"
[ ! -e $pc/tree.id ] || { echo "FAIL stamp survived a failed update"; fails=$((fails + 1)); }
commit "after failed update" fail
said "cargo check"
restore app/src/main.rs

# (l) --locked: a staged Cargo.toml change without the matching Cargo.lock
# is rejected; with the updated lock staged it passes.
sed -i 's/^version = "0.0.0"/version = "0.0.1"/' app/Cargo.toml
git add app/Cargo.toml
commit "stale lock" fail
said "Cargo.lock"
cargo generate-lockfile --offline -q
git add Cargo.lock
commit "lock updated" pass

if [ "$fails" -ne 0 ]; then
  echo "$fails failure(s)"
  exit 1
fi
echo "all passed"
