//! The executor and supervisor: runs an action route (one or two actions)
//! over a batch of input files, sandboxed, speaking contract v0
//! (docs/architecture.md, "Invocation"), and moves the results into place
//! with the output policy.
//!
//! Inputs are grouped by their final target folder; each group gets one
//! work dir inside that folder (so placement is a same-filesystem rename)
//! and one process per route step (`batch=no`: one group per input, run in
//! turn). Step `k` runs with `--work <work>/step<k>` and reads its inputs
//! from `<work>/step<k>.inputs`; step 2's inputs are step 1's outputs, so
//! intermediate files never leave the work dir. Only the last step's
//! outputs are placed.
//!
//! The supervisor is a single-threaded `poll` loop over the child's stdout
//! and stderr, the cancel signalfd, the child's pidfd and the observer's
//! wake fd; besides the observer's `wake_at`, its only timers are the
//! hourly refresh of the work dir's mtime (so a long job's dir is never
//! taken for a stale one) and, after a SIGTERM, the 5 s grace before
//! SIGKILL.

pub use crate::output::FileId;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::catalog::Action;
use crate::output;
use crate::sandbox::{self, Policy};

/// How much of the child's stderr is kept (its tail).
const STDERR_TAIL: usize = 4096;
/// Longest accepted event line; longer is a contract violation.
const MAX_LINE: usize = 64 * 1024;
/// Time between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// Work dir mtime refresh period (stale dirs are those older than a day).
const REFRESH_EVERY: Duration = Duration::from_secs(60 * 60);
/// Warnings kept per input; the rest are only counted.
const MAX_WARNINGS: u32 = 16;
/// `done` outputs accepted from one action process, over all its inputs.
const MAX_OUTPUTS: usize = 1024;
/// Total bytes of `done` output paths accepted from one action process.
const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// Bytes read from one pipe per poll wakeup, so a flood cannot starve the
/// cancel signalfd or the exit pidfd.
const READ_QUOTA: usize = 64 * 1024;
/// How long the group may take to be gone after its SIGKILL.
const GROUP_GONE: Duration = Duration::from_secs(1);
/// Poll period when the kernel has no pidfd (before Linux 5.3).
const NO_PIDFD_POLL: Duration = Duration::from_millis(100);

/// What became of one input. One input can have several (warnings, or
/// several outputs).
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done {
        input: PathBuf,
        output: PathBuf,
        /// The file's identity right after placement.
        id: FileId,
    },
    Skipped {
        input: PathBuf,
        reason: String,
    },
    Failed {
        input: PathBuf,
        reason: String,
    },
    Warned {
        input: PathBuf,
        message: String,
    },
}

/// Per input, the strongest outcome so far: done (any output), else failed,
/// else skipped; an input with only warnings is not counted. One entry per
/// input, never per outcome.
#[derive(Debug, Default)]
pub struct Tally {
    rank: HashMap<PathBuf, u8>,
}

impl Tally {
    pub fn add(&mut self, o: &Outcome) {
        let (input, rank) = match o {
            Outcome::Done { input, .. } => (input, 3),
            Outcome::Failed { input, .. } => (input, 2),
            Outcome::Skipped { input, .. } => (input, 1),
            Outcome::Warned { input, .. } => (input, 0),
        };
        match self.rank.get_mut(input.as_path()) {
            Some(r) => *r = (*r).max(rank),
            None => {
                self.rank.insert(input.clone(), rank);
            }
        }
    }

    /// `(done, skipped, failed)` inputs.
    pub fn counts(&self) -> (usize, usize, usize) {
        let n = |r| self.rank.values().filter(|&&v| v == r).count();
        (n(3), n(1), n(2))
    }
}

/// The end of a run. Outcomes are not kept: they go to the [`Observer`]
/// as they happen, so a run holds nothing proportional to its outputs.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Whether Landlock confined the actions (seccomp and limits always apply).
    pub sandboxed: bool,
}

/// Live feedback while a route runs.
pub trait Observer {
    /// Every outcome, as it happens (`Done` once the output is in place).
    fn event(&mut self, o: &Outcome);
    /// A folder created for several outputs of one input, before their
    /// `Done` (undo may remove it if empty).
    fn dir(&mut self, path: &Path);
    /// `done` of `total` inputs finished; `fraction` of the whole run.
    fn progress(&mut self, done: usize, total: usize, fraction: f32);
    /// An fd the supervisor polls for `POLLIN` while an action runs;
    /// [`Observer::wake`] is called when it is readable. Asked again
    /// before every poll. After a hangup (or an error, or an invalid fd)
    /// `wake` is called once and the fd is not polled again while this
    /// returns the same number; return a different one to resume.
    fn wake_fd(&self) -> Option<RawFd> {
        None
    }
    /// A deadline for the supervisor's poll; [`Observer::wake`] is called
    /// once it has passed. Asked again before every poll. A deadline fires
    /// once: returned again unchanged after its wake, it is ignored.
    fn wake_at(&self) -> Option<Instant> {
        None
    }
    /// The wake point: `wake_fd` is readable or `wake_at` has passed.
    /// Called on the supervisor's thread between polls: must not block
    /// (read what is there, queue writes); the action's pipes go unread
    /// meanwhile. The observer drains its fd: one left readable wakes it
    /// again at once.
    fn wake(&mut self) -> Wake {
        Wake::Continue
    }
}

/// What the supervisor does after [`Observer::wake`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    Continue,
    /// Cancel the run, exactly as a SIGTERM does.
    Cancel,
}

/// SIGTERM and SIGINT, blocked in the calling thread and read through a
/// signalfd, so the supervisor sees a cancel as a poll event. Not `Send`:
/// the mask is per thread.
///
/// Install it before any other thread is spawned: threads inherit the
/// mask, and a process-directed SIGTERM delivered to a thread without it
/// blocked would kill the process instead of cancelling.
///
/// Dropping it restores the previous mask, so a cancel signal still
/// pending then is delivered and terminates the process. The caller checks
/// [`CancelToken::is_cancelled`] before dropping it (which also consumes
/// pending signals) and handles a late cancel itself.
pub struct CancelToken {
    fd: OwnedFd,
    old: libc::sigset_t,
    cancelled: Cell<bool>,
    _thread: PhantomData<*const ()>,
}

impl CancelToken {
    pub fn install() -> io::Result<CancelToken> {
        // SAFETY: zeroed sigset_t is valid storage; sigemptyset/sigaddset
        // initialize it; pthread_sigmask and signalfd read `set` and write
        // `old`, both on this stack frame.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            let r = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            if r != 0 {
                return Err(io::Error::from_raw_os_error(r));
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
            if fd < 0 {
                let e = io::Error::last_os_error();
                libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                return Err(e);
            }
            Ok(CancelToken {
                fd: OwnedFd::from_raw_fd(fd),
                old,
                cancelled: Cell::new(false),
                _thread: PhantomData,
            })
        }
    }

    /// Consumes pending cancel signals; true once one has arrived (sticky).
    pub fn is_cancelled(&self) -> bool {
        loop {
            let mut info = std::mem::MaybeUninit::<libc::signalfd_siginfo>::uninit();
            let size = std::mem::size_of::<libc::signalfd_siginfo>();
            // SAFETY: the fd is our signalfd; the buffer holds one siginfo.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), info.as_mut_ptr().cast(), size) };
            if n == size as isize {
                self.cancelled.set(true);
                continue;
            }
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return self.cancelled.get();
        }
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        // SAFETY: restores the mask saved by install, on the same thread
        // (the token is not Send).
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut()) };
    }
}

/// Runs `route` over `inputs` and places the results. Never panics on a
/// misbehaving action; every input ends with at least one outcome.
pub fn run(
    route: &[&Action],
    inputs: &[PathBuf],
    obs: &mut dyn Observer,
    cancel: &CancelToken,
) -> Report {
    let abi = sandbox::landlock_abi();
    let mut seen = HashSet::new();
    let uniq: Vec<PathBuf> = inputs.iter().filter(|p| seen.insert(*p)).cloned().collect();
    let mut sink = Sink::new(obs, &uniq);
    let all = |sink: &mut Sink, reason: &str| {
        for i in 0..sink.inputs.len() {
            sink.fail(i, reason.to_owned());
        }
    };
    if route.is_empty() || route.len() > 2 {
        all(&mut sink, "invalid route");
        return sink.report(false);
    }
    if abi.is_none() && route.iter().any(|a| !a.builtin) {
        all(&mut sink, "sandbox unavailable");
        return sink.report(false);
    }
    let mut ok = Vec::new();
    for (i, p) in uniq.iter().enumerate() {
        if !p.is_absolute() {
            sink.fail(i, "input path is not absolute".to_owned());
        } else if p.to_str().is_none() {
            sink.skip(i, "non-UTF-8 path".to_owned());
        } else {
            match fs::metadata(p) {
                Ok(m) if m.is_file() => ok.push(i),
                Ok(_) => sink.fail(i, "not a regular file".to_owned()),
                Err(e) => sink.fail(i, format!("cannot read input: {e}")),
            }
        }
    }
    if abi.is_none() {
        for &i in &ok {
            sink.warn(i, "ran without sandbox".to_owned());
        }
    }
    let batch = route.iter().all(|a| a.batch);
    // Target folder per source folder; an input without one fails.
    let mut targets: HashMap<PathBuf, Result<PathBuf, String>> = HashMap::new();
    ok.retain(|&i| {
        let dir = uniq[i].parent().unwrap_or(Path::new("/"));
        let t = targets
            .entry(dir.to_path_buf())
            .or_insert_with(|| output::writable_target(dir).map_err(|e| e.to_string()));
        match t {
            Ok(_) => true,
            Err(e) => {
                sink.fail(i, format!("no folder for the output: {e}"));
                false
            }
        }
    });
    let ok_paths: Vec<PathBuf> = ok.iter().map(|&i| uniq[i].clone()).collect();
    let groups = group_by_target(&ok_paths, batch, |dir| match &targets[dir] {
        Ok(t) => t.clone(),
        Err(_) => unreachable!("inputs without a target were dropped"),
    });
    let mut cleaned = HashSet::new();
    for g in groups {
        let members: Vec<usize> = g.members.iter().map(|&j| ok[j]).collect();
        sink.begin_group(members.len());
        if sink.is_cancelled(cancel) {
            for &i in &members {
                sink.fail(i, "cancelled".to_owned());
            }
        } else {
            if cleaned.insert(g.target.clone()) {
                // The download fallback may not exist yet.
                let _ = fs::create_dir_all(&g.target);
                output::clean_stale(&g.target);
            }
            run_group(route, &members, &g.target, abi.is_some(), cancel, &mut sink);
        }
        sink.end_group();
    }
    sink.report(abi.is_some())
}

/// Forwards outcomes to the observer, tracks progress.
struct Sink<'a> {
    obs: &'a mut dyn Observer,
    inputs: &'a [PathBuf],
    finished: Vec<bool>,
    /// Warnings shown per input, and those dropped past `MAX_WARNINGS`.
    warned: Vec<(u32, u64)>,
    done: usize,
    /// Inputs in the groups before the current one.
    base: usize,
    /// Inputs in the current group.
    span: usize,
    steps: usize,
    step: usize,
    /// The observer answered [`Wake::Cancel`]: sticky, like the token.
    cancelled: bool,
    /// The observer's wake fd that hung up (or is invalid): not polled
    /// again while `wake_fd` returns it.
    dead_wake_fd: Option<RawFd>,
    /// The observer's deadline that has fired: ignored while `wake_at`
    /// returns it.
    fired_at: Option<Instant>,
}

impl<'a> Sink<'a> {
    fn new(obs: &'a mut dyn Observer, inputs: &'a [PathBuf]) -> Sink<'a> {
        Sink {
            obs,
            inputs,
            finished: vec![false; inputs.len()],
            warned: vec![(0, 0); inputs.len()],
            done: 0,
            base: 0,
            span: 0,
            steps: 1,
            step: 0,
            cancelled: false,
            dead_wake_fd: None,
            fired_at: None,
        }
    }

    /// Cancelled by a signal or by the observer.
    fn is_cancelled(&self, cancel: &CancelToken) -> bool {
        self.cancelled || cancel.is_cancelled()
    }

    fn emit(&mut self, o: Outcome) {
        self.obs.event(&o);
    }

    fn input(&self, i: usize) -> PathBuf {
        self.inputs[i].clone()
    }

    fn finish(&mut self, i: usize) {
        if !self.finished[i] {
            self.finished[i] = true;
            self.done += 1;
        }
    }

    /// At most one `Failed`/`Skipped` per input (a two-step fan-out can
    /// report several parts of one input).
    fn fail(&mut self, i: usize, reason: String) {
        if self.finished[i] {
            return;
        }
        let input = self.input(i);
        self.emit(Outcome::Failed { input, reason });
        self.finish(i);
    }

    fn skip(&mut self, i: usize, reason: String) {
        if self.finished[i] {
            return;
        }
        let input = self.input(i);
        self.emit(Outcome::Skipped { input, reason });
        self.finish(i);
    }

    fn warn(&mut self, i: usize, message: String) {
        let w = &mut self.warned[i];
        if w.0 >= MAX_WARNINGS {
            w.1 += 1;
            return;
        }
        w.0 += 1;
        let input = self.input(i);
        self.emit(Outcome::Warned { input, message });
    }

    fn placed(&mut self, i: usize, output: PathBuf, id: FileId) {
        let input = self.input(i);
        self.emit(Outcome::Done { input, output, id });
        self.finish(i);
    }

    fn begin_group(&mut self, span: usize) {
        self.span = span;
        self.step = 0;
    }

    fn end_group(&mut self) {
        self.base += self.span;
        self.span = 0;
        self.progress(0.0);
    }

    /// `f`: the current process's own fraction.
    fn progress(&mut self, f: f32) {
        let total = self.inputs.len();
        if total == 0 {
            return;
        }
        let within = (self.step as f32 + f) / self.steps as f32;
        let frac = (self.base as f32 + self.span as f32 * within) / total as f32;
        let done = self.done;
        self.obs.progress(done, total, frac.clamp(0.0, 1.0));
    }

    fn report(&mut self, sandboxed: bool) -> Report {
        for i in 0..self.inputs.len() {
            let dropped = self.warned[i].1;
            if dropped > 0 {
                let input = self.input(i);
                let message = format!("{dropped} more warnings");
                self.emit(Outcome::Warned { input, message });
            }
        }
        Report { sandboxed }
    }
}

/// Inputs sharing a target folder.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    target: PathBuf,
    /// Indices into the grouped slice, in input order.
    members: Vec<usize>,
}

/// Groups `inputs` by `target_of(parent folder)`, in order of first
/// appearance; with `batch == false` every input is its own group.
fn group_by_target(
    inputs: &[PathBuf],
    batch: bool,
    mut target_of: impl FnMut(&Path) -> PathBuf,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for (i, p) in inputs.iter().enumerate() {
        let target = target_of(p.parent().unwrap_or(Path::new("/")));
        match groups.iter_mut().find(|g| batch && g.target == target) {
            Some(g) => g.members.push(i),
            None => groups.push(Group {
                target,
                members: vec![i],
            }),
        }
    }
    groups
}

/// One step's process: its inputs and which original input each came from.
struct StepIn {
    paths: Vec<PathBuf>,
    origin: Vec<usize>,
    /// The paths are the previous step's outputs, not the user's inputs.
    intermediate: bool,
}

fn run_group(
    route: &[&Action],
    members: &[usize],
    target: &Path,
    landlock: bool,
    cancel: &CancelToken,
    sink: &mut Sink,
) {
    let work = match output::make_work_dir(target) {
        Ok(w) => w,
        Err(e) => {
            for &i in members {
                sink.fail(i, format!("cannot create work directory: {e}"));
            }
            return;
        }
    };
    sink.steps = route.len();
    let mut step_in = StepIn {
        paths: members.iter().map(|&i| sink.inputs[i].clone()).collect(),
        origin: members.to_vec(),
        intermediate: false,
    };
    // The next step's ruleset, with the intermediate outputs' rules.
    let mut pre = None;
    for (s, action) in route.iter().enumerate() {
        sink.step = s;
        let last = s + 1 == route.len();
        let dir = work.path.join(format!("step{}", s + 1));
        let list = work.path.join(format!("step{}.inputs", s + 1));
        let ran = fs::create_dir(&dir).map_err(|e| format!("cannot create work directory: {e}"));
        let result = match ran {
            Ok(()) => run_step(
                action,
                &step_in,
                pre.take(),
                &dir,
                &list,
                &work.path,
                landlock,
                cancel,
                sink,
            ),
            Err(reason) => {
                for &o in &step_in.origin {
                    sink.fail(o, reason.clone());
                }
                return;
            }
        };
        let Some(outputs) = result else {
            return; // cancelled
        };
        if last {
            place_outputs(action, &step_in, outputs, &dir, target, sink);
            return;
        }
        // Each output was checked when its `done` arrived, but the action
        // could swap it for a symlink afterwards. Its whole process group is
        // gone now (`outputs` is `None` otherwise), but a descendant that
        // left the group (`setsid`, where seccomp does not deny it) may still
        // write its dir. So the next step's read rules are built here, from
        // the fds this check opens, and no path is resolved again.
        let rules = if landlock {
            match sandbox::create() {
                Ok(r) => r,
                Err(e) => {
                    for &o in &step_in.origin {
                        sink.fail(o, format!("sandbox setup failed: {e}"));
                    }
                    return;
                }
            }
        } else {
            None
        };
        let mut changed: Vec<usize> = Vec::new();
        for (j, outs) in outputs.iter().enumerate() {
            let o = step_in.origin[j];
            if changed.contains(&o) {
                continue;
            }
            for rel in outs {
                let allowed = output::open_regular_beneath(&dir, rel)
                    .map_err(|_| "intermediate output changed".to_owned())
                    .and_then(|fd| match &rules {
                        Some(r) => sandbox::allow_read_fd(r, fd.as_fd(), &dir.join(rel))
                            .map_err(|e| format!("sandbox setup failed: {e}")),
                        None => Ok(()),
                    });
                if let Err(reason) = allowed {
                    changed.push(o);
                    sink.fail(o, reason);
                    break;
                }
            }
        }
        #[cfg(test)]
        tests::AFTER_CHECK.with_borrow_mut(|f| f.as_mut().map(|f| f(&dir)));
        let mut next = StepIn {
            paths: Vec::new(),
            origin: Vec::new(),
            intermediate: true,
        };
        for (j, outs) in outputs.iter().enumerate() {
            let o = step_in.origin[j];
            if changed.contains(&o) {
                continue;
            }
            for rel in outs {
                next.paths.push(dir.join(rel));
                next.origin.push(o);
            }
        }
        if next.paths.is_empty() {
            return;
        }
        step_in = next;
        pre = rules;
    }
}

/// Moves the last step's outputs to `target`: one output per input with
/// `place_beneath`, several with `place_many_beneath`. Both resolve the
/// output from the work dir fd without following symlinks, so even a file
/// swapped after validation can only be one from inside the work dir.
fn place_outputs(
    action: &Action,
    step_in: &StepIn,
    outputs: Vec<Vec<PathBuf>>,
    dir: &Path,
    target: &Path,
    sink: &mut Sink,
) {
    // Outputs per original input, in order; the paths are moved, not copied.
    let mut per: Vec<(usize, Vec<PathBuf>)> = Vec::new();
    for (j, outs) in outputs.into_iter().enumerate() {
        if outs.is_empty() {
            continue;
        }
        let o = step_in.origin[j];
        let at = match per.iter().position(|(x, _)| *x == o) {
            Some(at) => at,
            None => {
                per.push((o, Vec::new()));
                per.len() - 1
            }
        };
        per[at].1.extend(outs);
    }
    for (o, rels) in per {
        let src = &sink.inputs[o];
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "output".to_owned());
        let placed = if rels.len() == 1 {
            output::place_beneath(dir, &rels[0], target, &stem, &action.ext, None).map(|f| {
                output::ManyPlaced {
                    folder: PathBuf::new(),
                    files: vec![f],
                }
            })
        } else {
            output::place_many_beneath(dir, &rels, target, &stem)
        };
        match placed {
            Ok(many) => {
                if !many.folder.as_os_str().is_empty() {
                    sink.obs.dir(&many.folder);
                }
                for (p, id) in many.files {
                    sink.placed(o, p, id);
                }
            }
            Err(e) => sink.fail(o, format!("cannot place output: {e}")),
        }
    }
}

/// One parsed, validated event line. Indices are into the process's inputs.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    /// `None`: NaN, dropped.
    Progress(Option<f32>),
    Done {
        index: usize,
        rel: PathBuf,
    },
    Skip {
        index: usize,
        reason: String,
    },
    Fail {
        index: usize,
        reason: String,
    },
    Warn {
        index: usize,
        message: String,
    },
}

/// Parses and validates one stdout line (without its newline). `Err` is a
/// contract violation, described.
fn parse_event(line: &str, inputs: &[PathBuf], work: &Path) -> Result<Event, String> {
    let l = manifest::parse_line(line).map_err(|e| format!("bad event line: {}", e.msg))?;
    let mut fields = Vec::with_capacity(l.items.len());
    for item in &l.items {
        match item {
            manifest::Item::Bare(v) => fields.push(v.as_str()),
            manifest::Item::Pair(..) => {
                return Err(format!("unescaped `=` in a {} event", l.kind));
            }
        }
    }
    let want = if l.kind == "progress" { 1 } else { 2 };
    let known = matches!(
        l.kind.as_str(),
        "progress" | "done" | "skip" | "fail" | "warn"
    );
    if !known {
        return Err(format!("unknown event kind {}", l.kind));
    }
    if fields.len() != want {
        return Err(format!(
            "{} event with {} fields, expected {want}",
            l.kind,
            fields.len()
        ));
    }
    let index_of = |input: &str| {
        inputs
            .iter()
            .position(|p| p.to_str() == Some(input))
            .ok_or_else(|| format!("{} event for unknown input {input}", l.kind))
    };
    Ok(match l.kind.as_str() {
        "progress" => {
            let f: f32 = fields[0]
                .parse()
                .map_err(|_| format!("bad progress value {}", fields[0]))?;
            Event::Progress((!f.is_nan()).then(|| f.clamp(0.0, 1.0)))
        }
        "done" => {
            let (input, rel) = (fields[0], fields[1]);
            let index = done_index(rel, inputs.len())?;
            if inputs[index].to_str() != Some(input) {
                return Err(format!(
                    "done output {rel} is not under the index of {input}"
                ));
            }
            regular_beneath(work, Path::new(rel))?;
            Event::Done {
                index,
                rel: PathBuf::from(rel),
            }
        }
        "skip" => Event::Skip {
            index: index_of(fields[0])?,
            reason: fields[1].to_owned(),
        },
        "fail" => Event::Fail {
            index: index_of(fields[0])?,
            reason: fields[1].to_owned(),
        },
        _ => Event::Warn {
            index: index_of(fields[0])?,
            message: fields[1].to_owned(),
        },
    })
}

/// The `<index>` of a `done` output path `<index>/<name>...`: relative,
/// every component a plain name (no empty, `.` or `..`), the first one a
/// canonical decimal below `n`.
fn done_index(rel: &str, n: usize) -> Result<usize, String> {
    if rel.starts_with('/') {
        return Err(format!("absolute done output {rel}"));
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(format!("done output {rel:?} is not a plain relative path"));
    }
    if parts.len() < 2 {
        return Err(format!(
            "done output {rel} is not under an <index>/ directory"
        ));
    }
    let first = parts[0];
    let canonical =
        first.bytes().all(|b| b.is_ascii_digit()) && (first == "0" || !first.starts_with('0'));
    match first.parse::<usize>() {
        Ok(i) if canonical && i < n => Ok(i),
        _ => Err(format!("done output {rel} has a bad index (batch of {n})")),
    }
}

/// `rel` names a regular file beneath `work`, reached through real
/// directories only (no symlink anywhere on the way).
fn regular_beneath(work: &Path, rel: &Path) -> Result<(), String> {
    let mut at = work.to_path_buf();
    let mut parts = rel.components().peekable();
    while let Some(c) = parts.next() {
        at.push(c);
        let meta =
            fs::symlink_metadata(&at).map_err(|e| format!("done output {}: {e}", rel.display()))?;
        let ok = if parts.peek().is_some() {
            meta.is_dir()
        } else {
            meta.is_file()
        };
        if !ok {
            return Err(format!(
                "done output {} is not a regular file in the work directory",
                rel.display()
            ));
        }
    }
    Ok(())
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Exit(i32),
    Signal(i32),
}

impl From<ExitStatus> for End {
    fn from(s: ExitStatus) -> End {
        match (s.code(), s.signal()) {
            (Some(c), _) => End::Exit(c),
            (None, Some(n)) => End::Signal(n),
            (None, None) => End::Exit(-1),
        }
    }
}

/// The failure reason for an input the process reported nothing about.
fn unreported_reason(violation: Option<&str>, end: End, stderr_tail: &[u8]) -> String {
    if let Some(v) = violation {
        return format!("contract violation: {v}");
    }
    match end {
        End::Signal(libc::SIGXFSZ) => "output too large".to_owned(),
        End::Signal(n) => format!("killed by signal {n}"),
        End::Exit(action::EXIT_USAGE) => match last_line(stderr_tail) {
            l if l.is_empty() => "action invocation error".to_owned(),
            l => format!("action invocation error: {l}"),
        },
        End::Exit(_) => "no result reported".to_owned(),
    }
}

/// The failure reason of an input that went over an output cap.
const TOO_MANY: &str = "too many outputs";

/// The input a malformed event line is about, if its first field names
/// one of `inputs` (for `done`, `skip`, `fail` and `warn` lines).
fn offender_of(line: &str, inputs: &[PathBuf]) -> Option<usize> {
    let l = manifest::parse_line(line).ok()?;
    if !matches!(l.kind.as_str(), "done" | "skip" | "fail" | "warn") {
        return None;
    }
    let first = l.bare_at(0)?;
    inputs.iter().position(|p| p.to_str() == Some(first))
}

/// The last non-empty line of `tail`, trimmed.
fn last_line(tail: &[u8]) -> String {
    let s = String::from_utf8_lossy(tail);
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Runs one step's process to its end. Returns the validated outputs
/// (relative to `dir`) per input, or `None` when cancelled (every
/// unfinished input is then failed as cancelled). With Landlock, an
/// intermediate step's ruleset comes as `pre`, already granting its inputs.
#[allow(clippy::too_many_arguments)]
fn run_step(
    action: &Action,
    step_in: &StepIn,
    pre: Option<sandbox::Ruleset>,
    dir: &Path,
    list: &Path,
    root: &Path,
    landlock: bool,
    cancel: &CancelToken,
    sink: &mut Sink,
) -> Option<Vec<Vec<PathBuf>>> {
    let n = step_in.paths.len();
    let fail_all = |sink: &mut Sink, reason: &str| {
        for &o in &step_in.origin {
            sink.fail(o, reason.to_owned());
        }
        Some(vec![Vec::new(); n])
    };
    // The engine never hands an action a path inside its own --work.
    if step_in.paths.iter().any(|p| p.starts_with(dir)) {
        return fail_all(sink, "input inside the work directory");
    }
    let mut text = String::new();
    for p in &step_in.paths {
        let Some(s) = p.to_str() else {
            return fail_all(sink, "non-UTF-8 path");
        };
        text.push_str(&manifest::escape(s));
        text.push('\n');
    }
    if let Err(e) = fs::write(list, text) {
        return fail_all(sink, &format!("cannot write input list: {e}"));
    }
    let mut exec: Vec<PathBuf> = sandbox::SYSTEM_EXEC_PATHS
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    exec.push(action.bin.clone());
    // The list is the engine's: never followed. Intermediate outputs are
    // granted in `pre`, from the fds their check opened.
    let exact = [list.to_path_buf()];
    let read = if step_in.intermediate {
        &[][..]
    } else {
        &step_in.paths[..]
    };
    let policy = Policy {
        exec: &exec,
        read,
        read_exact: &exact,
        write: dir,
    };
    let ruleset = if landlock {
        let built = match pre {
            Some(r) => sandbox::add(&r, &policy).map(|()| Some(r)),
            None if step_in.intermediate => Err(io::Error::other("no intermediate rules")),
            None => sandbox::build(&policy),
        };
        match built {
            Ok(r) => r,
            Err(e) => return fail_all(sink, &format!("sandbox setup failed: {e}")),
        }
    } else {
        None
    };
    if ruleset.is_none() && !action.builtin {
        return fail_all(sink, "sandbox unavailable");
    }
    let total: u64 = step_in
        .paths
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let limits = sandbox::limits_for(total);
    let fd = ruleset.as_ref().map(|r| r.as_raw_fd());
    let mut cmd = Command::new(&action.bin);
    cmd.arg("run")
        .arg(&action.id)
        .arg("--work")
        .arg(dir)
        .arg("--inputs-from")
        .arg(list)
        .arg("--")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No `process_group(0)`: std applies it before pre_exec, and the
    // child's `setsid` (which also makes it a group leader) would then fail.
    // SAFETY: the closure makes only async-signal-safe calls (sigemptyset,
    // sigprocmask, and apply_in_child's documented ones) on stack data; the
    // ruleset fd stays open until spawn returns (`ruleset` lives to the end
    // of this function).
    unsafe {
        cmd.pre_exec(move || {
            // The child inherits the supervisor's blocked SIGTERM/SIGINT.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            if libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            sandbox::apply_in_child(fd, &limits)
        });
    }
    if sink.is_cancelled(cancel) {
        for &o in &step_in.origin {
            sink.fail(o, "cancelled".to_owned());
        }
        return None;
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail_all(sink, &format!("cannot start action: {e}")),
    };
    drop(ruleset);
    let mut sup = Supervisor::new(child, step_in, dir, root, cancel);
    sup.run(sink);
    sup.finish(sink)
}

/// What one input of a process has reported so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reported {
    Nothing,
    /// One or more `done` (several outputs are allowed).
    Done,
    /// `skip` or `fail`: nothing may follow.
    Final,
}

/// The state of one running process.
struct Supervisor<'a> {
    child: Child,
    /// The child's pid, which is also its process group id (it calls
    /// `setsid` in `apply_in_child`).
    pid: libc::pid_t,
    pidfd: Option<OwnedFd>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    line: Vec<u8>,
    /// The current line ran past MAX_LINE (already a violation).
    overlong: bool,
    tail: Vec<u8>,
    step_in: &'a StepIn,
    dir: &'a Path,
    root: &'a Path,
    cancel: &'a CancelToken,
    reported: Vec<Reported>,
    /// `(input index, output)` in report order; each path stored once.
    outputs: Vec<(usize, PathBuf)>,
    /// Hashes of `outputs`' paths, for the duplicate check.
    output_hashes: HashSet<u64>,
    output_bytes: usize,
    violation: Option<String>,
    /// The input whose event caused the violation, and its failure reason:
    /// it fails and nothing of it is placed.
    offender: Option<(usize, String)>,
    cancelled: bool,
    term_at: Option<Instant>,
    killed: bool,
    /// The child is reaped: its pid, and so the group id, may now be
    /// reused by an unrelated process. Nothing is signalled after this.
    reaped: bool,
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: fd is an open pipe fd owned by the Child handles.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

fn pidfd_open(pid: libc::pid_t) -> Option<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags 0; the child is not reaped
    // yet, so the pid still names it.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    // SAFETY: a non-negative result is a new fd that we now own.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Whether our child `pid` has exited, observed without reaping it (the
/// fallback for kernels without pidfd).
fn exited_unreaped(pid: libc::pid_t) -> bool {
    // SAFETY: a zeroed siginfo_t is valid storage for waitid to fill.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
    // SAFETY: info is a valid out-pointer; WNOWAIT leaves the child unreaped.
    let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) };
    // With WNOHANG and no state change, si_pid stays 0.
    // SAFETY: waitid filled (or zeroed) the CLD fields that si_pid reads.
    r == 0 && unsafe { info.si_pid() } != 0
}

/// Waits, with signal 0 (which delivers nothing), until process group
/// `pgid` has no member left; false if it still has one after
/// `GROUP_GONE`. Called after the leader is reaped (a zombie leader still
/// counts as a member). If the id were reused meanwhile, the probe could
/// only report a false "left behind", never touch the other process.
fn group_gone(pgid: libc::pid_t) -> bool {
    let start = Instant::now();
    let mut nap = Duration::from_millis(1);
    loop {
        // SAFETY: kill with signal 0 only checks for existence.
        let r = unsafe { libc::kill(-pgid, 0) };
        if r != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        if start.elapsed() >= GROUP_GONE {
            return false;
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}

/// Sets the mtime (and atime) of `path` to now.
fn touch(path: &Path) {
    if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: c is a valid NUL-terminated path; NULL times means now.
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) };
    }
}

/// Reads at most `READ_QUOTA` bytes, handing each chunk to `f` at once.
/// Returns (still open, quota used up).
fn read_quota(r: &mut impl Read, mut f: impl FnMut(&[u8])) -> (bool, bool) {
    let mut buf = [0u8; 8192];
    let mut got = 0;
    while got < READ_QUOTA {
        match r.read(&mut buf) {
            Ok(0) => return (false, false),
            Ok(k) => {
                got += k;
                f(&buf[..k]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return (true, false),
            Err(_) => return (false, false),
        }
    }
    (true, true)
}

impl<'a> Supervisor<'a> {
    fn new(
        child: Child,
        step_in: &'a StepIn,
        dir: &'a Path,
        root: &'a Path,
        cancel: &'a CancelToken,
    ) -> Supervisor<'a> {
        let mut child = child;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Some(s) = &stdout {
            set_nonblocking(s.as_raw_fd());
        }
        if let Some(s) = &stderr {
            set_nonblocking(s.as_raw_fd());
        }
        let n = step_in.paths.len();
        let pid = child.id() as libc::pid_t;
        Supervisor {
            pid,
            pidfd: pidfd_open(pid),
            child,
            stdout,
            stderr,
            line: Vec::new(),
            overlong: false,
            tail: Vec::new(),
            step_in,
            dir,
            root,
            cancel,
            reported: vec![Reported::Nothing; n],
            outputs: Vec::new(),
            output_hashes: HashSet::new(),
            output_bytes: 0,
            violation: None,
            offender: None,
            cancelled: false,
            term_at: None,
            killed: false,
            reaped: false,
        }
    }

    /// Signals the child's whole process group.
    fn signal(&self, sig: libc::c_int) {
        debug_assert!(!self.reaped, "signal after reap");
        if self.reaped {
            return;
        }
        // SAFETY: kill has no memory preconditions. The child is not reaped
        // (checked above), so its pid, which is its process group id, has
        // not been reused: the group is the action's. setsid/setpgid are
        // denied in the sandbox, so every descendant is still in it.
        unsafe { libc::kill(-self.pid, sig) };
    }

    fn terminate(&mut self) {
        if self.term_at.is_none() {
            self.signal(libc::SIGTERM);
            self.term_at = Some(Instant::now());
        }
    }

    fn violate(&mut self, what: String) {
        if self.violation.is_none() && !self.cancelled {
            self.violation = Some(what);
            self.terminate();
        }
    }

    /// A violation caused by an event about input `index`; that input
    /// fails with `reason` (default: the violation) and nothing of it is
    /// placed.
    fn violate_by(&mut self, index: Option<usize>, what: String, reason: Option<&str>) {
        if self.violation.is_none() && !self.cancelled {
            if let Some(i) = index {
                let why =
                    reason.map_or_else(|| format!("contract violation: {what}"), str::to_owned);
                self.offender = Some((i, why));
            }
            self.violate(what);
        }
    }

    /// The poll loop, until the child has exited (it stays unreaped).
    fn run(&mut self, sink: &mut Sink) {
        let mut next_refresh = Instant::now() + REFRESH_EVERY;
        loop {
            let now = Instant::now();
            let mut deadline = next_refresh;
            if let (Some(t), false) = (self.term_at, self.killed) {
                deadline = deadline.min(t + TERM_GRACE);
            }
            if self.pidfd.is_none() {
                deadline = deadline.min(now + NO_PIDFD_POLL);
            }
            // Asked anew each time: the observer may change them in `wake`.
            // A dead fd or a fired deadline does not count while unchanged.
            let wake_fd = sink.obs.wake_fd();
            if wake_fd != sink.dead_wake_fd {
                sink.dead_wake_fd = None;
            }
            let wake_fd = wake_fd.filter(|_| sink.dead_wake_fd.is_none());
            let wake_at = sink.obs.wake_at();
            if wake_at != sink.fired_at {
                sink.fired_at = None;
            }
            let wake_at = wake_at.filter(|_| sink.fired_at.is_none());
            if let Some(t) = wake_at {
                deadline = deadline.min(t);
            }
            let wait = deadline.saturating_duration_since(now);
            // Round up so the deadline has passed when poll returns.
            let ms = wait.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            let mut fds = Vec::with_capacity(4);
            let mut push = |fd: RawFd| {
                fds.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            };
            let out_at = self.stdout.as_ref().map(|s| push(s.as_raw_fd()));
            let err_at = self.stderr.as_ref().map(|s| push(s.as_raw_fd()));
            let sig_at = (!self.cancelled).then(|| push(self.cancel.fd.as_raw_fd()));
            let pid_at = self.pidfd.as_ref().map(|p| push(p.as_raw_fd()));
            let wake_fd_at = wake_fd.map(push);
            // SAFETY: fds is a valid array of fds.len() pollfds.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
            if r < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Unexpected: stop supervising; finish kills and reaps.
                self.signal(libc::SIGKILL);
                return;
            }
            let ready = |at: Option<usize>| at.is_some_and(|i| fds[i].revents != 0);
            let now = Instant::now();
            if now >= next_refresh {
                touch(self.root);
                next_refresh = now + REFRESH_EVERY;
            }
            if let (Some(t), false) = (self.term_at, self.killed)
                && now >= t + TERM_GRACE
            {
                self.signal(libc::SIGKILL);
                self.killed = true;
            }
            // At most READ_QUOTA per pipe, then the signalfd and the pidfd
            // are looked at again: a flood cannot delay a cancel.
            if ready(out_at) {
                self.read_stdout(sink);
            }
            if ready(err_at) {
                self.read_stderr();
            }
            if ready(sig_at) && self.cancel.is_cancelled() {
                self.cancelled = true;
                self.terminate();
            }
            let due = wake_at.is_some_and(|t| now >= t);
            if ready(wake_fd_at) || due {
                let dead = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
                if wake_fd_at.is_some_and(|i| fds[i].revents & dead != 0) {
                    sink.dead_wake_fd = wake_fd;
                }
                if due {
                    sink.fired_at = wake_at;
                }
                if sink.obs.wake() == Wake::Cancel {
                    self.cancelled = true;
                    sink.cancelled = true;
                    self.terminate();
                }
            }
            let exited = match pid_at {
                Some(_) => ready(pid_at),
                None => exited_unreaped(self.pid),
            };
            if exited {
                return;
            }
        }
    }

    /// One quota of stdout, parsed as it is read. True if the quota was hit.
    fn read_stdout(&mut self, sink: &mut Sink) -> bool {
        let Some(mut out) = self.stdout.take() else {
            return false;
        };
        let (open, full) = read_quota(&mut out, |b| self.feed(b, sink));
        if open {
            self.stdout = Some(out);
        }
        full
    }

    /// One quota of stderr into the tail. True if the quota was hit.
    fn read_stderr(&mut self) -> bool {
        let Some(mut err) = self.stderr.take() else {
            return false;
        };
        let tail = &mut self.tail;
        let (open, full) = read_quota(&mut err, |b| {
            tail.extend_from_slice(b);
            if tail.len() > STDERR_TAIL {
                tail.drain(..tail.len() - STDERR_TAIL);
            }
        });
        if open {
            self.stderr = Some(err);
        }
        full
    }

    /// Splits stdout bytes into lines and handles each complete one.
    fn feed(&mut self, bytes: &[u8], sink: &mut Sink) {
        let mut rest = bytes;
        while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
            if !self.overlong {
                self.line.extend_from_slice(&rest[..nl]);
                let line = std::mem::take(&mut self.line);
                self.handle_line(&line, sink);
            }
            self.overlong = false;
            self.line.clear();
            rest = &rest[nl + 1..];
        }
        if !self.overlong {
            self.line.extend_from_slice(rest);
            if self.line.len() > MAX_LINE {
                self.overlong = true;
                self.line.clear();
                self.violate("event line too long".to_owned());
            }
        }
    }

    fn handle_line(&mut self, line: &[u8], sink: &mut Sink) {
        if self.violation.is_some() || self.cancelled {
            return;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            self.violate("event line is not UTF-8".to_owned());
            return;
        };
        let ev = match parse_event(line, &self.step_in.paths, self.dir) {
            Ok(ev) => ev,
            Err(what) => {
                let index = offender_of(line, &self.step_in.paths);
                self.violate_by(index, what, None);
                return;
            }
        };
        let again = |paths: &[PathBuf], index: usize| {
            format!(
                "second result for {} after skip or fail",
                paths[index].display()
            )
        };
        let origin = &self.step_in.origin;
        match ev {
            Event::Progress(Some(f)) => sink.progress(f),
            Event::Progress(None) => {}
            Event::Done { index, rel } => {
                let at = Some(index);
                if self.reported[index] == Reported::Final {
                    self.violate_by(at, again(&self.step_in.paths, index), None);
                } else if self.outputs.len() >= MAX_OUTPUTS {
                    let what = format!("more than {MAX_OUTPUTS} outputs");
                    self.violate_by(at, what, Some(TOO_MANY));
                } else if self.output_bytes + rel.as_os_str().len() > MAX_OUTPUT_BYTES {
                    let what = format!("more than {MAX_OUTPUT_BYTES} bytes of output paths");
                    self.violate_by(at, what, Some(TOO_MANY));
                } else if self.is_duplicate(&rel) {
                    let what = format!("duplicate output {}", rel.display());
                    self.violate_by(at, what, None);
                } else {
                    self.output_bytes += rel.as_os_str().len();
                    self.outputs.push((index, rel));
                    self.reported[index] = Reported::Done;
                }
            }
            Event::Skip { index, reason } | Event::Fail { index, reason }
                if self.reported[index] != Reported::Nothing =>
            {
                let _ = reason;
                self.violate_by(Some(index), again(&self.step_in.paths, index), None);
            }
            Event::Skip { index, reason } => {
                self.reported[index] = Reported::Final;
                sink.skip(origin[index], reason);
            }
            Event::Fail { index, reason } => {
                self.reported[index] = Reported::Final;
                sink.fail(origin[index], reason);
            }
            Event::Warn { index, message } => sink.warn(origin[index], message),
        }
    }

    /// Whether `rel` was reported before; records its hash if not.
    fn is_duplicate(&mut self, rel: &Path) -> bool {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let h = BuildHasherDefault::<DefaultHasher>::default().hash_one(rel);
        !self.output_hashes.insert(h) && self.outputs.iter().any(|(_, p)| p == rel)
    }

    /// After the exit (child still unreaped): kills what is left of the
    /// group while its id is still ours, drains the pipes, reaps, waits for
    /// the group to be gone, and settles every input without a result.
    /// Nothing is placed unless the group is confirmed gone.
    fn finish(mut self, sink: &mut Sink) -> Option<Vec<Vec<PathBuf>>> {
        self.signal(libc::SIGKILL);
        // Bounded: the writers are dead or dying, the pipes hold what is left.
        for _ in 0..64 {
            let out = self.read_stdout(sink);
            let err = self.read_stderr();
            if !out && !err {
                break;
            }
        }
        let end = match self.child.wait() {
            Ok(s) => End::from(s),
            Err(_) => End::Exit(-1),
        };
        self.reaped = true;
        // A line cut off by a crash is not an event; after a normal exit it
        // is a broken one. (No signal: the child is reaped.)
        if !self.line.is_empty()
            && !self.overlong
            && matches!(end, End::Exit(_))
            && self.violation.is_none()
            && !self.cancelled
        {
            self.violation = Some("unterminated event line".to_owned());
        }
        let gone = group_gone(self.pid);
        let origin = &self.step_in.origin;
        if self.cancelled {
            for &o in origin {
                sink.fail(o, "cancelled".to_owned());
            }
            return None;
        }
        if !gone {
            for &o in origin {
                sink.fail(o, "action left processes behind".to_owned());
            }
            return None;
        }
        // The offending input fails, and none of its outputs (nor those of
        // other fan-out parts of the same original input) is placed.
        if let Some((j, why)) = self.offender.take() {
            let o = origin[j];
            self.outputs.retain(|&(i, _)| origin[i] != o);
            for (k, &ok) in origin.iter().enumerate() {
                if ok == o {
                    self.reported[k] = Reported::Final;
                }
            }
            sink.fail(o, why);
        }
        let reason = unreported_reason(self.violation.as_deref(), end, &self.tail);
        for (j, &o) in origin.iter().enumerate() {
            if self.reported[j] == Reported::Nothing {
                sink.fail(o, reason.clone());
            }
        }
        let mut per = vec![Vec::new(); self.reported.len()];
        for (index, rel) in self.outputs {
            per[index].push(rel);
        }
        Some(per)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "inacircle-exec-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn done_line(input: &str, rel: &str) -> String {
        manifest::write_line(&manifest::Line::new("done").bare(input).bare(rel))
            .trim_end_matches('\n')
            .to_owned()
    }

    fn violation(r: Result<Event, String>) -> String {
        r.expect_err("expected a contract violation")
    }

    #[test]
    fn validates_done_path() {
        let t = Tmp::new("done");
        let work = t.0.join("work");
        fs::create_dir_all(work.join("0")).unwrap();
        fs::create_dir_all(work.join("1/sub")).unwrap();
        fs::write(work.join("0/a.md"), "x").unwrap();
        fs::write(work.join("1/sub/b.md"), "x").unwrap();
        fs::create_dir(work.join("0/dir.md")).unwrap();
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), work.join("0/link.md")).unwrap();
        std::os::unix::fs::symlink(&outside, work.join("1/up")).unwrap();
        let inputs = [PathBuf::from("/in/a.txt"), PathBuf::from("/in/b.txt")];
        let parse = |l: &str| parse_event(l, &inputs, &work);

        assert_eq!(
            parse(&done_line("/in/a.txt", "0/a.md")),
            Ok(Event::Done {
                index: 0,
                rel: PathBuf::from("0/a.md")
            })
        );
        assert_eq!(
            parse(&done_line("/in/b.txt", "1/sub/b.md")),
            Ok(Event::Done {
                index: 1,
                rel: PathBuf::from("1/sub/b.md")
            })
        );
        let bad = [
            ("/in/a.txt", "/etc/passwd"),  // absolute
            ("/in/a.txt", "0/../0/a.md"),  // ..
            ("/in/a.txt", "0/./a.md"),     // .
            ("/in/a.txt", "./0/a.md"),     // leading .
            ("/in/a.txt", "0//a.md"),      // empty component
            ("/in/a.txt", "0/a.md/"),      // trailing slash
            ("/in/a.txt", "a.md"),         // no index dir
            ("/in/a.txt", "00/a.md"),      // non-canonical index
            ("/in/a.txt", "+0/a.md"),      // not digits
            ("/in/a.txt", "2/a.md"),       // index out of range
            ("/in/a.txt", "1/sub/b.md"),   // index of another input
            ("/in/b.txt", "0/a.md"),       // input does not match index
            ("/in/a.txt", "0/missing.md"), // does not exist
            ("/in/a.txt", "0/dir.md"),     // not a regular file
            ("/in/a.txt", "0/link.md"),    // symlink
            ("/in/b.txt", "1/up/secret"),  // through a symlinked dir
            ("/in/a.txt", ""),             // empty
        ];
        for (input, rel) in bad {
            let e = violation(parse(&done_line(input, rel)));
            assert!(!e.is_empty(), "{rel}");
        }
        // Wrong field counts, unescaped `=` (a pair), unknown kinds.
        for l in [
            "done\t/in/a.txt",
            "done\t/in/a.txt\t0/a.md\textra",
            "done\t/in/a.txt\tk=0/a.md",
            "skip\t/in/a.txt",
            "fail",
            "warn\t/in/a.txt\tm\tn",
            "bogus\t/in/a.txt\tx",
            "",
            "#comment",
        ] {
            violation(parse(l));
        }
        // skip/fail/warn must name an input of this process.
        violation(parse("skip\t/in/other.txt\tunsupported"));
        assert_eq!(
            parse("skip\t/in/b.txt\tunsupported"),
            Ok(Event::Skip {
                index: 1,
                reason: "unsupported".into()
            })
        );
        assert_eq!(
            parse("warn\t/in/a.txt\ttables dropped"),
            Ok(Event::Warn {
                index: 0,
                message: "tables dropped".into()
            })
        );
        assert_eq!(
            parse("fail\t/in/a.txt\tread error"),
            Ok(Event::Fail {
                index: 0,
                reason: "read error".into()
            })
        );
    }

    #[test]
    fn progress_clamped_and_nan_dropped() {
        let inputs = [PathBuf::from("/in/a.txt")];
        let work = Path::new("/nonexistent");
        let p = |l: &str| parse_event(l, &inputs, work);
        assert_eq!(p("progress\t0.42"), Ok(Event::Progress(Some(0.42))));
        assert_eq!(p("progress\tNaN"), Ok(Event::Progress(None)));
        assert_eq!(p("progress\t-3"), Ok(Event::Progress(Some(0.0))));
        assert_eq!(p("progress\t7.5"), Ok(Event::Progress(Some(1.0))));
        assert_eq!(p("progress\tinf"), Ok(Event::Progress(Some(1.0))));
        assert_eq!(p("progress\t-inf"), Ok(Event::Progress(Some(0.0))));
        violation(p("progress\tlots"));
        violation(p("progress"));
        violation(p("progress\t0.5\t0.6"));
    }

    #[test]
    fn groups_by_target_dir() {
        let inputs: Vec<PathBuf> = ["/a/1.txt", "/b/2.txt", "/a/3.txt", "/ro/4.txt", "/c/5.txt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut probed = Vec::new();
        // /ro is not writable and falls back to /dl, which /c also maps to.
        let mut target = |d: &Path| {
            probed.push(d.to_path_buf());
            match d.to_str().unwrap() {
                "/ro" | "/c" => PathBuf::from("/dl"),
                d => PathBuf::from(d),
            }
        };
        let g = group_by_target(&inputs, true, &mut target);
        let want = [("/a", vec![0, 2]), ("/b", vec![1]), ("/dl", vec![3, 4])];
        assert_eq!(g.len(), want.len());
        for (g, (t, m)) in g.iter().zip(want) {
            assert_eq!(g.target, PathBuf::from(t));
            assert_eq!(g.members, m);
        }
        let g = group_by_target(&inputs, false, &mut target);
        assert_eq!(g.len(), 5);
        assert!(g.iter().enumerate().all(|(i, g)| g.members == [i]));
        assert_eq!(g[3].target, PathBuf::from("/dl"));
        assert!(group_by_target(&[], true, &mut target).is_empty());
    }

    #[test]
    fn reason_for_unreported_inputs() {
        let tail = b"warming up\nerror: bad option --frob\n\n";
        assert_eq!(
            unreported_reason(
                Some("unknown event kind bogus"),
                End::Signal(libc::SIGTERM),
                tail
            ),
            "contract violation: unknown event kind bogus"
        );
        assert_eq!(
            unreported_reason(None, End::Signal(libc::SIGXFSZ), tail),
            "output too large"
        );
        assert_eq!(
            unreported_reason(None, End::Signal(libc::SIGSEGV), tail),
            format!("killed by signal {}", libc::SIGSEGV)
        );
        assert_eq!(
            unreported_reason(None, End::Exit(2), tail),
            "action invocation error: error: bad option --frob"
        );
        assert_eq!(
            unreported_reason(None, End::Exit(2), b""),
            "action invocation error"
        );
        for code in [0, 1, 3] {
            assert_eq!(
                unreported_reason(None, End::Exit(code), tail),
                "no result reported"
            );
        }
    }

    #[test]
    fn malformed_event_blames_the_input_it_names() {
        let inputs = [PathBuf::from("/in/a.txt"), PathBuf::from("/in/b.txt")];
        assert_eq!(
            offender_of("done\t/in/b.txt\t/etc/passwd", &inputs),
            Some(1)
        );
        assert_eq!(offender_of("skip\t/in/a.txt", &inputs), Some(0));
        assert_eq!(offender_of("done\t/in/other.txt\t0/x", &inputs), None);
        assert_eq!(offender_of("bogus\t/in/a.txt\tx", &inputs), None);
        assert_eq!(offender_of("progress\tlots", &inputs), None);
    }

    #[test]
    fn stderr_tail_last_line() {
        assert_eq!(last_line(b"a\n  b  \n\n"), "b");
        assert_eq!(last_line(b""), "");
        assert_eq!(last_line(b"\xff\xfeoops"), "\u{fffd}\u{fffd}oops");
    }

    type Hook = Option<Box<dyn FnMut(&Path)>>;

    thread_local! {
        /// Runs in `run_group` after the intermediate outputs' check (and
        /// their rules), before the next step's ruleset is finished, with
        /// the checked step's dir.
        pub(super) static AFTER_CHECK: std::cell::RefCell<Hook> =
            const { std::cell::RefCell::new(None) };
    }

    #[derive(Default)]
    struct Seen(Vec<Outcome>);
    impl Observer for Seen {
        fn event(&mut self, o: &Outcome) {
            self.0.push(o.clone());
        }
        fn dir(&mut self, _: &Path) {}
        fn progress(&mut self, _: usize, _: usize, _: f32) {}
    }

    fn script_action(t: &Tmp, name: &str, body: &str, ext: &str) -> Action {
        use std::os::unix::fs::PermissionsExt;
        let bin = t.0.join(name);
        fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        Action {
            id: "test.fake".into(),
            family: "test".into(),
            bin,
            input: "text/plain".into(),
            output: "text/markdown".into(),
            ext: ext.into(),
            label: "Fake".into(),
            group: "test".into(),
            cost: 1,
            lossy: false,
            batch: true,
            requires: Vec::new(),
            builtin: true,
        }
    }

    #[test]
    fn intermediate_rule_comes_from_the_checked_fd() {
        if sandbox::landlock_abi().is_none() {
            eprintln!("SKIPPED intermediate_rule_comes_from_the_checked_fd: no Landlock");
            return;
        }
        let t = Tmp::new("ruleswap");
        let docs = t.0.join("docs");
        let secret = t.0.join("secret");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&secret).unwrap();
        fs::write(secret.join("a.mid"), "SECRET\n").unwrap();
        let input = docs.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let s1 = script_action(
            &t,
            "copy",
            "mkdir \"$4/0\"; read -r in < \"$6\"; cat \"$in\" > \"$4/0/a.mid\"\n\
             printf 'done\\t%s\\t0/a.mid\\n' \"$in\"",
            "mid",
        );
        let s2 = script_action(
            &t,
            "append",
            "read -r in < \"$6\"; mkdir \"$4/0\"\n\
             if cat \"$in\" > \"$4/0/out.md\"; then printf 'done\\t%s\\t0/out.md\\n' \"$in\"\n\
             else printf 'fail\\t%s\\tdenied\\n' \"$in\"; fi",
            "md",
        );
        // Between the check and the next step's rules, `<step1>/0` becomes
        // a link to the secret's folder, which holds a file of that name.
        let target = secret.clone();
        AFTER_CHECK.set(Some(Box::new(move |dir: &Path| {
            fs::rename(dir.join("0"), dir.join("moved")).unwrap();
            std::os::unix::fs::symlink(&target, dir.join("0")).unwrap();
        })));
        let tok = CancelToken::install().unwrap();
        let mut seen = Seen::default();
        let report = run(&[&s1, &s2], std::slice::from_ref(&input), &mut seen, &tok);
        AFTER_CHECK.set(None);
        assert!(report.sandboxed);
        for e in fs::read_dir(&docs).unwrap().flatten() {
            let body = fs::read(e.path()).unwrap_or_default();
            assert!(
                !String::from_utf8_lossy(&body).contains("SECRET"),
                "secret leaked into {}: {:?}",
                e.path().display(),
                seen.0
            );
        }
        // Step 2 ran and the read of the swapped path was denied.
        assert_eq!(
            seen.0,
            [Outcome::Failed {
                input: input.clone(),
                reason: "denied".to_owned()
            }]
        );
    }

    #[test]
    fn cancel_token_sees_sigterm_to_this_thread() {
        let t = CancelToken::install().unwrap();
        assert!(!t.is_cancelled());
        // SAFETY: signals this thread, which has SIGTERM blocked.
        unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) };
        assert!(t.is_cancelled());
        assert!(t.is_cancelled(), "stays cancelled");
    }
}
