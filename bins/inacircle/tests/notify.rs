//! `run --notify` end to end: the real `inacircle` binary and action
//! binaries against a scripted notification server on a private socket
//! (`DBUS_SESSION_BUS_ADDRESS=unix:path=…`). No real bus is needed.

#[path = "../../../crates/dbus/tests/common/mod.rs"]
mod fake;

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use dbus::{Address, Message, Value};
use fake::{Fake, Peer};

const BIN: &str = env!("CARGO_BIN_EXE_inacircle");
const ACTIONS: [&str; 2] = ["inacircle-text", "inacircle-testaction"];
const IFACE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const OWNER: &str = ":1.7";
const CAPS: [&str; 3] = ["actions", "body", "body-markup"];
/// The first id the server hands out.
const FIRST_ID: u32 = 7;

/// The action binaries live next to ours; `cargo test -p inacircle` alone
/// does not build them.
fn action_bin(name: &str) -> PathBuf {
    static BUILT: Once = Once::new();
    let dir = Path::new(BIN).parent().unwrap();
    BUILT.call_once(|| {
        let mut c = Command::new(env!("CARGO"));
        c.args(["build", "-q"]);
        for a in ACTIONS {
            c.args(["-p", a]);
        }
        if dir.file_name() == Some(OsStr::new("release")) {
            c.arg("--release");
        }
        c.arg("--target-dir").arg(dir.parent().unwrap());
        assert!(c.status().unwrap().success(), "building the actions failed");
    });
    dir.join(name)
}

/// A private root: actions, XDG dirs, HOME, and `files/` as the cwd.
struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root =
            std::env::temp_dir().join(format!("inacircle-notify-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "actions", "cache", "state", "runtime", "home", "config", "files",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        for a in ACTIONS {
            symlink(action_bin(a), root.join("actions").join(a)).unwrap();
        }
        Env { root }
    }

    fn files(&self) -> PathBuf {
        self.root.join("files")
    }

    fn file(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.files().join(rel);
        fs::write(&p, content).unwrap();
        p
    }

    /// A testaction input carrying its options.
    fn test_input(&self, rel: &str, opts: &[&str]) -> PathBuf {
        self.file(rel, &format!("INACIRCLE-TEST\n{}\n", opts.join("\n")))
    }

    /// `inacircle` with the bus at `bus` (none: no bus variable at all).
    fn cmd<S: AsRef<OsStr>>(&self, bus: Option<&str>, args: &[S]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("INACIRCLE_ACTIONS_DIR", self.root.join("actions"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("home/.local/share"))
            .env("HOME", self.root.join("home"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("INACIRCLE_UNDO_WAIT_MS")
            .current_dir(self.files())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(b) = bus {
            c.env("DBUS_SESSION_BUS_ADDRESS", b);
        }
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn bus_of(f: &Fake) -> String {
    match f.address() {
        Address::Path(p) => format!("unix:path={}", p.display()),
        Address::Abstract(_) => unreachable!("file sockets only"),
    }
}

/// A finished run. `stdout` is shared with the server scenario while the
/// run goes on.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
    /// The child's own CPU time (user + system).
    cpu: Duration,
}

impl Run {
    /// The rest of every stdout line of `kind` (fields tab-separated).
    fn lines(&self, kind: &str) -> Vec<String> {
        self.stdout
            .lines()
            .filter_map(|l| l.strip_prefix(kind)?.strip_prefix('\t').map(str::to_owned))
            .collect()
    }

    fn unavailable(&self) -> usize {
        self.stderr
            .lines()
            .filter(|l| l.starts_with("inacircle: notifications unavailable: "))
            .count()
    }
}

/// How long a run may take before the test kills it (a hung run must fail
/// the test, not hang the suite).
const WATCHDOG: Duration = Duration::from_secs(30);

/// Sends `sig` to `pid` with kill(1).
fn kill(pid: u32, sig: &str) {
    let _ = Command::new("kill")
        .args([sig, &pid.to_string()])
        .stderr(Stdio::null())
        .status();
}

/// Runs `c` to the end, its stdout copied into `seen` as it arrives.
fn run(c: Command, seen: &Arc<Mutex<String>>) -> Run {
    run_then(c, seen, |_| {})
}

/// `run` that calls `meanwhile` with the child's pid once it started.
fn run_then(mut c: Command, seen: &Arc<Mutex<String>>, meanwhile: impl FnOnce(u32)) -> Run {
    let start = Instant::now();
    let mut child = c.spawn().unwrap();
    let pid = child.id();
    let (ended, ended_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if ended_rx.recv_timeout(WATCHDOG).is_err() {
            kill(pid, "-KILL");
        }
    });
    meanwhile(pid);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let seen2 = seen.clone();
    let reader = std::thread::spawn(move || {
        let mut l = String::new();
        while out.read_line(&mut l).unwrap() > 0 {
            seen2.lock().unwrap().push_str(&l);
            l.clear();
        }
    });
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let cpu = cpu_at_exit(pid);
    let st = child.wait().unwrap();
    let elapsed = start.elapsed();
    let _ = ended.send(());
    watchdog.join().unwrap();
    reader.join().unwrap();
    let stdout = seen.lock().unwrap().clone();
    Run {
        code: st.code(),
        stdout,
        stderr,
        elapsed,
        cpu,
    }
}

/// Waits until `pid` (our child) has exited, without reaping it, and
/// returns its CPU time (user + system, its reaped children included).
fn cpu_at_exit(pid: u32) -> Duration {
    // SAFETY: an all-zero siginfo_t is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: pid is our unreaped child, info is writable; WNOWAIT leaves
    // it to `Child::wait`.
    let r = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
    assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // Fields after the command's `)`: utime, stime, cutime and cstime are
    // the 12th to 15th.
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let ticks: u64 = rest
        .split(' ')
        .skip(11)
        .take(4)
        .map(|f| f.parse::<u64>().unwrap())
        .sum();
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    Duration::from_millis(ticks * 1000 / hz)
}

fn plain_run(c: Command) -> Run {
    run(c, &Arc::new(Mutex::new(String::new())))
}

/// One `Notify` the server got, and when.
struct Got {
    at: Instant,
    id: u32,
    replaces: u32,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: Value,
}

fn strings(v: &[&str]) -> Value {
    Value::Array(
        "s".into(),
        v.iter().map(|s| Value::Str((*s).into())).collect(),
    )
}

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        v => panic!("not a string: {v:?}"),
    }
}

fn got(m: &Message, id: u32) -> Got {
    let Value::U32(replaces) = m.body[1] else {
        panic!("{:?}", m.body)
    };
    let Value::Array(_, actions) = &m.body[5] else {
        panic!("{:?}", m.body)
    };
    Got {
        at: Instant::now(),
        id,
        replaces,
        summary: text(&m.body[3]),
        body: text(&m.body[4]),
        actions: actions.iter().map(text).collect(),
        hints: m.body[6].clone(),
    }
}

impl Got {
    fn has_button(&self, key: &str) -> bool {
        self.actions.chunks(2).any(|kv| kv[0] == key)
    }
}

/// Handshake, capabilities, owner lookup and the match rule.
fn serve_open(p: &mut Peer) {
    p.handshake();
    let m = p.recv();
    assert_eq!(m.member.as_deref(), Some("GetCapabilities"));
    p.reply(&m, vec![strings(&CAPS)]);
    let m = p.recv();
    assert_eq!(m.member.as_deref(), Some("GetNameOwner"));
    p.reply(&m, vec![Value::Str(OWNER.into())]);
    let m = p.recv();
    assert_eq!(m.member.as_deref(), Some("AddMatch"));
    p.reply(&m, vec![]);
}

fn signal(p: &mut Peer, member: &str, id: u32, arg: Value) {
    p.signal_from(OWNER, PATH, IFACE, member, vec![Value::U32(id), arg]);
}

fn invoke(p: &mut Peer, id: u32, key: &str) {
    signal(p, "ActionInvoked", id, Value::Str(key.into()));
}

fn close(p: &mut Peer, id: u32) {
    signal(p, "NotificationClosed", id, Value::U32(2));
}

/// A notification server: answers every `Notify` (a new id unless it
/// replaces one), reports it on the channel, then lets `react` answer it.
fn server(react: impl FnMut(&mut Peer, &Got) + Send + 'static) -> (Fake, Receiver<Got>) {
    server_hooked(|_, _| {}, react)
}

/// `server` that also lets `before` act on a `Notify` before its reply.
fn server_hooked(
    mut before: impl FnMut(&mut Peer, &Got) + Send + 'static,
    mut react: impl FnMut(&mut Peer, &Got) + Send + 'static,
) -> (Fake, Receiver<Got>) {
    let (tx, rx): (Sender<Got>, _) = mpsc::channel();
    let fake = Fake::start(move |p| {
        serve_open(p);
        let mut next = FIRST_ID;
        while let Some(m) = p.try_recv() {
            match m.member.as_deref() {
                Some("Notify") => {
                    let id = match m.body[1] {
                        Value::U32(0) => {
                            next += 1;
                            next - 1
                        }
                        Value::U32(r) => r,
                        _ => panic!("{:?}", m.body),
                    };
                    let g = got(&m, id);
                    before(p, &g);
                    // A client that gave up (a test of that) is gone.
                    if p.try_send(Peer::reply_msg(&m, vec![Value::U32(id)])) {
                        react(p, &g);
                    }
                    tx.send(g).unwrap();
                }
                Some("CloseNotification") => {
                    p.try_send(Peer::reply_msg(&m, vec![]));
                }
                other => panic!("unexpected call {other:?}"),
            }
        }
    });
    (fake, rx)
}

/// Body of a run that may have had no Landlock.
fn plain_body(b: &str) -> bool {
    b.is_empty() || b == "Ran without sandbox."
}

#[test]
fn final_notice_after_a_run() {
    let e = Env::new("final");
    e.file("a.txt", "hello\n");
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = seen.clone();
    let (fake, rx) = server(move |p, g| {
        // The run is blocked in this call: what it printed came first.
        std::thread::sleep(Duration::from_millis(50));
        let out = seen2.lock().unwrap().clone();
        assert!(out.lines().any(|l| l.starts_with("done\t")), "{out}");
        assert!(out.lines().any(|l| l.starts_with("summary\t")), "{out}");
        close(p, g.id);
    });
    let mut c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    );
    c.env("INACIRCLE_UNDO_WAIT_MS", "20000");
    let r = run(c, &seen);
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.unavailable(), 0, "{}", r.stderr);
    assert_eq!(n.len(), 1);
    assert_eq!((n[0].replaces, n[0].summary.as_str()), (0, "a.md"));
    assert!(plain_body(&n[0].body), "{}", n[0].body);
    assert_eq!(n[0].actions, ["undo", "Undo"]);
    // The close ended the Undo window at once.
    assert!(r.elapsed < Duration::from_secs(5), "{:?}", r.elapsed);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=0"]);
}

#[test]
fn measure_reports_the_notification_cpu_before_the_undo_window() {
    let e = Env::new("measure");
    e.file("a.txt", "hello\n");
    let (fake, _rx) = server(|p, g| close(p, g.id));
    let mut c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    );
    c.env("INACIRCLE_MEASURE", "1");
    let r = plain_run(c);
    fake.join();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    let l = r
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix("inacircle: measure notify_cpu_us="))
        .unwrap_or_else(|| panic!("no notify line: {}", r.stderr));
    assert!(l.parse::<i64>().is_ok_and(|n| n >= 0), "{l}");
}

#[test]
fn undo_button_removes_the_output() {
    let e = Env::new("undo");
    e.file("a.txt", "hello\n");
    let (fake, rx) = server(|p, g| {
        if g.has_button("undo") {
            invoke(p, g.id, "undo");
        }
    });
    let r = plain_run(e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    ));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert!(!e.files().join("a.md").exists());
    assert!(e.files().join("a.txt").exists());
    assert_eq!(n.len(), 2);
    assert_eq!(
        (n[1].summary.as_str(), n[1].body.as_str()),
        ("Undone", "1 removed")
    );
    assert!(n[1].actions.is_empty());
}

#[test]
fn undo_button_keeps_a_changed_output() {
    let e = Env::new("undo-kept");
    e.file("a.txt", "hello\n");
    let (notified, notified_rx) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let (fake, rx) = server(move |p, g| {
        if g.has_button("undo") {
            notified.send(()).unwrap();
            go_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            invoke(p, g.id, "undo");
        }
    });
    let c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    );
    let md = e.files().join("a.md");
    let editor = std::thread::spawn(move || {
        notified_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        fs::write(&md, "edited\n").unwrap();
        go.send(()).unwrap();
    });
    let r = plain_run(c);
    editor.join().unwrap();
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(
        fs::read_to_string(e.files().join("a.md")).unwrap(),
        "edited\n"
    );
    assert_eq!(n.len(), 2);
    assert_eq!(
        (n[1].summary.as_str(), n[1].body.as_str()),
        ("Undone", "0 removed, 1 kept (changed since)")
    );
}

#[test]
fn undo_window_ends_without_buttons() {
    let e = Env::new("window");
    e.file("a.txt", "hello\n");
    let (fake, rx) = server(|_, _| {});
    let mut c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    );
    c.env("INACIRCLE_UNDO_WAIT_MS", "300");
    let r = plain_run(c);
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert!(e.files().join("a.md").exists());
    assert_eq!(n.len(), 2);
    assert_eq!(n[0].actions, ["undo", "Undo"]);
    assert_eq!(n[1].replaces, n[0].id);
    assert!(n[1].actions.is_empty());
    assert_eq!((n[1].summary.as_str(), &n[1].body), ("a.md", &n[0].body));
    let gap = n[1].at - n[0].at;
    assert!(gap >= Duration::from_millis(250), "{gap:?}");
    assert!(r.elapsed < Duration::from_secs(5), "{:?}", r.elapsed);
}

#[test]
fn running_notice_and_cancel_button() {
    let e = Env::new("cancel");
    let a = e.test_input("a.test", &["sleep_ms=5000", "events=done"]);
    let start = Instant::now();
    let (fake, rx) = server(|p, g| {
        if g.has_button("cancel") && g.replaces == 0 {
            invoke(p, g.id, "cancel");
        }
    });
    let r = plain_run(e.cmd(
        Some(&bus_of(&fake)),
        &[
            OsStr::new("run"),
            OsStr::new("--notify"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            a.as_os_str(),
        ],
    ));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert!(r.elapsed < Duration::from_secs(6), "{:?}", r.elapsed);
    assert_eq!(r.code, Some(1), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("fail"), [format!("{}\tcancelled", a.display())]);
    let first = &n[0];
    assert_eq!(
        (first.summary.as_str(), first.body.as_str()),
        ("Test", "0 / 1 files")
    );
    assert_eq!(first.actions, ["cancel", "Cancel"]);
    let waited = first.at - start;
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    assert_eq!(
        first.hints,
        Value::Dict(
            "s".into(),
            "v".into(),
            vec![(
                Value::Str("value".into()),
                Value::Variant(Box::new(Value::I32(0)))
            )]
        )
    );
    let last = n.last().unwrap();
    assert_eq!(last.summary, "Cancelled");
    assert_eq!(last.replaces, first.id);
    assert!(
        last.body.contains("0 done, 1 not converted"),
        "{}",
        last.body
    );
    assert!(last.actions.is_empty());
}

#[test]
fn late_cancel_click_is_ignored() {
    let e = Env::new("late-cancel");
    let a = e.test_input("a.test", &["sleep_ms=1300", "events=done"]);
    // The click lands once the run is over: it cannot cancel anything.
    let (fake, rx) = server(|p, g| {
        if g.has_button("undo") {
            invoke(p, g.id, "cancel");
            close(p, g.id);
        }
    });
    let r = plain_run(e.cmd(
        Some(&bus_of(&fake)),
        &[
            OsStr::new("run"),
            OsStr::new("--notify"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            a.as_os_str(),
        ],
    ));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert!(r.lines("fail").is_empty(), "{}", r.stdout);
    assert!(e.files().join("a.bin").exists());
    assert!(n[0].has_button("cancel"), "no running notice");
    let last = n.last().unwrap();
    assert_eq!(last.summary, "a.bin");
    assert_eq!(last.actions, ["undo", "Undo"]);
    assert_eq!(last.replaces, n[0].id);
}

#[test]
fn fast_run_connects_once_at_the_end() {
    let e = Env::new("fast");
    e.file("a.txt", "hello\n");
    let (fake, rx) = server(|p, g| close(p, g.id));
    let r = plain_run(e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    ));
    assert_eq!(fake.join_counting(), 0, "more than one connection");
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(n.len(), 1);
    assert!(!n[0].has_button("cancel"));
    assert_eq!(n[0].summary, "a.md");
}

/// The same run with and without `--notify` on `bus`: the notify run's
/// result is unchanged; returns it and how much later it ended.
fn compare(name: &str, bus: &str) -> (Run, Duration) {
    let plain = Env::new(&format!("{name}-plain"));
    plain.file("a.txt", "hello\n");
    let p = plain_run(plain.cmd(None, &["run", "markdown", "a.txt"]));
    let e = Env::new(name);
    e.file("a.txt", "hello\n");
    let r = plain_run(e.cmd(Some(bus), &["run", "--notify", "markdown", "a.txt"]));
    assert_eq!(r.code, p.code, "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("summary"), p.lines("summary"));
    assert_eq!(r.lines("done").len(), 1, "{}", r.stdout);
    assert!(e.files().join("a.md").exists());
    let late = r.elapsed.saturating_sub(p.elapsed);
    (r, late)
}

#[test]
fn no_bus_runs_normally() {
    let missing = std::env::temp_dir().join(format!("inacircle-no-bus-{}", std::process::id()));
    let (r, late) = compare("no-bus", &format!("unix:path={}", missing.display()));
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
    assert!(late < Duration::from_millis(400), "{late:?}");
    // A path that is not a socket, and an address with no unix entry.
    for bus in ["unix:path=/dev/null", "tcp:host=localhost,port=1"] {
        let (r, _) = compare("not-socket", bus);
        assert_eq!(r.unavailable(), 1, "{bus}: {}", r.stderr);
    }
}

#[test]
fn silent_server_does_not_delay_the_run() {
    let fake = Fake::start(|p| p.drain());
    let (r, late) = compare("silent", &bus_of(&fake));
    fake.join();
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
    assert!(late < Duration::from_secs(1), "{late:?}");
}

#[test]
fn without_notify_flag_no_bus_is_touched() {
    let e = Env::new("no-flag");
    e.file("a.txt", "hello\n");
    let sock = e.root.join("bus");
    let listener = UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let bus = format!("unix:path={}", sock.display());
    let r = plain_run(e.cmd(Some(&bus), &["run", "markdown", "a.txt"]));
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert!(listener.accept().is_err(), "the run connected to the bus");
    assert!(r.stderr.is_empty(), "{}", r.stderr);
}

#[test]
fn dismissed_running_notice_is_not_shown_again() {
    let e = Env::new("dismissed");
    fs::create_dir_all(e.files().join("d1")).unwrap();
    fs::create_dir_all(e.files().join("d2")).unwrap();
    // Two folders, two processes: the first ends after the running notice,
    // which would update it.
    let a = e.test_input("d1/a.test", &["sleep_ms=1300", "events=done"]);
    let b = e.test_input("d2/b.test", &["sleep_ms=500", "events=done"]);
    let (fake, rx) = server(|p, g| close(p, g.id));
    let r = plain_run(e.cmd(
        Some(&bus_of(&fake)),
        &[
            OsStr::new("run"),
            OsStr::new("--notify"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            a.as_os_str(),
            b.as_os_str(),
        ],
    ));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("done").len(), 2, "{}", r.stdout);
    let running = n.iter().filter(|g| g.has_button("cancel")).count();
    assert_eq!(running, 1, "running notice shown again after its close");
    assert_eq!(n.last().unwrap().summary, "2 done");
}

/// `inacircle run --notify --action test.script <inputs>`.
fn test_script(e: &Env, bus: Option<&str>, notify: bool, inputs: &[&Path]) -> Command {
    let mut args = vec![OsStr::new("run")];
    if notify {
        args.push(OsStr::new("--notify"));
    }
    args.extend([OsStr::new("--action"), OsStr::new("test.script")]);
    args.extend(inputs.iter().map(|p| p.as_os_str()));
    e.cmd(bus, &args)
}

#[test]
fn signal_flood_does_not_stall_the_run() {
    let e = Env::new("flood");
    let a = e.test_input("a.test", &["sleep_ms=2000", "events=done"]);
    let (fake, _rx) = server(|p, g| {
        if g.replaces == 0 && g.has_button("cancel") {
            // Endless clicks on our own notification, from the server.
            let m = Message {
                kind: dbus::Kind::Signal,
                serial: 1000,
                reply_serial: None,
                path: Some(PATH.into()),
                interface: Some(IFACE.into()),
                member: Some("ActionInvoked".into()),
                error_name: None,
                destination: None,
                sender: Some(OWNER.into()),
                signature: String::new(),
                body: vec![Value::U32(g.id), Value::Str("bogus".into())],
            };
            let one = dbus::encode(&m).unwrap();
            p.flood(&one.repeat(100));
        }
    });
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("done").len(), 1, "{}", r.stdout);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=0"]);
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
    assert!(r.elapsed < Duration::from_secs(3), "{:?}", r.elapsed);
}

#[test]
fn foreign_id_flood_turns_notifications_off() {
    let e = Env::new("foreign-flood");
    let a = e.test_input("a.test", &["sleep_ms=2000", "events=done"]);
    let (fake, _rx) = server(|p, g| {
        if g.replaces == 0 && g.has_button("cancel") {
            // Endless clicks on another application's notification.
            let m = Message {
                kind: dbus::Kind::Signal,
                serial: 1000,
                reply_serial: None,
                path: Some(PATH.into()),
                interface: Some(IFACE.into()),
                member: Some("ActionInvoked".into()),
                error_name: None,
                destination: None,
                sender: Some(OWNER.into()),
                signature: String::new(),
                body: vec![Value::U32(999), Value::Str("bogus".into())],
            };
            let one = dbus::encode(&m).unwrap();
            p.flood(&one.repeat(100));
        }
    });
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    assert!(
        r.cpu < r.elapsed / 4,
        "cpu {:?} over a {:?} run",
        r.cpu,
        r.elapsed
    );
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("done").len(), 1, "{}", r.stdout);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=0"]);
    assert_eq!(
        r.stderr
            .lines()
            .filter(|l| *l
                == "inacircle: notifications unavailable: \
                    too many signals from the notification server")
            .count(),
        1,
        "{}",
        r.stderr
    );
}

#[test]
fn cancel_during_a_progress_update_is_not_lost() {
    let e = Env::new("cancel-in-update");
    fs::create_dir_all(e.files().join("d1")).unwrap();
    fs::create_dir_all(e.files().join("d2")).unwrap();
    let a = e.test_input("d1/a.test", &["sleep_ms=1300", "events=done"]);
    let b = e.test_input("d2/b.test", &["sleep_ms=4000", "events=done"]);
    // The click arrives while the client waits for the update's reply.
    let (fake, rx) = server_hooked(
        |p, g| {
            if g.has_button("cancel") && g.body == "1 / 2 files" {
                invoke(p, g.id, "cancel");
            }
        },
        |p, g| {
            if g.has_button("undo") {
                close(p, g.id);
            }
        },
    );
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a, &b]));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert!(n.iter().any(|g| g.body == "1 / 2 files"), "no 1 / 2 update");
    assert_eq!(r.lines("fail"), [format!("{}\tcancelled", b.display())]);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=1"]);
    assert_eq!(n.last().unwrap().summary, "Cancelled");
    assert!(r.elapsed < Duration::from_secs(4), "{:?}", r.elapsed);
}

#[test]
fn sigterm_cancel_says_cancelled() {
    let e = Env::new("sigterm");
    let a = e.test_input("a.test", &["sleep_ms=5000", "events=done"]);
    let (fake, rx) = server(|_, _| {});
    let seen = Arc::new(Mutex::new(String::new()));
    let r = run_then(
        test_script(&e, Some(&bus_of(&fake)), true, &[&a]),
        &seen,
        |pid| {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                kill(pid, "-TERM");
            });
        },
    );
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(1), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("fail"), [format!("{}\tcancelled", a.display())]);
    let last = n.last().unwrap();
    assert_eq!(
        (
            last.summary.as_str(),
            last.body.contains("0 done, 1 not converted")
        ),
        ("Cancelled", true),
        "{}",
        last.body
    );
}

#[test]
fn slow_server_stalls_the_run_once_briefly() {
    let e = Env::new("slow");
    let a = e.test_input("a.test", &["sleep_ms=1100", "events=done"]);
    // Every call after `Hello` is answered 400 ms late; the client's open
    // during the run has 500 ms for all of them.
    let fake = Fake::start(|p| {
        p.handshake();
        while let Some(m) = p.try_recv() {
            std::thread::sleep(Duration::from_millis(400));
            let body = match m.member.as_deref() {
                Some("GetCapabilities") => vec![strings(&CAPS)],
                Some("GetNameOwner") => vec![Value::Str(OWNER.into())],
                Some("Notify") => vec![Value::U32(FIRST_ID)],
                _ => vec![],
            };
            // The client may have given up and gone.
            p.try_send(Peer::reply_msg(&m, body));
        }
    });
    let plain = plain_run(test_script(&e, None, false, &[&a]));
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
    let late = r.elapsed.saturating_sub(plain.elapsed);
    assert!(late < Duration::from_millis(600), "{late:?}");
}
