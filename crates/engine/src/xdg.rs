//! XDG base directories, resolved from the environment on every call.
//!
//! When `$HOME` is unset or relative, the home-based defaults do not exist:
//! cache, state, data and downloads then live under
//! `/tmp/inacircle-<uid>/{cache,state,data,downloads}`, never a relative path.
//! That folder is created mode 0700 and used only if it is a real directory
//! (not a symlink) owned by us with no group or other permissions; anyone
//! can create it first, so otherwise the lookup fails.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;

/// `$HOME`, only when absolute.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn uid() -> libc::uid_t {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn fallback_root() -> io::Result<PathBuf> {
    private_dir(PathBuf::from(format!("/tmp/inacircle-{}", uid())))
}

/// `dir`, created mode 0700 if missing; refused unless it is a directory
/// (not a symlink) owned by this user with no group or other permissions.
fn private_dir(dir: PathBuf) -> io::Result<PathBuf> {
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let m = std::fs::symlink_metadata(&dir)?;
    let why = if !m.file_type().is_dir() {
        "not a directory"
    } else if m.uid() != uid() {
        "owned by another user"
    } else if m.mode() & 0o077 != 0 {
        "accessible to other users"
    } else {
        return Ok(dir);
    };
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{}: {why}", dir.display()),
    ))
}

/// `$name` when set to an absolute path (relative values are invalid per the
/// XDG spec), else `$HOME/<under_home>`, else `<fallback_root>/<no_home>`.
fn base(name: &str, under_home: &str, no_home: &str) -> io::Result<PathBuf> {
    match (abs_env(name), home()) {
        (Some(p), _) => Ok(p),
        (None, Some(h)) => Ok(h.join(under_home)),
        (None, None) => Ok(fallback_root()?.join(no_home)),
    }
}

fn abs_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn cache_home() -> io::Result<PathBuf> {
    base("XDG_CACHE_HOME", ".cache", "cache")
}

pub fn state_home() -> io::Result<PathBuf> {
    base("XDG_STATE_HOME", ".local/state", "state")
}

pub fn data_home() -> io::Result<PathBuf> {
    base("XDG_DATA_HOME", ".local/share", "data")
}

/// `$XDG_RUNTIME_DIR` when set to an absolute path.
pub fn runtime_dir() -> Option<PathBuf> {
    abs_env("XDG_RUNTIME_DIR")
}

/// The longest `user-dirs.dirs` read (G0-P row `user_dirs_bytes`, D-34): xdg-user-dirs
/// writes about eight short lines (under 0.5 KiB); eight keys with a 4096 B
/// path each plus quotes stay under 34 KiB, so 64 KiB is about twice the
/// worst real shape. A longer file is no user-dirs file we know: the
/// fallback (`~/Downloads`) is used, as for an unreadable one.
const USER_DIRS_MAX_BYTES: usize = 64 * 1024;

/// The file's text, opened without blocking, only a regular file (checked
/// on the open fd), read at most `USER_DIRS_MAX_BYTES`. A symlink at its
/// name is followed: unlike the engine's own state files, this is the
/// user's config, which dotfile managers link (D-34). `None` for anything
/// else (the caller falls back).
fn read_user_dirs(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    String::from_utf8(action::read_bounded(f, USER_DIRS_MAX_BYTES).ok()?).ok()
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads` (or the
/// no-home fallback).
pub fn download_dir() -> io::Result<PathBuf> {
    let fallback = || match home() {
        Some(h) => Ok(h.join("Downloads")),
        None => Ok(fallback_root()?.join("downloads")),
    };
    let config = base("XDG_CONFIG_HOME", ".config", "config")?;
    let Some(text) = read_user_dirs(&config.join("user-dirs.dirs")) else {
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
        return if path.is_absolute() {
            Ok(path)
        } else {
            fallback()
        };
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
            assert_eq!(cache_home().unwrap(), PathBuf::from("/h/.cache"));
            assert_eq!(state_home().unwrap(), PathBuf::from("/h/.local/state"));
            assert_eq!(data_home().unwrap(), PathBuf::from("/h/.local/share"));
            assert_eq!(runtime_dir(), None);
        });
        let mut v = unset(Some("/h"));
        v[1].1 = Some("/c");
        v[2].1 = Some("/s");
        v[3].1 = Some("/d");
        v[4].1 = Some("/run/u");
        with_env(&v, || {
            assert_eq!(cache_home().unwrap(), PathBuf::from("/c"));
            assert_eq!(state_home().unwrap(), PathBuf::from("/s"));
            assert_eq!(data_home().unwrap(), PathBuf::from("/d"));
            assert_eq!(runtime_dir(), Some(PathBuf::from("/run/u")));
        });
        // Relative values are ignored, as the XDG spec requires.
        let mut v = unset(Some("/h"));
        v[1].1 = Some("rel");
        v[4].1 = Some("rel");
        with_env(&v, || {
            assert_eq!(cache_home().unwrap(), PathBuf::from("/h/.cache"));
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
            with_env(&e, download_dir).unwrap()
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

    /// `user-dirs.dirs` is read bounded; a symlink at its own name is followed
    /// to a regular file (D-34); a FIFO or directory, at the name or behind
    /// a link, falls back to `~/Downloads`.
    #[test]
    fn user_dirs_read_is_bounded_typed_and_followed() {
        use crate::sys::tests::{mkfifo, within};
        let tmp = std::env::temp_dir().join(format!("engine-xdg-ud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cfg = tmp.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let f = cfg.join("user-dirs.dirs");
        let want = "XDG_DOWNLOAD_DIR=\"/data/dl\"\n";
        let pad = |n: usize| {
            let mut c = String::from(want);
            c.push('#');
            while c.len() < n {
                c.push('x');
            }
            c
        };
        let (home, cfgs) = (
            tmp.to_str().unwrap().to_owned(),
            cfg.to_str().unwrap().to_owned(),
        );
        let get = || {
            let (h, c) = (home.clone(), cfgs.clone());
            with_env(
                &[
                    ("HOME", Some(h.as_str())),
                    ("XDG_CONFIG_HOME", Some(c.as_str())),
                ],
                || within(5, download_dir).unwrap(),
            )
        };
        // Max is read, max + 1 falls back.
        std::fs::write(&f, pad(USER_DIRS_MAX_BYTES)).unwrap();
        assert_eq!(get(), PathBuf::from("/data/dl"));
        std::fs::write(&f, pad(USER_DIRS_MAX_BYTES + 1)).unwrap();
        assert_eq!(get(), tmp.join("Downloads"));
        // A symlink at the name is followed (a dotfile manager's link to
        // the user's own config, D-34).
        let real = tmp.join("real");
        std::fs::write(&real, want).unwrap();
        std::fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink(&real, &f).unwrap();
        assert_eq!(get(), PathBuf::from("/data/dl"));
        // ... but only to a regular file: a link to a FIFO neither blocks
        // nor is read, nor is a link to a directory.
        let fifo = tmp.join("fifo");
        mkfifo(&fifo);
        std::fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink(&fifo, &f).unwrap();
        assert_eq!(get(), tmp.join("Downloads"));
        std::fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink(&cfg, &f).unwrap();
        assert_eq!(get(), tmp.join("Downloads"));
        // A FIFO at the name itself neither blocks nor is read.
        std::fs::remove_file(&f).unwrap();
        mkfifo(&f);
        assert_eq!(get(), tmp.join("Downloads"));
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
                assert_eq!(cache_home().unwrap(), root.join("cache"));
                assert_eq!(state_home().unwrap(), root.join("state"));
                assert_eq!(data_home().unwrap(), root.join("data"));
                assert_eq!(download_dir().unwrap(), root.join("downloads"));
            });
        }
    }

    #[test]
    fn private_dir_is_created_0700_and_refused_unless_ours_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("engine-xdg-p-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let fresh = tmp.join("fresh");
        assert_eq!(private_dir(fresh.clone()).unwrap(), fresh);
        let m = std::fs::symlink_metadata(&fresh).unwrap();
        assert!(m.is_dir());
        assert_eq!(m.mode() & 0o777, 0o700);
        // Existing and private: used as is.
        assert_eq!(private_dir(fresh.clone()).unwrap(), fresh);
        let refused = |p: &std::path::Path| {
            let e = private_dir(p.to_path_buf()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        };
        let open = tmp.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        refused(&open);
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o710)).unwrap();
        refused(&open);
        // A symlink, even to a private dir of ours, is not followed.
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        refused(&link);
        let file = tmp.join("file");
        std::fs::write(&file, "").unwrap();
        refused(&file);
        let _ = std::fs::remove_dir_all(&tmp);
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
        assert_eq!(with_env(&v, download_dir).unwrap(), tmp.join("Downloads"));
        std::fs::write(&f, "XDG_DOWNLOAD_DIR=\"$HOME\"\n").unwrap();
        assert_eq!(with_env(&v, download_dir).unwrap(), tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
