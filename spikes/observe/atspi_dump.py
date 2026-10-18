#!/usr/bin/env python3
"""Dump an app's accessibility tree (role, name, description, attributes,
text, states) and flag anything that looks like a filesystem path.
Read-only. Usage: atspi_dump.py <app name> [max depth]"""
import sys
import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

APP = sys.argv[1]
MAXD = int(sys.argv[2]) if len(sys.argv) > 2 else 40
desk = Atspi.get_desktop(0)
apps = [desk.get_child_at_index(i) for i in range(desk.get_child_count())]
app = next((a for a in apps if a and a.get_name() == APP), None)
if not app:
    sys.exit(f"app {APP!r} not on the a11y bus; have: {[a.get_name() for a in apps if a]}")

def info(o):
    role = o.get_role_name()
    name = o.get_name() or ""
    desc = o.get_description() or ""
    try:
        attrs = o.get_attributes() or {}
    except Exception:
        attrs = {}
    text = ""
    try:
        t = o.get_text_iface()
        if t:
            text = t.get_text(0, min(t.get_character_count(), 200))
    except Exception:
        pass
    st = o.get_state_set()
    flags = [s for s in ("selected", "focused", "active", "showing") if st.contains(getattr(Atspi.StateType, s.upper()))]
    return role, name, desc, attrs, text, flags

def walk(o, d=0):
    if d > MAXD:
        return
    role, name, desc, attrs, text, flags = info(o)
    blob = " ".join([name, desc, text] + [f"{k}={v}" for k, v in attrs.items()])
    mark = "  <== PATH?" if ("/" in blob or "file:" in blob or "~" in blob) else ""
    parts = [f"{'  ' * d}{role}", f"name={name!r}" if name else "", f"desc={desc!r}" if desc else "",
             f"text={text!r}" if text and text != name else "", f"attrs={attrs}" if attrs else "",
             f"[{','.join(flags)}]" if flags else ""]
    print(" ".join(p for p in parts if p) + mark)
    for i in range(o.get_child_count()):
        c = o.get_child_at_index(i)
        if c:
            walk(c, d + 1)

for i in range(app.get_child_count()):
    w = app.get_child_at_index(i)
    if w and w.get_state_set().contains(Atspi.StateType.ACTIVE):
        walk(w)
        break
else:
    print("no active window; dumping first one")
    walk(app.get_child_at_index(0))
