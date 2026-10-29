#!/usr/bin/env bash
# Cargo's runner (.cargo/config.toml) for every binary it starts. A test
# binary (a path under `deps/`) never sees the user's home: it runs with
# HOME, every XDG base directory, the runtime dir, the download dir and the
# session bus pointed into a fresh private folder (a honeypot) under this
# tree's `target/test-home/`. Anything else (`cargo run`) runs untouched.
#
# The run fails, whatever the tests said, when
#   - anything appeared in the honeypot: a test resolved a default path
#     instead of its own temp dirs (on a machine without this runner, that
#     is the user's real state, config, cache or Downloads), or
#   - the real state folder or download folder changed while it ran
#     (a test reached them some other way, or the user used inacircle or
#     downloaded a file meanwhile: rerun).
# A clean honeypot is removed; a dirty one is kept for inspection.
#
# The tree is located from this script, so a copy (cargo-mutants) uses
# its own honeypot, never the original's.
set -eu

case $1 in
  */deps/*) ;;
  *) exec "$@" ;;
esac

real_home=${HOME:-}
real_config=${XDG_CONFIG_HOME:-$real_home/.config}
real_state=${XDG_STATE_HOME:-$real_home/.local/state}/inacircle
# The real download folder: user-dirs.dirs as the engine reads it, else
# ~/Downloads (approximate: a quoted path or `$HOME/...`).
real_dl=$real_home/Downloads
if [ -r "$real_config/user-dirs.dirs" ]; then
  v=$(sed -n 's/^[[:space:]]*XDG_DOWNLOAD_DIR=//p' "$real_config/user-dirs.dirs" | tail -n 1 | tr -d '"')
  case $v in
    '$HOME'*) real_dl=$real_home${v#\$HOME} ;;
    /*) real_dl=$v ;;
  esac
fi

# What a test could change there: the folders, their entries (names,
# modes, sizes, times) and the history's bytes. Missing is a state too.
snapshot() {
  for d in "$real_state" "$real_dl"; do
    if [ -e "$d" ]; then
      stat -c '%n %f %u %s %Y %Z' "$d"
      find "$d" -mindepth 1 -maxdepth 1 -exec stat -c '%n %f %u %s %Y %Z' {} + 2>/dev/null | LC_ALL=C sort
    else
      echo "$d missing"
    fi
  done
}

tree=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$tree/target/test-home"
pot=$(mktemp -d "$tree/target/test-home/run.XXXXXX")
h=$pot/home
mkdir -p "$h/.config" "$h/.cache" "$h/.local/state" "$h/.local/share" "$h/Downloads" "$pot/runtime"
chmod 700 "$pot/runtime"
printf 'XDG_DOWNLOAD_DIR="$HOME/Downloads"\n' >"$h/.config/user-dirs.dirs"
# The skeleton: what must still be all there is after the run.
skeleton=$(cd "$pot" && find . | LC_ALL=C sort)

before=$(snapshot)
status=0
HOME=$h \
  XDG_CONFIG_HOME=$h/.config \
  XDG_CACHE_HOME=$h/.cache \
  XDG_STATE_HOME=$h/.local/state \
  XDG_DATA_HOME=$h/.local/share \
  XDG_RUNTIME_DIR=$pot/runtime \
  DBUS_SESSION_BUS_ADDRESS=unix:path=$pot/runtime/no-bus \
  "$@" || status=$?
after=$(snapshot)

now=$(cd "$pot" && find . | LC_ALL=C sort)
# New paths, and skeleton paths that went away (a test removed them).
leaked=$(LC_ALL=C comm -3 <(echo "$skeleton") <(echo "$now"))
if [ -n "$leaked" ]; then
  echo "test-runner: $1 wrote to the default home (kept in $pot):" >&2
  echo "$leaked" | sed 's/^/  /' >&2
  [ "$status" -ne 0 ] || status=101
fi
if [ "$before" != "$after" ]; then
  echo "test-runner: the real state or download folder changed during $1:" >&2
  diff <(echo "$before") <(echo "$after") >&2 || true
  [ "$status" -ne 0 ] || status=101
fi
if [ -z "$leaked" ]; then
  rm -rf "$pot"
fi
exit "$status"
