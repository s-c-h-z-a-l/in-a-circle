//! The wheel's process-level safety, without a compositor: the single
//! instance lock is taken before Wayland is touched.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod fake_compositor;

use fake_compositor::{Compositor, source_writes};

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

#[test]
fn second_instance_exits_at_once() {
    let rt = runtime_dir("second");
    let lock = fs::File::create(rt.join("inacircle/wheel.lock")).unwrap();
    assert!(try_lock(&lock));
    let (code, stdout, stderr, took) = wheel(&rt);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(took < Duration::from_millis(100), "{took:?}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));

    // Without the lock held it goes on to Wayland, which is not there: an
    // error, and the lock is free again afterwards.
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
        let mut c = Command::new(BIN);
        c.env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env("WAYLAND_DISPLAY", "wayland-0")
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_DATA_HOME", self.0.join("home/.local/share"))
            .env("HOME", self.0.join("home"))
            .env("INACIRCLE_ACTIONS_DIR", self.0.join("actions"))
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
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
    rollup
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
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
    let pidfile = root.0.join("hang.pid");
    root.action(
        "zz-hang",
        &format!("echo $$ > {}\nexec sleep 30", pidfile.display()),
    );
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
    let w = c.drag_and_drop(Duration::from_secs(2));
    let t = Instant::now();
    source_writes(w, &list);
    c.wheel_drawn(Duration::from_secs(5));
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "stage 2 took {:?}",
        t.elapsed()
    );
    assert!(!pidfile.exists(), "a manifest ran under the overlay");

    c.close();
    let t = Instant::now();
    let st = wait_exit(&mut child, Duration::from_secs(8));
    assert_eq!(st.code(), Some(0));
    // The refresh ran it, then hit its deadline.
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(t.elapsed() >= Duration::from_secs(1), "{:?}", t.elapsed());
    assert!(dead(pid), "the hanging manifest outlived the wheel");
}

/// A drop of 1024 files stays within the 5 MB peak-RSS budget at stage 2.
/// The budget is for the optimized binary (`cargo test --release`); a
/// debug build, larger by itself, checks that the drop adds little over a
/// one-file drop instead.
#[test]
fn a_large_drop_stays_within_the_memory_budget() {
    let peak = |name: &str, n: usize| -> u64 {
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
        let w = c.drag_and_drop(Duration::from_secs(2));
        source_writes(w, &list);
        c.wheel_drawn(Duration::from_secs(5));
        let kb = pss_kb(child.id());
        c.close();
        assert_eq!(
            wait_exit(&mut child, Duration::from_secs(5)).code(),
            Some(0)
        );
        kb
    };
    // Shared and file-backed pages vary by tens of kB from run to run: the lower of
    // two runs each.
    let one = peak("rss1a", 1).min(peak("rss1b", 1));
    let many = peak("rss1024a", 1024).min(peak("rss1024b", 1024));
    eprintln!("PSS at stage 2: 1 file {one} kB, 1024 files {many} kB");
    if cfg!(debug_assertions) {
        assert!(many <= one + 256, "1 file {one} kB, 1024 files {many} kB");
    } else {
        assert!(many <= 5 * 1024, "1024 files: {many} kB");
    }
}
