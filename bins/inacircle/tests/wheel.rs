//! The wheel's process-level safety, without a compositor: the single
//! instance lock is taken before Wayland is touched.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
            t.elapsed() < Duration::from_secs(11),
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
