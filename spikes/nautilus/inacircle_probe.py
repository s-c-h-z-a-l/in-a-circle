# In.A.Circle experiment: what does Nautilus tell an extension, and when?
# Logs to $XDG_RUNTIME_DIR/inacircle-nautilus.log. Remove after the test.
import os, time
from gi.repository import GObject, Nautilus, Gtk, GLib

LOG = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "inacircle-nautilus.log")
T0 = time.monotonic()

def log(msg):
    with open(LOG, "a") as f:
        f.write(f"[{(time.monotonic() - T0) * 1000:10.1f} ms] {msg}\n")

def uris(files):
    return [f.get_uri() for f in files]

hooked = set()

def describe(item):
    # NautilusViewItem is internal; try its 'file' property (a Nautilus.FileInfo).
    for prop in ("file",):
        try:
            f = item.get_property(prop)
            if f is not None and hasattr(f, "get_uri"):
                return f.get_uri()
        except Exception:
            pass
    return repr(item)

def hook_models():
    # Walk Nautilus' GTK widgets looking for list/grid views with a selection model.
    for win in Gtk.Window.list_toplevels():
        stack = [win]
        while stack:
            w = stack.pop()
            if isinstance(w, (Gtk.GridView, Gtk.ColumnView, Gtk.ListView)):
                model = w.get_model()
                if model is not None and id(model) not in hooked:
                    hooked.add(id(model))
                    log(f"HACK: hooked {type(w).__name__} model {type(model).__name__}")
                    def on_sel(m, pos, n, m_ref=model):
                        bits = m_ref.get_selection()
                        sel = [describe(m_ref.get_item(bits.get_nth(i))) for i in range(bits.get_size())]
                        log(f"HACK selection-changed: {sel}")
                    model.connect("selection-changed", on_sel)
            child = w.get_first_child()
            while child is not None:
                stack.append(child)
                child = child.get_next_sibling()
    return True  # keep polling for new windows/views

class Probe(GObject.GObject, Nautilus.MenuProvider, Nautilus.InfoProvider):
    def __init__(self):
        super().__init__()
        log("extension loaded")
        GLib.timeout_add(1000, hook_models)

    def get_file_items(self, files):
        log(f"API MenuProvider.get_file_items: {uris(files)}")
        return []

    def get_background_items(self, folder):
        log(f"API MenuProvider.get_background_items: {folder.get_uri()}")
        return []

    def update_file_info(self, file):
        log(f"API InfoProvider.update_file_info: {file.get_uri()}")
