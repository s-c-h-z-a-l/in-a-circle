#!/usr/bin/env python3
"""Passive observer: what events does a file manager produce when you click,
drag or right-click a file? Watches, side by side, with one clock:

  - AT-SPI (accessibility bus): focus, selection, state changes
  - Wayland clipboard and primary selection (wl-paste --watch, data-control)
  - D-Bus session bus (signals and method calls)
  - inotify on a folder (opens/reads of the files)

Nothing is modified. Usage: observe.py <seconds> <folder> <logfile>
"""
import ctypes, os, struct, subprocess, sys, threading, time

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib

SECS, FOLDER, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3]
T0 = time.monotonic()
lock = threading.Lock()
children = []  # subprocesses, terminated on exit
out = open(LOG, "w", buffering=1)

def log(chan, msg):
    with lock:
        out.write(f"{(time.monotonic() - T0) * 1000:9.1f} ms  {chan:<7} {msg}\n")

# --- AT-SPI -----------------------------------------------------------------
def on_atspi(ev):
    try:
        src = ev.source
        app = src.get_application().get_name() if src else "?"
        name = src.get_name() if src else ""
        role = src.get_role_name() if src else ""
    except Exception as e:  # objects can vanish mid-event
        app, name, role = "?", f"<{e}>", ""
    log("atspi", f"[{app}] {ev.type} d1={ev.detail1} role={role!r} name={name!r}")

listener = Atspi.EventListener.new(on_atspi)
for t in ("object:state-changed:selected", "object:state-changed:focused",
          "object:selection-changed", "object:active-descendant-changed",
          "object:children-changed", "window:activate", "window:create",
          "focus:"):
    listener.register(t)

# --- Wayland clipboard / primary --------------------------------------------
def watch_selection(primary):
    chan = "primary" if primary else "clip"
    args = ["wl-paste", "--watch", "sh", "-c", "wl-paste %s --list-types | tr '\\n' ' '; echo" % ("--primary" if primary else "")]
    if primary:
        args.insert(1, "--primary")
    p = subprocess.Popen(args, stdout=subprocess.PIPE, text=True)
    children.append(p)
    for line in p.stdout:
        log(chan, "changed, types: " + line.strip())

# --- D-Bus ------------------------------------------------------------------
def watch_dbus():
    p = subprocess.Popen(["dbus-monitor", "--session"], stdout=subprocess.PIPE, text=True)
    children.append(p)
    for line in p.stdout:
        if line.startswith(("signal", "method call")) and "org.a11y" not in line and "org.freedesktop.DBus" not in line:
            log("dbus", line.strip()[:220])

# --- inotify ----------------------------------------------------------------
def watch_inotify():
    libc = ctypes.CDLL(None, use_errno=True)
    fd = libc.inotify_init1(0)
    mask = 0x1 | 0x2 | 0x8 | 0x10 | 0x20 | 0x100 | 0x200  # access modify close_w close_nw open create delete
    libc.inotify_add_watch(fd, FOLDER.encode(), mask)
    names = {0x1: "ACCESS", 0x2: "MODIFY", 0x8: "CLOSE_WRITE", 0x10: "CLOSE_NOWRITE", 0x20: "OPEN", 0x100: "CREATE", 0x200: "DELETE"}
    while True:
        buf = os.read(fd, 4096)
        i = 0
        while i < len(buf):
            _, m, _, ln = struct.unpack_from("iIII", buf, i)
            name = buf[i + 16:i + 16 + ln].rstrip(b"\0").decode(errors="replace")
            ev = "|".join(v for k, v in names.items() if m & k)
            log("inotify", f"{ev} {name or '(folder)'}")
            i += 16 + ln

for f in (lambda: watch_selection(False), lambda: watch_selection(True), watch_dbus, watch_inotify):
    threading.Thread(target=f, daemon=True).start()

log("info", f"observing for {SECS}s; folder={FOLDER}")
GLib.timeout_add(SECS * 1000, lambda: Atspi.event_quit() or False)
Atspi.event_main()
log("info", "done")
for c in children:
    c.terminate()
out.close()
os._exit(0)
