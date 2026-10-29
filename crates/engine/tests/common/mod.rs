//! Shared harness for the supervisor tests: shell-script fake actions run
//! through `exec::run` (argv: `run <id> --work $4 --inputs-from $6 --`).

#![allow(dead_code)]

use engine::catalog::Action;
use engine::exec::{self, CancelToken, Observer, Outcome};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new(tag: &str) -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "inacircle-sup-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    /// An input file in `<tmp>/docs`.
    pub fn input(&self, name: &str, body: &str) -> PathBuf {
        let d = self.0.join("docs");
        fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    /// A `#!/bin/sh` fake action in `<tmp>/bin`. This process never holds
    /// a writable fd on it: other tests fork at any time, and a child that
    /// inherited one until its own exec would make the action's exec fail
    /// with ETXTBSY. `cp` (its own process) writes a temporary name, which
    /// is then renamed into place.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        self.script_in("/bin/sh", name, body)
    }

    /// [`Tmp::script`] run by `shell`. Background jobs (`&`) need bash in
    /// the sandbox: dash opens `/dev/null` as their stdin and, as Landlock
    /// does not grant it, never starts them; bash goes on without it.
    pub fn script_in(&self, shell: &str, name: &str, body: &str) -> PathBuf {
        let d = self.0.join("bin");
        fs::create_dir_all(&d).unwrap();
        let src = self.0.join(format!("{name}.src"));
        fs::write(&src, format!("#!{shell}\n{body}\n")).unwrap();
        let tmp = d.join(format!(".{name}.tmp"));
        let ok = std::process::Command::new("cp")
            .arg(&src)
            .arg(&tmp)
            .status()
            .unwrap();
        assert!(ok.success());
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        let p = d.join(name);
        fs::rename(&tmp, &p).unwrap();
        p
    }

    /// Names in `<tmp>/docs` that look like our work dirs.
    pub fn work_dirs_left(&self) -> Vec<String> {
        fs::read_dir(self.0.join("docs"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".inacircle-"))
            .collect()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A built-in action (runs with or without Landlock; seccomp always applies).
pub fn action(bin: &Path, ext: &str) -> Action {
    Action {
        id: "test.fake".into(),
        family: "test".into(),
        bin: bin.into(),
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

/// What the observer saw of a run.
#[derive(Default)]
pub struct Ran {
    pub outcomes: Vec<Outcome>,
    pub dirs: Vec<PathBuf>,
    pub progress: usize,
}

impl Observer for Ran {
    fn event(&mut self, o: &Outcome) {
        self.outcomes.push(o.clone());
    }
    fn dir(&mut self, path: &Path) {
        self.dirs.push(path.to_owned());
    }
    fn progress(&mut self, _d: usize, _t: usize, _f: f32) {
        self.progress += 1;
    }
}

/// Runs `route` on `inputs`; with `cancel_after`, a SIGTERM is sent to this
/// thread after that long. Returns the report and the time from the cancel
/// (or the start) to the return.
pub fn run(
    route: &[&Action],
    inputs: &[PathBuf],
    cancel_after: Option<Duration>,
) -> (Ran, Duration) {
    let tok = CancelToken::install().unwrap();
    // SAFETY: pthread_self has no preconditions.
    let me = unsafe { libc::pthread_self() } as usize;
    let start = Instant::now();
    let sent = cancel_after.map(|d| {
        std::thread::spawn(move || {
            std::thread::sleep(d);
            // SAFETY: `me` is the test thread, alive until `run` returns
            // (which needs this signal); SIGTERM is blocked there.
            unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGTERM) };
            Instant::now()
        })
    });
    let mut obs = Ran::default();
    exec::run(route, inputs, &mut obs, &tok);
    let end = Instant::now();
    let from = sent.map_or(start, |h| h.join().unwrap());
    (obs, end.saturating_duration_since(from))
}

/// Runs `route` on `inputs` with the observer `obs`. Returns the time the
/// run took.
pub fn run_with(route: &[&Action], inputs: &[PathBuf], obs: &mut dyn Observer) -> Duration {
    let tok = CancelToken::install().unwrap();
    let start = Instant::now();
    exec::run(route, inputs, obs, &tok);
    start.elapsed()
}

pub fn failed(r: &Ran) -> Vec<(PathBuf, String)> {
    r.outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Failed { input, reason } => Some((input.clone(), reason.clone())),
            _ => None,
        })
        .collect()
}

/// Whether any process has `token` in its command line.
pub fn process_with(token: &str) -> bool {
    let Ok(d) = fs::read_dir("/proc") else {
        return false;
    };
    d.flatten().any(|e| {
        fs::read(e.path().join("cmdline"))
            .is_ok_and(|c| String::from_utf8_lossy(&c).contains(token))
    })
}

/// Peak RSS of this process in KiB.
pub fn vm_hwm_kb() -> u64 {
    let s = fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

/// The live processes whose command line carries `token` (unique to one
/// test), each with a pidfd. The pidfd is opened first and the command
/// line read again after it: if the pid had been reused in between, the
/// new process would not carry the token, so every pidfd returned names a
/// fixture process (start identity), never an unrelated one.
pub fn pidfds_with(token: &str) -> Vec<(u32, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let has = |pid: u32| {
        fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| String::from_utf8_lossy(&c).contains(token))
    };
    let Ok(d) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for e in d.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if !has(pid) {
            continue;
        }
        // SAFETY: pidfd_open takes a pid and flags 0.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            continue; // gone meanwhile
        }
        // SAFETY: a non-negative result is a new fd that we now own.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
        if has(pid) && alive(&fd) {
            found.push((pid, fd));
        }
    }
    found
}

/// Whether the process behind `pidfd` has not exited (a pidfd polls
/// readable once it has).
pub fn alive(pidfd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut p = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: p is one valid pollfd; timeout 0 does not block.
    let r = unsafe { libc::poll(&mut p, 1, 0) };
    assert!(r >= 0, "poll on a pidfd failed");
    p.revents == 0
}
