//! The journal: an append-only history log, the running-jobs registry and
//! undo. All formats go through the `manifest` crate.
//!
//! History (`$XDG_STATE_HOME/inacircle/history`), header `inacircle-history 0`:
//! `job`, `out`, `skip`, `fail` and `undo` lines, each written with a single
//! `write` on an `O_APPEND` descriptor. Running jobs live in
//! `$XDG_RUNTIME_DIR/inacircle/jobs/<job-id>`, one `job` line per file.

use crate::exec::{Outcome, Report};
use crate::xdg;
use manifest::{Line, parse_line, write_line};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &str = "inacircle-history";
/// Running-job files are rewritten at most this often.
const UPDATE_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(pub String);

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<unix-seconds>-<pid>`.
pub fn new_job_id() -> JobId {
    JobId(format!("{}-{}", unix_secs(), std::process::id()))
}

fn history_path() -> PathBuf {
    xdg::state_home().join("inacircle").join("history")
}

fn jobs_dir() -> Option<PathBuf> {
    xdg::runtime_dir().map(|r| r.join("inacircle").join("jobs"))
}

fn mtime_ns(m: &fs::Metadata) -> i128 {
    i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec())
}

/// Opens the history for appending. The header is written when the file is
/// created: a temp file holding it is hard-linked into place, which is atomic
/// and fails if the file exists, so no line can ever precede the header.
fn open_history() -> io::Result<File> {
    let path = history_path();
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
    fs::write(&tmp, manifest::header(MAGIC, 0))?;
    let linked = fs::hard_link(&tmp, &path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => append(),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => append(),
        Err(e) => Err(e),
    }
}

/// One line, one `write`.
fn append_line(f: &mut File, line: &Line) -> io::Result<()> {
    let text = write_line(line);
    let n = f.write(text.as_bytes())?;
    if n < text.len() {
        // A short write on a regular file is exceptional; finish the line.
        f.write_all(&text.as_bytes()[n..])?;
    }
    Ok(())
}

fn status(report: &Report) -> &'static str {
    let mut inputs: Vec<&Path> = Vec::new();
    let mut done: HashSet<&Path> = HashSet::new();
    for o in &report.outcomes {
        match o {
            Outcome::Done { input, .. } => {
                inputs.push(input);
                done.insert(input);
            }
            Outcome::Skipped { input, .. } | Outcome::Failed { input, .. } => inputs.push(input),
            Outcome::Warned { .. } => {}
        }
    }
    if done.is_empty() {
        "failed"
    } else if inputs.iter().all(|i| done.contains(i)) {
        "done"
    } else {
        "partial"
    }
}

fn path_str(p: &Path) -> String {
    String::from_utf8_lossy(p.as_os_str().as_bytes()).into_owned()
}

/// Appends the job's lines to the history. `out` lines carry the size, mtime
/// (ns) and inode taken with lstat now, so a later undo can tell whether the
/// file is still the one we made. Failures are ignored: the history must
/// never fail the action.
pub fn record(job: &JobId, action: &str, report: &Report) {
    let _ = try_record(job, action, report);
}

fn try_record(job: &JobId, action: &str, report: &Report) -> io::Result<()> {
    let mut f = open_history()?;
    let head = Line::new("job")
        .pair("id", &job.0)
        .pair("time", &unix_secs().to_string())
        .pair("action", action)
        .pair("status", status(report))
        .pair("sandbox", if report.sandboxed { "yes" } else { "no" });
    append_line(&mut f, &head)?;
    for o in &report.outcomes {
        let line = match o {
            Outcome::Done { input, output } => {
                let (size, mtime, ino) = match fs::symlink_metadata(output) {
                    Ok(m) => (m.len(), mtime_ns(&m), m.ino()),
                    Err(_) => (0, 0, 0),
                };
                Line::new("out")
                    .pair("job", &job.0)
                    .pair("in", &path_str(input))
                    .pair("path", &path_str(output))
                    .pair("size", &size.to_string())
                    .pair("mtime", &mtime.to_string())
                    .pair("ino", &ino.to_string())
            }
            Outcome::Skipped { input, reason } => Line::new("skip")
                .pair("job", &job.0)
                .pair("in", &path_str(input))
                .pair("reason", reason),
            Outcome::Failed { input, reason } => Line::new("fail")
                .pair("job", &job.0)
                .pair("in", &path_str(input))
                .pair("reason", reason),
            Outcome::Warned { .. } => continue,
        };
        append_line(&mut f, &line)?;
    }
    Ok(())
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

    /// Rewrites the file, at most four times a second.
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

fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks existence; no preconditions.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Running jobs: `(id, action, pid, progress)`. An entry whose pid is dead is
/// deleted and left out.
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
        if !pid_alive(pid) {
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

/// Parsed history lines. A corrupt line, or a last line cut off mid-write
/// (no trailing newline), is skipped.
fn read_history() -> io::Result<Vec<Line>> {
    let bytes = fs::read(history_path())?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    // The last piece is empty after a final newline, else a line cut off
    // mid-write; either way it is dropped.
    lines.pop();
    let mut it = lines.into_iter();
    match it.next() {
        Some(h) if h.split_once(' ').is_some_and(|(m, _)| m == MAGIC) => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad history header",
            ));
        }
    }
    Ok(it
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| parse_line(l).ok())
        .collect())
}

#[derive(Debug, Default)]
pub struct UndoResult {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<(PathBuf, String)>,
}

struct Out {
    input: String,
    path: PathBuf,
    size: u64,
    mtime: i128,
    ino: u64,
}

enum Gone {
    Removed,
    Kept(String),
}

/// Removes `path` only if it is still what we recorded. Never follows a
/// symlink: the parent directory is opened, the name is lstat'ed relative to
/// it and unlinked relative to it.
fn remove_if_unchanged(o: &Out) -> Gone {
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
        let mtime = i128::from(st.st_mtime) * 1_000_000_000 + i128::from(st.st_mtime_nsec);
        let same = st.st_mode & libc::S_IFMT == libc::S_IFREG
            && st.st_size as u64 == o.size
            && mtime == o.mtime
            && st.st_ino as u64 == o.ino;
        if !same {
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

/// Undoes a job: deletes the files it created, but only those unchanged
/// since (size, mtime, inode); the rest are kept with the reason. A folder
/// made for several outputs of one input is removed if it ended up empty.
pub fn undo(job: &JobId) -> io::Result<UndoResult> {
    let lines = read_history()?;
    let is_job =
        |l: &Line| l.get("job") == Some(&job.0) || (l.kind == "job" && l.get("id") == Some(&job.0));
    if !lines.iter().any(|l| l.kind == "job" && is_job(l)) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "unknown job"));
    }
    if lines.iter().any(|l| l.kind == "undo" && is_job(l)) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "already undone",
        ));
    }
    let outs: Vec<Out> = lines
        .iter()
        .filter(|l| l.kind == "out" && is_job(l))
        .filter_map(|l| {
            Some(Out {
                input: l.get("in")?.to_owned(),
                path: PathBuf::from(std::ffi::OsStr::from_bytes(l.get("path")?.as_bytes())),
                size: l.get("size")?.parse().ok()?,
                mtime: l.get("mtime")?.parse().ok()?,
                ino: l.get("ino")?.parse().ok()?,
            })
        })
        .collect();
    let mut res = UndoResult::default();
    for o in &outs {
        match remove_if_unchanged(o) {
            Gone::Removed => res.removed.push(o.path.clone()),
            Gone::Kept(why) => res.kept.push((o.path.clone(), why)),
        }
    }
    // Several outputs of one input share a folder made by place_many; a
    // single output may sit in a user's own directory, which is never touched.
    let mut folders: Vec<&Path> = Vec::new();
    for o in &outs {
        let Some(p) = o.path.parent() else { continue };
        let shared = outs
            .iter()
            .filter(|x| x.input == o.input && x.path.parent() == Some(p))
            .count()
            > 1;
        if shared && !folders.contains(&p) {
            folders.push(p);
        }
    }
    for p in folders {
        let _ = fs::remove_dir(p); // fails unless empty
    }
    let mut f = open_history()?;
    append_line(
        &mut f,
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

    fn done(input: &str, output: &Path) -> Outcome {
        Outcome::Done {
            input: PathBuf::from(input),
            output: output.to_owned(),
        }
    }

    fn report(outcomes: Vec<Outcome>) -> Report {
        Report {
            outcomes,
            sandboxed: true,
        }
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
            let hist = fs::read_to_string(history_path()).unwrap();
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
    fn undo_removes_emptied_place_many_folder() {
        let t = Tmp::new("folder");
        t.env(|| {
            let dir = t.0.join("out/doc");
            fs::create_dir(&dir).unwrap();
            let (a, b) = (dir.join("1.png"), dir.join("2.png"));
            fs::write(&a, "1").unwrap();
            fs::write(&b, "22").unwrap();
            let job = new_job_id();
            record(
                &job,
                "pdf",
                &report(vec![done("/in/doc.pdf", &a), done("/in/doc.pdf", &b)]),
            );
            undo(&job).unwrap();
            assert!(!dir.exists());

            // A kept file keeps its folder.
            fs::create_dir(&dir).unwrap();
            fs::write(&a, "1").unwrap();
            fs::write(&b, "22").unwrap();
            let job = JobId("1-2".into());
            record(
                &job,
                "pdf",
                &report(vec![done("/in/doc.pdf", &a), done("/in/doc.pdf", &b)]),
            );
            fs::write(&b, "edited!").unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(r.kept.len(), 1);
            assert!(dir.exists() && b.exists() && !a.exists());
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
                .open(history_path())
                .unwrap();
            h.write_all(b"garbage\\q\tzz\nundo\tjob=10-1\ttime=5\trem")
                .unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(r.removed, vec![f]);
        });
    }

    #[test]
    fn status_values() {
        let d = done("/a", Path::new("/o"));
        let s = Outcome::Skipped {
            input: "/b".into(),
            reason: "x".into(),
        };
        let f = Outcome::Failed {
            input: "/b".into(),
            reason: "x".into(),
        };
        assert_eq!(status(&report(vec![d.clone()])), "done");
        assert_eq!(status(&report(vec![d.clone(), s.clone()])), "partial");
        assert_eq!(status(&report(vec![f.clone()])), "failed");
        assert_eq!(status(&report(vec![])), "failed");
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
                assert!(history_path().exists());
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
                        let out = Path::new("/nonexistent").join(format!("{tag}{i}{long}"));
                        record(
                            &JobId(format!("{tag}-{i}")),
                            "a",
                            &report(vec![done("/in", &out)]),
                        );
                    }
                })
            };
            let (a, b) = (work("a"), work("b"));
            a.join().unwrap();
            b.join().unwrap();
            let text = fs::read_to_string(history_path()).unwrap();
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
}
