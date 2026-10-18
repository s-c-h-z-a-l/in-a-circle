#!/usr/bin/env python3
"""Measure the 'ask the file manager what is selected' query over AT-SPI:
find app -> active window -> selected cells + path bar. Read-only.
Usage: atspi_query.py <app name> [runs]"""
import sys, time
import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

APP, RUNS = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 10
calls = 0

def kids(o):
    global calls
    calls += 1
    n = o.get_child_count()
    calls += n
    return [c for c in (o.get_child_at_index(i) for i in range(n)) if c]

def query():
    global calls
    calls = 0
    t0 = time.perf_counter()
    desk = Atspi.get_desktop(0)
    app = next(a for a in kids(desk) if a.get_name() == APP)
    win = next((w for w in kids(app) if w.get_state_set().contains(Atspi.StateType.ACTIVE)), None) or app.get_child_at_index(0)
    t_win = time.perf_counter()
    selected, crumbs = [], []
    stack = [win]
    while stack:
        o = stack.pop()
        calls += 2
        role = o.get_role_name()
        if role == "table cell" and o.get_state_set().contains(Atspi.StateType.SELECTED):
            selected.append(o.get_name())
            continue
        if role == "button" and o.get_parent() and o.get_parent().get_role_name() == "panel":
            pass
        stack.extend(reversed(kids(o)))
    t_end = time.perf_counter()
    return (t_win - t0) * 1e3, (t_end - t0) * 1e3, selected, calls

Atspi.init()
res = [query() for _ in range(RUNS)]
for w, total, sel, c in res[:3]:
    print(f"find window {w:6.1f} ms | full walk {total:6.1f} ms | calls ~{c} | selected={sel}")
tot = sorted(r[1] for r in res)
print(f"full query: min {tot[0]:.1f} ms, median {tot[len(tot)//2]:.1f} ms, max {tot[-1]:.1f} ms over {RUNS} runs")

# --- Collection interface: one server-side match instead of a tree walk ----
def query_collection():
    t0 = time.perf_counter()
    desk = Atspi.get_desktop(0)
    app = next(a for a in (desk.get_child_at_index(i) for i in range(desk.get_child_count())) if a and a.get_name() == APP)
    win = next((w for w in (app.get_child_at_index(i) for i in range(app.get_child_count())) if w and w.get_state_set().contains(Atspi.StateType.ACTIVE)), None) or app.get_child_at_index(0)
    t_win = time.perf_counter()
    states = Atspi.StateSet.new([Atspi.StateType.SELECTED])
    rule = Atspi.MatchRule.new(states, Atspi.CollectionMatchType.ALL, None, Atspi.CollectionMatchType.NONE,
                               [Atspi.Role.TABLE_CELL], Atspi.CollectionMatchType.ANY, None, Atspi.CollectionMatchType.NONE, False)
    col = win.get_collection_iface()
    if col is None:
        return None
    hits = col.get_matches(rule, Atspi.CollectionSortOrder.CANONICAL, 0, True)
    names = [h.get_name() for h in hits]
    t_end = time.perf_counter()
    return (t_win - t0) * 1e3, (t_end - t0) * 1e3, names

r = [query_collection() for _ in range(RUNS)]
if r[0] is None:
    print("Collection interface not available on this window")
else:
    for w, total, names in r[:3]:
        print(f"[collection] find window {w:5.1f} ms | total {total:5.1f} ms | selected={names}")
    tot = sorted(x[1] for x in r)
    print(f"[collection] min {tot[0]:.1f} ms, median {tot[len(tot)//2]:.1f} ms, max {tot[-1]:.1f} ms")

# --- Stop at the table, then ask its Selection interface --------------------
def query_table_selection():
    n = 0
    t0 = time.perf_counter()
    desk = Atspi.get_desktop(0)
    app = next(a for a in (desk.get_child_at_index(i) for i in range(desk.get_child_count())) if a and a.get_name() == APP)
    win = next((w for w in (app.get_child_at_index(i) for i in range(app.get_child_count())) if w and w.get_state_set().contains(Atspi.StateType.ACTIVE)), None) or app.get_child_at_index(0)
    stack, table = [win], None
    while stack and table is None:
        o = stack.pop()
        n += 1
        if o.get_role_name() in ("table", "tree table", "list", "layered pane") and o.get_selection_iface():
            table = o
            break
        cs = [o.get_child_at_index(i) for i in range(o.get_child_count())]
        n += len(cs)
        stack.extend(reversed([c for c in cs if c]))
    t_table = time.perf_counter()
    sel = table.get_selection_iface()
    names = [sel.get_selected_child(i).get_name() for i in range(sel.get_n_selected_children())]
    t_end = time.perf_counter()
    return (t_table - t0) * 1e3, (t_end - t0) * 1e3, names, n

r = [query_table_selection() for _ in range(RUNS)]
for t, total, names, n in r[:3]:
    print(f"[table+selection] find table {t:5.1f} ms | total {total:5.1f} ms | nodes {n} | selected={names}")
tot = sorted(x[1] for x in r)
print(f"[table+selection] min {tot[0]:.1f} ms, median {tot[len(tot)//2]:.1f} ms, max {tot[-1]:.1f} ms")
