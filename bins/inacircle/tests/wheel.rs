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
