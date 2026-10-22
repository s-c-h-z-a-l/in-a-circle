//! Output policy: where results go and how they are moved into place
//! without ever overwriting anything.

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use crate::xdg;

const WORK_PREFIX: &str = ".inacircle-";
const MAX_CONFLICT: u32 = 999;
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// A hidden 0700 scratch directory inside the target folder, so the final
/// move is an atomic rename. Removed recursively on drop.
pub struct WorkDir {
    pub path: PathBuf,
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        remove_tree(&self.path);
    }
}

/// Removes `path` and everything under it, including read-only files and
/// directories. Never follows symlinks, never panics; errors are ignored.
fn remove_tree(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        // A directory without owner rwx cannot be listed or emptied.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(path) {
            for e in entries.flatten() {
                remove_tree(&e.path());
            }
        }
        let _ = fs::remove_dir(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn random_u64() -> io::Result<u64> {
    let mut buf = [0u8; 8];
    loop {
        // SAFETY: buf is a valid, writable 8-byte buffer and we pass its
        // exact length; flags 0 is valid.
        let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n == buf.len() as isize {
            return Ok(u64::from_ne_bytes(buf));
        }
        let err = io::Error::last_os_error();
        if n < 0 && err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if n < 0 {
            err
        } else {
            io::Error::new(io::ErrorKind::UnexpectedEof, "short getrandom")
        });
    }
}

fn mkdir_mode(path: &Path, mode: libc::mode_t) -> io::Result<()> {
    let c = cstr(path)?;
    // SAFETY: c is a valid NUL-terminated path that outlives the call.
    if unsafe { libc::mkdir(c.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Creates `<target>/.inacircle-<16 hex>` with mode 0700.
pub fn make_work_dir(target_dir: &Path) -> io::Result<WorkDir> {
    for _ in 0..16 {
        let path = target_dir.join(format!("{WORK_PREFIX}{:016x}", random_u64()?));
        match mkdir_mode(&path, 0o700) {
            Ok(()) => return Ok(WorkDir { path }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free work directory name",
    ))
}

/// Removes our own `.inacircle-*` directories older than a day (left by a
/// crash). Anything else in the folder is untouched.
pub fn clean_stale(target_dir: &Path) {
    let Ok(entries) = fs::read_dir(target_dir) else {
        return;
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let now = SystemTime::now();
    for e in entries.flatten() {
        if !e.file_name().as_bytes().starts_with(WORK_PREFIX.as_bytes()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(e.path()) else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if meta.is_dir() && meta.uid() == uid && old {
            remove_tree(&e.path());
        }
    }
}

/// The source's folder if a test directory can really be created there
/// (access(2) alone lies on read-only mounts and ACLs), else the download
/// folder.
pub fn writable_target(source_dir: &Path) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let probe = source_dir.join(format!(
        "{WORK_PREFIX}probe-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let ok = match mkdir_mode(&probe, 0o700) {
        Ok(()) => {
            let _ = fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    };
    if ok {
        source_dir.to_path_buf()
    } else {
        xdg::download_dir()
    }
}

/// `no-replace` move of `from` to `to`. `AlreadyExists` when `to` is taken.
fn move_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    // SAFETY: f and t are valid NUL-terminated paths that outlive the call;
    // AT_FDCWD makes both relative to the cwd (they are normally absolute).
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Filesystem or kernel without RENAME_NOREPLACE: link fails with
        // EEXIST rather than replacing, then drop the old name.
        Some(libc::EINVAL) | Some(libc::ENOSYS) => {
            // SAFETY: as above.
            if unsafe { libc::link(f.as_ptr(), t.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            fs::remove_file(from)
        }
        _ => Err(err),
    }
}

fn candidate(dir: &Path, stem: &str, suffix: &str, ext: &str, n: u32) -> PathBuf {
    let mut name = OsString::from(stem);
    name.push(suffix);
    if n > 0 {
        name.push(format!(" ({n})"));
    }
    if !ext.is_empty() {
        name.push(".");
        name.push(ext);
    }
    dir.join(name)
}

/// Moves `produced` to `dir/<stem><suffix>.<ext>`; a taken name becomes
/// `name (1).ext` ... `name (999).ext`. Each candidate is tried with an
/// atomic no-replace move, so nothing is ever overwritten, even under a race.
pub fn place(
    produced: &Path,
    dir: &Path,
    stem: &str,
    ext: &str,
    suffix: Option<&str>,
) -> io::Result<PathBuf> {
    let suffix = suffix.unwrap_or("");
    for n in 0..=MAX_CONFLICT {
        let target = candidate(dir, stem, suffix, ext, n);
        match move_noreplace(produced, &target) {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free output name",
    ))
}

fn split_name(name: &OsStr) -> (String, String) {
    let name = name.to_string_lossy();
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i + 1..].to_string()),
        _ => (name.into_owned(), String::new()),
    }
}

/// Several outputs from one input: moves them into a fresh `dir/<stem>/`
/// (a taken folder name becomes `<stem> (1)/` ...), files keep their names.
pub fn place_many(produced: &[PathBuf], dir: &Path, stem: &str) -> io::Result<Vec<PathBuf>> {
    let mut folder = None;
    for n in 0..=MAX_CONFLICT {
        let cand = candidate(dir, stem, "", "", n);
        match mkdir_mode(&cand, 0o777 & !current_umask()) {
            Ok(()) => {
                folder = Some(cand);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let folder = folder.ok_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "no free output folder name")
    })?;
    let mut placed = Vec::with_capacity(produced.len());
    for p in produced {
        let name = p
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output has no name"))?;
        let (s, e) = split_name(name);
        placed.push(place(p, &folder, &s, &e, None)?);
    }
    Ok(placed)
}

/// The process umask, read without changing it for longer than the call.
fn current_umask() -> libc::mode_t {
    // SAFETY: umask cannot fail; the original mask is restored immediately.
    unsafe {
        let m = libc::umask(0o077);
        libc::umask(m);
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xdg::tests::with_env;
    use std::os::unix::fs::MetadataExt;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "inacircle-out-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            remove_tree(&self.0);
        }
    }

    fn file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn places_next_to_source() {
        let t = Tmp::new("next");
        let src = file(&t.0, "tmp.bin", "x");
        let out = place(&src, &t.0, "a", "md", None).unwrap();
        assert_eq!(out, t.0.join("a.md"));
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(out).unwrap(), "x");
    }

    #[test]
    fn conflict_numbering() {
        let t = Tmp::new("num");
        file(&t.0, "a.md", "orig");
        let s1 = file(&t.0, "p1", "one");
        let s2 = file(&t.0, "p2", "two");
        assert_eq!(
            place(&s1, &t.0, "a", "md", None).unwrap(),
            t.0.join("a (1).md")
        );
        assert_eq!(
            place(&s2, &t.0, "a", "md", None).unwrap(),
            t.0.join("a (2).md")
        );
        assert_eq!(fs::read_to_string(t.0.join("a.md")).unwrap(), "orig");
        assert_eq!(fs::read_to_string(t.0.join("a (1).md")).unwrap(), "one");
    }

    #[test]
    fn fails_after_999() {
        let t = Tmp::new("max");
        file(&t.0, "a.md", "o");
        for n in 1..=999 {
            file(&t.0, &format!("a ({n}).md"), "o");
        }
        let s = file(&t.0, "p", "new");
        assert!(place(&s, &t.0, "a", "md", None).is_err());
        assert!(s.exists());
        assert_eq!(fs::read_to_string(t.0.join("a (999).md")).unwrap(), "o");
    }

    #[test]
    fn never_overwrites_under_race() {
        for round in 0..50 {
            let t = Tmp::new("race");
            let src = file(&t.0, "p", "mine");
            let dir = t.0.clone();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (s2, d2) = (stop.clone(), dir.clone());
            // Creates the first names as fast as it can, racing the move.
            let h = std::thread::spawn(move || {
                let mut n = 0;
                while !s2.load(Ordering::Relaxed) && n < 3 {
                    let name = if n == 0 {
                        "a.md".into()
                    } else {
                        format!("a ({n}).md")
                    };
                    let _ = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(d2.join(name))
                        .map(|mut f| std::io::Write::write_all(&mut f, b"theirs"));
                    n += 1;
                }
            });
            let out = place(&src, &dir, "a", "md", None).unwrap();
            stop.store(true, Ordering::Relaxed);
            h.join().unwrap();
            assert_eq!(fs::read_to_string(&out).unwrap(), "mine", "round {round}");
            for n in ["a.md", "a (1).md", "a (2).md"] {
                let p = dir.join(n);
                if p.exists() && p != out {
                    assert_eq!(fs::read_to_string(p).unwrap(), "theirs");
                }
            }
        }
    }

    #[test]
    fn taken_name_in_the_middle_of_a_move_is_skipped() {
        // Deterministic version of the race: the free-name "check" a naive
        // implementation would do is stale; the move itself must refuse.
        let t = Tmp::new("stale");
        let src = file(&t.0, "p", "mine");
        let target = t.0.join("a.md");
        file(&t.0, "a.md", "theirs");
        let e = move_noreplace(&src, &target).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(target).unwrap(), "theirs");
        assert!(src.exists());
    }

    #[test]
    fn link_fallback_does_not_overwrite() {
        let t = Tmp::new("link");
        let src = file(&t.0, "p", "mine");
        let taken = file(&t.0, "a.md", "theirs");
        let (f, to) = (cstr(&src).unwrap(), cstr(&taken).unwrap());
        // SAFETY: valid NUL-terminated paths outliving the call.
        let r = unsafe { libc::link(f.as_ptr(), to.as_ptr()) };
        assert_eq!(r, -1);
        assert_eq!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(taken).unwrap(), "theirs");
    }

    #[test]
    fn suffix_used() {
        let t = Tmp::new("suffix");
        let s1 = file(&t.0, "p1", "1");
        let s2 = file(&t.0, "p2", "2");
        assert_eq!(
            place(&s1, &t.0, "pic", "png", Some("-small")).unwrap(),
            t.0.join("pic-small.png")
        );
        assert_eq!(
            place(&s2, &t.0, "pic", "png", Some("-small")).unwrap(),
            t.0.join("pic-small (1).png")
        );
    }

    #[test]
    fn many_outputs_into_folder_with_conflict() {
        let t = Tmp::new("many");
        fs::create_dir(t.0.join("doc")).unwrap();
        file(&t.0.join("doc"), "keep.txt", "k");
        let a = file(&t.0, "doc-01.png", "1");
        let b = file(&t.0, "doc-02.png", "2");
        let out = place_many(&[a, b], &t.0, "doc").unwrap();
        assert_eq!(
            out,
            vec![
                t.0.join("doc (1)/doc-01.png"),
                t.0.join("doc (1)/doc-02.png")
            ]
        );
        assert_eq!(fs::read_to_string(t.0.join("doc/keep.txt")).unwrap(), "k");
        assert_eq!(fs::read_to_string(&out[1]).unwrap(), "2");
    }

    #[test]
    fn readonly_dir_falls_back_to_downloads() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let t = Tmp::new("ro");
        let ro = t.0.join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let home = t.0.join("home");
        with_env(
            &[
                ("HOME", Some(home.to_str().unwrap())),
                ("XDG_CONFIG_HOME", None),
            ],
            || {
                assert_eq!(writable_target(&ro), home.join("Downloads"));
                assert_eq!(writable_target(&t.0), t.0);
            },
        );
        assert_eq!(fs::read_dir(&t.0).unwrap().count(), 1, "probe left behind");
        assert_eq!(fs::read_dir(&ro).unwrap().count(), 0);
    }

    #[test]
    fn work_dir_hidden_0700_and_removed_on_drop() {
        let t = Tmp::new("work");
        let w = make_work_dir(&t.0).unwrap();
        let path = w.path.clone();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let hex = name.strip_prefix(".inacircle-").unwrap();
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        let w2 = make_work_dir(&t.0).unwrap();
        assert_ne!(w2.path, path);
        // Read-only file inside a read-only subdirectory must not block removal.
        let sub = path.join("sub");
        fs::create_dir(&sub).unwrap();
        let f = file(&sub, "ro.txt", "x");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o400)).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
        drop(w);
        assert!(!path.exists());
        assert!(w2.path.exists());
    }

    #[test]
    fn stale_cleanup_only_ours_and_old() {
        let t = Tmp::new("stale-clean");
        let old = t.0.join(".inacircle-0000000000000001");
        let fresh = t.0.join(".inacircle-0000000000000002");
        let other = t.0.join(".other-dir");
        let plain_file = t.0.join(".inacircle-file");
        for d in [&old, &fresh, &other] {
            fs::create_dir(d).unwrap();
        }
        fs::write(&plain_file, "x").unwrap();
        fs::write(old.join("junk"), "x").unwrap();
        for p in [&old, &other, &plain_file] {
            set_age(p, 2 * 24 * 3600);
        }
        clean_stale(&t.0);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
        assert!(plain_file.exists());
    }

    fn set_age(p: &Path, secs: i64) {
        let c = cstr(p).unwrap();
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: now is a valid out-pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
        let t = libc::timespec {
            tv_sec: now.tv_sec - secs,
            tv_nsec: 0,
        };
        let times = [t, t];
        // SAFETY: c is a valid path, times points to two timespecs.
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(r, 0);
    }
}
