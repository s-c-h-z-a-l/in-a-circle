//! `inacircle`: the wheel, and the engine's command line (headless mode).
//!
//! ```text
//! inacircle                                   the wheel (the hotkey)
//! inacircle run [--notify] <preset-id> <files…>          the menu path
//! inacircle run [--notify] --action <action-id> <files…> one action directly
//! inacircle offer <files…>                    the wheel for these files
//! inacircle undo <job-id>
//! inacircle jobs
//! inacircle cancel <job-id>
//! ```
//!
//! Output is manifest lines on stdout. `run` prints `job <id>`, then one
//! `done <input> <final path>` / `skip|fail|warn <input> <text>` line per
//! outcome as it happens (plus `progress <fraction>` of the whole run), and
//! ends with `summary done=<n> skipped=<n> failed=<n>`, counted per input.
//! Its exit code follows the batch rule: 0 if any input is done, else 1 if
//! any failed, else 3 (all unsupported). A cancel fails the inputs that had
//! not finished (`cancelled`); the batch rule then applies as usual.
//! An invocation error (unknown subcommand, preset or action, no files, an
//! empty path) is a message on stderr and exit 2, with nothing on stdout.
//! The same file (by canonical path) given more than once counts once; the
//! first spelling wins.
//!
//! `--notify` adds desktop notifications: "running" with Cancel after a
//! second, the result with Undo at the end. The stdout lines are written
//! before the Undo window starts; the exit code does not depend on them.
//!
//! Without arguments: the wheel. SIGALRM is reset to its default action
//! and unblocked, whatever the parent left us, and `alarm` armed first: it
//! kills the process if a stage (the lock included) outlives its deadline.
//! Then one instance at a time: a held lock makes a second one exit 0 at
//! once, before touching Wayland. The lock file is opened with
//! `O_NOFOLLOW | O_NONBLOCK` and must be a regular file; anything else
//! (a symlink, a FIFO) is an error, exit 1. Closed without a choice:
//! exit 0.
//! A choice tears the wheel down, releases the lock, then runs the preset
//! as `run --notify` does, exit code included.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use engine::catalog::{Action, Catalog, missing_tools};
use engine::classify::{Classified, Kind, classify};
use engine::exec::{self, CancelToken, Observer, Outcome, Wake};
use engine::journal::{self, JobId, Recorder, Running};
use engine::menu::{self, PRESETS};
use manifest::{Line, write_line};
use notice::Notifier;

mod measure;
mod notice;
mod surface;
mod uri;
mod wheel;

const USAGE: &str = "usage: inacircle run [--notify] <preset-id> <files…>\n       \
inacircle run [--notify] --action <action-id> <files…>\n       \
inacircle offer <files…>\n       \
inacircle undo <job-id>\n       \
inacircle jobs\n       \
inacircle cancel <job-id>";

const EXIT_DONE: u8 = 0;
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_UNSUPPORTED: u8 = 3;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let code = match args.first().and_then(|a| a.to_str()) {
        Some("run") => cmd_run(rest),
        Some("offer") => cmd_offer(rest),
        Some("undo") => cmd_undo(rest),
        Some("jobs") => cmd_jobs(rest),
        Some("cancel") => cmd_cancel(rest),
        Some(other) => usage(&format!("unknown command `{other}`")),
        None if args.is_empty() => cmd_wheel(),
        None => usage("invalid command"),
    };
    ExitCode::from(code)
}

fn usage(msg: &str) -> u8 {
    eprintln!("inacircle: {msg}\n{USAGE}");
    EXIT_USAGE
}

/// One line to stdout. A closed stdout must not stop the run (the history
/// still records it), so write errors are ignored.
fn out(line: &Line) {
    let _ = io::stdout().write_all(write_line(line).as_bytes());
}

fn lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn print_outcome(o: &Outcome) {
    out(&outcome_line(o));
}

/// The stdout line of an outcome.
fn outcome_line(o: &Outcome) -> Line {
    match o {
        Outcome::Done { input, output, .. } => {
            Line::new("done").bare(&lossy(input)).bare(&lossy(output))
        }
        Outcome::Skipped { input, reason } => Line::new("skip").bare(&lossy(input)).bare(reason),
        Outcome::Failed { input, reason } => Line::new("fail").bare(&lossy(input)).bare(reason),
        Outcome::Warned { input, message } => Line::new("warn").bare(&lossy(input)).bare(message),
        // Published, but not where (or not as) it could be proved: no
        // invented path, the operation and its anchor instead.
        Outcome::PublishedUnknown {
            input,
            group,
            location,
            reason,
            durability,
        } => {
            let a = &location.anchor;
            let mut l = Line::new("unknown")
                .bare(&lossy(input))
                .bare(&notice::unknown_reason(*reason))
                .pair("group", &format!("{:016x}", group.0))
                .pair("op", &format!("{:016x}", a.op.0))
                .pair("parent", &format!("{}:{}", a.parent_dev, a.parent_ino))
                .pair("object", &format!("{}:{}", a.object_dev, a.object_ino))
                .pair("leaf", &a.leaf.to_string_lossy())
                .pair("durability", journal::durability_str(durability));
            if let Some(p) = &location.path_at_check {
                l = l.pair("path", &lossy(p));
            }
            l
        }
    }
}

/// Absolute against the cwd, `.` components and trailing slashes dropped.
/// `..` stays: resolving it lexically would be wrong across symlinks.
fn absolute(p: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd?.join(p)
    };
    Some(joined.components().collect())
}

/// The files of a command as absolute paths, each file (by canonical path)
/// once, first spelling kept; `None` after reporting an invocation error.
fn files_of(args: &[OsString]) -> Option<Vec<PathBuf>> {
    if args.is_empty() {
        usage("no files");
        return None;
    }
    if args.iter().any(|a| a.is_empty()) {
        usage("empty path");
        return None;
    }
    let cwd = std::env::current_dir().ok();
    let mut files = Vec::with_capacity(args.len());
    for a in args {
        match absolute(Path::new(a), cwd.as_deref()) {
            Some(p) => files.push(p),
            None => {
                usage("relative path without a current directory");
                return None;
            }
        }
    }
    Some(dedup(files))
}

/// Each file (by canonical path) once, first spelling kept. Only a hash
/// of each kept file's canonical path is held, not the path: a hash match
/// is confirmed by canonicalizing the kept file again.
fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let state = std::hash::RandomState::new();
    let mut seen = HashSet::with_capacity(paths.len());
    let mut hashes: Vec<u64> = Vec::with_capacity(paths.len());
    let mut kept: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for p in paths {
        let c = canon(&p);
        let h = std::hash::BuildHasher::hash_one(&state, &c);
        let dup = !seen.insert(h)
            && kept
                .iter()
                .zip(&hashes)
                .any(|(k, &kh)| kh == h && canon(k) == c);
        if !dup {
            hashes.push(h);
            kept.push(p);
        }
    }
    kept
}

#[derive(Clone, Copy)]
enum Target<'a> {
    Preset(&'a str),
    Action(&'a str),
}

/// Forwards outcomes to stdout, the history and the notifier, progress to
/// stdout, the job registry and the notifier.
struct Printer<'a> {
    running: &'a mut Running,
    rec: &'a mut Recorder,
    notifier: Option<&'a mut Notifier>,
    last: Option<String>,
    /// The measurement switch's action-entry marks; `None` without it, so
    /// the callback then costs one branch.
    entry: Option<Entry>,
}

/// When the supervisor first observed an action child running: CPU and
/// `CLOCK_MONOTONIC`, absolute, in µs. An upper bound on the child's own
/// entry, taken at the first valid event the engine read, not at its
/// `main`. Only the first child is kept; `count` counts all.
#[derive(Default)]
struct Entry {
    first: Option<(i64, i64)>,
    count: u32,
}

impl Observer for Printer<'_> {
    fn event(&mut self, o: &Outcome) {
        print_outcome(o);
        self.rec.outcome(o);
        if let Some(n) = &mut self.notifier {
            n.event(o);
        }
    }

    fn started(&mut self, _pid: u32) {
        // Clocks first, nothing else between the event and the reading.
        if let Some(e) = &mut self.entry {
            let (cpu, mono) = (cpu_us(libc::RUSAGE_SELF), monotonic_us().unwrap_or(-1));
            e.count += 1;
            if e.first.is_none() && cpu >= 0 && mono >= 0 {
                e.first = Some((cpu, mono));
            }
        }
    }

    fn dir(&mut self, path: &Path) {
        self.rec.dir(path);
    }

    fn progress(&mut self, done: usize, total: usize, fraction: f32) {
        self.running.update(done, fraction);
        let s = format!("{fraction:.2}");
        if self.last.as_deref() != Some(s.as_str()) {
            out(&Line::new("progress").bare(&s));
            self.last = Some(s);
        }
        if let Some(n) = &mut self.notifier {
            // Before a notification can hold it up.
            let _ = io::stdout().flush();
            n.progress(done, total, fraction);
        }
    }

    fn wake_fd(&self) -> Option<std::os::fd::RawFd> {
        self.notifier.as_ref().and_then(|n| n.wake_fd())
    }

    fn wake_at(&self) -> Option<std::time::Instant> {
        self.notifier.as_ref().and_then(|n| n.wake_at())
    }

    fn wake(&mut self) -> Wake {
        match &mut self.notifier {
            Some(n) => n.wake(),
            None => Wake::Continue,
        }
    }
}

fn cmd_run(args: &[OsString]) -> u8 {
    // First, before anything that could start a thread: a thread without
    // SIGTERM blocked would take a cancel as a kill.
    let cancel = match CancelToken::install() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("inacircle: cannot set up cancel handling: {e}");
            return EXIT_FAILED;
        }
    };
    let (notify, args) = match args {
        [flag, rest @ ..] if flag == "--notify" => (true, rest),
        _ => (false, args),
    };
    let (target, rest) = match args {
        [flag, id, rest @ ..] if flag == "--action" => (id, rest),
        [flag] if flag == "--action" => return usage("--action needs an action id"),
        [id, rest @ ..] => (id, rest),
        [] => return usage("run needs a preset id"),
    };
    let Some(id) = target.to_str() else {
        return usage("invalid id");
    };
    let target = if args[0] == "--action" {
        Target::Action(id)
    } else {
        Target::Preset(id)
    };
    let Some(files) = files_of(rest) else {
        return EXIT_USAGE;
    };
    run(target, files, notify, cancel)
}

/// Runs `target` on `files` (absolute, each once): the shared tail of `run`
/// and the wheel.
fn run(target: Target, files: Vec<PathBuf>, notify: bool, cancel: CancelToken) -> u8 {
    let preset = match target {
        Target::Preset(id) => match PRESETS.iter().find(|p| p.id == id) {
            Some(p) => Some(p),
            None => return usage(&format!("unknown preset `{id}`")),
        },
        Target::Action(_) => None,
    };
    // The measurement switch: our CPU at each phase boundary of the run.
    let measure_on = measuring();
    let mut lap = measure_on.then(|| vec![("start", cpu_us(libc::RUSAGE_SELF))]);
    let mut mark = |name: &'static str| {
        if let Some(l) = &mut lap {
            l.push((name, cpu_us(libc::RUSAGE_SELF)));
        }
    };
    let cat = Catalog::load();
    mark("catalog");
    // The input type, the route, and why a matching input cannot run.
    let (mime, route, blocked): (&str, Vec<&Action>, Option<String>) = match (target, preset) {
        (Target::Preset(_), Some(p)) => match menu::route(&cat, p.input, p.output) {
            Some(r) => (p.input, r, None),
            None => (p.input, Vec::new(), Some(String::new())),
        },
        (Target::Action(id), _) => {
            let Some(a) = cat.get(id) else {
                return usage(&format!("unknown action `{id}`"));
            };
            match missing_tools(a).first() {
                None => (&a.input, vec![a], None),
                Some(t) => (&a.input, Vec::new(), Some(format!("needs {t}"))),
            }
        }
        (Target::Preset(_), None) => unreachable!("preset resolved above"),
    };
    let name: String = route
        .iter()
        .map(|a| a.id.as_str())
        .collect::<Vec<_>>()
        .join("+");
    let name = match (name.is_empty(), target) {
        (false, _) => name,
        (true, Target::Preset(id) | Target::Action(id)) => id.to_owned(),
    };

    // Classify before routing.
    let mut pre = Vec::new();
    let mut inputs = Vec::new();
    for f in files {
        match classify(&f) {
            Ok(Classified::NotRegular) => pre.push(Outcome::Skipped {
                input: f,
                reason: "not a regular file".into(),
            }),
            Err(e) => pre.push(Outcome::Failed {
                input: f,
                reason: format!("cannot read: {e}"),
            }),
            Ok(Classified::File(k)) if k.mime.as_deref() == Some(mime) => match &blocked {
                None => inputs.push(f),
                Some(why) if !why.is_empty() => pre.push(Outcome::Failed {
                    input: f,
                    reason: why.clone(),
                }),
                Some(_) => pre.push(Outcome::Skipped {
                    input: f,
                    reason: "unsupported".into(),
                }),
            },
            Ok(Classified::File(_)) => pre.push(Outcome::Skipped {
                input: f,
                reason: "unsupported".into(),
            }),
        }
    }

    let mut notifier = notify.then(|| {
        let label = route.last().map_or(name.as_str(), |a| a.label.as_str());
        Notifier::new(label, route.iter().any(|a| a.lossy), inputs.len())
    });

    mark("classify");
    let job = journal::new_job_id();
    let mut running = Running::start(&job, &name, inputs.len());
    let mut rec = Recorder::open(&job, &name);
    out(&Line::new("job").bare(&job.0));
    let _ = io::stdout().flush();
    for o in pre {
        print_outcome(&o);
        rec.outcome(&o);
        if let Some(n) = &mut notifier {
            n.event(&o);
        }
    }
    let mut entry = None;
    let sandboxed = if inputs.is_empty() {
        engine::sandbox::ready().is_ok()
    } else {
        let mut obs = Printer {
            running: &mut running,
            rec: &mut rec,
            notifier: notifier.as_mut(),
            last: None,
            entry: measure_on.then(Entry::default),
        };
        let sandboxed = exec::run(&route, &inputs, &mut obs, &cancel).sandboxed;
        entry = obs.entry.take();
        sandboxed
    };
    mark("exec");
    // Read now: a cancel after this point stopped nothing.
    let cancelled = cancel.is_cancelled();
    let closed = rec.finish(sandboxed);
    drop(running);
    mark("record");

    if let Some(w) = &closed.warning {
        eprintln!("inacircle: warning: {w}");
    }
    let (done, skipped, failed) = closed.tally.counts();
    out(&Line::new("summary")
        .pair("done", &done.to_string())
        .pair("skipped", &skipped.to_string())
        .pair("failed", &failed.to_string()));
    let _ = io::stdout().flush();
    // Before the notifications: their cost is measured on its own.
    if measure_on {
        measure();
    }
    if let Some(l) = &lap {
        let phases: Vec<String> = l
            .windows(2)
            .map(|w| format!("{}_cpu_us={}", w[1].0, w[1].1 - w[0].1))
            .collect();
        // Absent when no child emitted a valid event, never 0.
        let entry = entry.unwrap_or_default();
        let marks = entry.first.map_or(String::new(), |(c, w)| {
            format!(" action_entry_observed_abs_cpu_us={c} action_entry_observed_abs_wall_us={w}")
        });
        eprintln!(
            "inacircle: measure run pid={} start_abs_cpu_us={} {} action_entries={}{marks}",
            std::process::id(),
            l[0].1,
            phases.join(" "),
            entry.count
        );
    }
    // The job is closed in the history and gone from the registry, so an
    // Undo click can undo it. The token stays: a signal in the Undo window
    // ends the window, not the process.
    if let Some(n) = notifier {
        n.finish(
            &job,
            sandboxed,
            (done, skipped, failed, closed.tally.unknown()),
            cancelled,
            closed.undoable,
            &cancel,
        );
    }
    // A cancel that arrived after the run must be consumed here: dropping
    // the token with it pending would kill the process. Inputs it stopped
    // are already failed, so the batch rule covers a cancelled run too.
    let _ = cancel.is_cancelled();
    drop(cancel);
    if done > 0 {
        EXIT_DONE
    } else if failed > 0 {
        EXIT_FAILED
    } else if skipped > 0 {
        EXIT_UNSUPPORTED
    } else {
        EXIT_FAILED
    }
}

/// Whether the measurement switch `INACIRCLE_MEASURE=1` is set.
fn measuring() -> bool {
    std::env::var_os("INACIRCLE_MEASURE").is_some_and(|v| v == "1")
}

/// User + system CPU time of `who` (`RUSAGE_SELF` or `RUSAGE_CHILDREN`) in
/// microseconds, -1 if unavailable.
fn cpu_us(who: libc::c_int) -> i64 {
    // SAFETY: rusage is plain integers, so all-zero is a valid value.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes one rusage into `ru`, which outlives it.
    if unsafe { libc::getrusage(who, &mut ru) } != 0 {
        return -1;
    }
    let us = |t: libc::timeval| t.tv_sec * 1_000_000 + t.tv_usec;
    us(ru.ru_utime) + us(ru.ru_stime)
}

/// `CLOCK_MONOTONIC` in microseconds: the wall clock of the measurement
/// log, shared by every process on the machine, so the wheel's choice and
/// the run's action entry compare directly.
fn monotonic_us() -> io::Result<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one timespec into `ts`, which outlives it.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000)
}

/// Measurement switch (`INACIRCLE_MEASURE=1`): our CPU time, the reaped
/// children's, and peak RSS, on stderr.
fn measure() {
    let hwm = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_default();
    eprintln!(
        "inacircle: measure self_cpu_us={} children_cpu_us={} vm_hwm={hwm}",
        cpu_us(libc::RUSAGE_SELF),
        cpu_us(libc::RUSAGE_CHILDREN)
    );
}

fn cmd_offer(args: &[OsString]) -> u8 {
    let (groups, args) = match args.split_first() {
        Some((a, rest)) if a == "--groups" => (true, rest),
        _ => (false, args),
    };
    let Some(files) = files_of(args) else {
        return EXIT_USAGE;
    };
    let named = named_kinds(&files);
    let cat = Catalog::load();
    if groups {
        let mut tally = menu::Tally::default();
        for (name, kind) in &named {
            tally.add(name, kind);
        }
        for (dir, g) in menu::groups(&cat, &tally).iter().enumerate() {
            let Some(g) = g else { continue };
            out(&Line::new("group")
                .pair("dir", &dir.to_string())
                .pair("id", g.group)
                .pair("label", g.label)
                .pair("lit", if g.lit { "yes" } else { "no" }));
        }
        out(&Line::new("hub").bare(&tally.source_text()));
        return EXIT_DONE;
    }
    let offer = menu::offer(&cat, &named);
    for (dir, s) in offer.slices.iter().enumerate() {
        let Some(s) = s else { continue };
        let mut l = Line::new("slice")
            .pair("dir", &dir.to_string())
            .pair("preset", s.preset)
            .pair("label", &s.label)
            .pair("applicable", &s.applicable.to_string());
        if let Some(why) = &s.disabled {
            l = l.pair("disabled", why);
        }
        out(&l);
    }
    out(&Line::new("hub").bare(&offer.hub));
    EXIT_DONE
}

/// Each file's name and what it is, as the offer needs them.
fn named_kinds(files: &[PathBuf]) -> Vec<(String, Kind)> {
    files.iter().map(|p| named_kind(p)).collect()
}

/// A file's name and what it is.
fn named_kind(p: &Path) -> (String, Kind) {
    let name = p
        .file_name()
        .map_or_else(|| lossy(p), |n| lossy(Path::new(n)));
    let unknown = |label: &str| Kind {
        mime: None,
        label: label.into(),
    };
    let kind = match classify(p) {
        Ok(Classified::File(k)) => k,
        Ok(Classified::NotRegular) if p.is_dir() => unknown("folder"),
        Ok(Classified::NotRegular) if p.is_symlink() && !p.exists() => unknown("broken link"),
        Ok(Classified::NotRegular) => unknown("special file"),
        Err(_) => unknown("unreadable file"),
    };
    (name, kind)
}

fn one_job_id(args: &[OsString], cmd: &str) -> Result<JobId, u8> {
    match args {
        [id] => match id.to_str() {
            Some(s) => Ok(JobId(s.to_owned())),
            None => Err(usage("invalid job id")),
        },
        _ => Err(usage(&format!("{cmd} needs exactly one job id"))),
    }
}

fn cmd_undo(args: &[OsString]) -> u8 {
    let job = match one_job_id(args, "undo") {
        Ok(j) => j,
        Err(c) => return c,
    };
    match journal::undo(&job) {
        Ok(r) => {
            for p in &r.removed {
                out(&Line::new("removed").bare(&lossy(p)));
            }
            for (p, why) in &r.kept {
                out(&Line::new("kept").bare(&lossy(p)).bare(why));
            }
            EXIT_DONE
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("inacircle: unknown job {}", job.0);
            EXIT_USAGE
        }
        Err(e) if e.kind() == io::ErrorKind::ResourceBusy => {
            eprintln!("inacircle: job {} is running; cancel it first", job.0);
            EXIT_FAILED
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            eprintln!("inacircle: job {} already undone", job.0);
            EXIT_FAILED
        }
        Err(e) => {
            eprintln!("inacircle: undo {}: {e}", job.0);
            EXIT_FAILED
        }
    }
}

fn cmd_jobs(args: &[OsString]) -> u8 {
    if !args.is_empty() {
        return usage("jobs takes no arguments");
    }
    for (id, action, pid, progress) in journal::list_running() {
        out(&Line::new("job")
            .pair("id", &id.0)
            .pair("action", &action)
            .pair("pid", &pid.to_string())
            .pair("progress", &format!("{progress:.3}")));
    }
    EXIT_DONE
}

fn cmd_cancel(args: &[OsString]) -> u8 {
    let job = match one_job_id(args, "cancel") {
        Ok(j) => j,
        Err(c) => return c,
    };
    let Some((_, _, pid, _)) = journal::list_running()
        .into_iter()
        .find(|(id, ..)| *id == job)
    else {
        eprintln!("inacircle: no running job {}", job.0);
        return EXIT_USAGE;
    };
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        eprintln!("inacircle: invalid pid {pid} for job {}", job.0);
        return EXIT_FAILED;
    };
    // SAFETY: kill takes plain integers. The pid comes from a registry entry
    // that list_running just checked against the process start time, so it
    // names the job's supervisor (a pid reused since is filtered out there).
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        eprintln!(
            "inacircle: cancel {}: {}",
            job.0,
            io::Error::last_os_error()
        );
        return EXIT_FAILED;
    }
    EXIT_DONE
}

/// The wheel: lock, deadline, surface; then the chosen preset, if any.
fn cmd_wheel() -> u8 {
    let mut meter = measuring().then(|| {
        measure::Meter::new(
            cpu_us(libc::RUSAGE_SELF),
            monotonic_us().unwrap_or(-1),
            Instant::now(),
        )
    });
    // The deadline first: it covers the lock too (a slow runtime dir).
    if let Err(e) = default_sigalrm() {
        eprintln!("inacircle: cannot arm the deadline: {e}");
        return EXIT_FAILED;
    }
    // SAFETY: alarm(2) has no memory effects. SIGALRM's default action
    // kills the process: the backstop if anything below hangs.
    unsafe { libc::alarm(wheel::STAGE.as_secs() as u32) };
    // The lock comes before Wayland: a second instance must not draw.
    let lock = match lock_wheel() {
        Ok(Some(l)) => l,
        Ok(None) => return EXIT_DONE,
        Err(e) => {
            eprintln!("inacircle: wheel lock: {e}");
            return EXIT_FAILED;
        }
    };
    let mut machine = wheel::Machine::new();
    let chosen = surface::run(&mut machine, &mut meter);
    // The surface is gone (its connection closed); free the hotkey.
    drop(lock);
    if let Some(meter) = meter {
        let exit_cpu = cpu_us(libc::RUSAGE_SELF);
        eprintln!(
            "inacircle: {} exit_abs_cpu_us={exit_cpu}",
            meter.report(measure::vm_hwm_kb(), exit_cpu)
        );
    }
    // SAFETY: as above; 0 cancels the pending alarm.
    unsafe { libc::alarm(0) };
    // Stage 2 read the catalog from the cache only; now that the overlay
    // is gone, bring the cache up to date (manifests run with their usual
    // deadline and group kill). A choice's run loads the catalog itself,
    // which is that refresh.
    match chosen {
        Ok(Some((files, preset))) => run_choice(files, &preset),
        Ok(None) => {
            Catalog::load();
            EXIT_DONE
        }
        Err(e) => {
            eprintln!("inacircle: wheel: {e}");
            Catalog::load();
            EXIT_FAILED
        }
    }
}

/// Runs the wheel's choice exactly as `inacircle run --notify <preset>
/// <files…>`: the same function, so the same checks, output and exit code.
fn run_choice(files: Vec<PathBuf>, preset: &str) -> u8 {
    cmd_run(&choice_args(files, preset))
}

/// The `run` arguments for a choice. The paths are absolute (from
/// `file://` URIs), so none reads as a flag.
fn choice_args(files: Vec<PathBuf>, preset: &str) -> Vec<OsString> {
    let mut args = Vec::with_capacity(files.len() + 2);
    args.extend([OsString::from("--notify"), OsString::from(preset)]);
    args.extend(files.into_iter().map(PathBuf::into_os_string));
    args
}

/// SIGALRM back to its default action (terminate) and unblocked: an
/// ignored disposition and a blocked mask both survive `execve`, so
/// whoever started us could have disabled the alarm.
fn default_sigalrm() -> io::Result<()> {
    // SAFETY: a zeroed sigaction is valid storage; sa_sigaction = SIG_DFL
    // with an empty mask installs no handler code; sigaction reads `act`,
    // which lives on this frame.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut act.sa_mask);
        if libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: a zeroed sigset_t is valid storage, initialized by
    // sigemptyset/sigaddset; pthread_sigmask reads it from this frame. One
    // thread: this is the process mask.
    let r = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGALRM);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut())
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r));
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR/inacircle/wheel.lock`, held: `None` if another
/// instance holds it. Opened without following a symlink and without
/// blocking (a FIFO planted there would hang `open`), and refused unless it
/// is a regular file.
fn lock_wheel() -> io::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = engine::xdg::runtime_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?
        .join("inacircle");
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let path = dir.join("wheel.lock");
    // std adds O_CLOEXEC; create(true) is O_CREAT.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    // fstat of what we opened, not of the path.
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: not a regular file", path.display()),
        ));
    }
    // SAFETY: flock on an fd we own; the lock lives as long as `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(e)
    }
}

/// Level 1 for a `text/uri-list` (early or dropped). The list's bytes are
/// freed by the caller after this; the catalog comes from the cache alone,
/// loaded once here and kept in the menu for level 2: no action binary
/// runs while the overlay is up (the cache is refreshed once the wheel is
/// gone).
fn group_menu(list: &[u8]) -> wheel::GroupMenu {
    group_menu_for(dedup(uri::parse_uri_list(list)), Catalog::load_cached())
}

/// Level 1 for `paths` (absolute, each once) with catalog `cat` (`None`:
/// no cache yet, so `No actions yet`). The files are tallied by type as
/// they are classified, keeping one name per type, not one per file. The
/// hub is the source: the file's name (`1 file · text` when the atlas cannot
/// draw it), or the count.
fn group_menu_for(paths: Vec<PathBuf>, cat: Option<Catalog>) -> wheel::GroupMenu {
    let mut gm = wheel::GroupMenu {
        paths: Vec::new(),
        slices: Default::default(),
        hub: String::new(),
        tally: menu::Tally::default(),
        catalog: None,
    };
    let cat = match cat {
        Some(cat) if !paths.is_empty() => cat,
        // Nothing is offered, so nothing can be chosen: any click closes.
        _ => {
            gm.hub = if paths.is_empty() {
                wheel::NOTHING
            } else {
                wheel::NO_ACTIONS
            }
            .into();
            return gm;
        }
    };
    for p in &paths {
        let (name, kind) = named_kind(p);
        gm.tally.add(&name, &kind);
    }
    for (dir, g) in menu::groups(&cat, &gm.tally).into_iter().enumerate() {
        gm.slices[dir] = g.map(|g| wheel::GroupChoice {
            label: g.label.into(),
            group: g.group.into(),
            lit: g.lit,
        });
    }
    // A name the hub cannot show is replaced by the type's top-level name.
    gm.hub = match (&gm.tally.first_name, gm.tally.types.first()) {
        (Some(name), Some((mime, ..))) if gm.tally.total == 1 => {
            let label = mime.split('/').next().unwrap_or(mime);
            ui::text::hub_text(1, name, 1, label, ui::wheel::HUB_TEXT_W)
        }
        _ => gm.tally.source_text(),
    };
    gm.paths = paths;
    gm.catalog = Some(cat);
    gm
}

/// Level 2: the category's presets in their own directions (a preset's
/// compass direction is its slice, 0 = north); the hub is the category.
/// From the catalog level 1 was built with: the drop does no catalog I/O,
/// and both levels see the same presets.
fn target_menu(gm: &wheel::GroupMenu, group: &str) -> wheel::Menu {
    let mut slices: [Option<wheel::Choice>; 8] = Default::default();
    let Some(cat) = &gm.catalog else {
        return wheel::Menu {
            paths: Vec::new(),
            slices,
            hub: wheel::NO_ACTIONS.into(),
        };
    };
    let offer = menu::offer_group(cat, &gm.tally, group);
    for (dir, s) in offer.slices.iter().enumerate() {
        let Some(s) = s else { continue };
        slices[dir] = Some(wheel::Choice {
            label: s.label.clone(),
            preset: s.preset.into(),
            applicable: s.applicable > 0 && s.disabled.is_none(),
        });
    }
    wheel::Menu {
        paths: gm.paths.clone(),
        slices,
        hub: offer.hub,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A publication of unknown place is its own line: the input, the
    /// cause, the operation and anchor, never a `done` with a path.
    #[test]
    fn published_unknown_line_names_the_operation_not_a_path() {
        use engine::exec::{
            AnchorHint, CheckPoint, Durability, GroupId, LocationEvidence, OperationId,
            PublicationReason,
        };
        let o = Outcome::PublishedUnknown {
            input: "/in/a.txt".into(),
            group: GroupId(1),
            location: LocationEvidence {
                path_at_check: None,
                anchor: AnchorHint {
                    parent_dev: 8,
                    parent_ino: 9,
                    object_dev: 8,
                    object_ino: 10,
                    leaf: "a".into(),
                    op: OperationId(2),
                },
                checked_at: CheckPoint::PostPublish,
            },
            reason: PublicationReason::AncestorChanged,
            durability: Durability::Synced,
        };
        let l = outcome_line(&o);
        assert_eq!(l.kind, "unknown");
        assert_eq!(l.bare_at(0), Some("/in/a.txt"));
        assert!(l.bare_at(1).unwrap().contains("moved or replaced"));
        assert_eq!(l.get("op"), Some("0000000000000002"));
        assert_eq!(l.get("group"), Some("0000000000000001"));
        assert_eq!(l.get("parent"), Some("8:9"));
        assert_eq!(l.get("object"), Some("8:10"));
        assert_eq!(l.get("leaf"), Some("a"));
        assert_eq!(l.get("durability"), Some("synced"));
        assert_eq!(l.get("path"), None);
    }

    /// The wheel's choice runs as `inacircle run --notify <preset> <files…>`:
    /// through `cmd_run` itself, whose contract the CLI tests hold.
    #[test]
    fn run_path_is_shared_with_the_cli() {
        let files = vec![PathBuf::from("/a b"), PathBuf::from("/c")];
        assert_eq!(
            choice_args(files, "markdown"),
            ["--notify", "markdown", "/a b", "/c"].map(OsString::from)
        );
        // The CLI's own checks apply: an unknown preset is its usage error,
        // before anything is run or recorded.
        assert_eq!(
            run_choice(vec![PathBuf::from("/a")], "no-such-preset"),
            EXIT_USAGE
        );
    }

    /// Each file once by canonical path, first spelling kept: another
    /// spelling or a symlink of a kept file is dropped; a missing file is
    /// matched by its path as given.
    #[test]
    fn dedup_keeps_each_file_once() {
        let (dir, a) = text_file("dedup", "a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&b, "b\n").unwrap();
        std::os::unix::fs::symlink(&a, dir.join("link")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let missing = dir.join("gone.txt");
        let kept = dedup(vec![
            dir.join("./a.txt"),
            b.clone(),
            dir.join("link"),
            a.clone(),
            missing.clone(),
            dir.join("sub/../b.txt"),
            missing.clone(),
        ]);
        assert_eq!(kept, [dir.join("./a.txt"), b, missing]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A catalog with one plain-text-to-Markdown action.
    fn md_catalog() -> Catalog {
        Catalog {
            actions: vec![engine::catalog::Action {
                id: "t.md".into(),
                family: "t".into(),
                bin: "/bin/true".into(),
                input: "text/plain".into(),
                output: "text/markdown".into(),
                ext: "md".into(),
                label: "Markdown".into(),
                group: "g".into(),
                cost: 1,
                lossy: false,
                batch: true,
                requires: Vec::new(),
                builtin: true,
            }],
        }
    }

    /// Level 1 for a URI list, with `md_catalog`.
    fn groups_of(list: &str) -> wheel::GroupMenu {
        group_menu_for(
            dedup(uri::parse_uri_list(list.as_bytes())),
            Some(md_catalog()),
        )
    }

    /// A private dir with one plain-text file named `name`.
    fn text_file(tag: &str, name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("inacircle-drop-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, "plain words\n").unwrap();
        (dir, file)
    }

    /// One file whose name the atlas cannot draw: its count and type.
    #[test]
    fn group_menu_hub_falls_back_to_the_type() {
        let (dir, file) = text_file("cjk", "日本.txt");
        let g = groups_of(&format!(
            "file://{}/%E6%97%A5%E6%9C%AC.txt\n",
            dir.display()
        ));
        assert_eq!(g.paths, [file]);
        assert_eq!(g.hub, "1 file · text");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Level 1 from a text drop lights the category the files fit, once per
    /// file even when listed twice; the hub names the one file.
    #[test]
    fn groups_menu_from_a_text_drop() {
        let (dir, file) = text_file("map", "notes ő.txt");
        let g = groups_of(&format!(
            "# dropped\r\nfile://{}\r\nfile://{}\r\n",
            dir.join("notes%20%C5%91.txt").display(),
            file.display()
                .to_string()
                .replace(' ', "%20")
                .replace('ő', "%C5%91")
        ));
        assert_eq!(g.paths, std::slice::from_ref(&file), "the same file once");
        assert_eq!(g.hub, "notes ő.txt");
        let doc = g.slices[0].as_ref().expect("documents point north");
        assert_eq!(
            (doc.label.as_str(), doc.group.as_str(), doc.lit),
            ("Document", "document", true)
        );
        assert_eq!(g.slices.iter().flatten().count(), 1);

        let none = groups_of("https://example.com/\r\n");
        assert!(none.paths.is_empty() && none.slices.iter().all(Option::is_none));
        assert_eq!(none.hub, wheel::NOTHING);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Level 2 maps a preset's compass direction to its wheel slice (0 =
    /// north); the hub is the category.
    #[test]
    fn targets_menu_for_document() {
        let (dir, file) = text_file("targets", "a.txt");
        let g = group_menu_for(vec![file.clone()], Some(md_catalog()));
        let m = target_menu(&g, "document");
        assert_eq!(m.paths, std::slice::from_ref(&file));
        assert_eq!(m.hub, "Document");
        let north = m.slices[0].as_ref().expect("markdown points north");
        assert_eq!(
            (
                north.label.as_str(),
                north.preset.as_str(),
                north.applicable
            ),
            ("Markdown", "markdown", true)
        );
        let plain = m.slices[4].as_ref().expect("plain-text points south");
        assert!(!plain.applicable, "no action installed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Level 2 offers what level 1 was built with: the catalog is loaded
    /// once, and the drop does no catalog I/O (the signature has no way to
    /// reach the cache; a snapshot-less menu offers nothing).
    #[test]
    fn target_menu_uses_the_catalog_of_level_1() {
        let (dir, file) = text_file("snap", "a.txt");
        let g = group_menu_for(vec![file], Some(md_catalog()));
        assert_eq!(g.catalog, Some(md_catalog()));
        let m = target_menu(&g, "document");
        assert!(m.slices[0].as_ref().is_some_and(|c| c.applicable));
        let mut none = g.clone();
        none.catalog = None;
        assert_eq!(target_menu(&none, "document").hub, wheel::NO_ACTIONS);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Before the catalog was ever cached, nothing is offered (no manifest
    /// runs under the overlay) and the wheel says so; any click closes.
    #[test]
    fn group_menu_without_a_cache_offers_nothing() {
        let (dir, file) = text_file("nocache", "a.txt");
        let g = group_menu_for(vec![file], None);
        assert!(g.paths.is_empty() && g.slices.iter().all(Option::is_none));
        assert_eq!(g.hub, wheel::NO_ACTIONS);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Several files: the hub counts them; the category is lit when any of
    /// them fits, and a category nothing fits is dim.
    #[test]
    fn group_menu_counts_the_files() {
        let (dir, a) = text_file("count", "a.txt");
        let pdf = dir.join("c.pdf");
        std::fs::write(&pdf, "%PDF-1.4\n").unwrap();
        let g = group_menu_for(vec![a, pdf.clone()], Some(md_catalog()));
        assert_eq!(g.paths.len(), 2);
        assert_eq!(g.hub, "2 files");
        assert!(g.slices[0].as_ref().is_some_and(|d| d.lit));
        let g = group_menu_for(vec![pdf], Some(md_catalog()));
        assert!(g.slices[0].as_ref().is_some_and(|d| !d.lit), "dim");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
