//! The one way these tests start `inacircle`: every user path it could
//! resolve (state with the history, config with `user-dirs.dirs`, cache,
//! Trash, runtime, and the download folder a read-only source falls back
//! to through `HOME`) lies under the test's own `root`, and the session
//! bus is not the developer's. A caller sets what it needs after this
//! (a later `env` wins), never instead of it.

use std::path::Path;
use std::process::Command;

pub fn isolate<'a>(c: &'a mut Command, root: &Path) -> &'a mut Command {
    c.env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("home/.local/share"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
}
