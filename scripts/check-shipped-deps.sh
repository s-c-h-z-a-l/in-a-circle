#!/usr/bin/env bash
# Fail if a shipped binary's normal dependency tree contains anything
# other than our own crates and libc.
set -euo pipefail
cd "$(dirname "$0")/.."

SHIPPED="inacircle-text"
bad=0
for pkg in $SHIPPED; do
    names=$(cargo tree -e normal -p "$pkg" --prefix none | awk '{print $1}' | sort -u)
    for name in $names; do
        case "$name" in
            inacircle-*|manifest|doc|action|libc) ;;
            *) echo "$pkg: disallowed dependency: $name"; bad=1 ;;
        esac
    done
done
exit $bad
