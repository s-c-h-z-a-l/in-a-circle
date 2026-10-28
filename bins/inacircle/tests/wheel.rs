//! The wheel's process-level safety, without a compositor: the single
//! instance lock is taken before Wayland is touched.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod fake_compositor;

use fake_compositor::{Compositor, Delay, KEYBOARD, POINTER, Script, source_writes};

const BIN: &str = env!("CARGO_BIN_EXE_inacircle");

/// A private runtime dir for one test.
fn runtime_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("wheel-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("inacircle")).unwrap();
    dir
}

fn wheel(rt: &Path) -> (Option<i32>, String, String, Duration) {
    let t = Instant::now();
    let out = Command::new(BIN)
        .env("XDG_RUNTIME_DIR", rt)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        // Never the developer's session bus: a failed start notifies.
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        t.elapsed(),
    )
}

fn try_lock(f: &fs::File) -> bool {
    // SAFETY: flock on an fd the file owns.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Releases the lock at once. A plain `drop` closes only this process's
/// descriptor: a thread of this test binary that forks (every test spawns
/// children) holds a copy of it until its `exec` closes it, and `flock`
/// belongs to the open file description, so the lock would stay held for
/// that window and the next wheel would see a second instance (a flaky 0
/// where 1 is expected). `LOCK_UN` releases it for every copy.
fn unlock(f: &fs::File) {
    // SAFETY: flock on an fd the file owns.
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) }, 0);
}

#[test]
fn second_instance_exits_at_once() {
    let rt = runtime_dir("second");
    let lock = fs::File::create(rt.join("inacircle/wheel.lock")).unwrap();
    assert!(try_lock(&lock));
    let (code, stdout, stderr, took) = wheel(&rt);
    assert_eq!(code, Some(0), "{stderr}");
    // Far below the startup deadline it would hit if it went on to
    // Wayland; the bound only has to survive a loaded machine.
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));

    // Without the lock held it goes on to Wayland, which is not there: an
    // error, and the lock is free again afterwards.
    unlock(&lock);
    drop(lock);
    let (code, _, stderr, _) = wheel(&rt);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("wheel"), "{stderr}");
    let lock = fs::File::open(rt.join("inacircle/wheel.lock")).unwrap();
    assert!(try_lock(&lock), "lock left held");
}

/// A signal mask from `/proc/<pid>/status` (`SigIgn`, `SigBlk`, ...).
fn sig_field(pid: u32, name: &str) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let v = status
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("no {name} in {status}"));
    u64::from_str_radix(v.trim(), 16).unwrap()
}

/// The alarm is the backstop for hangs outside the event loop, so it must
/// not depend on the parent: a SIGALRM ignored or blocked by whoever
/// started us is reset before the alarm is armed.
#[test]
fn alarm_works_even_if_the_parent_disabled_sigalrm() {
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    const ALRM_BIT: u64 = 1 << (libc::SIGALRM - 1);

    let rt = runtime_dir("alarm");
    // A compositor that accepts and never answers.
    let listener = UnixListener::bind(rt.join("wayland-0")).unwrap();
    let mut cmd = Command::new(BIN);
    cmd.env("XDG_RUNTIME_DIR", &rt)
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the closure runs in the forked child before exec and calls
    // only async-signal-safe functions on its own stack.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGALRM, libc::SIG_IGN);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGALRM);
            libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    let t = Instant::now();
    let mut child = cmd.spawn().unwrap();
    // Connected: past the lock and the alarm, now waiting on us.
    let (_conn, _) = listener.accept().unwrap();
    let pid = child.id();
    assert_eq!(
        sig_field(pid, "SigIgn") & ALRM_BIT,
        0,
        "SIGALRM still ignored"
    );
    assert_eq!(
        sig_field(pid, "SigBlk") & ALRM_BIT,
        0,
        "SIGALRM still blocked"
    );
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(31),
            "wheel outlived its stage"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs the wheel with `wheel.lock` replaced by `plant`, against a fake
/// compositor: it must exit 1 within `limit` without ever connecting.
fn bad_lock(name: &str, plant: impl FnOnce(&Path), limit: Duration) {
    use std::os::unix::net::UnixListener;
    let rt = runtime_dir(name);
    plant(&rt.join("inacircle/wheel.lock"));
    let listener = UnixListener::bind(rt.join("wayland-0")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let t = Instant::now();
    let mut child = Command::new(BIN)
        .env("XDG_RUNTIME_DIR", &rt)
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if t.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name}: still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert_eq!(status.code(), Some(1), "{name}: {stderr}");
    assert!(stderr.contains("lock"), "{name}: {stderr}");
    assert!(listener.accept().is_err(), "{name}: it went on to Wayland");
}

/// A FIFO at the lock path must not hang the wheel in `open`.
#[test]
fn a_fifo_at_the_lock_path_fails_at_once() {
    bad_lock(
        "fifo",
        |p| {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: mkfifo reads the NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        },
        Duration::from_secs(1),
    );
}

/// The lock is never taken through a symlink.
#[test]
fn a_symlink_at_the_lock_path_fails() {
    bad_lock(
        "symlink",
        |p| {
            let target = p.with_file_name("elsewhere");
            fs::write(&target, "").unwrap();
            std::os::unix::fs::symlink(&target, p).unwrap();
        },
        Duration::from_secs(2),
    );
}

/// A private root for a wheel driven by the fake compositor: XDG dirs,
/// an actions dir, and `f/` for dropped files. Short (under `/tmp`), so
/// 1024 file URIs fit in a drop's 64 KiB.
struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Root {
        use std::os::unix::fs::PermissionsExt;
        let root = PathBuf::from(format!("/tmp/iacw-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["runtime", "cache", "state", "home", "actions", "f"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        Root(root)
    }

    fn cmd(&self) -> Command {
        // `INACIRCLE_TEST_BIN` runs another build against the same fake
        // compositor (the A/B hover measurement); never set in the suite.
        let bin = std::env::var("INACIRCLE_TEST_BIN").unwrap_or_else(|_| BIN.to_owned());
        let mut c = Command::new(bin);
        c.env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env("WAYLAND_DISPLAY", "wayland-0")
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_DATA_HOME", self.0.join("home/.local/share"))
            .env("HOME", self.0.join("home"))
            .env("INACIRCLE_ACTIONS_DIR", self.0.join("actions"))
            // Never the developer's session bus; the fallback
            // `<runtime>/bus` does not exist here.
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(std::process::Stdio::null());
        c
    }

    /// An executable script in the actions dir, installed without this
    /// process holding a writable fd on it (a concurrent fork would make
    /// its exec fail with ETXTBSY): `cp` writes it, then a rename.
    fn action(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let src = self.0.join(format!("{name}.src"));
        fs::write(&src, format!("#!/bin/sh\n{body}\n")).unwrap();
        let tmp = self.0.join("actions").join(format!(".{name}.tmp"));
        assert!(
            Command::new("cp")
                .arg(&src)
                .arg(&tmp)
                .status()
                .unwrap()
                .success()
        );
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, self.0.join("actions").join(name)).unwrap();
    }

    /// A plain-text-to-Markdown action, and the catalog cache primed with
    /// it (by `inacircle offer`, outside the wheel).
    fn markdown_action_cached(&self) {
        self.action(
            "md",
            "[ \"$1\" = --manifest ] || exit 1\nprintf 'inacircle-manifest 0\\n'\n\
             printf 'action\\tt.md\\tfamily=t\\tin=text/plain\\tout=text/markdown\\t\
             ext=md\\tlabel=Markdown\\tgroup=g\\tcost=1\\tlossy=no\\tbatch=yes\\n'",
        );
        let f = self.0.join("f/primer.txt");
        fs::write(&f, "words\n").unwrap();
        let out = self.cmd().arg("offer").arg(&f).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("preset=markdown"), "{stdout}");
        // The override's own cache (`catalog-<hash>`), primed by `offer`.
        assert!(
            fs::read_dir(self.0.join("cache/inacircle"))
                .unwrap()
                .any(|d| d
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("catalog-"))
        );
        fs::remove_file(f).unwrap();
    }

    /// Text files `f/<i>.txt` and their URI list.
    fn files(&self, n: usize) -> Vec<u8> {
        let mut list = Vec::new();
        for i in 0..n {
            let p = self.0.join(format!("f/{i}.txt"));
            fs::write(&p, "words\n").unwrap();
            list.extend_from_slice(format!("file://{}\r\n", p.display()).as_bytes());
        }
        list
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The proportional set size (`Pss`) of a live process, in kB: the
/// budget's measure (shared libc and loader pages count by their share).
fn pss_kb(pid: u32) -> u64 {
    rollup_kb(pid, "Pss:")
}

/// The anonymous memory (`Anonymous`) of a live process, in kB: its heap
/// and stacks, which no other process shares, so what a drop adds to it
/// does not depend on how many wheels run beside it.
fn anon_kb(pid: u32) -> u64 {
    rollup_kb(pid, "Anonymous:")
}

fn rollup_kb(pid: u32, field: &str) -> u64 {
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
    rollup
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap()
}

/// Whether `pid` is gone (or a zombie nobody waited for yet).
fn dead(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(s) => s.rsplit(')').next().unwrap().trim_start().starts_with('Z'),
    }
}

/// The pid of the process whose command line is exactly `sleep <secs>`,
/// found by scanning `/proc` (a sandboxed manifest cannot write a pid file).
fn find_sleeper(secs: u32) -> Option<i32> {
    let want = format!("sleep\0{secs}\0");
    fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: i32 = e.file_name().to_str()?.parse().ok()?;
        let cmd = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        (cmd == want.as_bytes() && !dead(pid)).then_some(pid)
    })
}

/// The early path to stage 2: the drag enters, the list is read, the drag
/// moves onto the Document slice (north of the center) and drops; returns
/// once stage 2 is on the wheel.
fn drag_to_stage2(c: &mut Compositor, list: &[u8]) {
    let w = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("the early receive");
    source_writes(w, list);
    // Attached so far: `Drop here`, then the categories.
    c.attached(2, Duration::from_secs(5));
    c.release_held();
    c.drag_motion(960, 390);
    // The hover on the lit Document slice.
    c.attached(3, Duration::from_secs(2));
    c.release_held();
    c.drop_it();
    c.attached(4, Duration::from_secs(5));
}

fn wait_exit(child: &mut std::process::Child, limit: Duration) -> std::process::ExitStatus {
    let t = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if t.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the wheel did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Stage 2 comes from the catalog cache: an action binary the cache does
/// not know, whose `--manifest` hangs, is not run while the overlay is up.
/// Once the wheel closed, the catalog is refreshed in the same process
/// with the usual deadline, and the hanging child is killed, not orphaned.
#[test]
fn a_hanging_manifest_never_runs_under_the_overlay() {
    let root = Root::new("hang");
    root.markdown_action_cached();
    // A unique duration tells this test's child from any other `sleep`.
    let secs = 1000 + std::process::id();
    root.action("zz-hang", &format!("exec sleep {secs}"));
    let list = root.files(1);
    let mut child = None;
    let mut c = Compositor::serve(&root.0.join("runtime"), Duration::from_secs(2), || {
        child = Some(
            root.cmd()
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    });
    let mut child = child.unwrap();
    c.mapped(Duration::from_secs(2));
    let t = Instant::now();
    drag_to_stage2(&mut c, &list);
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "stage 2 took {:?}",
        t.elapsed()
    );
    assert!(
        find_sleeper(secs).is_none(),
        "a manifest ran under the overlay"
    );

    c.close();
    let t = Instant::now();
    // The refresh runs it once the wheel is closed.
    let pid = loop {
        if let Some(pid) = find_sleeper(secs) {
            break pid;
        }
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "the refresh never ran it"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let st = wait_exit(&mut child, Duration::from_secs(8));
    assert_eq!(st.code(), Some(0));
    // The refresh ran it, then hit its deadline.
    assert!(t.elapsed() >= Duration::from_secs(1), "{:?}", t.elapsed());
    assert!(dead(pid), "the hanging manifest outlived the wheel");
}

/// A drop of 1024 files stays within the 5 MB peak-RSS budget at stage 2.
/// The budget is for the optimized binary (`cargo test --release`); a
/// debug build, larger by itself, checks that the drop adds little over a
/// one-file drop instead.
#[test]
fn a_large_drop_stays_within_the_memory_budget() {
    let peak = |name: &str, n: usize| -> (u64, u64) {
        let root = Root::new(name);
        root.markdown_action_cached();
        let list = root.files(n);
        assert!(list.len() <= 64 * 1024, "{} bytes", list.len());
        let mut child = None;
        let mut c = Compositor::serve(&root.0.join("runtime"), Duration::from_secs(2), || {
            child = Some(
                root.cmd()
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        });
        let mut child = child.unwrap();
        c.mapped(Duration::from_secs(2));
        drag_to_stage2(&mut c, &list);
        let kb = (pss_kb(child.id()), anon_kb(child.id()));
        c.close();
        assert_eq!(
            wait_exit(&mut child, Duration::from_secs(5)).code(),
            Some(0)
        );
        kb
    };
    // Shared and file-backed pages vary by tens of kB from run to run: the lower of
    // two runs each.
    let low = |a: (u64, u64), b: (u64, u64)| (a.0.min(b.0), a.1.min(b.1));
    let (one, one_anon) = low(peak("rss1a", 1), peak("rss1b", 1));
    let (many, many_anon) = low(peak("rss1024a", 1024), peak("rss1024b", 1024));
    eprintln!(
        "PSS at stage 2: 1 file {one} kB, 1024 files {many} kB (anonymous {one_anon} / {many_anon} kB)"
    );
    if cfg!(debug_assertions) {
        // Debug builds allocate more per file; the drop's own growth is its
        // anonymous memory (measured 160 kB). PSS is not the measure
        // here: it divides the shared pages among the processes alive at
        // that moment, so beside other tests' wheels the two runs differ by
        // more than the drop adds. The budget itself is the release
        // assertion below.
        assert!(
            many_anon <= one_anon + 512,
            "1 file {one_anon} kB, 1024 files {many_anon} kB anonymous"
        );
    } else {
        assert!(many <= 5 * 1024, "1024 files: {many} kB");
    }
}

/// `wl_data_offer` opcodes the tests look for.
const ACCEPT: u16 = 0;
const RECEIVE: u16 = 1;
const DESTROY: u16 = 2;
const FINISH: u16 = 3;
const SET_ACTIONS: u16 = 4;

/// Starts the wheel on `root` against a fake compositor, mapped.
fn start(root: &Root, measure: bool) -> (Compositor, std::process::Child) {
    let mut child = None;
    let mut c = Compositor::serve(&root.0.join("runtime"), Duration::from_secs(2), || {
        let mut cmd = root.cmd();
        if measure {
            cmd.env("INACIRCLE_MEASURE", "1");
        }
        child = Some(cmd.stderr(std::process::Stdio::piped()).spawn().unwrap());
    });
    c.mapped(Duration::from_secs(2));
    (c, child.unwrap())
}

/// Closes the overlay and checks the wheel exits cleanly; its stderr.
fn finish(mut c: Compositor, mut child: std::process::Child) -> String {
    c.close();
    let st = wait_exit(&mut child, Duration::from_secs(5));
    assert_eq!(st.code(), Some(0));
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    err
}

/// Whether the reader of the pipe whose write end is `w` is gone.
fn reader_gone(w: &std::os::fd::OwnedFd) -> bool {
    let mut p = libc::pollfd {
        fd: w.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, 0) };
    p.revents & libc::POLLERR != 0
}

/// The list is read once, on `enter`; the drop on a lit category is then
/// `finish`ed (copy) and `destroy`ed, and the offer got nothing else.
#[test]
fn early_read_then_drop_finishes_once() {
    let root = Root::new("early");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, false);
    drag_to_stage2(&mut c, &list);
    c.offer_destroyed(0, Duration::from_secs(2));
    let reqs = c.offer_requests(0);
    // Accepts and `set_actions` interleave as the drag moves.
    let ours: Vec<u16> = reqs
        .iter()
        .copied()
        .filter(|r| ![ACCEPT, SET_ACTIONS].contains(r))
        .collect();
    assert_eq!(ours, [RECEIVE, FINISH, DESTROY], "{reqs:?}");
    assert_eq!(reqs[0], RECEIVE, "the receive comes at the enter: {reqs:?}");
    finish(c, child);
}

/// A source that never answers the early read: after its 500 ms the wheel
/// stays `Drop here`, and the drop is read the old way, once more.
#[test]
fn silent_early_source_falls_back_to_drop_here() {
    let root = Root::new("silent");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, true);
    // The pipe stays open and silent.
    let silent = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("early receive");
    c.pump(Duration::from_millis(900));
    assert_eq!(c.attaches(), 1, "still Drop here");
    assert!(reader_gone(&silent), "the early read timed out and closed");
    c.drop_it();
    let w = c
        .next_receive(0, Duration::from_secs(2))
        .expect("the drop's own read");
    source_writes(w, &list);
    // The categories, after the read.
    c.attached(2, Duration::from_secs(5));
    c.offer_destroyed(0, Duration::from_secs(2));
    let ours: Vec<u16> = c
        .offer_requests(0)
        .into_iter()
        .filter(|r| ![ACCEPT, SET_ACTIONS].contains(r))
        .collect();
    assert_eq!(ours, [RECEIVE, RECEIVE, FINISH, DESTROY]);
    let err = finish(c, child);
    assert!(err.contains(" idle_wakeups=0 "), "{err}");
}

/// A second enter (another file is brought) closes the first early pipe
/// and retires the first offer; only the new list counts.
#[test]
fn reenter_closes_the_early_pipe() {
    let root = Root::new("reenter");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, false);
    let first = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("first receive");
    assert!(!reader_gone(&first));
    let second = c
        .drag_enters(1, Duration::from_secs(2))
        .expect("second receive");
    assert!(reader_gone(&first), "the first early pipe is still open");
    c.offer_destroyed(0, Duration::from_secs(2));
    assert_eq!(c.offer_requests(0).last(), Some(&DESTROY));
    source_writes(second, &list);
    c.attached(2, Duration::from_secs(5));
    finish(c, child);
}

/// The drag leaves without a drop (Esc, or out of the output): the early
/// pipe closes and the wheel exits cleanly.
#[test]
fn a_leave_closes_the_early_pipe_and_the_wheel() {
    let root = Root::new("leave");
    root.markdown_action_cached();
    let (mut c, mut child) = start(&root, false);
    let early = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("early receive");
    c.drag_leaves();
    // The wheel closes; its connection takes the offer with it.
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(5)).code(),
        Some(0)
    );
    assert!(reader_gone(&early));
}

/// Level 1 from the early list within the per-point CPU budget (release
/// builds; a debug build only has to report it).
#[test]
fn level1_from_the_early_list_within_budget() {
    let root = Root::new("level1");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, true);
    drag_to_stage2(&mut c, &list);
    let err = finish(c, child);
    let us: i64 = err
        .split_whitespace()
        .find_map(|w| w.strip_prefix("list_to_level1_cpu_us="))
        .unwrap_or_else(|| panic!("no list_to_level1_cpu_us in {err:?}"))
        .parse()
        .unwrap();
    eprintln!("list_to_level1_cpu_us={us}");
    assert!(us >= 0, "never measured: {err}");
    // The phases report CPU and wall separately; the queued commit and the
    // flush that sent it are two points, in that order.
    let get = |k: &str| -> i64 {
        err.split_whitespace()
            .find_map(|w| w.strip_prefix(k)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("no {k} in {err:?}"))
            .parse()
            .unwrap()
    };
    assert!(get("list_abs_cpu_us") >= get("start_abs_cpu_us"), "{err}");
    assert!(get("list_abs_wall_us") >= get("start_abs_wall_us"), "{err}");
    assert!(get("drop_abs_cpu_us") >= get("list_abs_cpu_us"), "{err}");
    assert!(get("drop_abs_wall_us") >= get("list_abs_wall_us"), "{err}");
    assert!(
        get("main_to_flush_cpu_us") >= get("main_to_commit_cpu_us"),
        "{err}"
    );
    assert!(
        get("main_to_flush_wall_us") >= get("main_to_commit_wall_us"),
        "{err}"
    );
    // Closed without a choice: no choice marker, not a zero.
    assert!(!err.contains("choice_abs"), "{err}");
    if !cfg!(debug_assertions) {
        assert!(us <= 1000, "{us} us");
    }
}

// Bounded startup and teardown (hardening task 11). The fake compositor's
// script decides what arrives, and when; the wheel has no bus to notify on
// (`Root::cmd` removes the variable), so these see the exit code, the
// timing and the log line. The notification itself is in `notify.rs`.

/// The proposed `ui_startup_deadline` (parameters.g0p.json), and the pair
/// of delays the preflight measured around it.
const STARTUP: Duration = Duration::from_millis(2000);
const BELOW_STARTUP: Duration = Duration::from_millis(1900);
const ABOVE_STARTUP: Duration = Duration::from_millis(2100);
/// `ui_teardown_deadline` and its pair.
const TEARDOWN: Duration = Duration::from_millis(1000);
const BELOW_TEARDOWN: Duration = Duration::from_millis(900);
const ABOVE_TEARDOWN: Duration = Duration::from_millis(1100);
/// How long a test waits for the wheel before it fails the test: a hung
/// wheel must fail, never hang the suite. Well past every deadline above.
const PATIENCE: Duration = Duration::from_secs(8);

/// The wheel against a fake compositor that behaves as `script` says.
fn start_with(root: &Root, script: Script) -> (Compositor, std::process::Child) {
    let mut child = None;
    let c = Compositor::serve_with(
        &root.0.join("runtime"),
        Duration::from_secs(2),
        script,
        || {
            child = Some(
                root.cmd()
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        },
    );
    (c, child.unwrap())
}

/// Serves the compositor until the wheel hangs up, then reaps it: exit
/// code, its stderr, and how long it ran.
fn until_gone(
    c: &mut Compositor,
    mut child: std::process::Child,
    t0: Instant,
) -> (Option<i32>, String, Duration) {
    if c.hangs_up(PATIENCE).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the wheel did not hang up within {PATIENCE:?}");
    }
    let st = wait_exit(&mut child, Duration::from_secs(5));
    let took = t0.elapsed();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    (st.code(), err, took)
}

/// A mapped overlay whose seat has `caps` and the devices they imply.
fn seat_up(c: &mut Compositor, caps: u32) {
    c.seat_bound(Duration::from_secs(2));
    c.seat_caps(caps);
    c.seat_devices(caps, Duration::from_secs(2));
}

#[test]
fn startup_without_a_configure_fails_at_the_startup_deadline() {
    let root = Root::new("startup-noconf");
    let t0 = Instant::now();
    let script = Script {
        configure: Delay::Never,
        ..Script::default()
    };
    let (mut c, child) = start_with(&root, script);
    let (code, err, took) = until_gone(&mut c, child, t0);
    eprintln!("PREFLIGHT startup no-configure: exit={code:?} took={took:?} stderr={err:?}");
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("did not configure"), "{err}");
    assert!(took >= STARTUP - Duration::from_millis(100), "{took:?}");
    assert!(took < STARTUP + Duration::from_millis(1500), "{took:?}");
}

/// The pair around the startup deadline: a configure 100 ms before it still
/// opens the wheel, one 100 ms after it does not.
#[test]
fn startup_configure_below_the_deadline_still_opens() {
    let root = Root::new("startup-below");
    root.markdown_action_cached();
    let script = Script {
        configure: Delay::After(BELOW_STARTUP),
        ..Script::default()
    };
    let t0 = Instant::now();
    let (mut c, mut child) = start_with(&root, script);
    c.mapped(Duration::from_secs(4));
    // The startup is over once the wheel is on the screen; the drag gives
    // the position.
    let _early = c.drag_enters(0, Duration::from_secs(2));
    c.attached(1, Duration::from_secs(4));
    eprintln!(
        "PREFLIGHT startup configure@{BELOW_STARTUP:?}: first wheel commit at {:?}",
        t0.elapsed()
    );
    c.close();
    let st = wait_exit(&mut child, Duration::from_secs(5));
    assert_eq!(st.code(), Some(0));
}

#[test]
fn startup_configure_above_the_deadline_fails() {
    let root = Root::new("startup-above");
    let script = Script {
        configure: Delay::After(ABOVE_STARTUP),
        ..Script::default()
    };
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, script);
    let (code, err, took) = until_gone(&mut c, child, t0);
    eprintln!(
        "PREFLIGHT startup configure@{ABOVE_STARTUP:?}: exit={code:?} took={took:?} stderr={err:?}"
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(
        took < ABOVE_STARTUP,
        "gave up only after the late configure: {took:?}"
    );
}

/// The wheel is configured but no position ever comes, and the seat has no
/// pointer: that is the named cause, at the startup deadline (not the
/// 30 s the stage used to take, and not a silent exit 0).
#[test]
fn startup_without_a_pointer_names_the_pointer() {
    let root = Root::new("startup-nopointer");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, KEYBOARD);
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("pointer position was unavailable"), "{err}");
    assert!(took >= STARTUP - Duration::from_millis(100), "{took:?}");
    assert!(took < STARTUP + Duration::from_millis(1500), "{took:?}");
}

/// With a pointer present the missing position has no proven cause: the
/// pointer is not blamed.
#[test]
fn startup_without_a_position_does_not_blame_the_pointer() {
    let root = Root::new("startup-nopos");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!(code, Some(1), "{err}");
    assert!(!err.contains("pointer position was unavailable"), "{err}");
    assert!(took < STARTUP + Duration::from_millis(1500), "{took:?}");
}

/// A seat without a keyboard cannot take Esc, but the capability may come
/// late: the wheel waits for it until the startup deadline, then fails
/// with the proven cause.
#[test]
fn startup_without_a_keyboard_fails_at_the_startup_deadline() {
    let root = Root::new("startup-nokbd");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER);
    c.pointer_enters();
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("keyboard was unavailable"), "{err}");
    assert!(took >= STARTUP - Duration::from_millis(100), "{took:?}");
    assert!(took < STARTUP + Duration::from_millis(1500), "{took:?}");
}

/// The keyboard capability arrives after the first (pointer-only) one,
/// inside the deadline: the wheel opens, and Esc closes it.
#[test]
fn startup_keyboard_arriving_late_within_the_deadline_still_opens() {
    let root = Root::new("startup-latekbd");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER);
    c.pointer_enters();
    c.pump(Duration::from_millis(300));
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_devices(POINTER | KEYBOARD, Duration::from_secs(2));
    c.attached(1, Duration::from_secs(4));
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
}

/// The keyboard goes away after the wheel is on the screen: Esc could no
/// longer close it, so it closes in order at once. Not a failure.
#[test]
fn keyboard_lost_after_the_first_commit_closes_in_order() {
    let root = Root::new("startup-lostkbd");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.pointer_enters();
    c.attached(1, Duration::from_secs(4));
    c.seat_caps(POINTER);
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(took < STARTUP, "{took:?}");
    assert!(
        c.layer_requests().contains(&7),
        "layer surface not destroyed"
    );
}

/// The pointer enters before the configure: the position waits for the
/// size, and the wheel opens.
#[test]
fn startup_pointer_before_the_configure_still_opens() {
    let root = Root::new("startup-reorder");
    let script = Script {
        configure: Delay::After(Duration::from_millis(300)),
        ..Script::default()
    };
    let (mut c, mut child) = start_with(&root, script);
    seat_up(&mut c, POINTER | KEYBOARD);
    c.pointer_enters();
    c.mapped(Duration::from_secs(4));
    c.attached(1, Duration::from_secs(4));
    c.key(wheel_esc());
    let st = wait_exit(&mut child, Duration::from_secs(5));
    assert_eq!(st.code(), Some(0));
}

fn wheel_esc() -> u32 {
    1
}

/// Esc between the configure and the first position is a clean cancel:
/// exit 0, nothing logged.
#[test]
fn startup_esc_before_the_first_commit_cancels_cleanly() {
    let root = Root::new("startup-esc");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(wheel_esc());
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(took < STARTUP, "{took:?}");
}

/// Esc once the wheel is on the screen is the same clean cancel.
#[test]
fn startup_esc_after_the_first_commit_cancels_cleanly() {
    let root = Root::new("startup-esc2");
    root.markdown_action_cached();
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    let _early = c.drag_enters(0, Duration::from_secs(2));
    c.attached(1, Duration::from_secs(4));
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
}

/// The compositor drops the connection during startup: a failure, at once.
#[test]
fn startup_compositor_hangup_fails_at_once() {
    let root = Root::new("startup-hangup");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    c.disconnect();
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("wheel:"), "{err}");
    assert!(took < STARTUP, "{took:?}");
}

/// Orderly teardown, below the deadline: the layer surface (the keyboard
/// grab) is destroyed first, and the wheel waits for the compositor's
/// answer to its `sync` before it closes the connection.
#[test]
fn startup_teardown_waits_for_the_ack_below_the_deadline() {
    let root = Root::new("startup-td-below");
    let script = Script {
        teardown_ack: Delay::After(BELOW_TEARDOWN),
        ..Script::default()
    };
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, script);
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(c.teardown_acked(), "the wheel left before the ack");
    assert!(c.layer_requests().contains(&7), "no layer surface destroy");
    let held = c.since_destroy().expect("destroyed");
    eprintln!(
        "PREFLIGHT teardown ack@{BELOW_TEARDOWN:?}: client left {held:?} after the layer destroy, acked={}",
        c.teardown_acked()
    );
    assert!(held >= BELOW_TEARDOWN, "{held:?}");
    assert!(held < TEARDOWN + Duration::from_millis(500), "{held:?}");
}

/// Above the deadline the wheel gives up on the answer and closes.
#[test]
fn startup_teardown_gives_up_above_the_deadline() {
    let root = Root::new("startup-td-above");
    let script = Script {
        teardown_ack: Delay::After(ABOVE_TEARDOWN),
        ..Script::default()
    };
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, script);
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(!c.teardown_acked(), "it waited past the deadline");
    let held = c.since_destroy().expect("destroyed");
    eprintln!(
        "PREFLIGHT teardown ack@{ABOVE_TEARDOWN:?}: client left {held:?} after the layer destroy, acked={}",
        c.teardown_acked()
    );
    assert!(held >= TEARDOWN - Duration::from_millis(100), "{held:?}");
    assert!(held < ABOVE_TEARDOWN, "{held:?}");
}

// Paint trace (hardening task 12). The wheel's measurement line names every
// paint: phase, buffer, wheel generation (`S`/`T` startup buffers 0/1, `F`
// full and `H` hover paint inside a render, `P` prepaint; `!` marks a paint
// onto a buffer the compositor still held).

/// The paint trace of a finished run's stderr.
fn trace(err: &str) -> Vec<String> {
    err.split_whitespace()
        .find_map(|w| w.strip_prefix("paint_trace="))
        .unwrap_or_else(|| panic!("no paint_trace in {err:?}"))
        .split(',')
        .map(str::to_owned)
        .collect()
}

/// A run of the wheel with no drag: the pointer opens the first wheel.
fn no_drag_run(name: &str) -> String {
    let root = Root::new(name);
    let mut child = None;
    let mut c = Compositor::serve(&root.0.join("runtime"), Duration::from_secs(2), || {
        child = Some(
            root.cmd()
                .env("INACIRCLE_MEASURE", "1")
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    });
    c.mapped(Duration::from_secs(2));
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_devices(POINTER | KEYBOARD, Duration::from_secs(2));
    c.pointer_enters();
    c.attached(1, Duration::from_secs(4));
    // The frame passes; whatever the wheel paints ahead of time is done.
    c.release_held();
    c.pump(Duration::from_millis(100));
    finish(c, child.unwrap())
}

/// With no drag the pointer replaces the `Drop here` wheel at once, so the
/// buffers painted for it while the compositor configured are obsolete:
/// before the first commit the wheel paints only what it shows (the
/// buffer-0 paint during the configure wait costs no waiting time and
/// cannot know what comes), and the second buffer is painted after the
/// commit, for the generation on screen.
#[test]
fn render_no_drag_startup_paints_no_obsolete_second_buffer() {
    let err = no_drag_run("render-nodrag");
    assert_eq!(trace(&err), ["S0g0", "F0g1", "P1g1"], "{err}");
    assert!(err.contains(" busy_paints=0 "), "{err}");
}

/// A drag whose list arrives: the second buffer is first painted for a
/// wheel that is shown (never the startup wheel nobody will see), and no
/// paint ever lands on a buffer the compositor holds.
#[test]
fn render_drag_to_stage2_paints_no_unused_buffer_and_never_a_busy_one() {
    let root = Root::new("render-drag");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, true);
    drag_to_stage2(&mut c, &list);
    let err = finish(c, child);
    let t = trace(&err);
    assert!(!t.iter().any(|p| p.starts_with("T1")), "{t:?}");
    // While the list is on its way the wheel is about to be replaced: the
    // free buffer is not made ready for the `Drop here` wheel (g0).
    assert!(!t.contains(&"P1g0".to_owned()), "{t:?}");
    assert!(err.contains(" busy_paints=0 "), "{err}");
    // A buffer's generation never goes back, and the stage swaps are in
    // order: Drop here (0), the categories (1), the targets (2).
    let gens: Vec<u32> = t
        .iter()
        .map(|p| {
            p[p.find('g').unwrap() + 1..]
                .trim_end_matches('!')
                .parse()
                .unwrap()
        })
        .collect();
    assert!(gens.windows(2).all(|w| w[0] <= w[1]), "{t:?}");
    assert_eq!(gens.last(), Some(&2), "{t:?}");
}

/// Both buffers held by the compositor: nothing is painted or attached
/// until the release (the compositor owns the pixels meanwhile), and the
/// paint after it lands on the released buffer.
#[test]
fn render_delayed_release_never_paints_a_held_buffer() {
    let root = Root::new("render-held");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, true);
    let w = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("the early receive");
    source_writes(w, &list);
    // `Drop here`, then the categories: both buffers are in use now.
    c.attached(2, Duration::from_secs(5));
    // The drag moves onto a slice before any release: the repaint waits.
    c.drag_motion(960, 390);
    c.pump(Duration::from_millis(150));
    assert_eq!(c.attaches(), 2, "painted onto a held buffer");
    c.release_held();
    c.attached(3, Duration::from_secs(2));
    c.drop_it();
    c.release_held();
    c.attached(4, Duration::from_secs(5));
    let err = finish(c, child);
    assert!(err.contains(" busy_paints=0 "), "{err}");
}

/// The hover series of the A/B measurement (`#[ignore]`: it measures, it
/// does not assert a budget). Stage 2 with two lit targets, then
/// `HOVER_EVENTS` pointer motions cycling Markdown, Plain text and the hub,
/// so every repaint changes the lit slice. One summary line on stdout:
/// `HOVER exit=.. misses=.. <the wheel's measurement line>`.
#[test]
#[ignore]
fn probe_hover_series() {
    let root = Root::new("hoverseries");
    root.markdown_action_cached();
    root.action(
        "pt",
        "[ \"$1\" = --manifest ] || exit 1\nprintf 'inacircle-manifest 0\\n'\n\
         printf 'action\\tt.txt\\tfamily=t\\tin=text/markdown\\tout=text/plain\\t\
         ext=txt\\tlabel=Plain\\tgroup=g\\tcost=1\\tlossy=yes\\tbatch=yes\\n'",
    );
    // Refresh the cache with both actions.
    let f = root.0.join("f/primer.txt");
    fs::write(&f, "words\n").unwrap();
    assert!(
        root.cmd()
            .arg("offer")
            .arg(&f)
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::remove_file(&f).unwrap();
    let a = root.0.join("f/a.txt");
    let b = root.0.join("f/b.md");
    fs::write(&a, "words\n").unwrap();
    fs::write(&b, "# words\n").unwrap();
    let list = format!("file://{}\r\nfile://{}\r\n", a.display(), b.display()).into_bytes();
    let (mut c, child) = start(&root, true);
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_devices(POINTER | KEYBOARD, Duration::from_secs(2));
    // `HOVER_SETTLE_MS` (diagnostic only, default 0): after every release
    // the fake waits this long before its next event, so the wheel can
    // prepaint the released buffer; 0 delivers the release and the next
    // event together, the worst case of the series.
    let settle = Duration::from_millis(
        std::env::var("HOVER_SETTLE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
    let w = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("the early receive");
    source_writes(w, &list);
    c.attached(2, Duration::from_secs(5));
    c.release_held();
    c.pump(settle);
    c.drag_motion(960, 390);
    c.attached(3, Duration::from_secs(2));
    c.release_held();
    c.pump(settle);
    c.drop_it();
    c.attached(4, Duration::from_secs(5));
    c.release_held();
    c.pump(settle);
    c.pointer_enters();
    // The frame passes: the compositor releases, the wheel prepaints.
    c.pump(Duration::from_millis(200));
    c.release_held();
    let n: usize = std::env::var("HOVER_EVENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let pts = [(960u32, 390u32), (960, 690), (960, 540)];
    let mut misses = 0;
    for k in 0..n {
        let before = c.attaches();
        let (x, y) = pts[k % pts.len()];
        c.pointer_motion(x, y);
        let t = Instant::now();
        while c.attaches() == before && t.elapsed() < Duration::from_millis(100) {
            c.pump(Duration::from_millis(2));
        }
        misses += usize::from(c.attaches() == before);
        c.release_held();
        c.pump(Duration::from_millis(2));
    }
    let attaches = c.attaches();
    let err = finish(c, child);
    let line = err
        .lines()
        .find(|l| l.contains("measure wheel"))
        .unwrap_or(&err);
    println!("HOVER misses={misses} attaches={attaches} {line}");
}

/// Whether the wheel with `SIGALRM` pending at `secs` of stopped time is
/// killed by it: the exit status of a wheel stopped right after it
/// connected, continued after `secs`.
fn stopped_wheel_after(name: &str, secs: u64) -> std::process::ExitStatus {
    use std::os::unix::net::UnixListener;
    let rt = runtime_dir(name);
    // A compositor that accepts and never answers.
    let listener = UnixListener::bind(rt.join("wayland-0")).unwrap();
    let mut child = Command::new(BIN)
        .env("XDG_RUNTIME_DIR", &rt)
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Connected: past the lock and the alarm.
    let (_conn, _) = listener.accept().unwrap();
    let pid = child.id() as i32;
    // SAFETY: signals to our own child.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    // A stopped process runs no code, so only the kernel's timer can end it
    // (the soft deadlines are not evaluated): the alarm stays pending and
    // acts when the process continues.
    std::thread::sleep(Duration::from_secs(secs));
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    wait_exit(&mut child, Duration::from_secs(5))
}

/// The hard backstop of the startup is `STARTUP_BACKSTOP` (5 s: the soft
/// path takes at most `STARTUP` + `TEARDOWN`, 3 s), not the user's 30 s: a
/// startup stuck where no deadline reaches must not hold the grab for the
/// user's stage.
#[test]
fn startup_alarm_backstop_kills_a_stuck_startup_after_five_seconds() {
    use std::os::unix::process::ExitStatusExt;
    let st = stopped_wheel_after("alarm5", 6);
    assert_eq!(st.signal(), Some(libc::SIGALRM), "{st:?}");
}
