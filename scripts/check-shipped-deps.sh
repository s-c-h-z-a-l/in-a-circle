#!/usr/bin/env bash
# Fail if a shipped binary's normal dependency tree contains anything
# other than our own crates and libc.
set -euo pipefail
cd "$(dirname "$0")/.."

SHIPPED="inacircle inacircle-text"
# Libraries the shipped binaries link: libc and our own crates only.
LIBS="wire ui"
bad=0
for pkg in $SHIPPED; do
    names=$(cargo tree -e normal -p "$pkg" --prefix none | awk '{print $1}' | sort -u)
    for name in $names; do
        case "$pkg:$name" in
            # The engine binary must stay free of the document model.
            inacircle:doc) echo "$pkg: disallowed dependency: $name"; bad=1 ;;
            *:inacircle|*:inacircle-*|*:manifest|*:doc|*:action|*:engine|*:libc) ;;
            # Notifications: our own libc-only D-Bus client.
            inacircle:dbus) ;;
            # Our own libc-only Wayland client.
            inacircle:wire) ;;
            # Wheel rasterizer and the baked glyph atlas: no dependencies.
            inacircle:ui) ;;
            *) echo "$pkg: disallowed dependency: $name"; bad=1 ;;
        esac
    done
done
for pkg in $LIBS; do
    names=$(cargo tree -e normal -p "$pkg" --prefix none | awk '{print $1}' | sort -u)
    for name in $names; do
        case "$name" in
            "$pkg"|libc) ;;
            *) echo "$pkg: disallowed dependency: $name"; bad=1 ;;
        esac
    done
done
exit $bad
