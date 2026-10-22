//! XDG base directories, resolved from the environment on every call.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `$name` when set to an absolute path (relative values are invalid per the
/// XDG spec), else `$HOME/<fallback>`.
fn base(name: &str, fallback: &str) -> PathBuf {
    abs_env(name).unwrap_or_else(|| home().join(fallback))
}

fn abs_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn cache_home() -> PathBuf {
    base("XDG_CACHE_HOME", ".cache")
}

pub fn state_home() -> PathBuf {
    base("XDG_STATE_HOME", ".local/state")
}

pub fn data_home() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_RUNTIME_DIR` when set to an absolute path.
pub fn runtime_dir() -> Option<PathBuf> {
    abs_env("XDG_RUNTIME_DIR")
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads`.
pub fn download_dir() -> PathBuf {
    let fallback = || home().join("Downloads");
    let config = base("XDG_CONFIG_HOME", ".config");
    let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) else {
        return fallback();
    };
    for line in text.lines() {
        let Some(v) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let v = v.trim().trim_matches('"');
        let path = match v.strip_prefix("$HOME") {
            Some(rest) => PathBuf::from(format!("{}{rest}", home().display())),
            None => PathBuf::from(v),
        };
        return if path.is_absolute() { path } else { fallback() };
    }
    fallback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that touch the environment take this lock and restore it.
    static ENV: Mutex<()> = Mutex::new(());

    fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: every test that mutates the environment holds ENV, and no
        // other thread in this test binary reads it concurrently.
        unsafe {
            for (k, v) in vars {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // SAFETY: as above.
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
}
