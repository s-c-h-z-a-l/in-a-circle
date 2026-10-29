//! The executor and supervisor: runs an action route (one or two actions)
//! over a batch of input files, sandboxed, speaking contract v0
//! (docs/architecture.md, "Invocation"), and publishes the results, one
//! input at a time, with the output policy.
//!
//! Inputs are grouped by their final target folder; each group gets one
//! work dir inside that folder (the producers' staging)
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

pub use crate::output::{
    AnchorHint, CheckPoint, ChildBaseline, DirectoryId, Durability, FileId, GroupId,
    LocationEvidence, NodeKind, OperationId, OutputGroup, Publication, PublicationReason,
};
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
// mutants: not equivalent — a shorter period (`60 + 60`, `60 / 60`) only adds
// wakeups and utimensat calls during an action (resource cost); the safety
// property (an active work dir never looks stale) holds; accepted.
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
/// The cleanup after a child's exit, from its group's SIGKILL: the leader
/// reaped, no process of the group left, and its pipes at EOF. A killed
/// process is gone in milliseconds unless it sits in an uninterruptible
/// kernel wait (a hung filesystem); 1 s covers a loaded machine with a wide
/// margin and stays short next to the 5 s `TERM_GRACE`. Past it nothing of
/// the process is accepted: its inputs fail, nothing is placed.
pub(crate) const GROUP_GONE: Duration = Duration::from_secs(1);
/// Longest sleep between two probes of a group that is not gone yet:
/// short next to `GROUP_GONE`, and few wakeups while a stuck one is waited
/// for.
const GROUP_PROBE_MAX: Duration = Duration::from_millis(50);
/// Poll period when the kernel has no pidfd (before Linux 5.3).
pub(crate) const NO_PIDFD_POLL: Duration = Duration::from_millis(100);

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
    /// The input's outputs were published (visible, never removed again),
    /// but where they are, or that they are still what was made, could
    /// not be proved: no `Done` with an invented path. Engine-internal:
    /// the action wire protocol has no such event.
    PublishedUnknown {
        input: PathBuf,
        group: GroupId,
        location: LocationEvidence,
        reason: PublicationReason,
        durability: Durability,
    },
}

/// Per input, the strongest outcome so far: done (any output, or a
/// publication whose place is unknown: the conversion happened), else
/// failed, else skipped; an input with only warnings is not counted. One
/// entry per input, never per outcome.
#[derive(Debug, Default)]
pub struct Tally {
    rank: HashMap<PathBuf, u8>,
    /// Inputs with a `PublishedUnknown`, once each however often it is seen.
    unknown: HashSet<PathBuf>,
}

impl Tally {
    pub fn add(&mut self, o: &Outcome) {
        if let Outcome::PublishedUnknown { input, .. } = o {
            self.unknown.insert(input.clone());
        }
        let (input, rank) = match o {
            Outcome::Done { input, .. } | Outcome::PublishedUnknown { input, .. } => (input, 3),
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

    /// Inputs published where it could not be confirmed (counted in
    /// `done` too: the conversion happened).
    pub fn unknown(&self) -> usize {
        self.unknown.len()
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
    /// Whether the sandbox gate passed ([`sandbox::ready`]); without it no
    /// action ran.
    pub sandboxed: bool,
}

/// Live feedback while a route runs.
pub trait Observer {
    /// Every outcome, as it happens (`Done` once the output is in place).
    fn event(&mut self, o: &Outcome);
    /// An input's wrapper folder, before its `Done`s (undo may remove it
    /// if empty). Called by the default [`Observer::group`] only.
    fn dir(&mut self, path: &Path);
    /// An input's outputs, complete and fixed in private at `private_path`,
    /// right before the publishing rename. An error stops this input's
    /// publication only: nothing of it becomes visible.
    fn prepare_group(&mut self, _group: &OutputGroup, _private_path: &Path) -> io::Result<()> {
        Ok(())
    }
    /// An input's group after its commit, before its `Done`s (or its
    /// `PublishedUnknown`). The default reports the wrapper to
    /// [`Observer::dir`], only when its place was proved.
    fn group(&mut self, _group: &OutputGroup, location: &LocationEvidence, _d: &Durability) {
        if let Some(p) = &location.path_at_check {
            self.dir(p);
        }
    }
    /// `done` of `total` inputs finished; `fraction` of the whole run.
    fn progress(&mut self, done: usize, total: usize, fraction: f32);
    /// The first valid action event of a child was read, once per child
    /// (a two-step route has two), before the event itself is handled.
    /// `pid` is the child's. It marks the moment the supervisor *observed*
    /// the child running: an upper bound on when the child reached its
    /// entry point, not that instant. A child that emits no valid event
    /// never gets the call.
    fn started(&mut self, _pid: u32) {}
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
    /// Called on the supervisor's thread between polls: may block only
    /// briefly, under a hard deadline (read what is there, queue writes);
    /// the action's pipes go unread meanwhile. The observer drains its fd:
    /// one left readable wakes it again at once.
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
            // mutants: the failing `signalfd` branch needs an fd/memory exhaustion
            // or a stale errno to reach; it restores the mask and returns the error,
            // so the token is not installed (fail closed).
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
            // mutants: EINTR from read(2) on a signalfd is not reachable here: no
            // handler, the cancel signals are blocked. A changed branch only skips or
            // repeats a read; a cancel is still seen by the next poll (fail closed).
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return self.cancelled.get();
        }
    }
}

/// The signalfd: readable while a cancel signal is pending, for a caller
/// that waits on other fds too (then [`CancelToken::is_cancelled`]).
impl AsRawFd for CancelToken {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
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
    // The gate before anything else: no action runs without the whole
    // sandbox, built-in or not, whether the catalog knows it from a fresh
    // manifest run or from its cache.
    let gate = sandbox::ready();
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
        // The gate's own result: a bad route is not a missing sandbox, and
        // the notice must not say it is.
        return sink.report(gate.is_ok());
    }
    if let Err(e) = gate {
        all(&mut sink, &format!("sandbox unavailable: {e}"));
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
            run_group(route, &members, &g.target, cancel, &mut sink);
        }
        sink.end_group();
    }
    sink.report(true)
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

    /// Published, but its place or baseline could not be proved.
    fn unknown(
        &mut self,
        i: usize,
        p: Publication,
        location: LocationEvidence,
        reason: PublicationReason,
    ) {
        let input = self.input(i);
        self.emit(Outcome::PublishedUnknown {
            input,
            group: p.group.group,
            location,
            reason,
            durability: p.durability,
        });
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
            place_outputs(action, &step_in, outputs, &dir, target, cancel, sink);
            return;
        }
        // Each output was checked when its `done` arrived, but the action
        // could swap it for a symlink afterwards. Its whole process group is
        // gone now and its pipes are at EOF (`outputs` is `None` otherwise;
        // seccomp denies leaving the group), so nothing changes the dir any
        // more. The next step's read rules are still built here, from the
        // fds this check opens, and no path is resolved again.
        let rules = match sandbox::create() {
            Ok(r) => r,
            Err(e) => {
                for &o in &step_in.origin {
                    sink.fail(o, format!("sandbox setup failed: {e}"));
                }
                return;
            }
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
                    .and_then(|fd| {
                        sandbox::allow_read_fd(&rules, fd.as_fd(), &dir.join(rel))
                            .map_err(|e| format!("sandbox setup failed: {e}"))
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
        pre = Some(rules);
    }
}

/// Publishes the last step's outputs to `target`, one input at a time:
/// each input's outputs are copied into a fresh private wrapper folder and
/// made visible by one no-replace rename ([`output::publish_beneath`]), so
/// an input is either wholly published or not at all, and another input's
/// failure never takes back a published one. `Done` comes only for files
/// whose place the postcheck proved; a committed group that cannot be
/// proved is `PublishedUnknown`, never removed. A cancel is honoured up to
/// each input's commit.
fn place_outputs(
    action: &Action,
    step_in: &StepIn,
    outputs: Vec<Vec<PathBuf>>,
    dir: &Path,
    target: &Path,
    cancel: &CancelToken,
    sink: &mut Sink,
) {
    // Outputs per original input, in order.
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
        if sink.is_cancelled(cancel) {
            sink.fail(o, "cancelled".to_owned());
            continue;
        }
        let src = &sink.inputs[o];
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "output".to_owned());
        let mut cancelled = false;
        let published = {
            let mut before = |g: &OutputGroup, private: &Path| -> io::Result<()> {
                #[cfg(test)]
                tests::BEFORE_PREPARE.with_borrow_mut(|f| f.as_mut().map(|f| f()));
                // Before the group is prepared (a later journal keeps a
                // durable intent for it), so a cancel that came in during
                // the copy leaves nothing behind but the private staging.
                if sink.is_cancelled(cancel) {
                    cancelled = true;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                sink.obs.prepare_group(g, private)?;
                // And again right before the commit: preparing may take time.
                if sink.is_cancelled(cancel) {
                    cancelled = true;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                Ok(())
            };
            output::publish_beneath(dir, &rels, target, &stem, &action.ext, &mut before)
        };
        let p = match published {
            Ok(p) => p,
            Err(_) if cancelled => {
                sink.fail(o, "cancelled".to_owned());
                continue;
            }
            Err(e) => {
                sink.fail(o, format!("cannot publish output: {e}"));
                continue;
            }
        };
        #[cfg(test)]
        tests::AFTER_PUBLISH.with_borrow_mut(|f| f.as_mut().map(|f| f(&p)));
        let checked = output::postcheck(&p);
        let mut location = p.location.clone();
        if checked == Err(PublicationReason::AncestorChanged) {
            location.path_at_check = None;
        }
        sink.obs.group(&p.group, &location, &p.durability);
        match checked {
            Ok(files) => {
                if let Durability::Unconfirmed(why) = &p.durability {
                    sink.warn(o, format!("saved, but not confirmed on disk: {why}"));
                }
                for (path, id) in files {
                    sink.placed(o, path, id);
                }
            }
            Err(reason) => sink.unknown(o, p, location, reason),
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
///
/// `data_limit` is the `RLIMIT_DATA` the action ran under, named in the
/// out-of-memory message.
fn unreported_reason(
    violation: Option<&str>,
    end: End,
    stderr_tail: &[u8],
    data_limit: u64,
) -> String {
    if let Some(v) = violation {
        return format!("contract violation: {v}");
    }
    match end {
        End::Signal(libc::SIGXFSZ) => "output too large".to_owned(),
        // A Rust action whose allocation fails under the RLIMIT_DATA net
        // prints this line to stderr and aborts.
        End::Signal(libc::SIGABRT) if has_alloc_failure(stderr_tail) => {
            let mib = data_limit >> 20;
            format!("out of memory (limit {mib} MiB)")
        }
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

/// Whether `tail` holds Rust's allocation-failure line
/// (`memory allocation of N bytes failed`).
fn has_alloc_failure(tail: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"memory allocation of ";
    tail.windows(NEEDLE.len()).any(|w| w == NEEDLE)
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

/// Where a step's Landlock ruleset comes from.
#[derive(Debug, PartialEq, Eq)]
enum RuleSource {
    /// The pre-built ruleset (it already grants an intermediate step's
    /// inputs, from the fds their check opened) gets this step's rules.
    Extend,
    /// An intermediate step without its pre-built ruleset: building one
    /// from paths would re-resolve them (a swap race), so the step fails.
    Refuse,
    /// A first step builds its ruleset from its input paths.
    Build,
}

fn rule_source(has_pre: bool, intermediate: bool) -> RuleSource {
    match (has_pre, intermediate) {
        (true, _) => RuleSource::Extend,
        (false, true) => RuleSource::Refuse,
        (false, false) => RuleSource::Build,
    }
}

/// Runs one step's process to its end. Returns the validated outputs
/// (relative to `dir`) per input, or `None` when cancelled (every
/// unfinished input is then failed as cancelled). An intermediate step's
/// ruleset comes as `pre`, already granting its inputs.
#[allow(clippy::too_many_arguments)]
fn run_step(
    action: &Action,
    step_in: &StepIn,
    pre: Option<sandbox::Ruleset>,
    dir: &Path,
    list: &Path,
    root: &Path,
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
        write: Some(dir),
    };
    let built = match (rule_source(pre.is_some(), step_in.intermediate), pre) {
        (RuleSource::Extend, Some(r)) => sandbox::add(&r, &policy).map(|()| r),
        (RuleSource::Extend, None) => Err(io::Error::other("no ruleset to extend")),
        (RuleSource::Refuse, _) => Err(io::Error::other("no intermediate rules")),
        (RuleSource::Build, _) => sandbox::build(&policy),
    };
    let ruleset = match built {
        Ok(r) => r,
        Err(e) => return fail_all(sink, &format!("sandbox setup failed: {e}")),
    };
    let total: u64 = step_in
        .paths
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let limits = sandbox::limits_for(total);
    let tmp = match sandbox::make_child_tmp(dir) {
        Ok(t) => t,
        Err(e) => return fail_all(sink, &format!("cannot create temp directory: {e}")),
    };
    let fd = ruleset.as_raw_fd();
    let mut cmd = Command::new(&action.bin);
    // Inside the work dir, the one place the ruleset lets it write.
    sandbox::configure_child_env(&mut cmd, Some(&tmp));
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
    // Orphans of the action come back to us (see `reap_group`); without
    // that nothing could tell whether one still runs.
    if let Err(e) = become_subreaper() {
        return fail_all(sink, &format!("cannot supervise the action: {e}"));
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail_all(sink, &format!("cannot start action: {e}")),
    };
    drop(ruleset);
    let mut sup = Supervisor::new(
        child,
        step_in,
        dir,
        root,
        cancel,
        sandbox::effective_data_limit(&limits),
    );
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
    /// The `RLIMIT_DATA` the child was started with.
    data_limit: u64,
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
    /// [`Observer::started`] was called for this child.
    started: bool,
    term_at: Option<Instant>,
    killed: bool,
    /// The child is reaped: its pid, and so the group id, may now be
    /// reused by an unrelated process. Nothing is signalled after this.
    reaped: bool,
    /// `run` stopped because poll failed: nothing is accepted.
    poll_failed: bool,
}

pub(crate) fn set_nonblocking(fd: RawFd) {
    // SAFETY: fd is an open pipe fd owned by the Child handles.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

pub(crate) fn pidfd_open(pid: libc::pid_t) -> Option<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags 0; the child is not reaped
    // yet, so the pid still names it.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    // SAFETY: a non-negative result is a new fd that we now own.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Whether our child `pid` has exited, observed without reaping it (the
/// fallback for kernels without pidfd).
pub(crate) fn exited_unreaped(pid: libc::pid_t) -> bool {
    #[cfg(test)]
    tests::PROBES.set(tests::PROBES.get() + 1);
    // SAFETY: a zeroed siginfo_t is valid storage for waitid to fill.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
    // SAFETY: info is a valid out-pointer; WNOWAIT leaves the child unreaped.
    let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) };
    // With WNOHANG and no state change, si_pid stays 0.
    // SAFETY: waitid filled (or zeroed) the CLD fields that si_pid reads.
    r == 0 && unsafe { info.si_pid() } != 0
}

/// Waits until our unreaped child `pid` has exited (it stays unreaped),
/// by its pidfd or, without one, by `waitid(WNOWAIT)`; false if it has not
/// by `until`.
pub(crate) fn wait_exited(pid: libc::pid_t, pidfd: Option<&OwnedFd>, until: Instant) -> bool {
    let mut nap = Duration::from_millis(1);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match pidfd {
            Some(fd) => {
                let mut p = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // Round up so the deadline has passed when poll times out.
                let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
                // SAFETY: p is one valid pollfd.
                let r = unsafe { libc::poll(&mut p, 1, ms) };
                if r > 0 {
                    return true;
                }
                // mutants: not killable by a test. No signal handler is
                // installed and SIGTERM/SIGINT are blocked, so poll does not
                // see EINTR; the `==` -> `!=` mutant on the EINTR test would
                // only matter then (an early `false`). Every caller of
                // `wait_exited` (the supervisor, catalog.rs twice) treats
                // `false` as "not gone" and fails closed: no security gap.
                if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // mutants: the `r < 0` fallback needs a poll error other than EINTR on a
                // valid pidfd; none can be provoked. It asks `waitid` instead (no verdict
                // is guessed), so a changed condition cannot open a gap.
                if r < 0 {
                    return exited_unreaped(pid);
                }
            }
            None if exited_unreaped(pid) => return true,
            None => {}
        }
        if left.is_zero() {
            return false;
        }
        if pidfd.is_none() {
            std::thread::sleep(nap.min(left));
            nap = (nap * 2).min(GROUP_PROBE_MAX);
        }
    }
}

/// Makes this process the subreaper of everything it starts: a process
/// whose parent dies is reparented to us, not to init. With `setsid` and
/// `setpgid` denied in the sandbox, every member of an action's process
/// group then descends from one of our own children until it is reaped,
/// and the kernel's child list ([`reap_group`]) shows it whatever `/proc`
/// shows (hidepid) and whatever the member made of itself (non-dumpable).
/// Not inherited across fork (prctl(2)), so no child is one; kept across
/// our own exec, which never happens. Idempotent and cheap: called before
/// every spawn of a manifest or an action.
pub(crate) fn become_subreaper() -> io::Result<()> {
    // SAFETY: prctl with an int option and plain integer arguments.
    let r = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reaps every member of process group `pgid` with `waitid(P_PGID,
/// WEXITED | WNOHANG | __WALL)` until none is left (`ECHILD`). Called after
/// the group got our SIGKILL (sent while its leader was unreaped, so to
/// no other group) and with the leader reaped, under [`become_subreaper`]:
/// every member then descends from one of our children, and a process's
/// children become ours before it is a zombie, so `ECHILD` means the group
/// is empty and no fork chain slips past.
///
/// True if any member was alive at the kill: one still running when
/// looked at, or reaped as killed by SIGKILL. False if all had exited on
/// their own. `__WALL` also reports a child whose exit signal is not
/// SIGCHLD. Fails closed: any error but `ECHILD`, or members still there
/// at `until`, is an error (the kill is already sent either way).
pub(crate) fn reap_group(pgid: libc::pid_t, until: Instant) -> io::Result<bool> {
    let mut alive = false;
    let mut nap = Duration::from_millis(1);
    loop {
        // SAFETY: a zeroed siginfo_t is valid storage for waitid to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        #[cfg(test)]
        let injected = {
            tests::PROBES.set(tests::PROBES.get() + 1);
            tests::WAITID_ERRNO.get()
        };
        #[cfg(not(test))]
        let injected: Option<i32> = None;
        let r = match injected {
            Some(e) => {
                // SAFETY: errno is thread-local storage of this thread.
                unsafe { *libc::__errno_location() = e };
                -1
            }
            // SAFETY: info is a valid out-pointer; pgid > 0 names a group.
            None => unsafe {
                libc::waitid(
                    libc::P_PGID,
                    pgid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::__WALL,
                )
            },
        };
        if r != 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ECHILD) => return Ok(alive),
                _ => return Err(e),
            }
        }
        let timed_out = || {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "the action's processes are not all gone",
            )
        };
        // With WNOHANG and no exited child, si_pid stays 0: one still runs
        // (dying of our SIGKILL, or stuck in the kernel).
        // SAFETY: waitid filled (or zeroed) the CLD fields that si_pid reads.
        if unsafe { info.si_pid() } == 0 {
            alive = true;
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timed_out());
            }
            std::thread::sleep(nap.min(left));
            nap = (nap * 2).min(GROUP_PROBE_MAX);
            continue;
        }
        // SAFETY: as above; si_status is set for a reaped child.
        if info.si_code == libc::CLD_KILLED && unsafe { info.si_status() } == libc::SIGKILL {
            alive = true;
        }
        if Instant::now() >= until {
            return Err(timed_out());
        }
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
        data_limit: u64,
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
            data_limit,
            reported: vec![Reported::Nothing; n],
            outputs: Vec::new(),
            output_hashes: HashSet::new(),
            output_bytes: 0,
            violation: None,
            offender: None,
            cancelled: false,
            started: false,
            term_at: None,
            killed: false,
            reaped: false,
            poll_failed: false,
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
        // Reaped: the group is being finished already, and its id is no
        // longer ours to signal (a violation found in the final drain).
        if self.term_at.is_none() && !self.reaped {
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
                // mutants: as for `drain`'s poll: no handler is installed and the cancel
                // signals are blocked, so EINTR does not come; early exit fails closed.
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Unexpected: stop supervising; finish kills and reaps.
                self.signal(libc::SIGKILL);
                self.poll_failed = true;
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
            let exited = match pid_at {
                Some(_) => ready(pid_at),
                None => exited_unreaped(self.pid),
            };
            // Once the leader is gone, nothing more is read until its whole
            // group is: `finish` kills it, then reads the rest.
            if exited {
                return;
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
        if !self.started {
            self.started = true;
            sink.obs.started(self.pid as u32);
        }
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

    /// Reads both pipes to EOF, parsing stdout, until `until`. True if
    /// both reached EOF. Called once the group is gone: no process holds a
    /// write end any more, so EOF comes as soon as the pipes are empty.
    fn drain(&mut self, sink: &mut Sink, until: Instant) -> bool {
        while self.stdout.is_some() || self.stderr.is_some() {
            let out = self.stdout.as_ref().map(|s| s.as_raw_fd());
            let err = self.stderr.as_ref().map(|s| s.as_raw_fd());
            let mut fds: Vec<libc::pollfd> = [out, err]
                .into_iter()
                .flatten()
                .map(|fd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            let left = until.saturating_duration_since(Instant::now());
            // The deadline holds while data keeps arriving, not only when
            // the pipes go quiet: poll alone returns at once as long as a
            // writer never stops, so the clock is checked on every pass. In
            // practice no writer is left (the group is reaped, the sandbox
            // forbids what could keep one), but "a hard deadline on every
            // wait, data pipe reads included" must not rest on that.
            if left.is_zero() {
                return false;
            }
            let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            // SAFETY: fds is a valid array of fds.len() pollfds.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
            // mutants: unreachable in a test: EINTR needs a signal handler or an
            // unblocked signal, and none is installed (SIGTERM/SIGINT are blocked by
            // the cancel token, nothing else handles signals). A poll that failed
            // early would end the drain with `false`: the caller fails closed.
            if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if r <= 0 {
                return false;
            }
            if out.is_some() && fds[0].revents != 0 {
                self.read_stdout(sink);
            }
            let e = usize::from(out.is_some());
            if err.is_some() && fds[e].revents != 0 {
                self.read_stderr();
            }
        }
        true
    }

    /// Kills the whole group first (the unreaped leader keeps `pid` the
    /// group id ours, so the kill reaches nothing else), waits for the
    /// leader's exit and reaps it, then asks the kernel's child list
    /// whether any other process of the group was still there (then every
    /// unsettled input of this process fails and nothing is placed),
    /// reaps every member until none is left, and only then reads the rest
    /// of the pipes, to EOF. EOF is the kernel's word that no write end is
    /// open anywhere, so no writer is left, whatever pids were reused. Then
    /// it settles every input without a result. Nothing is accepted unless
    /// all of this finishes within `GROUP_GONE`.
    fn finish(mut self, sink: &mut Sink) -> Option<Vec<Vec<PathBuf>>> {
        // A cancel that came in the same wake as the exit still counts.
        if self.cancel.is_cancelled() || sink.cancelled {
            self.cancelled = true;
        }
        // The whole group is killed first, whatever happens next: the
        // unreaped leader keeps `pid` the action's group id, so the kill
        // cannot reach another group, and nothing of the action outlives
        // us even if its members cannot be counted.
        self.signal(libc::SIGKILL);
        let until = Instant::now() + GROUP_GONE;
        if !wait_exited(self.pid, self.pidfd.as_ref(), until) {
            // Killed but not exiting: stuck in the kernel. It stays
            // unreaped (its group id stays ours); nothing is accepted.
            let why = if self.cancelled {
                "cancelled"
            } else {
                "action left processes behind"
            };
            for &o in &self.step_in.origin {
                sink.fail(o, why.to_owned());
            }
            return None;
        }
        // Exited: wait returns at once.
        let end = match self.child.wait() {
            Ok(s) => End::from(s),
            Err(_) => End::Exit(-1),
        };
        self.reaped = true;
        // A descendant still running after the leader's exit fails the
        // process, whatever it reported: killed, it may have been mid-write
        // to an output. The kernel's child list tells (see `reap_group`),
        // and fails closed: unreadable, or members left at the deadline,
        // counts as left behind.
        let reaped = reap_group(self.pid, until);
        let vanished = reaped.is_ok();
        let left_behind = !matches!(reaped, Ok(false));
        let gone = vanished && !left_behind && self.drain(sink, until);
        // A line cut off by a crash is not an event; after a normal exit it
        // is a broken one. (No signal: the child is reaped.)
        if gone
            && !self.line.is_empty()
            && !self.overlong
            && matches!(end, End::Exit(_))
            && self.violation.is_none()
            && !self.cancelled
        {
            self.violation = Some("unterminated event line".to_owned());
        }
        let origin = &self.step_in.origin;
        if self.cancelled {
            for &o in origin {
                sink.fail(o, "cancelled".to_owned());
            }
            return None;
        }
        if self.poll_failed {
            for &o in origin {
                sink.fail(o, "supervision failed (poll error)".to_owned());
            }
            return None;
        }
        if !gone {
            // After a contract violation the group got our SIGTERM, so a
            // member still dying is its consequence, not a cause: the
            // violation stays the reason. Still nothing is placed.
            if vanished && self.violation.is_some() {
                if let Some((j, why)) = self.offender.take() {
                    sink.fail(origin[j], why);
                }
                let reason =
                    unreported_reason(self.violation.as_deref(), end, &self.tail, self.data_limit);
                for &o in origin {
                    sink.fail(o, reason.clone());
                }
                return None;
            }
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
        let reason = unreported_reason(self.violation.as_deref(), end, &self.tail, self.data_limit);
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
pub(crate) mod tests {
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

    thread_local! {
        /// The errno `reap_group` gets from its `waitid`, on this thread.
        pub(crate) static WAITID_ERRNO: Cell<Option<i32>> = const { Cell::new(None) };
        /// How many times `exited_unreaped` and `reap_group` called
        /// `waitid` on this thread: a count of probes does not shrink with
        /// load the way a CPU-time budget does (a slow machine probes less,
        /// never more), so a bound on it holds on a busy machine and still
        /// catches a nap that stays at zero.
        pub(crate) static PROBES: Cell<u32> = const { Cell::new(0) };
    }

    /// Starts `sh -c script` as the leader of its own group and waits for
    /// its exit; returns its pid, still unreaped, and the child.
    fn leader(script: &str) -> (libc::pid_t, std::process::Child) {
        become_subreaper().unwrap();
        let child = Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let pidfd = pidfd_open(pid);
        let until = Instant::now() + Duration::from_secs(5);
        assert!(wait_exited(pid, pidfd.as_ref(), until));
        (pid, child)
    }

    /// Kills `pid`'s group (the leader exited, unreaped), reaps the
    /// leader, then the group.
    fn kill_and_reap(pid: libc::pid_t, mut c: std::process::Child) -> io::Result<bool> {
        // SAFETY: the leader is unreaped: the group id is ours.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        c.wait().unwrap();
        reap_group(pid, Instant::now() + Duration::from_secs(5))
    }

    /// The kernel's child list after the leader's exit: a background job
    /// still running at the kill is found (and reaped); one the leader
    /// waited for, or none at all, is not.
    #[test]
    fn reap_group_tells_a_running_orphan_from_a_finished_one() {
        let (pid, c) = leader("sleep 30 & exit 0");
        assert!(kill_and_reap(pid, c).unwrap());
        let (pid, c) = leader("sleep 0.01 & wait; exit 0");
        assert!(!kill_and_reap(pid, c).unwrap());
        let (pid, c) = leader("exit 0");
        assert!(!kill_and_reap(pid, c).unwrap());
    }

    /// A member already dead of our SIGKILL when the group is reaped (no
    /// longer running) still counts as alive at the kill, by its status.
    #[test]
    fn reap_group_counts_a_member_killed_by_our_kill() {
        let (pid, mut c) = leader("sleep 30 & exit 0");
        // SAFETY: the leader is unreaped: the group id is ours.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        c.wait().unwrap();
        // Wait, without reaping, until the sleep is a zombie.
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: zeroed siginfo is valid storage; WNOWAIT reaps nothing.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL;
            // SAFETY: info is a valid out-pointer.
            let r = unsafe { libc::waitid(libc::P_PGID, pid as libc::id_t, &mut info, flags) };
            assert_eq!(r, 0, "{}", io::Error::last_os_error());
            // SAFETY: waitid filled (or zeroed) the CLD fields.
            if unsafe { info.si_pid() } != 0 {
                break;
            }
            assert!(Instant::now() < until, "the sleep did not die");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(reap_group(pid, until).unwrap());
    }

    /// A member that made itself unreadable in `/proc` (non-dumpable, by
    /// executing a binary it may not read, as `hidepid` would hide any
    /// foreign one) is still on the kernel's child list.
    #[test]
    fn reap_group_sees_a_non_dumpable_member() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("nodump");
        let bin = t.0.join("hidden-sleep");
        // Copied by `cp`, not through a writable fd of ours (ETXTBSY).
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .unwrap();
        assert!(
            Command::new("cp")
                .arg(sleep)
                .arg(&bin)
                .status()
                .unwrap()
                .success()
        );
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o111)).unwrap();
        let token = format!("30.{}{}", std::process::id(), line!());
        let (pid, c) = leader(&format!("{} {token} & exit 0", bin.display()));
        // Positive control: it is non-dumpable, so its environ (a
        // ptrace-checked file) is closed to us, as all of it is to a
        // foreign reader under hidepid.
        let hidden = fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter(|e| {
                fs::read(e.path().join("cmdline"))
                    .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&token))
            })
            .map(|e| fs::File::open(e.path().join("environ")).map(drop))
            .collect::<Vec<_>>();
        assert!(
            matches!(&hidden[..], [Err(e)] if e.raw_os_error() == Some(libc::EACCES)),
            "{hidden:?}"
        );
        assert!(kill_and_reap(pid, c).unwrap());
    }

    /// Fails closed: an error from `waitid` other than ECHILD is an error,
    /// which the callers count as left behind.
    #[test]
    fn reap_group_fails_closed_on_a_waitid_error() {
        let (pid, mut c) = leader("exit 0");
        c.wait().unwrap();
        WAITID_ERRNO.set(Some(libc::EINVAL));
        let r = reap_group(pid, Instant::now() + Duration::from_secs(1));
        WAITID_ERRNO.set(None);
        assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    /// `__WALL`: a child whose exit signal is not SIGCHLD (raw `clone`
    /// with exit signal 0) is invisible to `waitid` without it. Our own
    /// member children are reparented ones, which the kernel gives
    /// SIGCHLD (`reparent_leader`), and the sandbox lets no action make
    /// us such a child directly, so the case is built here: a direct
    /// child in its own group, as a leader would be.
    #[test]
    fn reap_group_sees_a_child_without_exit_signal() {
        // SAFETY: a raw clone with flags 0 is a fork with exit signal 0.
        // The child only makes async-signal-safe calls (setpgid, pause,
        // _exit), as after any fork of a threaded process.
        let pid = unsafe { libc::syscall(libc::SYS_clone, 0, 0, 0, 0, 0) } as libc::pid_t;
        if pid == 0 {
            // SAFETY: as above.
            unsafe {
                libc::setpgid(0, 0);
                loop {
                    libc::pause();
                }
            }
        }
        assert!(pid > 0, "clone: {}", io::Error::last_os_error());
        let until = Instant::now() + Duration::from_secs(5);
        // SAFETY: getpgid only reads; the child is ours and unreaped.
        while unsafe { libc::getpgid(pid) } != pid {
            assert!(Instant::now() < until, "the child never led its group");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Without __WALL the running child is not seen at all.
        // SAFETY: zeroed siginfo is valid storage; WNOHANG does not block.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(
                libc::P_PGID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        assert_eq!(
            (r, io::Error::last_os_error().raw_os_error()),
            (-1, Some(libc::ECHILD)),
            "control"
        );
        // SAFETY: our unreaped child leads the group: the id is ours.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        assert!(reap_group(pid, until).unwrap(), "missed the killed child");
    }

    /// The subreaper flag is not inherited across fork: a child (here,
    /// in the gap between fork and exec) is not one.
    #[test]
    fn subreaper_is_not_inherited() {
        use std::os::unix::process::CommandExt;
        become_subreaper().unwrap();
        let mut cmd = Command::new("/bin/true");
        // SAFETY: prctl is async-signal-safe; `v` is on the child's stack.
        unsafe {
            cmd.pre_exec(|| {
                let mut v: libc::c_int = -1;
                libc::prctl(
                    libc::PR_GET_CHILD_SUBREAPER,
                    &mut v as *mut libc::c_int,
                    0,
                    0,
                    0,
                );
                if v != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
                Ok(())
            });
        }
        assert!(cmd.status().unwrap().success(), "the child is a subreaper");
        let mut v: libc::c_int = -1;
        // SAFETY: PR_GET_CHILD_SUBREAPER writes one int to `v`.
        unsafe {
            libc::prctl(
                libc::PR_GET_CHILD_SUBREAPER,
                &mut v as *mut libc::c_int,
                0,
                0,
                0,
            )
        };
        assert_eq!(v, 1, "control: we are one");
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
        const LIMIT: u64 = 1 << 30;
        let tail = b"warming up\nerror: bad option --frob\n\n";
        assert_eq!(
            unreported_reason(
                Some("unknown event kind bogus"),
                End::Signal(libc::SIGTERM),
                tail,
                LIMIT
            ),
            "contract violation: unknown event kind bogus"
        );
        assert_eq!(
            unreported_reason(None, End::Signal(libc::SIGXFSZ), tail, LIMIT),
            "output too large"
        );
        assert_eq!(
            unreported_reason(None, End::Signal(libc::SIGSEGV), tail, LIMIT),
            format!("killed by signal {}", libc::SIGSEGV)
        );
        assert_eq!(
            unreported_reason(
                None,
                End::Signal(libc::SIGABRT),
                b"memory allocation of 99 bytes failed\n",
                LIMIT
            ),
            "out of memory (limit 1024 MiB)"
        );
        // An abort for any other reason stays a plain signal.
        assert_eq!(
            unreported_reason(None, End::Signal(libc::SIGABRT), tail, LIMIT),
            format!("killed by signal {}", libc::SIGABRT)
        );
        assert_eq!(
            unreported_reason(None, End::Exit(2), tail, LIMIT),
            "action invocation error: error: bad option --frob"
        );
        assert_eq!(
            unreported_reason(None, End::Exit(2), b"", LIMIT),
            "action invocation error"
        );
        for code in [0, 1, 3] {
            assert_eq!(
                unreported_reason(None, End::Exit(code), tail, LIMIT),
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
        /// the checked step's dir. The check and its rule share one fd, so
        /// no window exists between them; the hook emulates a race between
        /// the rule and the next step's spawn.
        pub(super) static AFTER_CHECK: std::cell::RefCell<Hook> =
            const { std::cell::RefCell::new(None) };
        /// Runs in `place_outputs` between an input's commit and its
        /// postcheck: the window in which a moved ancestor must not turn
        /// into a `Done` with a wrong path.
        pub(super) static AFTER_PUBLISH: std::cell::RefCell<PublishHook> =
            const { std::cell::RefCell::new(None) };
    }

    type PublishHook = Option<Box<dyn FnMut(&Publication)>>;

    thread_local! {
        /// Runs in `place_outputs` as the first thing of each
        /// `before_commit`, before the cancel check and `prepare_group`:
        /// the window in which a cancel arrives during the copy.
        pub(super) static BEFORE_PREPARE: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
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
        if crate::sandbox::tests::abi_or_skip("intermediate_rule_comes_from_the_checked_fd", 0)
            .is_none()
        {
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

    /// A cancel that arrives while the outputs are copied stops before
    /// `prepare_group` (no intent for a group that is never published),
    /// not only before the commit.
    #[test]
    fn cancel_during_the_copy_stops_before_prepare_group() {
        #[derive(Default)]
        struct Prep(Seen, usize);
        impl Observer for Prep {
            fn event(&mut self, o: &Outcome) {
                self.0.event(o);
            }
            fn dir(&mut self, _: &Path) {}
            fn progress(&mut self, _: usize, _: usize, _: f32) {}
            fn prepare_group(&mut self, _: &OutputGroup, _: &Path) -> io::Result<()> {
                self.1 += 1;
                Ok(())
            }
        }
        let t = Tmp::new("cancel-prepare");
        let docs = t.0.join("docs");
        fs::create_dir(&docs).unwrap();
        let input = docs.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let bin = t.0.join("copy");
        install_script(&bin, COPY);
        let mut action = script_action(&t, "unused", "exit 1", "md");
        action.bin = bin;
        let tok = CancelToken::install().unwrap();
        BEFORE_PREPARE.set(Some(Box::new(|| {
            // SAFETY: signals this thread, which has SIGTERM blocked.
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) };
        })));
        let mut seen = Prep::default();
        run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
        BEFORE_PREPARE.set(None);
        assert_eq!(seen.1, 0, "prepare_group ran for a cancelled input");
        assert_eq!(
            seen.0.0,
            [Outcome::Failed {
                input: input.clone(),
                reason: "cancelled".to_owned()
            }]
        );
        let mut left: Vec<_> = fs::read_dir(&docs).unwrap().flatten().collect();
        left.sort_by_key(|e| e.file_name());
        let left: Vec<_> = left.iter().map(|e| e.file_name()).collect();
        assert_eq!(left, ["a.txt"], "something became visible");
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

    /// Installs `body` as an executable `#!/bin/sh` script at `path`
    /// without this process holding a writable fd on it (`cp` writes it):
    /// a parallel test's fork could inherit one and make our exec fail
    /// with ETXTBSY.
    fn install_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let src = path.with_extension("src");
        fs::write(&src, format!("#!/bin/sh\n{body}\n")).unwrap();
        let tmp = path.with_extension("tmp");
        let ok = std::process::Command::new("/bin/cp")
            .arg(&src)
            .arg(&tmp)
            .status()
            .unwrap();
        assert!(ok.success());
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    /// Copies the first input to `0/a.md` and reports it.
    const COPY: &str = "mkdir \"$4/0\"; read -r in < \"$6\"; cat \"$in\" > \"$4/0/a.md\"\n\
                        printf 'done\\t%s\\t0/a.md\\n' \"$in\"";

    /// The group is killed before its members are counted, so even when
    /// they cannot be counted (an injected `waitid` error) nothing of the
    /// action survives the run: not a lingering descendant, nor a chain
    /// that hops pids with its pipes closed. Each is seen running first,
    /// by its exact command line (the positive control).
    #[test]
    fn an_unreadable_child_list_still_kills_the_group() {
        use crate::catalog::tests::{procs_with, seen_all_running};
        use crate::sandbox::tests::abi_or_skip;
        if abi_or_skip("an_unreadable_child_list_still_kills_the_group", 0).is_none() {
            return;
        }
        let t = Tmp::new("waitid-kill");
        let input = t.0.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let token = format!("36.{}{}", std::process::id(), line!());
        let hop_token = format!("hop{}{}", std::process::id(), line!());
        let up = t.0.join("up");
        // Each link lives 50 ms before it forks the next and exits, long
        // enough for the sampler to see one; the pid still moves on. The
        // chain is bounded (200 links, about 10 s) so it cannot outlive a
        // harness that was killed.
        let hop = "exec >&- 2>&-; hop() { [ $1 -lt 200 ] || exit 0; sleep 0.05; ( hop $(($1+1)) ) & exit 0; }; hop 0";
        // The leader holds the `skip` line back until the sampler has seen
        // both (it creates `up`), bounded to 1000 x 10 ms: the kill then
        // always meets a running lingerer and a running chain, however long
        // the sandbox and bash take to start on a loaded machine.
        // `$` is escaped for the leader's own double quotes, so the chain's
        // command line is exactly `hop`.
        let hop_quoted = hop.replace('$', "\\$");
        let script = format!(
            "sleep {token} &\n\
             bash -c \"{hop_quoted}\" {hop_token} &\n\
             n=0; while [ ! -e {up} ] && [ $n -lt 1000 ]; do sleep 0.01; n=$((n+1)); done\n\
             read -r in < \"$6\"; printf \"skip\\t%s\\tx\\n\" \"$in\"",
            up = up.display()
        );
        let bin = t.0.join("lingers");
        install_script(&bin, &format!("exec /bin/bash -c '{script}' bash \"$@\""));
        let mut action = script_action(&t, "unused", "exit 1", "md");
        action.bin = bin;
        // The sampler's own bound: the longest the whole start may take.
        let seen = seen_all_running(
            vec![
                vec!["sleep".to_owned(), token.clone()],
                vec![
                    "bash".to_owned(),
                    "-c".to_owned(),
                    hop.to_owned(),
                    hop_token.clone(),
                ],
            ],
            up.clone(),
            Duration::from_secs(20),
        );
        let tok = CancelToken::install().unwrap();
        WAITID_ERRNO.set(Some(libc::EINVAL));
        let mut seen_obs = Seen::default();
        run(
            &[&action],
            std::slice::from_ref(&input),
            &mut seen_obs,
            &tok,
        );
        WAITID_ERRNO.set(None);
        assert!(seen.join().unwrap(), "the lingerer or the chain never ran");
        // A killed process stays in /proc until the kernel has torn it down,
        // which takes longer on a loaded machine; one that escaped the kill
        // never goes (the lingerer lives 36 s, the chain about 10 s). So wait,
        // bounded to 5 s, for 3 empty samples in a row 50 ms apart: the
        // spacing is the 50 ms a link lives, so a hop that escaped shows in
        // at least one of them.
        let until = Instant::now() + Duration::from_secs(5);
        let (mut quiet, mut left) = (0, Vec::new());
        while quiet < 3 && Instant::now() < until {
            left = procs_with(&token);
            left.extend(procs_with(&hop_token));
            quiet = if left.is_empty() { quiet + 1 } else { 0 };
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(quiet >= 3, "survived the run: {left:?}");
    }

    /// A child list that cannot be read (an injected `waitid` error)
    /// fails the action: nothing tells whether a descendant still runs.
    /// Positive control: the same action, then, places its output.
    #[test]
    fn an_unreadable_child_list_fails_the_action() {
        use crate::sandbox::tests::abi_or_skip;
        if abi_or_skip("an_unreadable_child_list_fails_the_action", 0).is_none() {
            return;
        }
        let t = Tmp::new("waitid-err");
        let input = t.0.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let out = t.0.join("a/a.md");
        let bin = t.0.join("copy");
        install_script(&bin, COPY);
        let mut action = script_action(&t, "unused", "exit 1", "md");
        action.bin = bin;
        let tok = CancelToken::install().unwrap();
        WAITID_ERRNO.set(Some(libc::EINVAL));
        let mut seen = Seen::default();
        run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
        WAITID_ERRNO.set(None);
        assert_eq!(
            seen.0,
            [Outcome::Failed {
                input: input.clone(),
                reason: "action left processes behind".to_owned()
            }]
        );
        assert!(!out.exists());
        let mut seen = Seen::default();
        run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
        assert!(matches!(seen.0[..], [Outcome::Done { .. }]), "{:?}", seen.0);
    }

    /// Between an input's commit and its postcheck the folder is moved, or
    /// a published file edited: the group stays where it is, no `Done`
    /// with a path that was not proved, one `PublishedUnknown` that counts
    /// as a conversion (once, however often it is seen), and the observer's
    /// group callback gets no path for a moved folder.
    #[test]
    fn a_change_after_the_commit_is_published_unknown_not_done() {
        use crate::sandbox::tests::abi_or_skip;
        if abi_or_skip("a_change_after_the_commit_is_published_unknown", 0).is_none() {
            return;
        }
        #[derive(Default)]
        struct Groups(Seen, Vec<Option<PathBuf>>);
        impl Observer for Groups {
            fn event(&mut self, o: &Outcome) {
                self.0.event(o);
            }
            fn dir(&mut self, _: &Path) {}
            fn progress(&mut self, _: usize, _: usize, _: f32) {}
            fn group(&mut self, _: &OutputGroup, l: &LocationEvidence, _: &Durability) {
                self.1.push(l.path_at_check.clone());
            }
        }
        for (change, reason) in [
            ("move", PublicationReason::AncestorChanged),
            ("edit", PublicationReason::BaselineChanged),
        ] {
            let t = Tmp::new("after-commit");
            let docs = t.0.join("docs");
            fs::create_dir(&docs).unwrap();
            let input = docs.join("a.txt");
            fs::write(&input, "a\n").unwrap();
            let bin = t.0.join("copy");
            install_script(&bin, COPY);
            let mut action = script_action(&t, "unused", "exit 1", "md");
            action.bin = bin;
            let tok = CancelToken::install().unwrap();
            let (from, to) = (docs.clone(), t.0.join("moved"));
            AFTER_PUBLISH.set(Some(Box::new(move |_p: &Publication| {
                if change == "move" {
                    fs::rename(&from, &to).unwrap();
                } else {
                    fs::write(from.join("a/a.md"), "edited").unwrap();
                }
            })));
            let mut seen = Groups::default();
            run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
            AFTER_PUBLISH.set(None);
            let [
                Outcome::PublishedUnknown {
                    input: i,
                    location,
                    reason: r,
                    durability,
                    ..
                },
            ] = &seen.0.0[..]
            else {
                panic!("{change}: {:?}", seen.0.0)
            };
            assert_eq!((i, *r, durability), (&input, reason, &Durability::Synced));
            let kept = if change == "move" {
                assert_eq!(location.path_at_check, None);
                assert_eq!(seen.1, [None]);
                t.0.join("moved/a/a.md")
            } else {
                assert_eq!(location.path_at_check, Some(docs.join("a")));
                docs.join("a/a.md")
            };
            assert!(kept.exists(), "{change}: the published output is gone");
            let mut tally = Tally::default();
            tally.add(&seen.0.0[0]);
            tally.add(&seen.0.0[0]);
            assert_eq!(tally.counts(), (1, 0, 0));
            assert_eq!(tally.unknown(), 1, "one input, two events");
        }
    }

    /// Every action, built-in or a plugin's, runs only behind the whole
    /// sandbox gate; a failure is a refusal of every input (never a
    /// warning and an unconfined run), and the action never starts.
    /// Positive control: the same action runs once the gate passes.
    #[test]
    fn every_action_needs_the_sandbox_gate() {
        use crate::sandbox::tests::{GATE_FAILURES, abi_or_skip, inject_failure};
        if abi_or_skip("every_action_needs_the_sandbox_gate", 0).is_none() {
            return;
        }
        let t = Tmp::new("gate");
        let input = t.0.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let out = t.0.join("a/a.md");
        for builtin in [true, false] {
            let bin = t.0.join(if builtin { "builtin" } else { "plugin" });
            install_script(&bin, COPY);
            let mut action = script_action(&t, "unused", "exit 1", "md");
            action.bin = bin;
            action.builtin = builtin;
            let tok = CancelToken::install().unwrap();
            for name in GATE_FAILURES {
                let _g = inject_failure(name);
                let mut seen = Seen::default();
                let r = run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
                // A setup error comes after the gate (which passed).
                assert_eq!(
                    r.sandboxed,
                    name == "setup error",
                    "builtin={builtin}, {name}"
                );
                match &seen.0[..] {
                    [Outcome::Failed { reason, .. }] => assert!(
                        reason.starts_with("sandbox unavailable: ")
                            || reason.starts_with("sandbox setup failed: "),
                        "builtin={builtin}, {name}: {reason}"
                    ),
                    other => panic!("builtin={builtin}, {name}: {other:?}"),
                }
                // The ambient case is refused on its own branch, not on
                // permitted or effective.
                if let ("ambient capability", [Outcome::Failed { reason, .. }]) =
                    (name, &seen.0[..])
                {
                    assert!(reason.contains("ambient capabilities"), "{reason}");
                }
                assert!(!out.exists(), "builtin={builtin}, {name}: the action ran");
            }
            let mut seen = Seen::default();
            let r = run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
            assert!(r.sandboxed);
            assert!(matches!(seen.0[..], [Outcome::Done { .. }]), "{:?}", seen.0);
            assert_eq!(fs::read_to_string(&out).unwrap(), "a\n");
            fs::remove_dir_all(t.0.join("a")).unwrap();
        }
    }

    /// An action the catalog knows from its cache (its manifest does not
    /// run again) still meets the gate when it runs.
    #[test]
    fn a_cached_action_still_needs_the_sandbox_gate() {
        use crate::catalog::Catalog;
        use crate::sandbox::tests::{abi_or_skip, inject_failure};
        if abi_or_skip("a_cached_action_still_needs_the_sandbox_gate", 0).is_none() {
            return;
        }
        let t = Tmp::new("cached");
        let dir = t.0.join("actions");
        fs::create_dir(&dir).unwrap();
        let cache = t.0.join("cache/catalog");
        install_script(
            &dir.join("conv"),
            &format!(
                "if [ \"$1\" = --manifest ]; then\n\
                 printf 'inacircle-manifest 0\\naction\\tx.conv\\tfamily=x\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=C\\tgroup=g\\tcost=1\\tlossy=no\\tbatch=yes\\n'\n\
                 exit 0\nfi\n{COPY}"
            ),
        );
        let input = t.0.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        // The manifest runs (sandboxed) and the cache is written.
        let warm = Catalog::load_from(&[(dir.clone(), true)], &cache);
        assert!(warm.get("x.conv").is_some());
        assert!(cache.exists());
        let tok = CancelToken::install().unwrap();
        for name in ["no landlock", "root"] {
            let _g = inject_failure(name);
            // No manifest could run now: the action comes from the cache.
            let cat = Catalog::load_from(&[(dir.clone(), true)], &cache);
            let action = cat.get("x.conv").expect("known from the cache");
            let mut seen = Seen::default();
            let r = run(&[action], std::slice::from_ref(&input), &mut seen, &tok);
            assert!(!r.sandboxed, "{name}");
            assert!(
                matches!(&seen.0[..], [Outcome::Failed { reason, .. }] if reason.starts_with("sandbox unavailable: ")),
                "{name}: {:?}",
                seen.0
            );
            assert!(!t.0.join("a").exists(), "{name}: the cached action ran");
        }
        // Control: the same cached action runs behind a passing gate.
        let cat = Catalog::load_from(&[(dir, true)], &cache);
        let mut seen = Seen::default();
        run(
            &[cat.get("x.conv").unwrap()],
            std::slice::from_ref(&input),
            &mut seen,
            &tok,
        );
        assert!(matches!(seen.0[..], [Outcome::Done { .. }]), "{:?}", seen.0);
    }

    #[test]
    fn an_intermediate_step_never_builds_its_own_ruleset() {
        assert_eq!(rule_source(true, true), RuleSource::Extend);
        assert_eq!(rule_source(true, false), RuleSource::Extend);
        assert_eq!(rule_source(false, true), RuleSource::Refuse);
        assert_eq!(rule_source(false, false), RuleSource::Build);
    }

    #[test]
    fn done_index_accepts_only_canonical_decimals_below_the_batch_size() {
        assert_eq!(done_index("0/a.md", 2), Ok(0));
        assert_eq!(done_index("1/a.md", 2), Ok(1));
        assert_eq!(done_index("10/a.md", 11), Ok(10));
        for bad in [
            "00/a.md",
            "01/a.md",
            "+1/a.md",
            "-1/a.md",
            "1a/a.md",
            "a/a.md",
            "2/a.md",
            "/1/a.md",
            "1",
            "1//a.md",
            "1/../a.md",
        ] {
            assert!(done_index(bad, 2).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn touch_refreshes_the_work_dir_mtime() {
        let t = Tmp::new("touch");
        let old = libc::timespec {
            tv_sec: 1_000_000,
            tv_nsec: 0,
        };
        let times = [old, old];
        let c = CString::new(t.0.as_os_str().as_bytes()).unwrap();
        // SAFETY: c is a valid path, times points to two timespecs.
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(r, 0);
        touch(&t.0);
        let age = fs::metadata(&t.0)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(
            age < Duration::from_secs(60),
            "mtime not refreshed: {age:?}"
        );
    }

    #[test]
    fn a_route_of_no_or_three_steps_fails_every_input() {
        let t = Tmp::new("route-len");
        let input = t.0.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let a = script_action(&t, "act", "exit 0", "md");
        for route in [&[][..], &[&a, &a, &a][..]] {
            let tok = CancelToken::install().unwrap();
            let mut seen = Seen::default();
            let report = run(route, std::slice::from_ref(&input), &mut seen, &tok);
            assert_eq!(
                seen.0,
                [Outcome::Failed {
                    input: input.clone(),
                    reason: "invalid route".to_owned()
                }]
            );
            // The sandbox was not the problem: the report carries the
            // gate's result, so the notice does not say "Sandbox
            // unavailable." for a route error.
            assert_eq!(report.sandboxed, sandbox::ready().is_ok());
        }
        assert_eq!(
            fs::read_dir(&t.0).unwrap().count(),
            2,
            "nothing was created"
        );
    }

    #[test]
    fn a_directory_is_not_an_input_and_nothing_is_placed_for_it() {
        let t = Tmp::new("dir-input");
        let dir_input = t.0.join("folder.txt");
        fs::create_dir(&dir_input).unwrap();
        let a = script_action(&t, "act", "echo ran > \"$4/ran\"", "md");
        let tok = CancelToken::install().unwrap();
        let mut seen = Seen::default();
        run(&[&a], std::slice::from_ref(&dir_input), &mut seen, &tok);
        assert_eq!(
            seen.0,
            [Outcome::Failed {
                input: dir_input.clone(),
                reason: "not a regular file".to_owned()
            }]
        );
        let left: Vec<_> = fs::read_dir(&t.0).unwrap().flatten().collect();
        assert_eq!(left.len(), 2, "only the input and the script: {left:?}");
    }

    /// Child side of the no-Landlock wiring test (the kernel itself has no
    /// Landlock there, no injection); a no-op without the env.
    #[test]
    fn helper_no_landlock() {
        let (Some(dir), Some(case)) = (
            std::env::var_os("INACIRCLE_HELPER"),
            std::env::var_os("INACIRCLE_CASE"),
        ) else {
            return;
        };
        // SAFETY: alarm has no preconditions; a watchdog against a hang.
        unsafe { libc::alarm(20) };
        // Landlock "does not exist" for this process and its children.
        crate::output::tests::deny_syscalls(
            &[libc::SYS_landlock_create_ruleset],
            libc::ENOSYS as u32,
        );
        assert!(sandbox::landlock_abi().is_none());
        let t = Tmp(PathBuf::from(dir));
        let input = t.0.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let mut action = script_action(
            &t,
            "act",
            "mkdir \"$4/0\"; read -r in < \"$6\"; cat \"$in\" > \"$4/0/a.md\"\n\
             printf 'done\\t%s\\t0/a.md\\n' \"$in\"",
            "md",
        );
        let tok = CancelToken::install().unwrap();
        let mut seen = Seen::default();
        // Built-in or not, no action runs without Landlock.
        action.builtin = match case.to_str().unwrap() {
            "plugin" => false,
            "builtin" => true,
            other => panic!("unknown case {other}"),
        };
        let r = run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
        assert!(!r.sandboxed);
        assert_eq!(
            seen.0,
            [Outcome::Failed {
                input: input.clone(),
                reason: "sandbox unavailable: Landlock unavailable".to_owned()
            }]
        );
        assert!(!t.0.join("a").exists(), "an action ran unconfined");
        // SAFETY: skips the harness' teardown, which the filter may affect.
        unsafe { libc::_exit(0) };
    }

    #[test]
    fn sandbox_without_landlock_refuses_every_action() {
        if !crate::sandbox::tests::seccomp_or_skip("without_landlock") {
            return;
        }
        for case in ["plugin", "builtin"] {
            let t = Tmp::new("no-landlock");
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "exec::tests::helper_no_landlock",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("INACIRCLE_HELPER", &t.0)
                .env("INACIRCLE_CASE", case)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "case {case}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    // ---- Mutation pass 2 (D-39): tests for cleanup, supervision and the
    // ---- cancel token.

    /// How long a probing wait runs, and how many probes it may make. The
    /// nap doubles from 1 ms up to `GROUP_PROBE_MAX` (50 ms), so 400 ms is
    /// about 14 probes at the very most (6 doubling steps, then one per
    /// 50 ms); oversleeping on a loaded machine only lowers it. A nap stuck
    /// at zero probes thousands of times, an additive nap about 28 times:
    /// 20 separates both from the real code with a margin, whatever the load.
    const PROBE_WAIT: Duration = Duration::from_millis(400);
    const PROBES_MAX: u32 = 20;

    fn blocked(sig: libc::c_int) -> bool {
        // SAFETY: with a null `set`, pthread_sigmask only reads the current
        // mask into `cur`, a valid out-pointer.
        unsafe {
            let mut cur: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut cur);
            libc::sigismember(&cur, sig) == 1
        }
    }

    /// Dropping the token puts the thread's signal mask back: SIGTERM and
    /// SIGINT are blocked while it lives (so only the signalfd sees them)
    /// and deliverable again after.
    #[test]
    fn dropping_the_token_restores_the_signal_mask() {
        // The mask is inherited through exec: a parent that blocks these
        // signals is not this test's failure, so compare with the start.
        let before = (blocked(libc::SIGTERM), blocked(libc::SIGINT));
        let tok = CancelToken::install().unwrap();
        assert!(blocked(libc::SIGTERM) && blocked(libc::SIGINT));
        drop(tok);
        assert_eq!((blocked(libc::SIGTERM), blocked(libc::SIGINT)), before);
    }

    /// The raw fd is the signalfd: silent until a cancel signal is
    /// pending, readable once one is, so a caller can poll it.
    #[test]
    fn the_token_fd_becomes_readable_on_a_cancel_signal() {
        let tok = CancelToken::install().unwrap();
        let readable = || {
            let mut p = libc::pollfd {
                fd: tok.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: p is one valid pollfd.
            unsafe { libc::poll(&mut p, 1, 0) };
            p.revents & libc::POLLIN != 0
        };
        assert!(!readable());
        // SAFETY: raise signals this thread, which blocks SIGTERM now.
        unsafe { libc::raise(libc::SIGTERM) };
        assert!(readable());
        assert!(tok.is_cancelled());
    }

    /// A reader that plays back a script of results (popped from the
    /// back), then data forever.
    struct Script(Vec<io::Result<usize>>);
    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop() {
                Some(Ok(n)) => {
                    buf[..n].fill(b'x');
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => {
                    buf.fill(b'x');
                    Ok(buf.len())
                }
            }
        }
    }

    /// `read_quota` reads the whole quota from a flood (a smaller one only
    /// adds wakeups, a larger one delays the cancel) and stops within one
    /// buffer of it.
    #[test]
    fn read_quota_takes_the_whole_quota_from_a_flood() {
        let mut total = 0;
        let (open, full) = read_quota(&mut Script(Vec::new()), |b| total += b.len());
        assert!(open && full);
        // 64 KiB spelled out: the constant under test must not be its own
        // oracle.
        assert!((65_536..65_536 + 8192).contains(&total), "{total}");
    }

    /// An interrupted read is retried, WouldBlock leaves the pipe open for
    /// the next wakeup, and any other error closes it (fail closed: a
    /// broken pipe is never polled again as if healthy).
    #[test]
    fn read_quota_handles_interrupts_would_block_and_errors() {
        let ek = |k| Err(io::Error::from(k));
        let mut got = 0;
        let r = read_quota(
            &mut Script(vec![
                ek(io::ErrorKind::WouldBlock),
                Ok(5),
                ek(io::ErrorKind::Interrupted),
            ]),
            |b| got += b.len(),
        );
        assert_eq!((r, got), ((true, false), 5), "interrupt, data, would-block");
        let r = read_quota(&mut Script(vec![ek(io::ErrorKind::BrokenPipe)]), |_| {});
        assert_eq!(r, (false, false));
        let r = read_quota(&mut Script(vec![Ok(0)]), |_| {});
        assert_eq!(r, (false, false));
    }

    /// A status that is neither an exit nor a signal (a stop) reads as exit
    /// -1; exits and signals keep their numbers.
    #[test]
    fn end_from_status_keeps_codes_and_maps_a_stop_to_minus_one() {
        assert_eq!(End::from(ExitStatus::from_raw(3 << 8)), End::Exit(3));
        assert_eq!(
            End::from(ExitStatus::from_raw(libc::SIGKILL)),
            End::Signal(libc::SIGKILL)
        );
        // WIFSTOPPED (0x137f): neither exited nor signaled.
        assert_eq!(End::from(ExitStatus::from_raw(0x137f)), End::Exit(-1));
    }

    /// `set_nonblocking` sets O_NONBLOCK and keeps it when called again
    /// (it must not toggle).
    #[test]
    fn set_nonblocking_is_idempotent() {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: fds holds two ints, as pipe2 writes.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: F_GETFL on an open fd.
        let nb = |fd| unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK != 0;
        assert!(!nb(fds[0]));
        set_nonblocking(fds[0]);
        assert!(nb(fds[0]));
        set_nonblocking(fds[0]);
        assert!(nb(fds[0]));
        // SAFETY: both fds are ours.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    fn spawn_sh(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    /// The fallback exit probe (no pidfd): false while the child runs,
    /// true once it has exited, still unreaped; false for a pid that is not
    /// our child.
    #[test]
    fn exited_unreaped_tells_running_from_exited_without_reaping() {
        let mut running = spawn_sh("sleep 30");
        let pid = running.id() as libc::pid_t;
        assert!(!exited_unreaped(pid));
        // SAFETY: our own unreaped child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let until = Instant::now() + Duration::from_secs(5);
        while !exited_unreaped(pid) {
            assert!(
                Instant::now() < until,
                "a killed child never showed as exited"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(exited_unreaped(pid), "the probe must not reap");
        assert!(running.try_wait().unwrap().is_some(), "still reapable once");
        // Not our child: waitid fails (ECHILD), which reads as not exited.
        assert!(!exited_unreaped(1));
    }

    /// Without a pidfd `wait_exited` polls `waitid`: it sees an exit, gives
    /// up at the deadline for a child that runs on, and does not spin while
    /// it waits.
    #[test]
    fn wait_exited_without_a_pidfd_waits_and_gives_up_cheaply() {
        let mut quick = spawn_sh("exit 0");
        let t0 = Instant::now();
        assert!(wait_exited(
            quick.id() as libc::pid_t,
            None,
            t0 + Duration::from_secs(5)
        ));
        quick.wait().unwrap();

        let mut slow = spawn_sh("sleep 30");
        PROBES.set(0);
        let t0 = Instant::now();
        assert!(!wait_exited(
            slow.id() as libc::pid_t,
            None,
            t0 + PROBE_WAIT
        ));
        assert!(t0.elapsed() >= PROBE_WAIT, "returned before the deadline");
        let probes = PROBES.get();
        assert!(probes <= PROBES_MAX, "the wait spun: {probes} probes");
        slow.kill().unwrap();
        slow.wait().unwrap();
    }

    /// `reap_group` sleeps between probes of a member that still runs: it
    /// gives up at the deadline without spinning.
    #[test]
    fn reap_group_waits_for_a_running_member_without_spinning() {
        let (pid, c) = leader("sleep 30 & exit 0");
        PROBES.set(0);
        let t0 = Instant::now();
        let r = reap_group(pid, t0 + PROBE_WAIT);
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() >= PROBE_WAIT);
        let probes = PROBES.get();
        assert!(probes <= PROBES_MAX, "the reap spun: {probes} probes");
        // The member still runs: end the group (reap_group took the leader).
        drop(c);
        // SAFETY: the group id stays ours while a member of it exists.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        reap_group(pid, Instant::now() + Duration::from_secs(5)).unwrap();
    }

    /// Whether process group `pgid` holds an exited, unreaped member
    /// (`waitid(P_PGID, WNOWAIT)` sees it; the leader is already reaped).
    fn member_exited_unreaped(pgid: libc::pid_t) -> bool {
        // SAFETY: a zeroed siginfo_t is valid storage for waitid to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG | libc::__WALL;
        // SAFETY: info is a valid out-pointer; WNOWAIT reaps nothing.
        let r = unsafe { libc::waitid(libc::P_PGID, pgid as libc::id_t, &mut info, flags) };
        // SAFETY: waitid filled (or zeroed) the CLD fields that si_pid reads.
        r == 0 && unsafe { info.si_pid() } != 0
    }

    /// Only a member that was running at our kill counts as alive: one that
    /// ended by itself before it (a zombie of ours once its leader is gone)
    /// does not, whether it exited with 0, exited with the number 9 (the
    /// number of SIGKILL), or died of its own SIGTERM.
    #[test]
    fn reap_group_counts_only_members_running_at_the_kill() {
        for script in [
            "sleep 0.1 & exit 0",
            "sh -c 'sleep 0.1; exit 9' & exit 0",
            "sh -c 'sleep 0.1; kill -TERM $$' & exit 0",
        ] {
            let (pid, mut c) = leader(script);
            // The member must have ended on its own before the kill: wait
            // (bounded) until the group holds an exited member besides the
            // leader, which is reaped first so it cannot answer for it. A
            // fixed sleep would break on a loaded machine.
            c.wait().unwrap();
            let until = Instant::now() + Duration::from_secs(30);
            while !member_exited_unreaped(pid) {
                assert!(Instant::now() < until, "{script}: the member never ended");
                std::thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: the zombie member keeps the group id ours.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let r = reap_group(pid, Instant::now() + Duration::from_secs(5));
            assert!(!r.unwrap(), "{script}");
        }
    }

    fn step_over(t: &Tmp) -> StepIn {
        StepIn {
            paths: vec![t.0.join("in.txt")],
            origin: vec![0],
            intermediate: false,
        }
    }

    /// A supervisor over `script`, for driving its parts directly.
    fn sup_over<'a>(
        script: &str,
        step: &'a StepIn,
        t: &'a Tmp,
        tok: &'a CancelToken,
    ) -> Supervisor<'a> {
        become_subreaper().unwrap();
        Supervisor::new(spawn_sh(script), step, &t.0, &t.0, tok, 0)
    }

    /// Ends and reaps a test supervisor's child group.
    fn stop(sup: &mut Supervisor) {
        sup.signal(libc::SIGKILL);
        sup.child.wait().unwrap();
    }

    /// A line of exactly `MAX_LINE` bytes (no newline yet) is pending; one
    /// byte more is a violation.
    #[test]
    fn feed_limits_a_line_to_max_line() {
        let t = Tmp::new("feed");
        let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
        let (mut seen, inputs) = (Seen::default(), step.paths.clone());
        let mut sink = Sink::new(&mut seen, &inputs);
        let mut sup = sup_over("sleep 30", &step, &t, &tok);
        sup.feed(&vec![b'x'; MAX_LINE], &mut sink);
        assert_eq!(sup.violation, None);
        sup.feed(b"x", &mut sink);
        assert_eq!(sup.violation.as_deref(), Some("event line too long"));
        stop(&mut sup);
    }

    /// The stderr tail keeps exactly the last `STDERR_TAIL` bytes, and
    /// `drain` reads both pipes to EOF.
    #[test]
    fn stderr_tail_keeps_the_last_bytes_and_drain_reaches_eof() {
        let t = Tmp::new("tail");
        let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
        // stdout is closed at once while stderr carries 200 KB, more than
        // one `READ_QUOTA` and than a pipe holds: `drain` must go on for
        // stderr after stdout is done.
        let mut sup = sup_over(
            "exec >&-; head -c 200000 /dev/zero | tr '\\0' a >&2; head -c 100 /dev/zero | tr '\\0' b >&2",
            &step,
            &t,
            &tok,
        );
        let until = Instant::now() + Duration::from_secs(10);
        let (mut seen, inputs) = (Seen::default(), step.paths.clone());
        let mut sink = Sink::new(&mut seen, &inputs);
        assert!(sup.drain(&mut sink, until));
        assert!(sup.stdout.is_none() && sup.stderr.is_none());
        assert_eq!(sup.tail.len(), STDERR_TAIL);
        assert!(sup.tail.ends_with(&[b'b'; 100]));
        assert!(sup.tail[..STDERR_TAIL - 100].iter().all(|&b| b == b'a'));
        sup.child.wait().unwrap();
    }

    /// `drain` gives up at its deadline while writers never stop (three of
    /// them, so the pipe is hardly ever empty and poll returns ready, EOF
    /// never comes): it returns false at `until`,
    /// not when the writer ends. The writer is bounded to 10 s (`timeout`)
    /// so a broken `drain` fails the test instead of hanging it; the group
    /// is killed at the end.
    #[test]
    fn drain_gives_up_at_the_deadline_while_data_keeps_arriving() {
        let t = Tmp::new("drain-flood");
        let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
        let mut sup = sup_over(
            "exec >&-; timeout 10 yes >&2 & timeout 10 yes >&2 & exec timeout 10 yes >&2",
            &step,
            &t,
            &tok,
        );
        let (mut seen, inputs) = (Seen::default(), step.paths.clone());
        let mut sink = Sink::new(&mut seen, &inputs);
        let t0 = Instant::now();
        let given_up = !sup.drain(&mut sink, t0 + Duration::from_millis(300));
        let took = t0.elapsed();
        stop(&mut sup);
        assert!(given_up, "drain ran on to EOF past its deadline");
        // 5 s: far past the deadline, far short of the writer's 10 s.
        assert!(took < Duration::from_secs(5), "drain took {took:?}");
    }

    /// A deadline that has passed ends `drain` with false even though the
    /// pipes are ready and would reach EOF at once: data waiting is not a
    /// reason to go on past it. Deterministic, unlike the flood above (which
    /// shows the wall-clock effect, but with a writer that may fall behind).
    #[test]
    fn drain_does_not_read_on_after_its_deadline() {
        let t = Tmp::new("drain-late");
        let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
        let mut sup = sup_over("exec >&-; echo data >&2", &step, &t, &tok);
        // Exited: the data is in the pipe and EOF follows it.
        sup.child.wait().unwrap();
        let (mut seen, inputs) = (Seen::default(), step.paths.clone());
        let mut sink = Sink::new(&mut seen, &inputs);
        let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(!sup.drain(&mut sink, past));
        assert!(sup.tail.is_empty(), "read past the deadline");
    }

    fn done_event(t: &Tmp, rel: &str) -> Vec<u8> {
        fs::create_dir_all(t.0.join(rel).parent().unwrap()).unwrap();
        fs::write(t.0.join(rel), "x").unwrap();
        format!("done\t{}\t{rel}", t.0.join("in.txt").display()).into_bytes()
    }

    /// Output paths: a second `done` for the same path is a violation, a
    /// different path is not; the byte total may reach `MAX_OUTPUT_BYTES`
    /// exactly and not exceed it.
    #[test]
    fn done_events_are_checked_for_duplicates_and_the_byte_cap() {
        let t = Tmp::new("done");
        let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
        let (mut seen, inputs) = (Seen::default(), step.paths.clone());
        let mut sink = Sink::new(&mut seen, &inputs);
        let (a, b) = (done_event(&t, "0/a.md"), done_event(&t, "0/b.md"));
        // The same path twice while it is the only one recorded.
        let mut sup = sup_over("sleep 30", &step, &t, &tok);
        sup.handle_line(&a, &mut sink);
        sup.handle_line(&a, &mut sink);
        let v = sup.violation.clone().unwrap();
        assert!(v.starts_with("duplicate output"), "{v}");
        assert_eq!(sup.outputs.len(), 1);
        stop(&mut sup);
        // A different path is no duplicate.
        let mut sup = sup_over("sleep 30", &step, &t, &tok);
        sup.handle_line(&a, &mut sink);
        sup.handle_line(&b, &mut sink);
        assert_eq!(sup.violation, None);
        assert_eq!(sup.outputs.len(), 2);
        stop(&mut sup);

        // The path "0/a.md" is 6 bytes: at the cap exactly is accepted, one
        // byte over is refused.
        for (used, ok) in [(MAX_OUTPUT_BYTES - 6, true), (MAX_OUTPUT_BYTES - 5, false)] {
            let mut sup = sup_over("sleep 30", &step, &t, &tok);
            sup.output_bytes = used;
            sup.handle_line(&a, &mut sink);
            assert_eq!(sup.violation.is_none(), ok, "used {used}");
            stop(&mut sup);
        }
    }

    /// A violation is recorded once (the first stays), never while
    /// cancelled, and ends the group with SIGTERM.
    #[test]
    fn violate_keeps_the_first_reason_and_ignores_a_cancelled_run() {
        let t = Tmp::new("violate");
        let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
        let mut sup = sup_over("sleep 30", &step, &t, &tok);
        sup.violate("first".to_owned());
        sup.violate("second".to_owned());
        assert_eq!(sup.violation.as_deref(), Some("first"));
        assert!(sup.term_at.is_some());
        stop(&mut sup);

        let mut sup = sup_over("sleep 30", &step, &t, &tok);
        sup.cancelled = true;
        sup.violate("late".to_owned());
        sup.violate_by(Some(0), "late".to_owned(), None);
        assert_eq!(sup.violation, None);
        assert!(sup.offender.is_none() && sup.term_at.is_none());
        stop(&mut sup);
    }

    /// A cancel seen by either the token or the observer, at the child's
    /// exit, fails the input as cancelled and places nothing.
    #[test]
    fn finish_honours_a_cancel_from_the_token_or_the_observer() {
        for from_observer in [true, false] {
            let t = Tmp::new("finish-cancel");
            let (step, tok) = (step_over(&t), CancelToken::install().unwrap());
            let (mut seen, inputs) = (Seen::default(), step.paths.clone());
            let mut sink = Sink::new(&mut seen, &inputs);
            let sup = sup_over("exit 0", &step, &t, &tok);
            if from_observer {
                sink.cancelled = true;
            } else {
                // SAFETY: raise signals this thread, which blocks SIGTERM.
                unsafe { libc::raise(libc::SIGTERM) };
            }
            assert!(sup.finish(&mut sink).is_none());
            drop(sink);
            assert_eq!(
                seen.0,
                [Outcome::Failed {
                    input: step.paths[0].clone(),
                    reason: "cancelled".to_owned()
                }],
                "observer: {from_observer}"
            );
            tok.is_cancelled();
        }
    }

    /// The default `group` callback reports the published wrapper folder to
    /// `dir`.
    #[test]
    fn the_default_group_callback_reports_the_wrapper_folder() {
        use crate::sandbox::tests::abi_or_skip;
        if abi_or_skip("the_default_group_callback_reports_the_wrapper_folder", 0).is_none() {
            return;
        }
        #[derive(Default)]
        struct Dirs(Seen, Vec<PathBuf>);
        impl Observer for Dirs {
            fn event(&mut self, o: &Outcome) {
                self.0.event(o);
            }
            fn dir(&mut self, p: &Path) {
                self.1.push(p.to_owned());
            }
            fn progress(&mut self, _: usize, _: usize, _: f32) {}
        }
        let t = Tmp::new("group-default");
        let docs = t.0.join("docs");
        fs::create_dir(&docs).unwrap();
        let input = docs.join("a.txt");
        fs::write(&input, "a\n").unwrap();
        let bin = t.0.join("copy");
        install_script(&bin, COPY);
        let mut action = script_action(&t, "unused", "exit 1", "md");
        action.bin = bin;
        let tok = CancelToken::install().unwrap();
        let mut seen = Dirs::default();
        run(&[&action], std::slice::from_ref(&input), &mut seen, &tok);
        assert!(
            matches!(seen.0.0[..], [Outcome::Done { .. }]),
            "{:?}",
            seen.0.0
        );
        assert_eq!(seen.1, [docs.join("a")]);
    }

    /// Progress is `(base + span * (step + f) / steps) / total`: the second
    /// step of two, halfway, in a group of two after one input.
    #[test]
    fn sink_progress_scales_by_steps_group_and_total() {
        #[derive(Default)]
        struct P(Vec<(usize, usize, f32)>);
        impl Observer for P {
            fn event(&mut self, _: &Outcome) {}
            fn dir(&mut self, _: &Path) {}
            fn progress(&mut self, d: usize, t: usize, f: f32) {
                self.0.push((d, t, f));
            }
        }
        let inputs: Vec<PathBuf> = (0..4).map(|i| PathBuf::from(format!("/in/{i}"))).collect();
        let mut obs = P::default();
        let mut sink = Sink::new(&mut obs, &inputs);
        sink.base = 1;
        sink.span = 2;
        sink.steps = 2;
        sink.step = 1;
        sink.progress(0.5);
        drop(sink);
        // within = (1 + 0.5) / 2 = 0.75; frac = (1 + 2 * 0.75) / 4 = 0.625.
        assert_eq!(obs.0, [(0, 4, 0.625)]);
    }
}
