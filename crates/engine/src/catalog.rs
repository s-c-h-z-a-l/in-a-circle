//! The action catalog: which actions exist, found by asking every action
//! binary for its manifest (`<bin> --manifest`) and cached so that a normal
//! start costs a few `stat` calls.
//!
//! Cache format (`$XDG_CACHE_HOME/inacircle/catalog`): header
//! `inacircle-catalog 0`; per binary a `bin` line (`path`, `mtime` in ns,
//! `size`, `builtin`) followed by that binary's `action` lines. The cache is
//! valid only when the current set of binaries and their (mtime, size) match
//! it exactly; otherwise everything is rebuilt. A binary whose manifest is
//! broken is cached with no actions, so it is not rerun until it changes.

use manifest::Line;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MANIFEST_MAGIC: &str = "inacircle-manifest";
const CACHE_MAGIC: &str = "inacircle-catalog";
/// One manifest's deadline: it prints a few lines and exits in milliseconds,
/// so 2 s is far above a healthy run and short enough to cut a hang.
const DEADLINE: Duration = Duration::from_secs(2);
/// The whole refresh's budget, shared by all manifests it runs. Only new or
/// changed binaries run a manifest, each in milliseconds, so a healthy
/// refresh uses a tiny part of it; 10 s cuts in after five hanging binaries
/// in a row (5 x `DEADLINE`) instead of N x 2 s. What is left unrun is
/// transient: a binary with an old cache entry keeps it (so its family stays
/// on the wheel), one without stays out, and either is retried at the next
/// start. Not `alarm`: SIGALRM would kill only our process, and the manifest
/// child, in its own process group, would be orphaned; each step's deadline
/// instead ends in `kill_group`.
const REFRESH_BUDGET: Duration = Duration::from_secs(10);
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub id: String,
    pub family: String,
    pub bin: PathBuf,
    pub input: String,
    pub output: String,
    pub ext: String,
    pub label: String,
    pub group: String,
    pub cost: u32,
    pub lossy: bool,
    pub batch: bool,
    pub requires: Vec<String>,
    pub builtin: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub actions: Vec<Action>,
}

/// One action binary as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bin {
    path: PathBuf,
    mtime: i128,
    size: u64,
    builtin: bool,
}

/// A binary plus its (validated) `action` lines.
struct Entry {
    bin: Bin,
    lines: Vec<Line>,
}

impl Catalog {
    /// Loads from the standard directories and cache location.
    /// A refused cache or data dir (see [`crate::xdg`]) is reported and
    /// not used.
    pub fn load() -> Catalog {
        let (dirs, cache) = standard();
        Catalog::load_with(&dirs, cache.as_deref())
    }

    /// Testable core. `dirs` is in priority order; the bool marks built-in.
    pub fn load_from(dirs: &[(PathBuf, bool)], cache: &Path) -> Catalog {
        Catalog::load_with(dirs, Some(cache))
    }

    /// The catalog from the cache alone, for the wheel: no manifest is run
    /// while the overlay is up. A cached binary still found in the action
    /// directories is used as cached, even if it changed since; one the
    /// cache does not know is left out until the next [`Catalog::load`].
    /// `None` without a readable cache.
    pub fn load_cached() -> Option<Catalog> {
        let (dirs, cache) = standard();
        Catalog::load_cached_from(&dirs, cache.as_deref()?)
    }

    /// Testable core of [`Catalog::load_cached`].
    pub fn load_cached_from(dirs: &[(PathBuf, bool)], cache: &Path) -> Option<Catalog> {
        let bins = scan(dirs);
        let mut entries = read_cache(cache)?;
        entries.retain(|e| bins.iter().any(|b| b.path == e.bin.path));
        Some(Catalog::of(&entries))
    }

    fn load_with(dirs: &[(PathBuf, bool)], cache: Option<&Path>) -> Catalog {
        let refresh_end = Instant::now() + REFRESH_BUDGET;
        let bins = scan(dirs);
        let cached = cache.and_then(read_cache).unwrap_or_default();
        let hit = cached.iter().map(|e| &e.bin).eq(bins.iter());
        let entries: Vec<Entry> = if hit {
            cached
        } else {
            // Rerun only binaries that are new or changed. When the rerun
            // fails transiently, the binary's old entry (same path, old
            // mtime and size) stays, so its family stays on the wheel,
            // which reads the cache alone, until a rerun succeeds; the old
            // mtime/size differ from the file on disk, so the next start
            // reruns it. A binary with no old entry stays out.
            let mut cached = cached;
            let entries: Vec<Entry> = bins
                .into_iter()
                .filter_map(|bin| {
                    if let Some(i) = cached.iter().position(|e| e.bin == bin) {
                        return Some(cached.swap_remove(i));
                    }
                    match run_manifest(&bin.path, bin.builtin, refresh_end) {
                        Some(lines) => Some(Entry { bin, lines }),
                        None => cached
                            .iter()
                            .position(|e| e.bin.path == bin.path)
                            .map(|i| cached.swap_remove(i)),
                    }
                })
                .collect();
            if let Some(cache) = cache {
                write_cache(cache, &entries);
            }
            entries
        };
        Catalog::of(&entries)
    }

    /// The actions of `entries`; a duplicate id after the first is ignored.
    fn of(entries: &[Entry]) -> Catalog {
        let mut actions: Vec<Action> = Vec::new();
        for e in entries {
            for l in &e.lines {
                let Some(a) = parse_action(l) else {
                    continue;
                };
                let a = Action {
                    bin: e.bin.path.clone(),
                    builtin: e.bin.builtin,
                    ..a
                };
                if actions.iter().any(|x| x.id == a.id) {
                    eprintln!(
                        "inacircle: duplicate action {} in {} ignored",
                        a.id,
                        e.bin.path.display()
                    );
                } else {
                    actions.push(a);
                }
            }
        }
        Catalog { actions }
    }

    pub fn get(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.id == id)
    }
}

/// `requires=` tools that are not executable files on `PATH`.
pub fn missing_tools(a: &Action) -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    a.requires
        .iter()
        .filter(|t| {
            !std::env::split_paths(&path).any(|d| {
                std::fs::metadata(d.join(t)).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
            })
        })
        .cloned()
        .collect()
}

/// The standard action directories and cache path.
fn standard() -> (Vec<(PathBuf, bool)>, Option<PathBuf>) {
    let over = std::env::var_os("INACIRCLE_ACTIONS_DIR");
    let cache = match crate::xdg::cache_home() {
        Ok(c) => Some(c.join("inacircle").join(cache_name(over.as_deref()))),
        Err(e) => {
            eprintln!("inacircle: no catalog cache: {e}");
            None
        }
    };
    let data = crate::xdg::data_home()
        .inspect_err(|e| eprintln!("inacircle: no user actions: {e}"))
        .ok();
    (dirs_from(over, data), cache)
}

/// The cache file for a set of action directories: `catalog` for the
/// standard ones, `catalog-<hash>` for an `INACIRCLE_ACTIONS_DIR` override.
/// One file per set: a run with another set (a test, a shell without the
/// variable) must not overwrite the catalog the wheel reads from the cache.
fn cache_name(over: Option<&std::ffi::OsStr>) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Some(v) = over else {
        return "catalog".to_owned();
    };
    // FNV-1a, 64 bit: stable across runs and builds (unlike std's hasher).
    let h = v.as_bytes().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("catalog-{h:016x}")
}

/// Action directories: the override (colon-separated, all built-in; empty
/// segments dropped, relative ones made absolute) or the standard three
/// (the user's only with a data home).
fn dirs_from(over: Option<std::ffi::OsString>, data_home: Option<PathBuf>) -> Vec<(PathBuf, bool)> {
    if let Some(v) = over {
        return std::env::split_paths(&v)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| (std::path::absolute(&p).unwrap_or(p), true))
            .collect();
    }
    let mut dirs = vec![
        (PathBuf::from("/usr/lib/inacircle/actions"), true),
        (PathBuf::from("/usr/local/lib/inacircle/actions"), true),
    ];
    dirs.extend(data_home.map(|d| (d.join("inacircle/actions"), false)));
    dirs
}

/// Executable regular files (links resolved) in each dir, sorted by name.
fn scan(dirs: &[(PathBuf, bool)]) -> Vec<Bin> {
    let mut out = Vec::new();
    for (dir, builtin) in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<Bin> = rd
            .flatten()
            .filter_map(|e| {
                if e.file_name().as_encoded_bytes().first() == Some(&b'.') {
                    return None;
                }
                let m = std::fs::metadata(e.path()).ok()?;
                if !m.is_file() || m.mode() & 0o111 == 0 {
                    return None;
                }
                Some(Bin {
                    path: e.path(),
                    mtime: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                    size: m.len(),
                    builtin: *builtin,
                })
            })
            .collect();
        found.sort_by(|a, b| a.path.cmp(&b.path));
        out.extend(found);
    }
    out
}

/// A valid `action` line as an `Action` (with `bin` and `builtin` unset).
fn parse_action(l: &Line) -> Option<Action> {
    if l.kind != "action" {
        return None;
    }
    let yes_no = |k: &str| match l.get(k)? {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    };
    let text = |k: &str| l.get(k).filter(|v| !v.is_empty()).map(str::to_owned);
    Some(Action {
        id: l.bare_at(0).filter(|v| !v.is_empty())?.to_owned(),
        family: text("family")?,
        bin: PathBuf::new(),
        input: text("in")?,
        output: text("out")?,
        ext: text("ext")?,
        label: text("label")?,
        group: text("group")?,
        cost: l.get("cost")?.parse().ok()?,
        lossy: yes_no("lossy")?,
        batch: yes_no("batch")?,
        requires: l
            .get("requires")
            .map(|r| {
                r.split(',')
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        builtin: false,
    })
}

/// Why a manifest run failed. Transient failures (the binary may work next
/// time) are not cached; deterministic ones are.
enum Fail {
    Transient(String),
    Deterministic(String),
}

/// Runs `<bin> --manifest` and returns its valid `action` lines. A broken
/// binary yields no lines (message on stderr); `None` means a transient
/// failure: nothing new is cached, and the caller keeps the binary's old
/// entry, if any.
fn run_manifest(bin: &Path, builtin: bool, refresh_end: Instant) -> Option<Vec<Line>> {
    match manifest_lines(bin, builtin, refresh_end) {
        Ok(lines) => Some(lines),
        Err(Fail::Deterministic(why)) => {
            eprintln!("inacircle: {}: {why}; skipped", bin.display());
            Some(Vec::new())
        }
        Err(Fail::Transient(why)) => {
            eprintln!("inacircle: {}: {why}; skipped, will retry", bin.display());
            None
        }
    }
}

/// The deadline of one manifest: `DEADLINE`, but not past the refresh's end;
/// `None` when the refresh has no time left.
fn step_deadline(now: Instant, refresh_end: Instant) -> Option<Instant> {
    (now < refresh_end).then(|| (now + DEADLINE).min(refresh_end))
}

fn manifest_lines(bin: &Path, builtin: bool, refresh_end: Instant) -> Result<Vec<Line>, Fail> {
    let deadline = step_deadline(Instant::now(), refresh_end)
        .ok_or_else(|| Fail::Transient("refresh budget spent".to_owned()))?;
    let out = capture(bin, builtin, deadline)?;
    let bad = |why: String| Fail::Deterministic(why);
    let text = String::from_utf8(out).map_err(|_| bad("manifest is not UTF-8".to_owned()))?;
    match manifest::parse(&text, MANIFEST_MAGIC) {
        Ok((0, lines)) => Ok(lines
            .into_iter()
            .filter(|l| {
                // Other kinds are ignored silently (forward compatibility).
                if l.kind != "action" || parse_action(l).is_some() {
                    return l.kind == "action";
                }
                eprintln!("inacircle: {}: invalid action line ignored", bin.display());
                false
            })
            .collect()),
        Ok((v, _)) => Err(bad(format!("unknown manifest version {v}"))),
        Err(e) => Err(bad(format!("bad manifest: line {}: {}", e.line, e.msg))),
    }
}

/// Stdout of `<bin> --manifest` within `deadline` and the size limit, run
/// in the sandbox of `run` but narrower: Landlock lets it read and execute
/// only the system directories and the binary, with no input and no
/// writable directory. A plugin's code is not trusted, so without Landlock
/// its manifest does not run (transient: a later start retries); a
/// built-in one then runs under seccomp and the rlimits only, as a built-in
/// `run` does. The child's whole process group is killed and reaped on
/// failure.
fn capture(bin: &Path, builtin: bool, deadline: Instant) -> Result<Vec<u8>, Fail> {
    let mut child = spawn_manifest(bin, builtin)?;
    let result = read_limited(&mut child, deadline);
    if result.is_err() {
        kill_group(&child);
    }
    let status = wait_until(&mut child, deadline);
    let out = result?;
    match status {
        Some(s) if s.success() => Ok(out),
        Some(s) => Err(Fail::Transient(format!("exited with {s}"))),
        None => Err(Fail::Transient("did not exit in time".to_owned())),
    }
}

/// Starts `<bin> --manifest` in the manifest sandbox (see [`capture`]).
fn spawn_manifest(bin: &Path, builtin: bool) -> Result<std::process::Child, Fail> {
    use crate::sandbox::{self, Limits, Policy};
    use std::os::unix::process::CommandExt;
    // Paths that do not exist are skipped by the ruleset builder.
    let mut exec: Vec<PathBuf> = sandbox::SYSTEM_EXEC_PATHS
        .iter()
        .map(PathBuf::from)
        .collect();
    exec.push(bin.to_path_buf());
    let policy = Policy {
        exec: &exec,
        read: &[],
        read_exact: &[],
        write: None,
    };
    let ruleset = sandbox::build(&policy)
        .map_err(|e| Fail::Transient(format!("sandbox setup failed: {e}")))?;
    if !manifest_policy_allows(builtin, ruleset.is_some()) {
        return Err(Fail::Transient("sandbox unavailable".to_owned()));
    }
    // A manifest writes only to its stdout pipe, to which RLIMIT_FSIZE does
    // not apply, and may write no file: 0 makes any file write fail.
    let limits = Limits {
        fsize: 0,
        data: sandbox::data_cap(sandbox::total_ram()),
    };
    let fd = ruleset.as_ref().map(|r| r.as_raw_fd());
    let mut cmd = Command::new(bin);
    cmd.arg("--manifest")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // No `process_group(0)`: std applies it before pre_exec, and the
    // child's `setsid` in `apply_in_child` (which also makes it a group
    // leader, so `kill_group` still reaches its descendants) would fail.
    // SAFETY: the closure makes only the async-signal-safe calls of
    // `apply_in_child`; the ruleset fd stays open until spawn returns
    // (`ruleset` is dropped only after the spawn).
    unsafe {
        cmd.pre_exec(move || sandbox::apply_in_child(fd, &limits));
    }
    let child = cmd
        .spawn()
        .map_err(|e| Fail::Transient(format!("cannot run: {e}")))?;
    drop(ruleset);
    Ok(child)
}

/// Whether a manifest may run: a plugin's only inside Landlock (its code is
/// not trusted, like its `run`); a built-in one also without it, under
/// seccomp and the rlimits only, as a built-in `run` does.
fn manifest_policy_allows(builtin: bool, landlock: bool) -> bool {
    builtin || landlock
}

fn kill_group(child: &std::process::Child) {
    // SAFETY: kill has no memory-safety preconditions. The child leads its own
    // process group (it calls `setsid` in `apply_in_child`), so -pid
    // addresses only its group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
}

fn read_limited(child: &mut std::process::Child, deadline: Instant) -> Result<Vec<u8>, Fail> {
    let t = |s: &str| Fail::Transient(s.to_owned());
    let stdout = child.stdout.as_ref().ok_or_else(|| t("no stdout"))?;
    let fd = stdout.as_raw_fd();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(t("timed out"));
        }
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as i32 + 1;
        // SAFETY: `p` is a valid pollfd and nfds is 1.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(t("poll failed"));
        }
        if r == 0 {
            continue; // re-check the deadline
        }
        // SAFETY: `buf` is valid for `buf.len()` bytes; `fd` is open (owned
        // by `child.stdout`, which outlives this call).
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(t("read failed"));
        }
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n as usize]);
        if out.len() > MAX_OUTPUT {
            return Err(Fail::Deterministic("manifest larger than 64 KB".to_owned()));
        }
    }
}

/// Waits for exit until `deadline`, then kills the group; always reaps.
fn wait_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            _ => {
                kill_group(child);
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn read_cache(path: &Path) -> Option<Vec<Entry>> {
    parse_cache(&std::fs::read(path).ok()?)
}

/// The cache file's content as entries; `None` for anything that is not
/// exactly what `write_cache` writes (not UTF-8, wrong header or version,
/// a line of an unknown kind or with a field missing or malformed, an
/// action before any bin). Pure: no file.
fn parse_cache(bytes: &[u8]) -> Option<Vec<Entry>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (0, lines) = manifest::parse(text, CACHE_MAGIC).ok()? else {
        return None;
    };
    let mut entries: Vec<Entry> = Vec::new();
    for l in lines {
        match l.kind.as_str() {
            "bin" => entries.push(Entry {
                bin: Bin {
                    path: PathBuf::from(l.get("path")?),
                    mtime: l.get("mtime")?.parse().ok()?,
                    size: l.get("size")?.parse().ok()?,
                    builtin: match l.get("builtin")? {
                        "yes" => true,
                        "no" => false,
                        _ => return None,
                    },
                },
                lines: Vec::new(),
            }),
            "action" => entries.last_mut()?.lines.push(l),
            _ => return None,
        }
    }
    Some(entries)
}

/// Fuzz entry point: the cache parser on arbitrary bytes.
#[cfg(fuzzing)]
pub fn fuzz_read_cache(bytes: &[u8]) {
    let _ = parse_cache(bytes);
}

/// Atomic (temp file + rename); any failure just means no cache.
fn write_cache(path: &Path, entries: &[Entry]) {
    let mut s = manifest::header(CACHE_MAGIC, 0);
    for e in entries {
        let b = &e.bin;
        let line = Line::new("bin")
            .pair("path", &b.path.to_string_lossy())
            .pair("mtime", &b.mtime.to_string())
            .pair("size", &b.size.to_string())
            .pair("builtin", if b.builtin { "yes" } else { "no" });
        s.push_str(&manifest::write_line(&line));
        for l in &e.lines {
            s.push_str(&manifest::write_line(l));
        }
    }
    let _ = (|| -> std::io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".catalog.{}.tmp", std::process::id()));
        let r = std::fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(s.as_bytes()))
            .and_then(|()| std::fs::rename(&tmp, path));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    })();
}

#[cfg(test)]
mod tests {

    /// Regression: a run without `INACIRCLE_ACTIONS_DIR` (other dirs, no
    /// binaries) wrote an empty catalog over the one the wheel reads from
    /// the cache, so its Markdown slice came up dim.
    #[test]
    fn each_set_of_action_dirs_has_its_own_cache() {
        use std::ffi::OsStr;
        assert_eq!(cache_name(None), "catalog");
        let a = cache_name(Some(OsStr::new("/a/actions")));
        let b = cache_name(Some(OsStr::new("/b/actions")));
        assert!(
            a.starts_with("catalog-") && a.len() == "catalog-".len() + 16,
            "{a}"
        );
        assert_ne!(a, b);
        assert_ne!(a, "catalog");
        assert_eq!(a, cache_name(Some(OsStr::new("/a/actions"))), "stable");
    }

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Serializes the catalog tests (they share nothing else, but keep
    /// timing-sensitive cache checks apart).
    static LOCK: Mutex<()> = Mutex::new(());

    struct Fix {
        root: PathBuf,
        _g: std::sync::MutexGuard<'static, ()>,
    }

    impl Fix {
        fn new(name: &str) -> Fix {
            let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let root =
                std::env::temp_dir().join(format!("engine-cat-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Fix { root, _g: g }
        }
        fn dir(&self, name: &str) -> PathBuf {
            let d = self.root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        }
        fn cache(&self) -> PathBuf {
            self.root.join("cache/inacircle/catalog")
        }
    }

    impl Drop for Fix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Installs an executable script without this process ever holding a
    /// writable fd on it. Other tests fork at any time; a child that
    /// inherited such an fd until its own exec would make our exec of the
    /// script fail with ETXTBSY. So `cp` (its own process) writes the file
    /// under a temporary name, which is then renamed into place.
    fn script(path: &Path, body: &str) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let src = std::env::temp_dir().join(format!("engine-cat-src-{}-{n}", std::process::id()));
        std::fs::write(&src, format!("#!/bin/sh\n{body}\n")).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        let tmp = path.with_file_name(format!(".{name}.tmp"));
        let ok = Command::new("cp").arg(&src).arg(&tmp).status().unwrap();
        assert!(ok.success());
        std::fs::remove_file(&src).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    /// Sets the mtime by path (no writable fd on an executable, see `script`).
    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap();
        let ts = libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        };
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c is a valid path; times points to two timespecs.
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [ts, ts].as_ptr(), 0) };
        assert_eq!(r, 0);
    }

    fn act(id: &str, label: &str) -> String {
        format!(
            "action\t{id}\tfamily=f\tin=text/plain\tout=text/markdown\text=md\tlabel={label}\tgroup=g\tcost=10\tlossy=no\tbatch=yes"
        )
    }

    fn manifest_script(path: &Path, lines: &[String]) {
        script(path, &manifest_body("", lines));
    }

    /// The shell body of a manifest: `pre` (shell), then the header and `lines`.
    fn manifest_body(pre: &str, lines: &[String]) -> String {
        let mut body = format!("[ \"$1\" = --manifest ] || exit 1\n{pre}\n");
        body.push_str("printf 'inacircle-manifest 0\\n'\n");
        for l in lines {
            body.push_str(&format!(
                "printf '%s\\n' '{}'\n",
                l.replace('\t', "'\"\t\"'")
            ));
        }
        body
    }

    // The sandbox is applied by the same `apply_in_child` and ruleset
    // builder as `run`, whose tests cover the network denial; denied writes
    // and reads here prove it is applied to the manifest too.
    #[test]
    fn manifest_cannot_write_outside() {
        if crate::sandbox::tests::abi_or_skip("manifest_cannot_write_outside", 1).is_none() {
            return;
        }
        let f = Fix::new("mf-write");
        let d = f.dir("a");
        let outside = f.root.join("outside");
        let pre = format!("echo x > '{}' 2>/dev/null", outside.display());
        script(&d.join("one"), &manifest_body(&pre, &[act("x.a", "A")]));
        let c = Catalog::load_from(&[(d, false)], &f.cache());
        assert!(c.get("x.a").is_some(), "the manifest itself still loads");
        assert!(!outside.exists(), "the manifest wrote outside the sandbox");
    }

    #[test]
    fn manifest_cannot_read_outside() {
        if crate::sandbox::tests::abi_or_skip("manifest_cannot_read_outside", 1).is_none() {
            return;
        }
        let f = Fix::new("mf-read");
        let d = f.dir("a");
        let secret = f.root.join("secret");
        std::fs::write(&secret, "topsecret-4711").unwrap();
        // The secret goes into a label, so any leak shows in the catalog.
        let body = format!(
            "[ \"$1\" = --manifest ] || exit 1\nprintf 'inacircle-manifest 0\\n'\n\
             printf 'action\\tx.s\\tfamily=f\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=%s\\tgroup=g\\tcost=10\\tlossy=no\\tbatch=yes\\n' \"$(cat '{}' 2>/dev/null)\"\n\
             printf 'action\\tx.a\\tfamily=f\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=A\\tgroup=g\\tcost=10\\tlossy=no\\tbatch=yes\\n'\n",
            secret.display()
        );
        script(&d.join("one"), &body);
        let c = Catalog::load_from(&[(d, false)], &f.cache());
        assert!(c.get("x.a").is_some(), "the manifest itself still loads");
        for a in &c.actions {
            assert!(!a.label.contains("topsecret"), "read outside: {a:?}");
        }
    }

    #[test]
    fn manifest_policy_without_landlock() {
        assert!(manifest_policy_allows(true, false));
        assert!(!manifest_policy_allows(false, false));
        assert!(manifest_policy_allows(false, true));
    }

    #[test]
    fn loads_actions_from_dirs() {
        let f = Fix::new("loads");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A"), act("x.b", "B")]);
        std::fs::write(d.join("notexec"), "data").unwrap();
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        let ids: Vec<_> = c.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["x.a", "x.b"]);
        let a = c.get("x.a").unwrap();
        assert_eq!(a.family, "f");
        assert_eq!(a.bin, d.join("one"));
        assert_eq!(a.input, "text/plain");
        assert_eq!(a.output, "text/markdown");
        assert_eq!(a.ext, "md");
        assert_eq!(a.label, "A");
        assert_eq!(a.group, "g");
        assert_eq!(a.cost, 10);
        assert!(!a.lossy && a.batch && a.builtin);
        assert!(a.requires.is_empty());
        assert!(c.get("nope").is_none());
    }

    #[test]
    fn requires_and_missing_fields() {
        let f = Fix::new("req");
        let d = f.dir("a");
        manifest_script(
            &d.join("one"),
            &[
                format!("{}\trequires=foo,bar", act("x.a", "A")),
                "action\tx.bad\tfamily=f".to_owned(),
            ],
        );
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        assert_eq!(c.actions[0].requires, ["foo", "bar"]);
    }

    #[test]
    fn cache_reused_when_unchanged() {
        let f = Fix::new("reuse");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        let first = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(first.actions.len(), 1);
        assert!(f.cache().exists());
        // The manifest cannot leave a run marker (it is sandboxed): change
        // its output behind the cache's back instead (same size and mtime).
        let mtime = std::fs::metadata(d.join("one"))
            .unwrap()
            .modified()
            .unwrap();
        manifest_script(&d.join("one"), &[act("x.a", "B")]);
        set_mtime(&d.join("one"), mtime);
        let second = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(second.actions, first.actions, "the manifest ran again");
    }

    #[test]
    fn cache_invalidated_on_mtime_change_and_on_new_binary() {
        let f = Fix::new("inval");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "A");
        // Same size, different mtime.
        manifest_script(&d.join("one"), &[act("x.a", "B")]);
        let t = std::fs::metadata(d.join("one"))
            .unwrap()
            .modified()
            .unwrap()
            + Duration::from_secs(5);
        set_mtime(&d.join("one"), t);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "B");
        manifest_script(&d.join("two"), &[act("y.a", "Y")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert!(c.get("y.a").is_some() && c.get("x.a").is_some());
        std::fs::remove_file(d.join("two")).unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(c.get("y.a").is_none());
    }

    #[test]
    fn unwritable_cache_does_not_fail_load() {
        let f = Fix::new("nocache");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        std::fs::write(f.root.join("blocker"), "").unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.root.join("blocker/inacircle/catalog"));
        assert_eq!(c.actions.len(), 1);
    }

    const CACHE_OK: &str = "inacircle-catalog 0\nbin\tpath=/a/one\tmtime=5\tsize=7\tbuiltin=yes\n";

    #[test]
    fn parse_cache_reads_what_write_cache_writes() {
        let text = format!("{CACHE_OK}{}\n", act("x.a", "A"));
        let e = parse_cache(text.as_bytes()).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].bin.path, Path::new("/a/one"));
        assert_eq!(
            (e[0].bin.mtime, e[0].bin.size, e[0].bin.builtin),
            (5, 7, true)
        );
        assert_eq!(e[0].lines.len(), 1);
        assert_eq!(parse_cache(b"inacircle-catalog 0\n").unwrap().len(), 0);
    }

    #[test]
    fn parse_cache_refuses_truncated_and_bad_input() {
        // Cut mid-field and mid-header.
        assert!(parse_cache(&CACHE_OK.as_bytes()[..CACHE_OK.len() - 9]).is_none());
        assert!(parse_cache(b"inacircle-cat").is_none());
        assert!(parse_cache(b"").is_none());
        // Wrong magic, unsupported version, not UTF-8, an action before any
        // bin, an unknown line kind.
        assert!(parse_cache(b"other 0\n").is_none());
        assert!(parse_cache(b"inacircle-catalog 1\n").is_none());
        assert!(parse_cache(b"inacircle-catalog 0\n\xff\n").is_none());
        let orphan = format!("inacircle-catalog 0\n{}\n", act("x.a", "A"));
        assert!(parse_cache(orphan.as_bytes()).is_none());
        assert!(parse_cache(b"inacircle-catalog 0\nzzz\n").is_none());
    }

    #[test]
    fn corrupt_cache_is_rebuilt() {
        let f = Fix::new("corrupt");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        std::fs::create_dir_all(f.cache().parent().unwrap()).unwrap();
        std::fs::write(f.cache(), "garbage\n").unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
    }

    #[test]
    fn broken_manifest_skipped_others_kept() {
        let f = Fix::new("broken");
        let d = f.dir("a");
        script(&d.join("a-garbage"), "echo nonsense");
        script(&d.join("b-fail"), "exit 3");
        script(&d.join("c-version"), "printf 'inacircle-manifest 9\\n'");
        script(
            &d.join("d-huge"),
            "printf 'inacircle-manifest 0\\n'; yes '# padding padding padding' | head -c 200000",
        );
        manifest_script(&d.join("e-good"), &[act("x.a", "A")]);
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        assert_eq!(c.actions[0].id, "x.a");
    }

    #[test]
    fn step_deadline_takes_the_smaller() {
        let now = Instant::now();
        assert_eq!(
            step_deadline(now, now + Duration::from_secs(9)),
            Some(now + DEADLINE)
        );
        let end = now + Duration::from_secs(1);
        assert_eq!(step_deadline(now, end), Some(end));
        assert_eq!(step_deadline(now, now), None);
        assert_eq!(step_deadline(now + Duration::from_secs(1), now), None);
    }

    /// Pids whose command line mentions `marker` (a unique fixture path).
    fn procs_with(marker: &str) -> Vec<u32> {
        let mut found = Vec::new();
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Ok(cmd) = std::fs::read(e.path().join("cmdline"))
                && String::from_utf8_lossy(&cmd).contains(marker)
            {
                found.push(pid);
            }
        }
        found
    }

    #[test]
    fn refresh_budget_leaves_no_child() {
        let f = Fix::new("budget");
        let d = f.dir("a");
        // Eight manifests of 1.5 s each: every one fits its own 2 s with
        // margin, six fit the 10 s budget, the seventh is cut while running
        // and the eighth finds the budget spent.
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        // A sleep duration unique to this process (1.50<pid> s), so the
        // sleep itself, not just its shell, can be found in /proc.
        let sleep_marker = format!("1.50{}", std::process::id());
        for (i, n) in names.iter().enumerate() {
            let pre = format!("sleep {sleep_marker}");
            let body = manifest_body(&pre, &[act(&format!("x.{n}"), "L")]);
            script(&d.join(format!("s{i}")), &body);
        }
        let dirs = [(d.clone(), true)];
        let t = Instant::now();
        let c = Catalog::load_from(&dirs, &f.cache());
        assert!(t.elapsed() < Duration::from_secs(11), "{:?}", t.elapsed());
        for n in &names[..6] {
            assert!(c.get(&format!("x.{n}")).is_some(), "x.{n} missing");
        }
        for n in &names[6..] {
            assert!(
                c.get(&format!("x.{n}")).is_none(),
                "x.{n} is in the catalog"
            );
        }
        // The sleep, not just the shell, must be gone: the shell's command
        // line holds the fixture dir (scripts run by path), the sleep's its
        // unique duration.
        let mut left = procs_with(&d.to_string_lossy());
        left.extend(procs_with(&sleep_marker));
        assert!(left.is_empty(), "children left alive: {left:?}");
        // Not cached: the next load retries them (each with its own 2 s).
        let c = Catalog::load_from(&dirs, &f.cache());
        for n in &names[6..] {
            assert!(c.get(&format!("x.{n}")).is_some(), "x.{n} was cached");
        }
    }

    #[test]
    fn hanging_manifest_killed_after_deadline() {
        let f = Fix::new("hang");
        let d = f.dir("a");
        script(&d.join("hang"), "exec sleep 10");
        let t = Instant::now();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(c.actions.is_empty());
    }

    /// The wheel's load: from the cache alone, never running a manifest.
    /// Stale entries are used as cached while their binary is still there;
    /// a binary the cache does not know is left out; no cache, no catalog.
    #[test]
    fn cached_load_never_runs_a_manifest() {
        let f = Fix::new("cached");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        assert_eq!(Catalog::load_cached_from(&dirs, &f.cache()), None);
        manifest_script(&d.join("two"), &[act("y.a", "Y")]);
        Catalog::load_from(&dirs, &f.cache());
        let cache = std::fs::read(f.cache()).unwrap();
        // Changed since it was cached: used as cached.
        manifest_script(&d.join("one"), &[act("x.a", "B")]);
        let t = std::fs::metadata(d.join("one"))
            .unwrap()
            .modified()
            .unwrap();
        set_mtime(&d.join("one"), t + Duration::from_secs(5));
        // Gone: not offered.
        std::fs::remove_file(d.join("two")).unwrap();
        // New, and its manifest would hang: not run.
        let marker = f.root.join("marker");
        script(
            &d.join("three"),
            &format!("echo run >> {}\nexec sleep 10", marker.display()),
        );
        let t = Instant::now();
        let c = Catalog::load_cached_from(&dirs, &f.cache()).expect("a cache");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        let ids: Vec<_> = c
            .actions
            .iter()
            .map(|a| (a.id.as_str(), a.label.as_str()))
            .collect();
        assert_eq!(ids, [("x.a", "A")]);
        assert!(c.actions[0].builtin);
        assert!(!marker.exists(), "a manifest ran");
        assert_eq!(
            std::fs::read(f.cache()).unwrap(),
            cache,
            "the cache is not rewritten"
        );
        // A corrupt cache is no cache.
        std::fs::write(f.cache(), "garbage\n").unwrap();
        assert_eq!(Catalog::load_cached_from(&dirs, &f.cache()), None);
    }

    #[test]
    fn duplicate_id_first_dir_wins() {
        let f = Fix::new("dup");
        let (d1, d2) = (f.dir("a"), f.dir("b"));
        manifest_script(&d1.join("one"), &[act("x.a", "first")]);
        manifest_script(&d2.join("two"), &[act("x.a", "second"), act("x.b", "B")]);
        let c = Catalog::load_from(&[(d1, true), (d2, true)], &f.cache());
        assert_eq!(c.actions.len(), 2);
        assert_eq!(c.get("x.a").unwrap().label, "first");
        assert!(c.get("x.b").is_some());
    }

    #[test]
    fn transient_failure_retried_deterministic_cached() {
        let f = Fix::new("transient");
        let d = f.dir("a");
        // A manifest cannot leave a run marker (it is sandboxed), so reruns
        // are told by a changed outcome: `a-fails` exits 1 now and is fixed
        // after the first load; `b-garbage` has a malformed header and is
        // fixed after it with the same size and mtime.
        let good_a = manifest_body("", &[act("y.a", "Y")]);
        // Same size as the good body: a wrongly cached failure would then
        // stay cached after the fix (size and mtime unchanged).
        let failing = {
            let head = "exit 1\n#";
            format!("{head}{}", "x".repeat(good_a.len() - head.len()))
        };
        script(&d.join("a-fails"), &failing);
        let mtime_a = std::fs::metadata(d.join("a-fails"))
            .unwrap()
            .modified()
            .unwrap();

        let good_b = manifest_body("", &[act("x.b", "B")]);
        let garbage = good_b.replace("inacircle-manifest 0", "inacircle-garbage-00");
        script(&d.join("b-garbage"), &garbage);
        manifest_script(&d.join("c-good"), &[act("x.a", "A")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        script(&d.join("a-fails"), &good_a);
        set_mtime(&d.join("a-fails"), mtime_a);
        let mtime = std::fs::metadata(d.join("b-garbage"))
            .unwrap()
            .modified()
            .unwrap();
        script(&d.join("b-garbage"), &good_b);
        set_mtime(&d.join("b-garbage"), mtime);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert!(
            c.get("y.a").is_some(),
            "the transient failure was not retried"
        );
        assert!(
            c.get("x.b").is_none(),
            "the deterministic failure was rerun"
        );
        assert!(c.get("x.a").is_some());
    }

    #[test]
    fn transient_rerun_failure_keeps_the_old_entry() {
        let f = Fix::new("keep-old");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        manifest_script(&d.join("tool"), &[act("x.a", "old")]);
        manifest_script(&d.join("other"), &[act("y.a", "Y")]);
        let c = Catalog::load_from(&dirs, &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "old");
        // The binary changes and its manifest now fails transiently (exit 1
        // is a crash, not a broken manifest). A new binary that fails the
        // same way has no old entry and stays out.
        script(&d.join("tool"), "exit 1");
        script(&d.join("fresh"), "exit 1");
        let c = Catalog::load_from(&dirs, &f.cache());
        assert_eq!(c.get("x.a").map(|a| a.label.as_str()), Some("old"));
        assert!(c.get("y.a").is_some());
        // The wheel reads the cache alone and still offers the family.
        let w = Catalog::load_cached_from(&dirs, &f.cache()).expect("a cache");
        assert_eq!(w.get("x.a").map(|a| a.label.as_str()), Some("old"));
        assert_eq!(w.actions.len(), 2);
        // The old entry kept its old mtime/size, so the next start reruns it.
        manifest_script(&d.join("tool"), &[act("x.a", "new")]);
        let c = Catalog::load_from(&dirs, &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "new");
    }

    /// Benchmark, not a test: the cost of the sandboxed manifest run against
    /// a plain spawn, for docs/benchmarks/manifest-sandbox.md. Needs a
    /// release `inacircle-text` (or `INACIRCLE_BENCH_BIN`). Run:
    /// `INACIRCLE_REQUIRE_LANDLOCK=1 cargo test --release -p engine --lib
    /// manifest_sandbox_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn manifest_sandbox_cost() {
        /// Runs per variant; enough for a stable p50 and a max that shows
        /// the occasional outlier.
        const RUNS: usize = 300;
        /// Runs dropped first: cold page cache and CPU caches.
        const WARMUP: usize = 20;
        let bin = std::env::var_os("INACIRCLE_BENCH_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/inacircle-text")
            });
        assert!(bin.is_file(), "build {} first", bin.display());
        let deadline = || Instant::now() + DEADLINE;
        // (spawn returned, output complete and reaped), in microseconds.
        let mut sandboxed = Vec::new();
        let mut plain = Vec::new();
        for i in 0..WARMUP + RUNS {
            let t = Instant::now();
            let mut c = spawn_manifest(&bin, true).ok().expect("sandboxed spawn");
            let spawned = t.elapsed();
            let out = read_limited(&mut c, deadline()).ok().expect("output");
            wait_until(&mut c, deadline());
            let total = t.elapsed();
            assert!(!out.is_empty());
            if i >= WARMUP {
                sandboxed.push((spawned.as_micros(), total.as_micros()));
            }
            let t = Instant::now();
            let mut c = Command::new(&bin)
                .arg("--manifest")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("plain spawn");
            let spawned = t.elapsed();
            let out = read_limited(&mut c, deadline()).ok().expect("output");
            wait_until(&mut c, deadline());
            let total = t.elapsed();
            assert!(!out.is_empty());
            if i >= WARMUP {
                plain.push((spawned.as_micros(), total.as_micros()));
            }
        }
        let report = |name: &str, v: &[(u128, u128)]| {
            let col = |f: fn(&(u128, u128)) -> u128| {
                let mut x: Vec<u128> = v.iter().map(f).collect();
                x.sort_unstable();
                (x[x.len() / 2], x[x.len() - 1])
            };
            let (sp50, smax) = col(|p| p.0);
            let (tp50, tmax) = col(|p| p.1);
            eprintln!(
                "{name}: spawn p50 {sp50} us max {smax} us; output p50 {tp50} us max {tmax} us"
            );
        };
        report("sandboxed", &sandboxed);
        report("plain    ", &plain);
    }

    #[test]
    fn symlink_dotfile_unknown_kind_and_builtin_on_hit() {
        let f = Fix::new("misc");
        let (d, real) = (f.dir("a"), f.dir("real"));
        let extra = "future\tthing=1".to_owned();
        manifest_script(&real.join("tool"), &[act("x.a", "A"), extra]);
        std::os::unix::fs::symlink(real.join("tool"), d.join("link")).unwrap();
        manifest_script(&d.join(".hidden"), &[act("x.h", "H")]);
        // A plugin dir needs Landlock for its manifest to run; without it
        // (and without INACIRCLE_REQUIRE_LANDLOCK=1) the dir counts as
        // built in here and only the built-in flag check is lost.
        let plugin = crate::sandbox::tests::abi_or_skip("symlink_dotfile", 1).is_some();
        for _ in 0..2 {
            let c = Catalog::load_from(&[(d.clone(), !plugin)], &f.cache());
            assert_eq!(c.actions.len(), 1);
            assert_eq!(c.actions[0].bin, d.join("link"));
            assert_eq!(c.actions[0].builtin, !plugin);
        }
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(c.actions[0].builtin);
    }

    #[test]
    fn dirs_from_override_and_defaults() {
        let o = |s: &str| Some(std::ffi::OsString::from(s));
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            dirs_from(o("/a::rel:/b"), Some("/d".into())),
            [
                ("/a".into(), true),
                (cwd.join("rel"), true),
                ("/b".into(), true)
            ]
        );
        assert!(dirs_from(o(""), Some("/d".into())).is_empty());
        let d = dirs_from(None, Some("/d".into()));
        assert_eq!(d.len(), 3);
        assert_eq!(d[2], (PathBuf::from("/d/inacircle/actions"), false));
        assert!(d[0].1 && d[1].1);
        // A refused data home leaves out the user's folder only.
        assert_eq!(dirs_from(None, None), d[..2]);
    }

    #[test]
    fn missing_tools_checks_path() {
        let a = Catalog::load_from(&[], Path::new("/nonexistent/c")).actions;
        assert!(a.is_empty());
        let act = Action {
            id: "i".into(),
            family: "f".into(),
            bin: "/b".into(),
            input: "a/b".into(),
            output: "a/b".into(),
            ext: "x".into(),
            label: "l".into(),
            group: "g".into(),
            cost: 1,
            lossy: false,
            batch: false,
            requires: vec!["sh".into(), "no-such-tool-inacircle".into()],
            builtin: true,
        };
        assert_eq!(missing_tools(&act), ["no-such-tool-inacircle"]);
    }
}
