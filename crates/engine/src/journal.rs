//! The journal: an append-only history log, the running-jobs registry and
//! undo. All formats go through the `manifest` crate.
//!
//! History (`$XDG_STATE_HOME/inacircle/history`), header `inacircle-history 0`:
//! `job`, `out`, `dir`, `skip`, `fail` and `undo` lines, each written with a
//! single `write` on an `O_APPEND` descriptor. A job's `out`, `dir`, `skip`
//! and `fail` lines are written as they happen, its `job` line (with the
//! status) when it ends; any of them makes the job known to undo. Running
//! jobs live in `$XDG_RUNTIME_DIR/inacircle/jobs/<job-id>`, one `job` line
//! per file.

use crate::exec::{FileId, Outcome, Tally};
use crate::xdg;
use manifest::{Line, parse_line, write_line};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &str = "inacircle-history";
const VERSION: u32 = 0;
/// Running-job files are rewritten at most this often.
const UPDATE_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(pub String);

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<unix-seconds>-<pid>-<8 hex digits>`: seconds and a pid can repeat
/// (pid reuse, a namespace), the random part makes a clash unlikely.
pub fn new_job_id() -> JobId {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Without getrandom (never on a supported kernel), the nanoseconds.
    let r = crate::output::random_u64().unwrap_or(u64::from(now.subsec_nanos()));
    JobId(format!(
        "{}-{}-{:08x}",
        now.as_secs(),
        std::process::id(),
        r as u32
    ))
}

fn history_path() -> io::Result<PathBuf> {
    Ok(xdg::state_home()?.join("inacircle").join("history"))
}

fn jobs_dir() -> Option<PathBuf> {
    xdg::runtime_dir().map(|r| r.join("inacircle").join("jobs"))
}

/// Opens the history for appending. The header is written when the file is
/// created: a temp file holding it is hard-linked into place, which is atomic
/// and fails if the file exists, so no line can ever precede the header. On a
/// filesystem without hard links, the file is created with `O_EXCL` and the
/// header written after (a tiny window in which another process's first line
/// could precede it is accepted).
fn open_history() -> io::Result<File> {
    open_history_with(|from, to| fs::hard_link(from, to))
}

fn open_history_with(link: impl Fn(&Path, &Path) -> io::Result<()>) -> io::Result<File> {
    let path = history_path()?;
    let dir = path.parent().expect("history path has a parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let append = || OpenOptions::new().append(true).open(&path);
    if path.exists() {
        return append();
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".history.{}.{seq}.tmp", std::process::id()));
    fs::write(&tmp, manifest::header(MAGIC, VERSION))?;
    let linked = link(&tmp, &path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => append(),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => append(),
        Err(_) => {
            // No hard links here: create it ourselves.
            match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    f.write_all(manifest::header(MAGIC, VERSION).as_bytes())?;
                    Ok(f)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => append(),
                Err(e) => Err(e),
            }
        }
    }
}

/// One line, one `write`.
fn append_line(f: &mut File, line: &Line) -> io::Result<()> {
    let text = write_line(line);
    let n = f.write(text.as_bytes())?;
    if n < text.len() {
        // A short write to a regular file only happens on a full disk or a
        // signal; finish the line (it may then interleave, and a reader skips
        // a line that does not parse).
        f.write_all(&text.as_bytes()[n..])?;
    }
    Ok(())
}

/// `done` when every input that ended is done, `partial` when some are,
/// else `failed`.
fn status(t: &Tally) -> &'static str {
    let (done, skipped, failed) = t.counts();
    if done == 0 {
        "failed"
    } else if skipped + failed == 0 {
        "done"
    } else {
        "partial"
    }
}

/// The path as text, and whether that was lossless.
fn path_str(p: &Path) -> (String, bool) {
    match p.to_str() {
        Some(s) => (s.to_owned(), true),
        None => (p.to_string_lossy().into_owned(), false),
    }
}

/// The history line of an outcome (`None` for a warning). `out` lines carry
/// the identity the executor took right after placing the file, so a later
/// undo can tell whether it is still the file we made.
fn outcome_line(job: &str, o: &Outcome) -> Option<Line> {
    Some(match o {
        Outcome::Done { input, output, id } => {
            let (path, ok) = path_str(output);
            let mut l = Line::new("out")
                .pair("job", job)
                .pair("in", &path_str(input).0)
                .pair("path", &path)
                .pair("size", &id.size.to_string())
                .pair("mtime", &id.mtime_ns.to_string())
                .pair("ctime", &id.ctime_ns.to_string())
                .pair("dev", &id.dev.to_string())
                .pair("ino", &id.ino.to_string());
            if !ok {
                l = l.pair("undo", "no");
            }
            l
        }
        Outcome::Skipped { input, reason } => Line::new("skip")
            .pair("job", job)
            .pair("in", &path_str(input).0)
            .pair("reason", reason),
        Outcome::Failed { input, reason } => Line::new("fail")
            .pair("job", job)
            .pair("in", &path_str(input).0)
            .pair("reason", reason),
        Outcome::Warned { .. } => return None,
    })
}

/// Writes a job into the history as it runs: each outcome and folder is
/// appended when it arrives (one `write` per line, so nothing waits in a
/// buffer), and the `job` line with the final status at [`finish`]. A run
/// that dies midway leaves its lines so far, all of placed files, and undo
/// still finds them. Holds one tally entry per input, nothing per outcome.
/// History failures are reported once on stderr and otherwise ignored: the
/// history must never fail the action.
///
/// [`finish`]: Recorder::finish
pub struct Recorder {
    file: Option<File>,
    job: String,
    action: String,
    tally: Tally,
}

impl Recorder {
    pub fn open(job: &JobId, action: &str) -> Recorder {
        let file = open_history()
            .inspect_err(|e| eprintln!("inacircle: history not written: {e}"))
            .ok();
        Recorder {
            file,
            job: job.0.clone(),
            action: action.to_owned(),
            tally: Tally::default(),
        }
    }

    fn append(&mut self, line: &Line) {
        if let Some(f) = &mut self.file {
            let _ = append_line(f, line);
        }
    }

    pub fn outcome(&mut self, o: &Outcome) {
        self.tally.add(o);
        if let Some(l) = outcome_line(&self.job, o) {
            self.append(&l);
        }
    }

    /// A folder made for several outputs (non-UTF-8 ones are not recorded,
    /// so undo never removes them).
    pub fn dir(&mut self, d: &Path) {
        if let (path, true) = path_str(d) {
            let l = Line::new("dir").pair("job", &self.job).pair("path", &path);
            self.append(&l);
        }
    }

    /// Appends the `job` line and returns the per-input tally.
    pub fn finish(mut self, sandboxed: bool) -> Tally {
        let l = Line::new("job")
            .pair("id", &self.job)
            .pair("time", &unix_secs().to_string())
            .pair("action", &self.action)
            .pair("status", status(&self.tally))
            .pair("sandbox", if sandboxed { "yes" } else { "no" });
        self.append(&l);
        self.tally
    }
}

/// A registered running job. The file is removed on drop.
pub struct Running {
    path: Option<PathBuf>,
    job: String,
    action: String,
    inputs: usize,
    started: u64,
    last: Instant,
}

impl Running {
    /// Registers the job; without `$XDG_RUNTIME_DIR` (or on any I/O error)
    /// the registry is skipped silently.
    pub fn start(job: &JobId, action: &str, inputs: usize) -> Running {
        let path = jobs_dir().and_then(|d| {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&d)
                .ok()?;
            Some(d.join(&job.0))
        });
        let mut r = Running {
            path,
            job: job.0.clone(),
            action: action.to_owned(),
            inputs,
            started: unix_secs(),
            last: Instant::now(),
        };
        r.write(0, 0.0);
        r
    }

    /// Rewrites the file, at most four times a second. A call inside the
    /// interval is dropped; every call carries the whole run's current
    /// state, so the next call after the interval writes a value at least
    /// as new as any dropped one, with no pending copy and no timer.
    pub fn update(&mut self, done: usize, progress: f32) {
        if self.last.elapsed() < UPDATE_EVERY {
            return;
        }
        self.last = Instant::now();
        self.write(done, progress);
    }

    fn write(&mut self, done: usize, progress: f32) {
        let Some(path) = &self.path else { return };
        let line = Line::new("job")
            .pair("id", &self.job)
            .pair("pid", &std::process::id().to_string())
            .pair("action", &self.action)
            .pair("inputs", &self.inputs.to_string())
            .pair("done", &done.to_string())
            .pair("progress", &format!("{progress:.3}"))
            .pair("started", &self.started.to_string());
        let tmp = path.with_file_name(format!(".{}.tmp", self.job));
        if fs::write(&tmp, write_line(&line)).is_ok() && fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            let _ = fs::remove_file(p);
        }
    }
}

/// Start time of `pid` in unix seconds, from `/proc/<pid>/stat` (field 22,
/// clock ticks since boot) and the boot time in `/proc/stat`.
fn proc_start_secs(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` may contain spaces and parentheses: fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest starts at field 3 (state), so field 22 is index 19.
    let ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let btime: u64 = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (hz > 0).then(|| btime + ticks / hz as u64)
}

/// Whether the process that registered the job (at `started`) still runs. A
/// pid reused by a later process shows up as a start time after `started`.
fn job_alive(pid: u32, started: Option<u64>) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks existence; no preconditions.
    let r = unsafe { libc::kill(p, 0) };
    if r != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return false;
    }
    // EPERM (another user's process) counts as alive, unless /proc says it
    // started after the job did.
    match (started, proc_start_secs(pid)) {
        (Some(s), Some(ps)) => ps <= s + 2,
        _ => true,
    }
}

/// Running jobs: `(id, action, pid, progress)`. An entry whose process is
/// gone is deleted and left out.
pub fn list_running() -> Vec<(JobId, String, u32, f32)> {
    let Some(dir) = jobs_dir() else {
        return Vec::new();
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if e.file_name().as_bytes().starts_with(b".") {
            continue;
        }
        let Ok(text) = fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(line) = parse_line(&text) else {
            continue;
        };
        if line.kind != "job" {
            continue;
        }
        let (Some(id), Some(Ok(pid))) = (line.get("id"), line.get("pid").map(str::parse::<u32>))
        else {
            continue;
        };
        let started = line.get("started").and_then(|s| s.parse().ok());
        if !job_alive(pid, started) {
            let _ = fs::remove_file(e.path());
            continue;
        }
        let progress = line
            .get("progress")
            .and_then(|p| p.parse().ok())
            .unwrap_or(0.0);
        out.push((
            JobId(id.to_owned()),
            line.get("action").unwrap_or("").to_owned(),
            pid,
            progress,
        ));
    }
    out
}

#[derive(Debug, Default)]
pub struct UndoResult {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<(PathBuf, String)>,
}

struct Out {
    path: PathBuf,
    id: FileId,
    /// False when the path was not valid UTF-8 and the history holds only a
    /// lossy copy.
    undoable: bool,
}

enum Gone {
    Removed,
    Kept(String),
}

/// Removes the file only if it still has the recorded identity. Never
/// follows a symlink at the file's name: the parent directory is opened, the
/// name is lstat'ed relative to it and unlinked relative to it. A parent
/// swapped for another directory or symlink holds a file with another
/// identity, so nothing is removed.
fn remove_if_unchanged(o: &Out) -> Gone {
    if !o.undoable {
        return Gone::Kept("name not representable".to_owned());
    }
    let (Some(parent), Some(name)) = (o.path.parent(), o.path.file_name()) else {
        return Gone::Kept("no parent".to_owned());
    };
    let (Ok(cparent), Ok(cname)) = (
        CString::new(parent.as_os_str().as_bytes()),
        CString::new(name.as_bytes()),
    ) else {
        return Gone::Kept("invalid path".to_owned());
    };
    // SAFETY: cparent is a valid NUL-terminated string.
    let dfd = unsafe {
        libc::open(
            cparent.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if dfd < 0 {
        return match io::Error::last_os_error().kind() {
            io::ErrorKind::NotFound => Gone::Kept("missing".to_owned()),
            e => Gone::Kept(format!("cannot open parent: {e}")),
        };
    }
    let r = (|| {
        // SAFETY: an all-zero stat is a valid out-parameter.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: dfd is open, cname is NUL-terminated, st is writable.
        let rc = unsafe { libc::fstatat(dfd, cname.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            return match io::Error::last_os_error().kind() {
                io::ErrorKind::NotFound => Gone::Kept("missing".to_owned()),
                e => Gone::Kept(format!("cannot stat: {e}")),
            };
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFREG || FileId::from_stat(&st) != o.id {
            return Gone::Kept("changed since".to_owned());
        }
        // SAFETY: dfd is open and cname is NUL-terminated.
        if unsafe { libc::unlinkat(dfd, cname.as_ptr(), 0) } != 0 {
            return Gone::Kept(format!("cannot remove: {}", io::Error::last_os_error()));
        }
        Gone::Removed
    })();
    // SAFETY: dfd is an open descriptor owned here.
    unsafe { libc::close(dfd) };
    r
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn parse_out(l: &Line) -> Option<Out> {
    let n = |k: &str| l.get(k)?.parse::<u64>().ok();
    let t = |k: &str| l.get(k)?.parse::<i128>().ok();
    Some(Out {
        path: PathBuf::from(std::ffi::OsString::from_vec(
            l.get("path")?.as_bytes().to_vec(),
        )),
        id: FileId {
            dev: n("dev")?,
            ino: n("ino")?,
            size: n("size")?,
            mtime_ns: t("mtime")?,
            ctime_ns: t("ctime")?,
        },
        undoable: l.get("undo") != Some("no"),
    })
}

/// Undoes a job: deletes the files it created, but only those still having
/// the recorded identity (dev, inode, size, mtime, ctime); the rest are kept
/// with the reason. Folders recorded as made for this job are removed if
/// empty afterwards. The history is held under an exclusive `flock` for the
/// whole undo, so two undos of one job cannot both run. A job that is
/// still running is refused (`ResourceBusy`): it could place more files.
///
/// The history is streamed: only lines that mention the job are parsed.
pub fn undo(job: &JobId) -> io::Result<UndoResult> {
    if list_running().iter().any(|(id, ..)| id == job) {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "job is running; cancel it first",
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(history_path()?)?;
    // SAFETY: the fd is open for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (mut known, mut undone) = (false, false);
    let (mut outs, mut dirs) = (Vec::new(), Vec::new());
    {
        let mut rd = BufReader::new(&file);
        let mut buf = Vec::new();
        rd.read_until(b'\n', &mut buf)?;
        let head = String::from_utf8_lossy(&buf);
        let head = head.trim_end_matches('\n');
        let ok = head
            .split_once(' ')
            .is_some_and(|(m, v)| m == MAGIC && v.parse::<u32>().is_ok_and(|v| v == VERSION));
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad or unsupported history header",
            ));
        }
        let needle = format!("job={}", job.0);
        let id_needle = format!("id={}", job.0);
        loop {
            buf.clear();
            if rd.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            // A last line cut off mid-write has no newline: skip it.
            if buf.pop() != Some(b'\n') {
                break;
            }
            if !(contains(&buf, needle.as_bytes()) || contains(&buf, id_needle.as_bytes())) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&buf) else {
                continue;
            };
            let Ok(l) = parse_line(text) else { continue };
            match l.kind.as_str() {
                "job" if l.get("id") == Some(&job.0) => known = true,
                "undo" if l.get("job") == Some(&job.0) => undone = true,
                "out" if l.get("job") == Some(&job.0) => {
                    known = true;
                    outs.extend(parse_out(&l));
                }
                "dir" if l.get("job") == Some(&job.0) => {
                    known = true;
                    dirs.extend(l.get("path").map(PathBuf::from));
                }
                "skip" | "fail" if l.get("job") == Some(&job.0) => known = true,
                _ => {}
            }
        }
    }
    if !known {
        return Err(io::Error::new(io::ErrorKind::NotFound, "unknown job"));
    }
    if undone {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "already undone",
        ));
    }
    let mut res = UndoResult::default();
    for o in &outs {
        match remove_if_unchanged(o) {
            Gone::Removed => res.removed.push(o.path.clone()),
            Gone::Kept(why) => res.kept.push((o.path.clone(), why)),
        }
    }
    for d in &dirs {
        let _ = fs::remove_dir(d); // never follows a symlink; fails unless empty
    }
    append_line(
        &mut file,
        &Line::new("undo")
            .pair("job", &job.0)
            .pair("time", &unix_secs().to_string())
            .pair("removed", &res.removed.len().to_string())
            .pair("kept", &res.kept.len().to_string()),
    )?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xdg::tests::with_env;
    use std::os::unix::fs::symlink;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p =
                std::env::temp_dir().join(format!("engine-journal-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(p.join("state")).unwrap();
            fs::create_dir_all(p.join("run")).unwrap();
            fs::create_dir_all(p.join("out")).unwrap();
            Tmp(p)
        }
        fn env<R>(&self, f: impl FnOnce() -> R) -> R {
            let s = self.0.join("state");
            let r = self.0.join("run");
            with_env(
                &[
                    ("XDG_STATE_HOME", Some(s.to_str().unwrap())),
                    ("XDG_RUNTIME_DIR", Some(r.to_str().unwrap())),
                ],
                f,
            )
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn zero_id() -> FileId {
        FileId {
            dev: 0,
            ino: 0,
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        }
    }

    fn id_of(p: &Path) -> FileId {
        use std::os::unix::fs::MetadataExt;
        let m = fs::symlink_metadata(p).unwrap();
        FileId {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ctime_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
        }
    }

    /// An outcome as the executor makes it: identity taken now, at placement.
    fn done(input: &str, output: &Path) -> Outcome {
        Outcome::Done {
            input: PathBuf::from(input),
            output: output.to_owned(),
            id: id_of(output),
        }
    }

    /// A finished job's outcomes, as the old one-shot record took them.
    struct Rec {
        outcomes: Vec<Outcome>,
        dirs: Vec<PathBuf>,
    }

    fn report(outcomes: Vec<Outcome>) -> Rec {
        Rec {
            outcomes,
            dirs: Vec::new(),
        }
    }

    /// Records a whole job through the incremental recorder.
    fn record(job: &JobId, action: &str, r: &Rec) {
        let mut rec = Recorder::open(job, action);
        for d in &r.dirs {
            rec.dir(d);
        }
        for o in &r.outcomes {
            rec.outcome(o);
        }
        rec.finish(true);
    }

    #[test]
    fn lines_are_written_as_outcomes_arrive_and_a_dead_run_is_undoable() {
        let t = Tmp::new("stream");
        t.env(|| {
            let (a, b) = (t.0.join("out/a.png"), t.0.join("out/b.png"));
            fs::write(&a, "aaa").unwrap();
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            rec.outcome(&done("/in/a", &a));
            let hist = fs::read_to_string(history_path().unwrap()).unwrap();
            assert!(hist.contains("out\tjob="), "{hist}");
            assert!(!hist.contains("id="), "job line only at the end: {hist}");
            fs::write(&b, "bb").unwrap();
            rec.outcome(&done("/in/b", &b));
            // The run dies here: no finish, no `job` line.
            drop(rec);
            let r = undo(&job).unwrap();
            assert_eq!(r.removed, vec![a.clone(), b.clone()]);
            assert!(!a.exists() && !b.exists());
        });
    }

    #[test]
    fn finish_writes_status_and_returns_tally() {
        let t = Tmp::new("finish");
        t.env(|| {
            let a = t.0.join("out/a.png");
            fs::write(&a, "a").unwrap();
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            rec.outcome(&done("/in/a", &a));
            rec.outcome(&Outcome::Failed {
                input: "/in/b".into(),
                reason: "broken".into(),
            });
            let tally = rec.finish(false);
            assert_eq!(tally.counts(), (1, 0, 1));
            let hist = fs::read_to_string(history_path().unwrap()).unwrap();
            let last = hist.lines().last().unwrap();
            assert!(last.starts_with(&format!("job\tid={}\t", job.0)), "{last}");
            assert!(
                last.ends_with("\taction=convert\tstatus=partial\tsandbox=no"),
                "{last}"
            );
        });
    }

    #[test]
    fn record_and_undo_removes_outputs() {
        let t = Tmp::new("undo");
        t.env(|| {
            let (a, b) = (t.0.join("out/a.png"), t.0.join("out/b.png"));
            fs::write(&a, "aaa").unwrap();
            fs::write(&b, "bb").unwrap();
            let job = new_job_id();
            record(
                &job,
                "convert",
                &report(vec![done("/in/a", &a), done("/in/b", &b)]),
            );
            let hist = fs::read_to_string(history_path().unwrap()).unwrap();
            assert!(hist.starts_with("inacircle-history 0\n"));
            assert!(hist.contains("status=done\tsandbox=yes"));
            let r = undo(&job).unwrap();
            assert_eq!(r.removed, vec![a.clone(), b.clone()]);
            assert!(r.kept.is_empty());
            assert!(!a.exists() && !b.exists());
            assert!(
                t.0.join("out").exists(),
                "a user directory is never removed"
            );
        });
    }

    #[test]
    fn undo_removes_recorded_empty_folder_only() {
        let t = Tmp::new("folder");
        t.env(|| {
            let dir = t.0.join("out/doc");
            fs::create_dir(&dir).unwrap();
            let (a, b) = (dir.join("1.png"), dir.join("2.png"));
            fs::write(&a, "1").unwrap();
            fs::write(&b, "22").unwrap();
            let mut rep = report(vec![done("/in/doc.pdf", &a), done("/in/doc.pdf", &b)]);
            rep.dirs = vec![dir.clone()];
            let job = JobId("1-1".into());
            record(&job, "pdf", &rep);
            undo(&job).unwrap();
            assert!(!dir.exists());

            // A kept file keeps its folder.
            fs::create_dir(&dir).unwrap();
            fs::write(&a, "1").unwrap();
            fs::write(&b, "22").unwrap();
            let job = JobId("1-2".into());
            let mut rep = report(vec![done("/in/doc.pdf", &a), done("/in/doc.pdf", &b)]);
            rep.dirs = vec![dir.clone()];
            record(&job, "pdf", &rep);
            fs::write(&b, "edited!").unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(r.kept.len(), 1);
            assert!(dir.exists() && b.exists() && !a.exists());

            // An unrecorded folder (the user's own) is never removed, even
            // with two outputs in it.
            let own = t.0.join("out/own");
            fs::create_dir(&own).unwrap();
            let (f, g) = (own.join("x"), own.join("y"));
            fs::write(&f, "1").unwrap();
            fs::write(&g, "1").unwrap();
            let job = JobId("1-3".into());
            record(&job, "a", &report(vec![done("/i", &f), done("/i", &g)]));
            undo(&job).unwrap();
            assert!(own.exists());
        });
    }

    #[test]
    fn undo_keeps_changed_file() {
        let t = Tmp::new("changed");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "one").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            fs::write(&f, "longer content").unwrap();
            let r = undo(&job).unwrap();
            assert!(r.removed.is_empty());
            assert_eq!(r.kept, vec![(f.clone(), "changed since".to_owned())]);
            assert_eq!(fs::read_to_string(&f).unwrap(), "longer content");
        });
    }

    #[test]
    fn undo_keeps_replaced_same_size_file() {
        let t = Tmp::new("replaced");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "one").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            // Same size, other inode (and mtime forced equal is not needed:
            // the inode differs).
            let other = t.0.join("out/y");
            fs::write(&other, "two").unwrap();
            fs::rename(&other, &f).unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(r.kept, vec![(f.clone(), "changed since".to_owned())]);
            assert!(f.exists());
        });
    }

    #[test]
    fn undo_never_follows_or_deletes_symlinks() {
        let t = Tmp::new("symlink");
        t.env(|| {
            let f = t.0.join("out/x");
            let victim = t.0.join("victim");
            fs::write(&f, "data").unwrap();
            fs::write(&victim, "data").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            fs::remove_file(&f).unwrap();
            symlink(&victim, &f).unwrap();
            let r = undo(&job).unwrap();
            assert!(r.removed.is_empty());
            assert_eq!(r.kept[0].1, "changed since");
            assert!(victim.exists());
            assert!(fs::symlink_metadata(&f).unwrap().file_type().is_symlink());
        });
    }

    #[test]
    fn undo_reports_missing() {
        let t = Tmp::new("missing");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            fs::remove_file(&f).unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(r.kept, vec![(f, "missing".to_owned())]);
        });
    }

    #[test]
    fn undo_twice_is_already_exists() {
        let t = Tmp::new("twice");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            undo(&job).unwrap();
            assert_eq!(undo(&job).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        });
    }

    #[test]
    fn unknown_job_not_found() {
        let t = Tmp::new("unknown");
        t.env(|| {
            // No history at all, then a history without that job.
            assert_eq!(
                undo(&JobId("9-9".into())).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            record(&JobId("1-1".into()), "a", &report(vec![]));
            assert_eq!(
                undo(&JobId("9-9".into())).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        });
    }

    #[test]
    fn paths_with_tabs_newlines_eq_roundtrip() {
        let t = Tmp::new("weird");
        t.env(|| {
            let f = t.0.join("out/a\tb\nc=d\\e #x");
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a=b", &report(vec![done("/in/p\tq\n=", &f)]));
            let r = undo(&job).unwrap();
            assert_eq!(r.removed, vec![f.clone()]);
            assert!(!f.exists());
        });
    }

    #[test]
    fn corrupt_and_partial_last_line_is_skipped() {
        let t = Tmp::new("corrupt");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            let mut h = OpenOptions::new()
                .append(true)
                .open(history_path().unwrap())
                .unwrap();
            h.write_all(b"garbage\\q\tzz\nundo\tjob=10-1\ttime=5\trem")
                .unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(r.removed, vec![f]);
        });
    }

    #[test]
    fn status_values() {
        let d = Outcome::Done {
            input: "/a".into(),
            output: "/o".into(),
            id: zero_id(),
        };
        let s = Outcome::Skipped {
            input: "/b".into(),
            reason: "x".into(),
        };
        let f = Outcome::Failed {
            input: "/b".into(),
            reason: "x".into(),
        };
        let st = |os: &[&Outcome]| {
            let mut t = Tally::default();
            for o in os {
                t.add(o);
            }
            status(&t)
        };
        assert_eq!(st(&[&d]), "done");
        assert_eq!(st(&[&d, &s]), "partial");
        assert_eq!(st(&[&f]), "failed");
        assert_eq!(st(&[]), "failed");
    }

    #[test]
    fn running_file_lifecycle() {
        let t = Tmp::new("running");
        t.env(|| {
            let job = JobId("10-1".into());
            let path = jobs_dir().unwrap().join("10-1");
            {
                let mut r = Running::start(&job, "convert", 3);
                assert!(path.exists());
                let l = list_running();
                assert_eq!(l.len(), 1);
                assert_eq!(l[0].0, job);
                assert_eq!(l[0].1, "convert");
                assert_eq!(l[0].2, std::process::id());
                r.update(1, 0.5); // throttled: too soon after start
                assert_eq!(list_running()[0].3, 0.0);
                std::thread::sleep(UPDATE_EVERY + Duration::from_millis(20));
                r.update(1, 0.5);
                assert_eq!(list_running()[0].3, 0.5);
            }
            assert!(!path.exists());
            assert!(list_running().is_empty());
        });
    }

    #[test]
    fn job_ids_differ_within_one_second_and_process() {
        let (a, b) = (new_job_id(), new_job_id());
        assert_ne!(a, b);
        let parts: Vec<&str> = a.0.split('-').collect();
        assert_eq!(parts.len(), 3, "{}", a.0);
        assert!(parts[0].parse::<u64>().is_ok(), "{}", a.0);
        assert_eq!(parts[1], std::process::id().to_string());
        assert!(
            parts[2].len() == 8 && parts[2].bytes().all(|c| c.is_ascii_hexdigit()),
            "{}",
            a.0
        );
    }

    #[test]
    fn undo_of_a_running_job_is_refused() {
        let t = Tmp::new("undorun");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "x").unwrap();
            let job = JobId("10-1".into());
            let running = Running::start(&job, "a", 1);
            let mut rec = Recorder::open(&job, "a");
            rec.outcome(&done("/i", &f));
            let e = undo(&job).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::ResourceBusy);
            assert!(f.exists());
            drop(running);
            drop(rec.finish(true));
            assert_eq!(undo(&job).unwrap().removed, std::slice::from_ref(&f));
            assert!(!f.exists());
        });
    }

    #[test]
    fn throttled_update_is_written_by_the_next_call_after_the_interval() {
        let t = Tmp::new("throttle");
        t.env(|| {
            let job = JobId("10-2".into());
            let mut r = Running::start(&job, "convert", 4);
            r.update(1, 0.25); // dropped: too soon after start
            r.update(2, 0.5); // dropped
            assert_eq!(list_running()[0].3, 0.0);
            std::thread::sleep(UPDATE_EVERY + Duration::from_millis(20));
            r.update(3, 0.75);
            let f = fs::read_to_string(jobs_dir().unwrap().join("10-2")).unwrap();
            assert!(f.contains("\tdone=3\t"), "{f}");
            assert_eq!(list_running()[0].3, 0.75);
        });
    }

    #[test]
    fn no_runtime_dir_skips_registry_but_history_works() {
        let t = Tmp::new("norun");
        let s = t.0.join("state");
        with_env(
            &[
                ("XDG_STATE_HOME", Some(s.to_str().unwrap())),
                ("XDG_RUNTIME_DIR", None),
            ],
            || {
                let job = JobId("10-1".into());
                drop(Running::start(&job, "a", 1));
                assert!(list_running().is_empty());
                record(&job, "a", &report(vec![]));
                assert!(history_path().unwrap().exists());
            },
        );
    }

    #[test]
    fn dead_pid_entries_pruned() {
        let t = Tmp::new("dead");
        t.env(|| {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let line = Line::new("job")
                .pair("id", "5-5")
                .pair("pid", &pid.to_string())
                .pair("action", "x")
                .pair("progress", "0.1");
            let p = dir.join("5-5");
            fs::write(&p, write_line(&line)).unwrap();
            assert!(list_running().is_empty());
            assert!(!p.exists());
        });
    }

    #[test]
    fn concurrent_appends_do_not_interleave() {
        let t = Tmp::new("conc");
        t.env(|| {
            let long = "x".repeat(6000);
            let work = |tag: &'static str| {
                let long = long.clone();
                std::thread::spawn(move || {
                    for i in 0..1000 {
                        let out = Outcome::Done {
                            input: "/in".into(),
                            output: Path::new("/nonexistent").join(format!("{tag}{i}{long}")),
                            id: zero_id(),
                        };
                        record(&JobId(format!("{tag}-{i}")), "a", &report(vec![out]));
                    }
                })
            };
            let (a, b) = (work("a"), work("b"));
            a.join().unwrap();
            b.join().unwrap();
            let text = fs::read_to_string(history_path().unwrap()).unwrap();
            let (v, lines) = manifest::parse(&text, MAGIC).unwrap();
            assert_eq!(v, 0);
            assert_eq!(lines.len(), 4000);
            assert_eq!(lines.iter().filter(|l| l.kind == "job").count(), 2000);
            assert!(
                lines
                    .iter()
                    .filter(|l| l.kind == "out")
                    .all(|l| l.get("ino") == Some("0"))
            );
        });
    }

    #[test]
    fn edit_between_placement_and_record_is_kept() {
        let t = Tmp::new("late");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "ours").unwrap();
            // The executor took the identity at placement ...
            let outcome = done("/i", &f);
            // ... the user edited the file before the run ended ...
            std::thread::sleep(Duration::from_millis(5));
            fs::write(&f, "user edit").unwrap();
            // ... and only then is the history written.
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![outcome]));
            let r = undo(&job).unwrap();
            assert!(r.removed.is_empty());
            assert_eq!(r.kept, vec![(f.clone(), "changed since".to_owned())]);
            assert_eq!(fs::read_to_string(&f).unwrap(), "user edit");
        });
    }

    #[test]
    fn every_identity_field_must_match() {
        let t = Tmp::new("fields");
        t.env(|| {
            let f = t.0.join("out/x");
            type Change = fn(&mut FileId);
            let variants: [(&str, Change); 5] = [
                ("dev", |i| i.dev += 1),
                ("ino", |i| i.ino += 1),
                ("size", |i| i.size += 1),
                ("mtime", |i| i.mtime_ns += 1),
                ("ctime", |i| i.ctime_ns += 1),
            ];
            for (n, (name, change)) in variants.iter().enumerate() {
                fs::write(&f, "d").unwrap();
                let mut id = id_of(&f);
                change(&mut id);
                let out = Outcome::Done {
                    input: "/i".into(),
                    output: f.clone(),
                    id,
                };
                let job = JobId(format!("20-{n}"));
                record(&job, "a", &report(vec![out]));
                let r = undo(&job).unwrap();
                assert!(f.exists(), "{name} differing must keep the file");
                assert_eq!(r.kept.len(), 1, "{name}");
            }
        });
    }

    #[test]
    fn replaced_file_with_restored_mtime_is_kept() {
        let t = Tmp::new("restored");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "one").unwrap();
            let rec = id_of(&f);
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            // Same size, other inode, mtime put back by hand.
            let other = t.0.join("out/y");
            fs::write(&other, "two").unwrap();
            let when = UNIX_EPOCH
                + Duration::new(
                    (rec.mtime_ns / 1_000_000_000) as u64,
                    (rec.mtime_ns % 1_000_000_000) as u32,
                );
            File::options()
                .write(true)
                .open(&other)
                .unwrap()
                .set_modified(when)
                .unwrap();
            fs::rename(&other, &f).unwrap();
            assert_eq!(id_of(&f).mtime_ns, rec.mtime_ns);
            let r = undo(&job).unwrap();
            assert_eq!(r.kept.len(), 1);
            assert_eq!(fs::read_to_string(&f).unwrap(), "two");
        });
    }

    #[test]
    fn edit_with_restored_mtime_is_kept_by_ctime() {
        let t = Tmp::new("ctime");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "one").unwrap();
            let rec = id_of(&f);
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            std::thread::sleep(Duration::from_millis(5));
            let h = File::options().write(true).open(&f).unwrap();
            (&h).write_all(b"two").unwrap(); // same size, same inode
            let when = UNIX_EPOCH
                + Duration::new(
                    (rec.mtime_ns / 1_000_000_000) as u64,
                    (rec.mtime_ns % 1_000_000_000) as u32,
                );
            h.set_modified(when).unwrap();
            drop(h);
            let now = id_of(&f);
            assert_eq!(
                (now.ino, now.size, now.mtime_ns),
                (rec.ino, rec.size, rec.mtime_ns)
            );
            let r = undo(&job).unwrap();
            assert_eq!(r.kept.len(), 1);
            assert_eq!(fs::read_to_string(&f).unwrap(), "two");
        });
    }

    #[test]
    fn swapped_parent_symlink_keeps_lookalike() {
        let t = Tmp::new("parent");
        t.env(|| {
            let sub = t.0.join("out/sub");
            fs::create_dir(&sub).unwrap();
            let f = sub.join("x");
            fs::write(&f, "data").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            // The folder is moved away and a symlink to another directory
            // holding a same-named, same-content file takes its place.
            let elsewhere = t.0.join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            fs::write(elsewhere.join("x"), "data").unwrap();
            fs::rename(&sub, t.0.join("moved")).unwrap();
            symlink(&elsewhere, &sub).unwrap();
            let r = undo(&job).unwrap();
            assert!(r.removed.is_empty());
            assert_eq!(r.kept[0].1, "changed since");
            assert!(elsewhere.join("x").exists());
            assert!(t.0.join("moved/x").exists());
        });
    }

    #[test]
    fn concurrent_undos_one_wins() {
        let t = Tmp::new("race");
        t.env(|| {
            let f = t.0.join("out/x");
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            let results: Vec<_> = (0..4)
                .map(|_| {
                    let j = job.clone();
                    std::thread::spawn(move || undo(&j))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect();
            let ok = results.iter().filter(|r| r.is_ok()).count();
            let dup = results
                .iter()
                .filter(|r| {
                    r.as_ref()
                        .is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
                })
                .count();
            assert_eq!((ok, dup), (1, 3));
            let text = fs::read_to_string(history_path().unwrap()).unwrap();
            assert_eq!(text.lines().filter(|l| l.starts_with("undo\t")).count(), 1);
        });
    }

    #[test]
    fn non_utf8_path_is_recorded_lossy_and_kept() {
        let t = Tmp::new("bytes");
        t.env(|| {
            let f = t.0.join(std::ffi::OsStr::from_bytes(b"out/bad\xff.png"));
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            let hist = fs::read_to_string(history_path().unwrap()).unwrap();
            assert!(hist.contains("undo=no"));
            assert!(hist.contains("bad\u{fffd}.png"));
            let r = undo(&job).unwrap();
            assert!(r.removed.is_empty());
            assert_eq!(r.kept.len(), 1);
            assert_eq!(r.kept[0].1, "name not representable");
            assert!(f.exists());
        });
    }

    #[test]
    fn unsupported_header_version_is_rejected() {
        let t = Tmp::new("version");
        t.env(|| {
            let p = history_path().unwrap();
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "inacircle-history 1\njob\tid=1-1\n").unwrap();
            let e = undo(&JobId("1-1".into())).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn history_is_created_without_hard_links() {
        let t = Tmp::new("nolink");
        t.env(|| {
            let f = open_history_with(|_, _| Err(io::Error::from_raw_os_error(libc::EPERM)));
            drop(f.unwrap());
            assert_eq!(
                fs::read_to_string(history_path().unwrap()).unwrap(),
                "inacircle-history 0\n"
            );
            // And an existing file is just appended to.
            drop(open_history_with(|_, _| Err(io::Error::from_raw_os_error(libc::EPERM))).unwrap());
            assert_eq!(
                fs::read_to_string(history_path().unwrap()).unwrap(),
                "inacircle-history 0\n"
            );
        });
    }

    #[test]
    fn reused_pid_entry_is_pruned() {
        let t = Tmp::new("reuse");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            // A live pid (ours) that registered the job long before it started.
            let old = Line::new("job")
                .pair("id", "5-5")
                .pair("pid", &std::process::id().to_string())
                .pair("action", "x")
                .pair("started", &(unix_secs() - 100_000).to_string());
            fs::write(dir.join("5-5"), write_line(&old)).unwrap();
            assert!(list_running().is_empty());
            assert!(!dir.join("5-5").exists());
        });
    }

    fn vm_hwm_kb() -> u64 {
        let s = fs::read_to_string("/proc/self/status").unwrap();
        let l = s.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
        l.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    /// Runs `helper_undo_memory` alone in a child process, so other tests'
    /// allocations cannot disturb its peak-RSS reading.
    #[test]
    fn undo_streams_a_large_history() {
        let t = Tmp::new("mem");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "journal::tests::helper_undo_memory",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("INACIRCLE_HELPER", &t.0)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "helper failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("ran helper"));
    }

    /// Child side of `undo_streams_a_large_history`; a no-op otherwise.
    #[test]
    fn helper_undo_memory() {
        let Some(dir) = std::env::var_os("INACIRCLE_HELPER").map(PathBuf::from) else {
            return;
        };
        let state = dir.join("state");
        let state_s = state.to_str().unwrap().to_owned();
        // No registry: the env inherited from the parent may name another
        // test's runtime dir, whose "running" job 10-1 would block the undo.
        let v = [
            ("XDG_STATE_HOME", Some(&*state_s)),
            ("XDG_RUNTIME_DIR", None),
        ];
        with_env(&v, || {
            fs::create_dir_all(dir.join("out")).unwrap();
            let f = dir.join("out/x");
            fs::write(&f, "d").unwrap();
            let job = JobId("10-1".into());
            record(&job, "a", &report(vec![done("/i", &f)]));
            // ~50 MB of other jobs' lines in front of ours.
            let p = history_path().unwrap();
            let mut w = io::BufWriter::new(OpenOptions::new().append(true).open(&p).unwrap());
            let filler = Line::new("out")
                .pair("job", "99-9")
                .pair("in", "/some/input/file")
                .pair("path", &format!("/some/output/{}", "p".repeat(120)))
                .pair("size", "1")
                .pair("mtime", "1")
                .pair("ctime", "1")
                .pair("dev", "1")
                .pair("ino", "1");
            let text = write_line(&filler);
            let n = 50_000_000 / text.len() + 1;
            for _ in 0..n {
                w.write_all(text.as_bytes()).unwrap();
            }
            w.flush().unwrap();
            drop(w);
            assert!(fs::metadata(&p).unwrap().len() >= 50_000_000);
            let before = vm_hwm_kb();
            let r = undo(&job).unwrap();
            let after = vm_hwm_kb();
            assert_eq!(r.removed, vec![f]);
            assert!(
                after.saturating_sub(before) < 8 * 1024,
                "peak RSS grew by {} kB",
                after.saturating_sub(before)
            );
        });
        println!("ran helper");
    }
}
