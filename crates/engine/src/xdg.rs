//! XDG base directories, resolved from the environment on every call.
//!
//! When `$HOME` is unset or relative, the home-based defaults do not exist:
//! cache, state, data and downloads then live under
//! `/tmp/inacircle-<uid>/{cache,state,data,downloads}`, never a relative path.

use std::path::PathBuf;

/// `$HOME`, only when absolute.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn fallback_root() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/inacircle-{uid}"))
}

/// `$name` when set to an absolute path (relative values are invalid per the
/// XDG spec), else `$HOME/<under_home>`, else `<fallback_root>/<no_home>`.
fn base(name: &str, under_home: &str, no_home: &str) -> PathBuf {
    abs_env(name).unwrap_or_else(|| match home() {
        Some(h) => h.join(under_home),
        None => fallback_root().join(no_home),
    })
}

fn abs_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn cache_home() -> PathBuf {
    base("XDG_CACHE_HOME", ".cache", "cache")
}

pub fn state_home() -> PathBuf {
    base("XDG_STATE_HOME", ".local/state", "state")
}

pub fn data_home() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share", "data")
}

/// `$XDG_RUNTIME_DIR` when set to an absolute path.
pub fn runtime_dir() -> Option<PathBuf> {
    abs_env("XDG_RUNTIME_DIR")
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads` (or the
/// no-home fallback).
pub fn download_dir() -> PathBuf {
    let fallback = || match home() {
        Some(h) => h.join("Downloads"),
        None => fallback_root().join("downloads"),
    };
    let config = base("XDG_CONFIG_HOME", ".config", "config");
    let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) else {
        return fallback();
    };
    for line in text.lines() {
        let Some(v) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let v = v.trim().trim_matches('"');
        // `$HOME` expands only as a whole path component.
        let path = match (v.strip_prefix("$HOME"), home()) {
            (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => {
                PathBuf::from(format!("{}{rest}", h.display()))
            }
            (Some(_), _) => return fallback(),
            (None, _) => PathBuf::from(v),
        };
        return if path.is_absolute() { path } else { fallback() };
    }
    fallback()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that touch the environment take this lock and restore it.
    static ENV: Mutex<()> = Mutex::new(());

    pub(crate) fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: std serializes set_var/remove_var against getenv, and every
        // test that mutates or depends on the environment holds ENV, so no
        // code observes a half-changed environment.
        unsafe {
            for (k, v) in vars {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // SAFETY: as above (ENV is still held).
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        match r {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    #[test]
    fn xdg_defaults_and_overrides() {
        let all = [
            "HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
        ];
        let unset = |home: Option<&'static str>| -> Vec<(&'static str, Option<&'static str>)> {
            all.iter()
                .map(|k| (*k, if *k == "HOME" { home } else { None }))
                .collect()
        };
        with_env(&unset(Some("/h")), || {
            assert_eq!(cache_home(), PathBuf::from("/h/.cache"));
            assert_eq!(state_home(), PathBuf::from("/h/.local/state"));
            assert_eq!(data_home(), PathBuf::from("/h/.local/share"));
            assert_eq!(runtime_dir(), None);
        });
        let mut v = unset(Some("/h"));
        v[1].1 = Some("/c");
        v[2].1 = Some("/s");
        v[3].1 = Some("/d");
        v[4].1 = Some("/run/u");
        with_env(&v, || {
            assert_eq!(cache_home(), PathBuf::from("/c"));
            assert_eq!(state_home(), PathBuf::from("/s"));
            assert_eq!(data_home(), PathBuf::from("/d"));
            assert_eq!(runtime_dir(), Some(PathBuf::from("/run/u")));
        });
        // Relative values are ignored, as the XDG spec requires.
        let mut v = unset(Some("/h"));
        v[1].1 = Some("rel");
        v[4].1 = Some("rel");
        with_env(&v, || {
            assert_eq!(cache_home(), PathBuf::from("/h/.cache"));
            assert_eq!(runtime_dir(), None);
        });
    }

    #[test]
    fn download_dir_from_user_dirs() {
        let tmp = std::env::temp_dir().join(format!("engine-xdg-{}", std::process::id()));
        let cfg = tmp.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let env = |cfg: &std::path::Path| {
            vec![
                ("HOME", Some(tmp.to_str().unwrap().to_owned())),
                ("XDG_CONFIG_HOME", Some(cfg.to_str().unwrap().to_owned())),
            ]
        };
        let run = |content: Option<&str>| {
            match content {
                Some(c) => std::fs::write(cfg.join("user-dirs.dirs"), c).unwrap(),
                None => {
                    let _ = std::fs::remove_file(cfg.join("user-dirs.dirs"));
                }
            }
            let e = env(&cfg);
            let e: Vec<(&str, Option<&str>)> = e.iter().map(|(k, v)| (*k, v.as_deref())).collect();
            with_env(&e, download_dir)
        };
        assert_eq!(
            run(Some(
                "# c\nXDG_DESKTOP_DIR=\"$HOME/Desk\"\nXDG_DOWNLOAD_DIR=\"$HOME/Letöltések\"\n"
            )),
            tmp.join("Letöltések")
        );
        assert_eq!(
            run(Some("XDG_DOWNLOAD_DIR=\"/data/dl\"\n")),
            PathBuf::from("/data/dl")
        );
        assert_eq!(
            run(Some("XDG_DOWNLOAD_DIR=\"relative\"\n")),
            tmp.join("Downloads")
        );
        assert_eq!(
            run(Some("XDG_MUSIC_DIR=\"$HOME/m\"\n")),
            tmp.join("Downloads")
        );
        assert_eq!(run(None), tmp.join("Downloads"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_home_falls_back_to_tmp_never_relative() {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let root = PathBuf::from(format!("/tmp/inacircle-{uid}"));
        for home in [None, Some("rel/home")] {
            let v = [
                ("HOME", home),
                ("XDG_CACHE_HOME", None),
                ("XDG_STATE_HOME", None),
                ("XDG_DATA_HOME", None),
                ("XDG_CONFIG_HOME", None),
            ];
            with_env(&v, || {
                assert_eq!(cache_home(), root.join("cache"));
                assert_eq!(state_home(), root.join("state"));
                assert_eq!(data_home(), root.join("data"));
                assert_eq!(download_dir(), root.join("downloads"));
            });
        }
    }

    #[test]
    fn home_expands_only_as_a_component() {
        let tmp = std::env::temp_dir().join(format!("engine-xdg-h-{}", std::process::id()));
        let cfg = tmp.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let (h, c) = (tmp.to_str().unwrap(), cfg.to_str().unwrap());
        let v = [("HOME", Some(h)), ("XDG_CONFIG_HOME", Some(c))];
        let f = cfg.join("user-dirs.dirs");
        std::fs::write(&f, "XDG_DOWNLOAD_DIR=\"$HOMEX/dl\"\n").unwrap();
        assert_eq!(with_env(&v, download_dir), tmp.join("Downloads"));
        std::fs::write(&f, "XDG_DOWNLOAD_DIR=\"$HOME\"\n").unwrap();
        assert_eq!(with_env(&v, download_dir), tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
