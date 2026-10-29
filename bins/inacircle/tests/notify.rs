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

mod fake_compositor;
use fake_compositor::{Compositor, Delay, KEYBOARD, POINTER, Script};

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

/// The summary of a `Got` that records a `CloseNotification`.
const CLOSED: &str = "<closed>";

/// One `Notify` the server got (or a `CloseNotification`), and when.
struct Got {
    at: Instant,
    id: u32,
    replaces: u32,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: Value,
    /// Which connection it came on (0: the first).
    conn: usize,
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
        conn: 0,
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

/// A signal about notification `id`. A client that already finished (a
/// final notice with no button to wait for) is gone by the time the
/// server reacts: the write then fails with a broken pipe, and that is not
/// the server's failure (the fixture used to panic here, a flaky test).
fn signal(p: &mut Peer, member: &str, id: u32, arg: Value) {
    let _ = p.try_signal_from(OWNER, PATH, IFACE, member, vec![Value::U32(id), arg]);
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
    before: impl FnMut(&mut Peer, &Got) + Send + 'static,
    react: impl FnMut(&mut Peer, &Got) + Send + 'static,
) -> (Fake, Receiver<Got>) {
    servers(1, before, react)
}

/// `server_hooked` for `clients` connections one after the other (a client
/// that reconnects); ids go on across them, `Got::conn` says which.
fn servers(
    clients: usize,
    mut before: impl FnMut(&mut Peer, &Got) + Send + 'static,
    mut react: impl FnMut(&mut Peer, &Got) + Send + 'static,
) -> (Fake, Receiver<Got>) {
    let (tx, rx): (Sender<Got>, _) = mpsc::channel();
    let mut next = FIRST_ID;
    let fake = Fake::start_clients(clients, move |p, conn| {
        serve_open(p);
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
                    let g = Got {
                        conn,
                        ..got(&m, id)
                    };
                    before(p, &g);
                    // A client that gave up (a test of that) is gone.
                    if p.try_send(Peer::reply_msg(&m, vec![Value::U32(id)])) {
                        react(p, &g);
                    }
                    tx.send(g).unwrap();
                }
                Some("CloseNotification") => {
                    p.try_send(Peer::reply_msg(&m, vec![]));
                    // Recorded as a pseudo notice with the closed id.
                    let Value::U32(id) = m.body[0] else {
                        panic!("CloseNotification without an id");
                    };
                    tx.send(Got {
                        at: Instant::now(),
                        id,
                        replaces: 0,
                        summary: CLOSED.into(),
                        body: String::new(),
                        actions: Vec::new(),
                        hints: Value::Bool(false),
                        conn,
                    })
                    .unwrap();
                }
                other => panic!("unexpected call {other:?}"),
            }
        }
    });
    (fake, rx)
}

/// Body of a run that may have had no Landlock.
fn plain_body(b: &str) -> bool {
    b.is_empty() || b == "Sandbox unavailable."
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
    // `text.txt-to-md` is `lossy=yes`, but this text lost nothing: no
    // formatting line.
    assert!(plain_body(&n[0].body), "{}", n[0].body);
    assert_eq!(n[0].actions, ["undo", "Undo"]);
    // The close ended the Undo window at once.
    assert!(r.elapsed < Duration::from_secs(5), "{:?}", r.elapsed);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=0"]);
}

#[test]
fn final_notice_says_formatting_was_lost_when_it_was() {
    let e = Env::new("final-lost");
    e.file("a.txt", "alpha\n\n\nbeta\n");
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
    // The blank-line run was merged: the run lost something, and says so.
    assert!(
        n[0].body.starts_with("Formatting was lost."),
        "{:?} {}",
        n[0].body,
        r.stdout
    );
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
fn undo_button_moves_the_output_to_the_trash() {
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
    assert!(!e.files().join("a").exists());
    assert!(e.files().join("a.txt").exists());
    let trash = e.root.join("home/.local/share/Trash");
    assert!(trash.join("files/a/a.md").is_file());
    assert!(trash.join("info/a.trashinfo").exists());
    assert_eq!(n.len(), 2);
    assert_eq!(
        (n[1].summary.as_str(), n[1].body.as_str()),
        ("Undone", "Moved to Trash")
    );
    assert!(n[1].actions.is_empty());
    // It replaces the result, whose Undo button is spent.
    assert_eq!(n[1].replaces, n[0].id);
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
    let md = e.files().join("a/a.md");
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
        fs::read_to_string(e.files().join("a/a.md")).unwrap(),
        "edited\n"
    );
    assert_eq!(n.len(), 2);
    assert_eq!(
        (n[1].summary.as_str(), n[1].body.as_str()),
        ("Nothing undone", "1 kept because it changed")
    );
}

#[test]
fn undo_window_ends_by_closing_the_notice() {
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
    assert!(e.files().join("a/a.md").exists());
    assert_eq!(n.len(), 2);
    assert_eq!(n[0].actions, ["undo", "Undo"]);
    // Closed, not resent: a resend pops up again on common servers.
    assert_eq!((n[1].summary.as_str(), n[1].id), (CLOSED, n[0].id));
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
    assert!(e.files().join("a/a.bin").exists());
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
    assert!(e.files().join("a/a.md").exists());
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
    // The supervisor's stall shows in when the output is placed; the end
    // of the run adds the one reopen (300 ms here: nobody answers).
    let (plain, plain_done) = run_until_done(test_script(&e, None, false, &[&a]));
    let (r, done) = run_until_done(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    assert_eq!(fake.join_counting(), 1, "no reopen at the end");
    assert_eq!(plain.code, Some(0));
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
    let late = done.saturating_sub(plain_done);
    assert!(late < Duration::from_millis(600), "{late:?}");
}

/// `run`, and how long after the start the `done` line came.
fn run_until_done(c: Command) -> (Run, Duration) {
    let seen = Arc::new(Mutex::new(String::new()));
    let at = Arc::new(Mutex::new(None));
    let (seen2, at2) = (seen.clone(), at.clone());
    let start = Instant::now();
    let r = run_then(c, &seen, move |_| {
        std::thread::spawn(move || {
            while start.elapsed() < WATCHDOG {
                if seen2
                    .lock()
                    .unwrap()
                    .lines()
                    .any(|l| l.starts_with("done\t"))
                {
                    *at2.lock().unwrap() = Some(start.elapsed());
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
    });
    // The poller may lag the end of the run by a tick.
    std::thread::sleep(Duration::from_millis(10));
    let at = at.lock().unwrap().expect("no done line");
    (r, at)
}

/// The server goes away: the client sees a hangup.
fn hang_up(p: &mut Peer) {
    let _ = p.stream.shutdown(std::net::Shutdown::Both);
}

#[test]
fn sigterm_in_the_undo_window_keeps_the_exit_code() {
    let e = Env::new("term-window");
    e.file("a.txt", "hello\n");
    let (fake, rx) = server(|_, _| {});
    let seen = Arc::new(Mutex::new(String::new()));
    let signalled = Arc::new(Mutex::new(None));
    let (seen2, signalled2) = (seen.clone(), signalled.clone());
    let c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    );
    let r = run_then(c, &seen, move |pid| {
        std::thread::spawn(move || {
            let start = Instant::now();
            while !seen2.lock().unwrap().contains("summary\t") {
                assert!(start.elapsed() < Duration::from_secs(10), "no summary");
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_secs(1));
            *signalled2.lock().unwrap() = Some(Instant::now());
            kill(pid, "-TERM");
        });
    });
    let ended = Instant::now();
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    // The work was done: the signal ends the window, not the run.
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    let after = ended - signalled.lock().unwrap().expect("no signal sent");
    assert!(after < Duration::from_millis(600), "{after:?}");
    assert_eq!(n.len(), 2);
    assert_eq!(n[0].actions, ["undo", "Undo"]);
    assert_eq!((n[1].summary.as_str(), n[1].id), (CLOSED, n[0].id));
    assert!(e.files().join("a/a.md").exists());
}

#[test]
fn slow_update_still_ends_with_a_result() {
    let e = Env::new("slow-update");
    let a = e.test_input(
        "a.test",
        &[
            "sleep_ms=1300",
            "events=progress:0.5",
            "sleep_ms=500",
            "events=done",
        ],
    );
    // The first progress update is answered late: the client gives up on
    // that connection, and reconnects for the result.
    let (fake, rx) = servers(
        2,
        |_, g| {
            if g.conn == 0 && g.replaces != 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
        },
        |p, g| {
            if g.has_button("undo") {
                close(p, g.id);
            }
        },
    );
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.unavailable(), 0, "{}", r.stderr);
    let last = n.last().unwrap();
    assert_eq!((last.conn, last.summary.as_str()), (1, "a.bin"));
    // It replaces the running notice, whose Cancel is dead now.
    assert_eq!(last.replaces, n[0].id);
    assert!(n[0].has_button("cancel"));
}

#[test]
fn server_back_at_the_end_gets_the_result() {
    let e = Env::new("server-back");
    let a = e.test_input("a.test", &["sleep_ms=1500", "events=done"]);
    let (fake, rx) = servers(
        2,
        |_, _| {},
        |p, g| {
            if g.conn == 0 {
                hang_up(p);
            } else if g.has_button("undo") {
                close(p, g.id);
            }
        },
    );
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.unavailable(), 0, "{}", r.stderr);
    assert_eq!(n.len(), 2);
    assert!(n[0].has_button("cancel"));
    assert_eq!((n[1].conn, n[1].replaces), (1, n[0].id));
    assert_eq!(n[1].summary, "a.bin");
}

#[test]
fn dead_server_at_the_end_is_one_line() {
    let e = Env::new("server-dead");
    let a = e.test_input("a.test", &["sleep_ms=1500", "events=done"]);
    // Gone mid-run and never back: the reopen finds nobody answering.
    let (fake, rx) = server(|p, _| hang_up(p));
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    assert_eq!(fake.join_counting(), 1, "no reopen at the end");
    assert_eq!(rx.try_iter().count(), 1);
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=0"]);
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
}

#[test]
fn notify_id_0_turns_notifications_off() {
    let e = Env::new("id-0");
    let mut opts = vec!["sleep_ms=1100".to_owned()];
    let p: Vec<String> = (1..10).map(|i| format!("progress:0.{i}")).collect();
    opts.push(format!("events={}", p.join(",")));
    opts.push("events=done".to_owned());
    let opts: Vec<&str> = opts.iter().map(String::as_str).collect();
    let a = e.test_input("a.test", &opts);
    let (tx, rx) = mpsc::channel();
    let fake = Fake::start(move |p| {
        serve_open(p);
        while let Some(m) = p.try_recv() {
            tx.send(m.member.clone()).unwrap();
            p.try_send(Peer::reply_msg(&m, vec![Value::U32(0)]));
        }
    });
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    let calls: Vec<_> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    // One answer of 0 is enough: no new notification per progress event.
    assert_eq!(calls, [Some("Notify".to_owned())]);
    assert_eq!(r.unavailable(), 1, "{}", r.stderr);
}

#[test]
fn no_update_once_all_inputs_are_done() {
    let e = Env::new("no-last-update");
    let a = e.test_input("a.test", &["sleep_ms=1300", "events=done"]);
    let (fake, rx) = server(|p, g| {
        if g.has_button("undo") {
            close(p, g.id);
        }
    });
    let r = plain_run(test_script(&e, Some(&bus_of(&fake)), true, &[&a]));
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    // The running notice, then the result: no "1 / 1 files" in between.
    assert_eq!(
        n.len(),
        2,
        "{:?}",
        n.iter().map(|g| &g.body).collect::<Vec<_>>()
    );
    assert!(n[0].has_button("cancel") && n[1].has_button("undo"));
}

/// Where `needle` starts in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The history record after the publishing rename cannot be written (the
/// history may not grow past the intent: the commit record is cut short):
/// the output stays, the run is a success, the warning is shown and Undo is
/// not offered.
#[test]
fn history_failure_after_the_commit_keeps_the_output_without_undo() {
    use std::os::unix::process::CommandExt;
    let e = Env::new("histfail");
    e.file("a.txt", "hello\n");
    e.file("b.txt", "hello\n");
    let hist = e.root.join("state/inacircle/history");
    // Calibration: what an ordinary run of the same shape writes before
    // its commit record (from its job start).
    let (fake, _rx) = server(|p, g| close(p, g.id));
    let r = plain_run(e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    ));
    fake.join();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    let text = fs::read(&hist).unwrap();
    let begin = find(&text, b"\njob-start\t").expect("no job start") + 1;
    let commit = find(&text, b"\npublish-commit\t").expect("no commit record") + 1;
    // The next run may grow the history by that and 40 bytes more: its
    // intent fits (the lengths differ by a few digits at most), its commit
    // record (well over 100 bytes) is cut short.
    let limit = text.len() as u64 + (commit - begin) as u64 + 40;
    let (fake, rx) = server(|p, g| close(p, g.id));
    let mut c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "b.txt"],
    );
    // SAFETY: only async-signal-safe calls (signal, setrlimit) between
    // fork and exec.
    unsafe {
        c.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let lim = libc::rlimit {
                rlim_cur: limit,
                rlim_max: libc::RLIM_INFINITY,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let r = plain_run(c);
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!(r.lines("summary"), ["done=1\tskipped=0\tfailed=0"]);
    assert_eq!(
        fs::read_to_string(e.files().join("b/b.md")).unwrap(),
        "hello\n"
    );
    assert!(r.stderr.contains("history not written"), "{}", r.stderr);
    let last = n.last().unwrap();
    assert_eq!(last.summary, "b.md");
    assert!(last.body.contains("history not written"), "{}", last.body);
    assert!(last.body.contains("undo unavailable"), "{}", last.body);
    assert!(last.actions.is_empty(), "{:?}", last.actions);
    // The cut record is no commit: nothing in the history grants an undo.
    let text = fs::read(&hist).unwrap();
    assert_eq!(text.len() as u64, limit);
    let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    let (cut, whole) = lines.split_last().unwrap();
    assert!(cut.starts_with(b"publish-commit\t"), "{cut:?}");
    let commits = whole
        .iter()
        .filter(|l| l.starts_with(b"publish-commit\t"))
        .count();
    assert_eq!(commits, 1, "only the calibration run's");
}

/// No history at all: nothing is published (an intent cannot be recorded),
/// the notice says why, and offers no Undo.
#[test]
fn unwritable_history_offers_no_undo_and_says_why() {
    let e = Env::new("nohist");
    e.file("a.txt", "hello\n");
    let state = e.root.join("state-file");
    fs::write(&state, b"").unwrap();
    let (fake, rx) = server(|p, g| close(p, g.id));
    let mut c = e.cmd(
        Some(&bus_of(&fake)),
        &["run", "--notify", "markdown", "a.txt"],
    );
    c.env("XDG_STATE_HOME", &state);
    let r = plain_run(c);
    fake.join();
    let n: Vec<Got> = rx.try_iter().collect();
    let last = n.last().unwrap();
    assert!(last.actions.is_empty(), "{:?}", last.actions);
    assert!(last.body.contains("history not written"), "{}", last.body);
    assert!(!e.files().join("a").exists());
    assert!(e.files().join("a.txt").exists());
    assert_eq!(r.code, Some(1), "{}{}", r.stdout, r.stderr);
}

// The wheel's failed start (hardening task 11): a fake compositor that
// does not let the wheel open, a scripted notification server, and the
// order of what happens between them.

/// The wheel (no arguments), its compositor `wayland-0` in the runtime dir.
fn wheel_cmd(e: &Env, bus: Option<&str>) -> Command {
    let mut c = e.cmd(bus, &[] as &[&str]);
    c.env("WAYLAND_DISPLAY", "wayland-0");
    c
}

/// How long a test waits before it fails: a hung wheel must fail the test.
const PATIENCE: Duration = Duration::from_secs(10);
/// The proposed `ui_startup_deadline` (`docs/resource-limits.md`).
const STARTUP: Duration = Duration::from_millis(2000);
/// Esc, as the wheel reads it.
const KEY_ESC: u32 = 1;

/// The notification server a failed start meets.
#[derive(Clone, Copy)]
enum Bus {
    /// No session bus at all.
    Missing,
    /// A server that answers as a notification server does.
    Working,
    /// One that accepts the connection and never says a word.
    Silent,
}

/// What a failed start looked like from outside.
struct Failed {
    code: Option<i32>,
    stderr: String,
    took: Duration,
    notices: Vec<Got>,
    /// At the moment the first `Notify` arrived (the wheel then waits for
    /// the reply): was the wheel lock already free, was the wheel's
    /// compositor connection already closed, and had the layer surface
    /// (the keyboard grab) been destroyed. `None`: no `Notify` came.
    at_notify: Option<(bool, bool, bool)>,
}

/// Whether `wheel.lock` can be taken now (and is left free).
fn lock_is_free(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = fs::File::open(path) else {
        return true;
    };
    // SAFETY: flock on an fd the file owns; LOCK_UN releases what we took.
    unsafe {
        let free = libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0;
        libc::flock(f.as_raw_fd(), libc::LOCK_UN);
        free
    }
}

/// Reaps `child` within `PATIENCE` (a hung wheel is killed and fails the
/// test); its exit code and stderr.
fn reap(mut child: std::process::Child) -> (Option<i32>, String) {
    let until = Instant::now() + PATIENCE;
    let st = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the wheel did not exit within {PATIENCE:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (st.code(), stderr)
}

/// Starts the wheel against a fake compositor with `script`, lets `drive`
/// bring it to the point of failing, and watches until it exits. The bus
/// server holds the first `Notify` until this thread has looked at the
/// compositor and the lock: a barrier, not a timing guess.
fn failed_start(e: &Env, bus: Bus, script: Script, drive: impl FnOnce(&mut Compositor)) -> Failed {
    let lock_path = e.root.join("runtime/inacircle/wheel.lock");
    let (arrive_tx, arrive_rx) = mpsc::channel::<bool>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (fake, notices) = match bus {
        Bus::Missing => (None, None),
        Bus::Working => {
            let (f, rx) = server_hooked(
                move |_, _| {
                    let _ = arrive_tx.send(lock_is_free(&lock_path));
                    let _ = go_rx.recv_timeout(PATIENCE);
                },
                |_, _| {},
            );
            (Some(f), Some(rx))
        }
        Bus::Silent => (
            Some(Fake::start(|p| {
                // Reads what the client sends and never answers; the client
                // gives up at its deadline and closes.
                let mut b = [0u8; 256];
                while matches!(p.stream.read(&mut b), Ok(n) if n > 0) {}
            })),
            None,
        ),
    };
    let addr = fake.as_ref().map(bus_of);
    let t0 = Instant::now();
    let mut child = None;
    let mut c = Compositor::serve_with(
        &e.root.join("runtime"),
        Duration::from_secs(2),
        script,
        || {
            child = Some(wheel_cmd(e, addr.as_deref()).spawn().unwrap());
        },
    );
    let child = child.unwrap();
    drive(&mut c);
    let until = Instant::now() + PATIENCE;
    let mut at_notify = None;
    let mut child = child;
    loop {
        c.service();
        if let Ok(free) = arrive_rx.try_recv() {
            let closed = c.eof();
            at_notify = Some((free, closed, c.layer_requests().contains(&7)));
            let _ = go_tx.send(());
            break;
        }
        if child.try_wait().unwrap().is_some() || Instant::now() >= until {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let (code, stderr) = reap(child);
    let took = t0.elapsed();
    if let Some(f) = fake {
        f.join();
    }
    Failed {
        code,
        stderr,
        took,
        notices: notices.map_or_else(Vec::new, |rx| rx.try_iter().collect()),
        at_notify,
    }
}

/// The overlay mapped, and a seat with `caps` and the devices they imply.
fn seat_up(c: &mut Compositor, caps: u32) {
    c.mapped(Duration::from_secs(4));
    c.seat_bound(Duration::from_secs(2));
    c.seat_caps(caps);
    c.seat_devices(caps, Duration::from_secs(2));
}

const COULD_NOT_OPEN: &str = "Could not open the circle.";

/// The notice a failed start must leave: once, after the keyboard grab and
/// the connection and the lock are gone, with no Undo or any button.
fn assert_one_notice(f: &Failed) -> &Got {
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.notices.len(), 1, "{}", f.stderr);
    let g = &f.notices[0];
    assert_eq!(g.summary, COULD_NOT_OPEN);
    assert_eq!(g.replaces, 0);
    assert!(g.actions.is_empty(), "{:?}", g.actions);
    assert_eq!(
        f.at_notify,
        Some((true, true, true)),
        "lock free, connection closed, layer destroyed before the Notify"
    );
    assert_eq!(f.unavailable(), 0, "{}", f.stderr);
    g
}

impl Failed {
    fn unavailable(&self) -> usize {
        self.stderr
            .lines()
            .filter(|l| l.starts_with("inacircle: notifications unavailable: "))
            .count()
    }
}

/// No configure: the surface and the connection and the lock go first, then
/// one notice that names the real cause.
#[test]
fn startup_failure_notifies_after_the_surface_and_the_lock_are_gone() {
    let e = Env::new("startup-order");
    let script = Script {
        configure: Delay::Never,
        ..Script::default()
    };
    let f = failed_start(&e, Bus::Working, script, |_| {});
    eprintln!(
        "PREFLIGHT order (no configure): at_notify(lock_free,conn_closed,layer_destroyed)={:?} took={:?} summary={:?} body={:?}",
        f.at_notify,
        f.took,
        f.notices.first().map(|g| g.summary.clone()),
        f.notices.first().map(|g| g.body.clone())
    );
    let g = assert_one_notice(&f);
    assert!(g.body.contains("did not respond"), "{}", g.body);
    assert!(
        f.took >= STARTUP - Duration::from_millis(100),
        "{:?}",
        f.took
    );
}

/// A proven missing pointer is named, in the product's words.
#[test]
fn startup_names_a_missing_pointer() {
    let e = Env::new("startup-pointer");
    let f = failed_start(&e, Bus::Working, Script::default(), |c| {
        seat_up(c, KEYBOARD)
    });
    let g = assert_one_notice(&f);
    assert_eq!(g.body, "The pointer position was unavailable.");
}

/// The cause is not known (the seat has a pointer, nothing came): the
/// notice does not blame the pointer.
#[test]
fn startup_does_not_blame_the_pointer_for_an_unknown_cause() {
    let e = Env::new("startup-unknown");
    let f = failed_start(&e, Bus::Working, Script::default(), |c| {
        seat_up(c, POINTER | KEYBOARD)
    });
    let g = assert_one_notice(&f);
    assert!(!g.body.to_lowercase().contains("pointer"), "{}", g.body);
    assert!(!g.body.is_empty());
}

/// The wheel was on the screen when the connection broke: "Could not open
/// the circle." would be false. The log line and the exit stay; no notice.
#[test]
fn failure_after_the_first_commit_does_not_say_it_could_not_open() {
    let e = Env::new("startup-late-fail");
    let listener = UnixListener::bind(e.root.join("runtime/bus")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = None;
    let mut c = Compositor::serve_with(
        &e.root.join("runtime"),
        Duration::from_secs(2),
        Script::default(),
        || child = Some(wheel_cmd(&e, None).spawn().unwrap()),
    );
    seat_up(&mut c, POINTER | KEYBOARD);
    c.pointer_enters();
    c.attached(1, Duration::from_secs(4));
    c.disconnect();
    let (code, stderr) = reap(child.unwrap());
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("inacircle: wheel:"), "{stderr}");
    assert!(
        !stderr.contains("notifications unavailable"),
        "a notice was attempted: {stderr}"
    );
    assert!(listener.accept().is_err(), "the bus was touched");
}

/// Esc is a cancel, not an error: exit 0, and the bus is never touched.
#[test]
fn startup_esc_is_a_cancel_without_a_notification() {
    let e = Env::new("startup-esc");
    let listener = UnixListener::bind(e.root.join("runtime/bus")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = None;
    let mut c = Compositor::serve_with(
        &e.root.join("runtime"),
        Duration::from_secs(2),
        Script::default(),
        || child = Some(wheel_cmd(&e, None).spawn().unwrap()),
    );
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(KEY_ESC);
    let _ = c.hangs_up(PATIENCE);
    let (code, stderr) = reap(child.unwrap());
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(listener.accept().is_err(), "the bus was touched");
}

/// A second instance leaves quietly: exit 0, no Wayland, no notification.
#[test]
fn startup_second_instance_stays_silent() {
    use std::os::fd::AsRawFd;
    let e = Env::new("startup-second");
    fs::create_dir_all(e.root.join("runtime/inacircle")).unwrap();
    let lock = fs::File::create(e.root.join("runtime/inacircle/wheel.lock")).unwrap();
    // SAFETY: flock on an fd the file owns.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let bus = e.root.join("runtime/bus");
    let bus_listener = UnixListener::bind(&bus).unwrap();
    bus_listener.set_nonblocking(true).unwrap();
    let wayland = UnixListener::bind(e.root.join("runtime/wayland-0")).unwrap();
    wayland.set_nonblocking(true).unwrap();
    let child = wheel_cmd(&e, None).spawn().unwrap();
    let (code, stderr) = reap(child);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(bus_listener.accept().is_err(), "the bus was touched");
    assert!(wayland.accept().is_err(), "it went on to Wayland");
}

/// A setup failure before the surface (the lock path is a FIFO) is told
/// too, as a setup failure; the compositor is never reached.
#[test]
fn startup_lock_setup_error_notifies_without_touching_wayland() {
    let e = Env::new("startup-lockerr");
    let dir = e.root.join("runtime/inacircle");
    fs::create_dir_all(&dir).unwrap();
    let c = std::ffi::CString::new(dir.join("wheel.lock").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: mkfifo reads the NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let wayland = UnixListener::bind(e.root.join("runtime/wayland-0")).unwrap();
    wayland.set_nonblocking(true).unwrap();
    let (fake, rx) = server(|_, _| {});
    let bus = bus_of(&fake);
    let child = wheel_cmd(&e, Some(&bus)).spawn().unwrap();
    let (code, stderr) = reap(child);
    fake.join();
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("lock"), "{stderr}");
    let notices: Vec<Got> = rx.try_iter().collect();
    assert_eq!(notices.len(), 1, "{stderr}");
    assert_eq!(notices[0].summary, COULD_NOT_OPEN);
    assert_eq!(notices[0].body, "The wheel could not be set up.");
    assert!(notices[0].actions.is_empty());
    assert!(wayland.accept().is_err(), "it went on to Wayland");
}

/// No session bus: one log line, the same exit, nothing hangs.
#[test]
fn startup_failure_without_a_bus_logs_and_exits() {
    let e = Env::new("startup-nobus");
    let script = Script {
        configure: Delay::Never,
        ..Script::default()
    };
    let f = failed_start(&e, Bus::Missing, script, |_| {});
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.unavailable(), 1, "{}", f.stderr);
    assert!(
        f.took < STARTUP + Duration::from_millis(1500),
        "{:?}",
        f.took
    );
}

/// A notification server that never answers costs at most the aggregate
/// 500 ms open frame, and the exit code is the same.
#[test]
fn startup_failure_with_a_silent_bus_is_bounded() {
    let e = Env::new("startup-silent");
    let script = Script {
        configure: Delay::Never,
        ..Script::default()
    };
    let f = failed_start(&e, Bus::Silent, script, |_| {});
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.unavailable(), 1, "{}", f.stderr);
    assert!(
        f.took < STARTUP + Duration::from_millis(500) + Duration::from_millis(1500),
        "{:?}",
        f.took
    );
}

/// A notification server with a slow tongue: each of the three open steps
/// (capabilities, owner, match rule) is answered `open_step` late, and
/// every `Notify` `notify` late. The delays are the scenario, not a
/// guess at the client's speed.
fn slow_server(open_step: Duration, notify: Duration) -> (Fake, Receiver<Got>) {
    let (tx, rx) = mpsc::channel();
    let fake = Fake::start(move |p| {
        p.handshake();
        for (member, body) in [
            ("GetCapabilities", vec![strings(&CAPS)]),
            ("GetNameOwner", vec![Value::Str(OWNER.into())]),
            ("AddMatch", vec![]),
        ] {
            let Some(m) = p.try_recv() else { return };
            assert_eq!(m.member.as_deref(), Some(member));
            std::thread::sleep(open_step);
            if !p.try_send(Peer::reply_msg(&m, body)) {
                return;
            }
        }
        while let Some(m) = p.try_recv() {
            if m.member.as_deref() == Some("Notify") {
                std::thread::sleep(notify);
                let g = got(&m, FIRST_ID);
                if p.try_send(Peer::reply_msg(&m, vec![Value::U32(FIRST_ID)])) {
                    tx.send(g).unwrap();
                }
            }
        }
    });
    (fake, rx)
}

/// A failed start (no configure) against `slow_server`.
fn failed_start_slow(name: &str, open_step: Duration, notify: Duration) -> Failed {
    let e = Env::new(name);
    let script = Script {
        configure: Delay::Never,
        ..Script::default()
    };
    let (fake, rx) = slow_server(open_step, notify);
    let addr = bus_of(&fake);
    let t0 = Instant::now();
    let mut child = None;
    let mut c = Compositor::serve_with(
        &e.root.join("runtime"),
        Duration::from_secs(2),
        script,
        || child = Some(wheel_cmd(&e, Some(&addr)).spawn().unwrap()),
    );
    let child = child.unwrap();
    // Serve the compositor until the wheel hangs up and a moment more: the
    // notification comes after.
    let _ = c.hangs_up(PATIENCE);
    let (code, stderr) = reap(child);
    let took = t0.elapsed();
    fake.join();
    Failed {
        code,
        stderr,
        took,
        notices: rx.try_iter().collect(),
        at_notify: None,
    }
}

/// The named pair around `notification_rpc_deadline` (500 ms per call): a
/// `Notify` answered after 450 ms is shown; one after 550 ms is given up.
#[test]
fn startup_notice_call_below_the_rpc_deadline_is_shown() {
    let f = failed_start_slow(
        "startup-rpc-below",
        Duration::ZERO,
        Duration::from_millis(450),
    );
    eprintln!(
        "PREFLIGHT rpc notify@450ms: notices={} unavailable={} took={:?} stderr={:?}",
        f.notices.len(),
        f.unavailable(),
        f.took,
        f.stderr
    );
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.notices.len(), 1, "{}", f.stderr);
    assert_eq!(f.unavailable(), 0, "{}", f.stderr);
}

#[test]
fn startup_notice_call_above_the_rpc_deadline_is_given_up() {
    let f = failed_start_slow(
        "startup-rpc-above",
        Duration::ZERO,
        Duration::from_millis(550),
    );
    eprintln!(
        "PREFLIGHT rpc notify@550ms: notices={} unavailable={} took={:?} stderr={:?}",
        f.notices.len(),
        f.unavailable(),
        f.took,
        f.stderr
    );
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.unavailable(), 1, "{}", f.stderr);
    assert!(
        f.took < STARTUP + Duration::from_millis(1500),
        "{:?}",
        f.took
    );
}

/// The open is one 500 ms frame, not 500 ms per step: three steps of
/// 140 ms (420 ms in all) open; three of 200 ms (600 ms) are given up.
#[test]
fn startup_notice_open_within_the_aggregate_frame_is_shown() {
    let f = failed_start_slow(
        "startup-open-below",
        Duration::from_millis(140),
        Duration::ZERO,
    );
    eprintln!(
        "PREFLIGHT open 3x140ms: notices={} unavailable={} took={:?} stderr={:?}",
        f.notices.len(),
        f.unavailable(),
        f.took,
        f.stderr
    );
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.notices.len(), 1, "{}", f.stderr);
    assert_eq!(f.unavailable(), 0, "{}", f.stderr);
}

#[test]
fn startup_notice_open_past_the_aggregate_frame_is_given_up() {
    let f = failed_start_slow(
        "startup-open-above",
        Duration::from_millis(200),
        Duration::ZERO,
    );
    eprintln!(
        "PREFLIGHT open 3x200ms: notices={} unavailable={} took={:?} stderr={:?}",
        f.notices.len(),
        f.unavailable(),
        f.took,
        f.stderr
    );
    assert_eq!(f.code, Some(1), "{}", f.stderr);
    assert_eq!(f.unavailable(), 1, "{}", f.stderr);
    assert!(f.notices.is_empty());
}

/// The engine memory ceiling (docs/benchmarks/acceptance.md): 5 MB PSS as
/// a maximum over every observation, not a percentile.
const PSS_CEILING_BYTES: u64 = 5_000_000;

/// External sampling cadence for the memory phases. A `smaps_rollup` read
/// takes well under a millisecond and the phases last from tens of
/// milliseconds to seconds, so 2 ms puts dozens of samples in the shortest
/// phase; PSS has no kernel-kept peak, so what is reported is the sampled
/// maximum, not a proof of every transient.
const SAMPLE_EVERY: Duration = Duration::from_millis(2);

/// (PSS, RSS) of `pid` in bytes, from `smaps_rollup` (kB are KiB), or
/// `None` once the process is gone.
fn pss_rss(pid: u32) -> Option<(u64, u64)> {
    let t = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let field = |name: &str| -> Option<u64> {
        let l = t.lines().find(|l| l.starts_with(name))?;
        Some(l.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    };
    Some((field("Pss:")?, field("Rss:")?))
}

/// Runs `args` under `--notify` with a server that never answers (so the
/// Undo window runs out), sampling the supervisor's PSS and RSS from
/// outside every `SAMPLE_EVERY`. Prints the sampled maximum per phase (the
/// phase changes at each notification the server gets) and fails when any
/// observation is above the ceiling or a phase has too few samples.
fn memory_phases(name: &str, e: &Env, args: &[&OsStr], phases_wanted: usize) {
    let (fake, rx) = server(|_, _| {});
    let mut c = e.cmd(Some(&bus_of(&fake)), args);
    c.env("INACIRCLE_UNDO_WAIT_MS", "1000")
        .env("INACIRCLE_MEASURE", "1");
    let samples = Arc::new(Mutex::new(Vec::<(Instant, u64, u64)>::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s2, stop2) = (samples.clone(), stop.clone());
    let r = run_then(c, &Arc::new(Mutex::new(String::new())), move |pid| {
        std::thread::spawn(move || {
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some((p, r)) = pss_rss(pid) {
                    s2.lock().unwrap().push((Instant::now(), p, r));
                }
                std::thread::sleep(SAMPLE_EVERY);
            }
        });
    });
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    fake.join();
    assert_eq!(r.code, Some(0), "{}{}", r.stdout, r.stderr);
    let notices: Vec<Got> = rx.try_iter().collect();
    let samples = samples.lock().unwrap().clone();
    // Phase k: after the k-th notice event and before the next one.
    let mut worst = 0;
    let mut seen_phases = 0;
    for k in 0..=notices.len() {
        let from = if k == 0 {
            None
        } else {
            Some(notices[k - 1].at)
        };
        let to = notices.get(k).map(|g| g.at);
        let inside: Vec<_> = samples
            .iter()
            .filter(|(t, _, _)| from.is_none_or(|f| *t >= f) && to.is_none_or(|e| *t < e))
            .collect();
        let label = match k {
            0 => "run, before any notice".to_string(),
            k => format!("after notice {k} ({:?})", notices[k - 1].summary),
        };
        let pmax = inside.iter().map(|s| s.1).max().unwrap_or(0);
        let rmax = inside.iter().map(|s| s.2).max().unwrap_or(0);
        eprintln!(
            "MEMORY {name}: {label}: samples={} max_pss_B={pmax} max_rss_B={rmax}",
            inside.len()
        );
        if inside.len() >= 5 {
            seen_phases += 1;
        }
        worst = worst.max(pmax);
    }
    // The kernel's own peak RSS of the process (`VmHWM`, kB are KiB), printed
    // by the measurement switch before the notices at the end. RSS is never
    // below PSS, so the peak bounds every transient of the supervisor up to
    // that point, which the sampling cannot see.
    let hwm = r
        .stderr
        .lines()
        .find_map(|l| l.split("vm_hwm=").nth(1))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kib| kib * 1024)
        .unwrap_or_else(|| panic!("{name}: no vm_hwm in {}", r.stderr));
    eprintln!(
        "MEMORY {name}: total samples={} worst_pss_B={worst} vm_hwm_B={hwm} cadence={SAMPLE_EVERY:?}",
        samples.len()
    );
    assert!(
        hwm > 0 && hwm <= PSS_CEILING_BYTES,
        "{name}: peak RSS {hwm} B outside 1..={PSS_CEILING_BYTES} B"
    );
    assert!(worst > 0, "{name}: no PSS observation: incomplete evidence");
    assert!(
        worst <= PSS_CEILING_BYTES,
        "{name}: {worst} B exceeds {PSS_CEILING_BYTES} B"
    );
    assert!(
        seen_phases >= phases_wanted,
        "{name}: only {seen_phases} phases with 5 or more samples, wanted {phases_wanted}"
    );
}

#[test]
fn supervisor_memory_stays_within_the_ceiling_in_every_phase() {
    // One input: conversion, the final notice, the Undo window.
    let e = Env::new("mem-one");
    e.file("a.txt", "hello\n");
    memory_phases(
        "one input",
        &e,
        &["run", "--notify", "markdown", "a.txt"].map(OsStr::new),
        1,
    );

    // 1024 inputs in one run.
    let e = Env::new("mem-many");
    let names: Vec<String> = (0..1024).map(|i| format!("f{i:04}.txt")).collect();
    for n in &names {
        e.file(n, "hello\n");
    }
    let mut args: Vec<&OsStr> = ["run", "--notify", "markdown"].map(OsStr::new).to_vec();
    args.extend(names.iter().map(OsStr::new));
    memory_phases("1024 inputs", &e, &args, 2);

    // A run that outlasts RUNNING_AFTER: the running notice with Cancel
    // is up while the action works, then the final notice and the window.
    let e = Env::new("mem-running");
    let a = e.test_input("a.test", &["sleep_ms=2500", "events=done"]);
    memory_phases(
        "running notice",
        &e,
        &[
            OsStr::new("run"),
            OsStr::new("--notify"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            a.as_os_str(),
        ],
        3,
    );
}
