//! `inacircle`: the engine's command line (headless mode).
//!
//! ```text
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

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use engine::catalog::{Action, Catalog, missing_tools};
use engine::classify::{Classified, Kind, classify};
use engine::exec::{self, CancelToken, Observer, Outcome, Wake};
use engine::journal::{self, JobId, Recorder, Running};
use engine::menu::{self, PRESETS};
use manifest::{Line, write_line};
use notice::Notifier;

mod notice;

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
        None => usage("no command"),
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
    let line = match o {
        Outcome::Done { input, output, .. } => {
            Line::new("done").bare(&lossy(input)).bare(&lossy(output))
        }
        Outcome::Skipped { input, reason } => Line::new("skip").bare(&lossy(input)).bare(reason),
        Outcome::Failed { input, reason } => Line::new("fail").bare(&lossy(input)).bare(reason),
        Outcome::Warned { input, message } => Line::new("warn").bare(&lossy(input)).bare(message),
    };
    out(&line);
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
    let mut seen = HashSet::with_capacity(args.len());
    for a in args {
        match absolute(Path::new(a), cwd.as_deref()) {
            Some(p) => {
                let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                if seen.insert(key) {
                    files.push(p);
                }
            }
            None => {
                usage("relative path without a current directory");
                return None;
            }
        }
    }
    Some(files)
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
}

impl Observer for Printer<'_> {
    fn event(&mut self, o: &Outcome) {
        print_outcome(o);
        self.rec.outcome(o);
        if let Some(n) = &mut self.notifier {
            n.event(o);
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
    let preset = match target {
        Target::Preset(id) => match PRESETS.iter().find(|p| p.id == id) {
            Some(p) => Some(p),
            None => return usage(&format!("unknown preset `{id}`")),
        },
        Target::Action(_) => None,
    };
    let cat = Catalog::load();
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
    let sandboxed = if inputs.is_empty() {
        engine::sandbox::landlock_abi().is_some()
    } else {
        let mut obs = Printer {
            running: &mut running,
            rec: &mut rec,
            notifier: notifier.as_mut(),
            last: None,
        };
        exec::run(&route, &inputs, &mut obs, &cancel).sandboxed
    };
    let tally = rec.finish(sandboxed);
    drop(running);

    let (done, skipped, failed) = tally.counts();
    out(&Line::new("summary")
        .pair("done", &done.to_string())
        .pair("skipped", &skipped.to_string())
        .pair("failed", &failed.to_string()));
    let _ = io::stdout().flush();
    // A cancel that arrived after the run must be consumed here: dropping
    // the token with it pending would kill the process. Inputs it stopped
    // are already failed, so the batch rule covers a cancelled run too.
    let _ = cancel.is_cancelled();
    drop(cancel);
    // Before the notifications: their cost is measured on its own.
    if std::env::var_os("INACIRCLE_MEASURE").is_some_and(|v| v == "1") {
        measure();
    }
    // The job is closed in the history and gone from the registry, so an
    // Undo click can undo it.
    if let Some(n) = notifier {
        n.finish(&job, sandboxed, (done, skipped, failed));
    }
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

/// Measurement switch (`INACIRCLE_MEASURE=1`): our CPU time, the reaped
/// children's, and peak RSS, on stderr.
fn measure() {
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
    let Some(files) = files_of(args) else {
        return EXIT_USAGE;
    };
    let named: Vec<(String, Kind)> = files
        .iter()
        .map(|p| {
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
                Ok(Classified::NotRegular) if p.is_symlink() && !p.exists() => {
                    unknown("broken link")
                }
                Ok(Classified::NotRegular) => unknown("special file"),
                Err(_) => unknown("unreadable file"),
            };
            (name, kind)
        })
        .collect();
    let cat = Catalog::load();
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
