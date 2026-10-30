//! The wheel's process-level safety, without a compositor: the single
//! instance lock is taken before Wayland is touched.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod fake_compositor;
mod isolate;

use fake_compositor::{Compositor, Delay, KEYBOARD, POINTER, Script, source_writes};
use isolate::isolate;

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
    // Never the developer's session bus (`isolate`): a failed start
    // notifies.
    let out = isolate(&mut Command::new(BIN), rt)
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
    isolate(&mut cmd, &rt)
        .env("XDG_RUNTIME_DIR", &rt)
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
    let mut child = isolate(&mut Command::new(BIN), &rt)
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
/// 1024 file URIs make a short list, well under the byte limit.
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
        // Never the developer's session bus; the fallback `<runtime>/bus`
        // does not exist here.
        isolate(&mut c, &self.0)
            .env("WAYLAND_DISPLAY", "wayland-0")
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
    rollup_kb(pid, "Pss:")
}

/// The peak resident set (`VmHWM`) of a live process, in kB.
fn vm_hwm_kb(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap()
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
    drag_to_category(c, list, DOCUMENT);
}

/// Like [`drag_to_stage2`], onto the category slice at `at`.
fn drag_to_category(c: &mut Compositor, list: &[u8], at: (u32, u32)) {
    let w = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("the early receive");
    source_writes(w, list);
    // Attached so far: `Drop here`, then the categories.
    c.attached(2, Duration::from_secs(5));
    c.release_held();
    c.drag_motion(at.0, at.1);
    // The hover on the lit category slice.
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

/// The real action binaries live next to the wheel's; build them once.
fn action_bin(name: &str) -> PathBuf {
    use std::sync::Once;
    static BUILT: Once = Once::new();
    let dir = Path::new(BIN).parent().unwrap();
    BUILT.call_once(|| {
        let mut c = Command::new(env!("CARGO"));
        c.args([
            "build",
            "-q",
            "-p",
            "inacircle-text",
            "-p",
            "inacircle-pages",
        ]);
        if dir.file_name() == Some(std::ffi::OsStr::new("release")) {
            c.arg("--release");
        }
        c.arg("--target-dir").arg(dir.parent().unwrap());
        assert!(c.status().expect("cannot run cargo").success());
    });
    let p = dir.join(name);
    assert!(p.is_file(), "{} not built", p.display());
    p
}

impl Root {
    /// Links the real action binary `name` into the actions dir and primes
    /// the catalog cache with it by `inacircle offer sample` (outside the
    /// wheel), checking that the offer names `preset`.
    fn real_action_cached(&self, name: &str, sample: (&str, &[u8]), preset: &str) {
        std::os::unix::fs::symlink(action_bin(name), self.0.join("actions").join(name)).unwrap();
        let f = self.0.join("f").join(sample.0);
        fs::write(&f, sample.1).unwrap();
        let out = self.cmd().arg("offer").arg(&f).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(&format!("preset={preset}")), "{stdout}");
        fs::remove_file(f).unwrap();
    }
}

/// Starts the wheel with a pointer over it, drags `list` onto the category
/// at `category`, moves onto the target slice at `target`, clicks the left
/// button and waits for the wheel to exit; its exit code.
fn choose_on_the_wheel(
    root: &Root,
    list: &[u8],
    category: (u32, u32),
    target: (u32, u32),
) -> Option<i32> {
    let (mut c, mut child) = start(root, false);
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_devices(POINTER | KEYBOARD, Duration::from_secs(2));
    c.pointer_enters();
    drag_to_category(&mut c, list, category);
    // Past the guard against the second half of a double click
    // (`SWAP_GUARD`, 200 ms): 300 ms leaves 100 ms for a loaded machine.
    c.pump(Duration::from_millis(300));
    c.release_held();
    c.pointer_motion(target.0, target.1);
    // 50 ms: the wheel handles the motion before the button arrives.
    c.pump(Duration::from_millis(50));
    // BTN_LEFT (evdev 0x110), the only button that chooses.
    c.pointer_button(0x110);
    // The wheel closes and the supervisor runs the action; the compositor
    // only has to answer until the wheel hangs up. 20 s: the sandboxed
    // action of a tiny fixture takes well under a second even in a debug
    // build; past 20 s it is hung, and the test fails instead of waiting.
    let t = Instant::now();
    let hung = c.hangs_up(Duration::from_secs(20));
    assert!(hung.is_ok(), "the wheel never closed after the click");
    let st = wait_exit(&mut child, Duration::from_secs(20));
    eprintln!("choice to exit: {:?}", t.elapsed());
    st.code()
}

/// Real `inacircle-pages` on the wheel: a dropped `.cbz` lights Book (east),
/// its PDF slice (north) is clicked, and the PDF is written.
#[test]
fn wheel_offers_and_runs_cbz_to_pdf() {
    let root = Root::new("cbz");
    let rows: Vec<u8> = (0..8).flat_map(|_| [0u8; 9]).collect();
    let page = testzip::png(8, 8, 0, None, false, false, &rows);
    let cbz = testzip::zip(&[
        (b"p2.png", &page, testzip::Store::Stored),
        (b"p1.png", &page, testzip::Store::Deflated),
    ]);
    root.real_action_cached("inacircle-pages", ("primer.cbz", &cbz), "pdf");
    let src = root.0.join("f/képregény.cbz");
    fs::write(&src, &cbz).unwrap();
    let list = format!("file://{}\r\n", src.display()).into_bytes();
    // Book is east of the center, PDF is its north slice.
    let code = choose_on_the_wheel(&root, &list, (1110, 540), (960, 390));
    assert_eq!(code, Some(0));
    let pdf = fs::read(root.0.join("f/képregény/képregény.pdf")).expect("no PDF written");
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(fs::read(&src).unwrap(), cbz, "the source changed");
}

/// Real `inacircle-pages` on the wheel: three images and a text file are
/// dropped, Book (east) lights, the PDF slice (north) is clicked, and one
/// PDF of the three images appears; the hub counts `3 / 4 files`.
#[test]
fn wheel_offers_and_runs_images_to_pdf() {
    let root = Root::new("img2pdf");
    let rows: Vec<u8> = (0..8).flat_map(|_| [0u8; 9]).collect();
    let png = testzip::png(8, 8, 0, None, false, false, &rows);
    let jpg_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/probe/tests/data/rgb.jpg");
    let jpg = fs::read(&jpg_path).unwrap();
    root.real_action_cached("inacircle-pages", ("primer.png", &png), "pdf");
    let dir = root.0.join("f/Nyaralás");
    fs::create_dir_all(&dir).unwrap();
    let mut list = Vec::new();
    for (name, data) in [
        ("1.jpg", &jpg),
        ("2.png", &png),
        ("3.png", &png),
        ("jegyzet.txt", &b"nem kep\n".to_vec()),
    ] {
        let p = dir.join(name);
        fs::write(&p, data).unwrap();
        list.extend_from_slice(format!("file://{}\r\n", p.display()).as_bytes());
    }
    // The hub text the wheel shows for this set (the same classification
    // and count as the drop's): 3 of the 4 files apply to the PDF slice.
    let offer = root
        .cmd()
        .arg("offer")
        .args(["1.jpg", "2.png", "3.png", "jegyzet.txt"].map(|n| dir.join(n)))
        .output()
        .unwrap();
    let offer = String::from_utf8_lossy(&offer.stdout).into_owned();
    assert!(
        offer.contains("preset=pdf") && offer.contains("hub\t3 / 4 files"),
        "{offer}"
    );
    // Book is east of the center, PDF is its north slice.
    let code = choose_on_the_wheel(&root, &list, (1110, 540), (960, 390));
    assert_eq!(code, Some(0));
    let pdf = fs::read(dir.join("Nyaralás/Nyaralás.pdf")).expect("no PDF written");
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(
        pdf.windows(12).filter(|w| w == b"/Type /Page ").count(),
        3,
        "one page per image"
    );
    assert_eq!(
        fs::read(dir.join("1.jpg")).unwrap(),
        jpg,
        "the source changed"
    );
}

/// The same path with `inacircle-text`: Document (north), Markdown (north).
/// Until now no test ran an action chosen on the wheel.
#[test]
fn wheel_offers_and_runs_txt_to_md() {
    let root = Root::new("txt2md");
    root.real_action_cached("inacircle-text", ("primer.txt", b"words\n"), "markdown");
    let src = root.0.join("f/jegyzet.txt");
    fs::write(&src, "Hello *world*\n").unwrap();
    let list = format!("file://{}\r\n", src.display()).into_bytes();
    let code = choose_on_the_wheel(&root, &list, (960, 390), (960, 390));
    assert_eq!(code, Some(0));
    assert_eq!(
        fs::read_to_string(root.0.join("f/jegyzet/jegyzet.md")).expect("no Markdown written"),
        "Hello \\*world\\*\n"
    );
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

/// The engine memory ceiling (docs/benchmarks/acceptance.md): 5 MB PSS,
/// 5,000,000 bytes. `smaps_rollup` reports kB that are KiB, so a sample is
/// multiplied by 1024 before it is compared.
const PSS_CEILING_BYTES: u64 = 5_000_000;

/// The gate over every PSS observation (`label`, KiB): each one must be a
/// real measurement (non-zero) and at or below the ceiling. No sample is
/// dropped, no minimum is taken, and an empty list is missing evidence,
/// not a pass.
fn pss_gate(samples: &[(&str, u64)]) -> Result<(), String> {
    if samples.is_empty() {
        return Err("no PSS observation: incomplete evidence".into());
    }
    for &(label, kib) in samples {
        if kib == 0 {
            return Err(format!("{label}: PSS 0 kB is not a measurement"));
        }
        if kib * 1024 > PSS_CEILING_BYTES {
            return Err(format!(
                "{label}: {kib} KiB = {} B exceeds {PSS_CEILING_BYTES} B",
                kib * 1024
            ));
        }
    }
    Ok(())
}

#[test]
fn the_pss_gate_checks_every_sample_in_bytes() {
    // 4882 KiB = 4,999,168 B passes; 4883 KiB = 5,000,192 B is over 5 MB
    // although it is under the old 5120 KiB smoke limit.
    let ok = [("a", 4882), ("b", 4882), ("c", 4882), ("d", 4882)];
    assert_eq!(pss_gate(&ok), Ok(()));
    // One bad sample among four fails, even when the others are low: the
    // gate is not the minimum of two runs.
    let one_over = [("1a", 2500), ("1b", 2500), ("1024a", 4883), ("1024b", 2500)];
    let err = pss_gate(&one_over).unwrap_err();
    assert!(err.starts_with("1024a:"), "{err}");
    // The worst of the four decides, wherever it sits.
    assert!(pss_gate(&[("x", 6000), ("y", 100)]).is_err());
    assert!(pss_gate(&[("x", 100), ("y", 6000)]).is_err());
}

#[test]
fn the_pss_gate_rejects_missing_or_zero_evidence() {
    assert!(pss_gate(&[]).is_err());
    assert!(pss_gate(&[("a", 2500), ("b", 0)]).is_err());
}

/// The largest list the wheel accepts stays within the 5 MB PSS ceiling
/// at every phase up to the choice: on the early path through stage 2,
/// and on the fallback (no early answer, the drop's own read) through
/// level 1, where its bytes become paths. Two one-file drops are the
/// baseline. Every observation (each run's sampled peak per phase and its
/// PSS where it stops) is kept and must pass `pss_gate`: no minimum, no
/// 5120 KiB smoke limit. The budget is for the optimized binary (`cargo
/// test --release`); a debug build, larger by itself, checks instead that
/// the largest drop adds about its paths once over a one-file drop.
#[test]
fn the_largest_drop_stays_within_the_memory_budget() {
    // `late`: the early read is left unanswered and the list comes with
    // the drop; the run ends at level 1. Else it ends at stage 2.
    let peak = |name: &str, largest: bool, late: bool| -> (Vec<(String, u64)>, u64) {
        let root = Root::new(name);
        root.markdown_action_cached();
        let list = if largest {
            root.largest_list().1
        } else {
            root.files(1)
        };
        let (mut c, mut child) = start(&root, false);
        let pid = child.id();
        let sampler = PssSampler::start(pid);
        let label = format!("{name} ({} B)", list.len());
        let mut obs = Vec::new();
        if late {
            let silent = c
                .drag_enters(0, Duration::from_secs(2))
                .expect("early receive");
            // Past the early read's 500 ms deadline, so the drop reads.
            c.pump(Duration::from_millis(900));
            assert!(reader_gone(&silent));
            c.drop_it();
            let w = c
                .next_receive(0, Duration::from_secs(2))
                .expect("the drop's own read");
            source_writes(w, &list);
            c.attached(2, Duration::from_secs(5));
            obs.push((format!("{label}, at level 1"), pss_kb(pid)));
        } else {
            drag_to_stage2(&mut c, &list);
            obs.push((format!("{label}, at stage 2"), pss_kb(pid)));
        }
        let anon = anon_kb(pid);
        let peaks = sampler.finish();
        obs.extend(peaks.observations(&label, 1));
        c.close();
        assert_eq!(
            wait_exit(&mut child, Duration::from_secs(5)).code(),
            Some(0)
        );
        (obs, anon)
    };
    let runs = [
        (1, peak("rss1a", false, false)),
        (1, peak("rss1b", false, false)),
        (2, peak("rss-largest", true, false)),
        (2, peak("rss-largest-late", true, true)),
    ];
    for (_, (obs, anon)) in &runs {
        for (label, kb) in obs {
            eprintln!("PSS {label}: {kb} kB");
        }
        eprintln!("  anonymous at the end: {anon} kB");
    }
    if cfg!(debug_assertions) {
        // PSS is not the measure here: it divides the shared pages among
        // the processes alive at that moment, so beside other tests'
        // wheels the runs differ by more than the drop adds. The drop's own
        // growth is its anonymous memory: the paths, held once (the list's
        // size in KiB), plus the 512 kB a small drop was allowed before.
        // A second copy of the paths would add another 1024 kB and fail.
        // Worst against best, so no run is hidden.
        let anon = |k: u8| runs.iter().filter(move |r| r.0 == k).map(|r| (r.1).1);
        let one = anon(1).min().unwrap();
        let many = anon(2).max().unwrap();
        let paths = (action::INPUT_LIST_BYTES / 1024) as u64;
        assert!(
            many <= one + paths + 512,
            "1 file {one} kB, the largest list {many} kB anonymous"
        );
    } else {
        let obs: Vec<(&str, u64)> = runs
            .iter()
            .flat_map(|r| (r.1).0.iter())
            .map(|(l, kb)| (l.as_str(), *kb))
            .collect();
        if let Err(why) = pss_gate(&obs) {
            panic!("{why}");
        }
    }
}

impl Root {
    /// `n` PNGs in `f/` whose paths are about 62 bytes (a long-ish real
    /// path), their URI list, and the `inacircle-pages` action cached so
    /// that Book (east) is lit for them.
    fn png_list(&self, n: usize) -> Vec<u8> {
        let rows: Vec<u8> = (0..8).flat_map(|_| [0u8; 9]).collect();
        let png = testzip::png(8, 8, 0, None, false, false, &rows);
        self.real_action_cached("inacircle-pages", ("primer.png", &png), "pdf");
        let dir = self.0.join("f");
        let width = 62usize.saturating_sub(dir.as_os_str().len() + 1 + 4);
        let mut list = Vec::new();
        for i in 0..n {
            let p = dir.join(format!("{i:_<width$}.png"));
            fs::write(&p, &png).unwrap();
            assert!(p.as_os_str().len() >= 60, "{}", p.display());
            list.extend_from_slice(format!("file://{}\r\n", p.display()).as_bytes());
        }
        list
    }
}

/// Book is east of the center.
const BOOK: (u32, u32) = (1110, 540);

/// The 1000-image drag that used to do nothing: its URI list is over the
/// old 64 KiB read limit. Early: level 1 comes with Book lit and the drop
/// reaches stage 2. Peak memory is reported against the 5 MB PSS budget
/// (checked in a release build).
#[test]
fn a_thousand_image_list_over_64_kib_reaches_stage_2() {
    let root = Root::new("big1000");
    let list = root.png_list(1000);
    assert!(list.len() > 64 * 1024, "{} bytes", list.len());
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
    drag_to_category(&mut c, &list, BOOK);
    let (pss, hwm) = (pss_kb(child.id()), vm_hwm_kb(child.id()));
    eprintln!("1000-path drag at stage 2: PSS {pss} kB, VmHWM {hwm} kB");
    if !cfg!(debug_assertions) {
        assert!(pss * 1024 <= 5_000_000, "{pss} kB PSS");
    }
    c.close();
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(5)).code(),
        Some(0)
    );
}

/// The same list through the fallback: no early answer, the drop's own
/// read, then level 1 with Book lit (a click would open stage 2).
#[test]
fn a_thousand_image_list_over_64_kib_is_read_after_the_drop() {
    let root = Root::new("big1000drop");
    let list = root.png_list(1000);
    let (mut c, child) = start(&root, false);
    let silent = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("early receive");
    c.pump(Duration::from_millis(900));
    assert!(reader_gone(&silent));
    c.drop_it();
    let w = c
        .next_receive(0, Duration::from_secs(2))
        .expect("the drop's own read");
    source_writes(w, &list);
    c.attached(2, Duration::from_secs(5));
    finish(c, child);
}

/// A list past the limits (1025 URIs, or over 1 MiB) is refused with the
/// hub text and nothing is offered: the early wheel shows it, the drop on
/// it is rejected and closes, and the fallback shows it after the drop.
#[test]
fn a_list_over_the_limits_offers_nothing() {
    // Real text files, so the old truncation to 1024 would have lit
    // Document and taken the drop (the test fails without the refusal).
    let files_root = Root::new("refuse-files");
    let many = files_root.files(1025);
    let huge: Vec<u8> = (0..200)
        .flat_map(|i| format!("file:///{i}/{}\r\n", "a".repeat(6000)).into_bytes())
        .collect();
    assert!(huge.len() > 1_048_576);
    for (name, list) in [("many", many), ("huge", huge)] {
        let root = Root::new(&format!("refuse-{name}"));
        root.markdown_action_cached();
        // Early: the refusal wheel replaces `Drop here`; the drop on a
        // slice is rejected, so the wheel closes with no stage 2.
        let (mut c, mut child) = start(&root, false);
        let w = c
            .drag_enters(0, Duration::from_secs(2))
            .expect("early receive");
        // The wheel stops reading past the limit: a closed pipe is fine.
        std::thread::spawn({
            let list = list.clone();
            move || {
                let _ = std::io::Write::write_all(&mut std::fs::File::from(w), &list);
            }
        });
        c.attached(2, Duration::from_secs(5));
        c.release_held();
        // North: Document, which the truncated list would have lit.
        c.drag_motion(960, 390);
        c.pump(Duration::from_millis(200));
        c.drop_it();
        assert_eq!(
            wait_exit(&mut child, Duration::from_secs(5)).code(),
            Some(0),
            "{name}"
        );
        assert_eq!(c.attaches(), 2, "{name}: no stage 2");
        drop(c);
        // After the drop: the refusal wheel stays up (a click closes it).
        let root = Root::new(&format!("refuse-drop-{name}"));
        root.markdown_action_cached();
        let (mut c, child) = start(&root, false);
        let silent = c
            .drag_enters(0, Duration::from_secs(2))
            .expect("early receive");
        c.pump(Duration::from_millis(900));
        assert!(reader_gone(&silent));
        c.drop_it();
        let w = c
            .next_receive(0, Duration::from_secs(2))
            .expect("the drop's own read");
        std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut std::fs::File::from(w), &list);
        });
        c.attached(2, Duration::from_secs(5));
        finish(c, child);
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

/// The context switches of every thread of `pid`, voluntary and
/// involuntary: each is one time the kernel took the process off the CPU,
/// so while the wheel waits for the user, no switch means no wakeup.
fn context_switches(pid: u32) -> u64 {
    let mut total = 0;
    for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let status = fs::read_to_string(task.unwrap().path().join("status")).unwrap();
        total += status
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .map(|l| l.split_whitespace().last().unwrap().parse::<u64>().unwrap())
            .sum::<u64>();
    }
    total
}

/// The number of live child processes of `pid` (any thread).
fn children(pid: u32) -> usize {
    fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .map(|t| fs::read_to_string(t.unwrap().path().join("children")).unwrap_or_default())
        .map(|c| c.split_whitespace().count())
        .sum()
}

/// Observation window of a stationary wheel: 2 s is far above any startup
/// settle and long enough that a periodic timer of 1 s or faster would show
/// at least one switch, yet short for the suite. The wheel's own deadlines
/// are 30 s (user) and 2 s (startup, long over after the first 300 ms
/// settle below), so none can fire inside the window.
const IDLE_WINDOW: Duration = Duration::from_secs(2);

/// Idle, property by property (acceptance.md): a mapped wheel that nobody
/// touches (1) takes no context switch during the window, so no
/// autonomous periodic work (the poll sleeps on a deadline), (2) has no
/// child process (no resident engine), (3) reports `idle_wakeups=0` (no
/// premature empty poll) and (4) is gone, exit 0, once closed.
#[test]
fn a_stationary_wheel_wakes_for_nothing_and_leaves_no_engine() {
    let root = Root::new("idle");
    root.markdown_action_cached();
    let (mut c, child) = start(&root, true);
    let pid = child.id();
    // A seat with pointer and keyboard and the pointer over the surface
    // end the startup waits: what remains is the steady state of a wheel
    // that waits for the user.
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_devices(POINTER | KEYBOARD, Duration::from_secs(2));
    c.pointer_enters();
    // The frame passes; the wheel prepaints the released buffer.
    c.pump(Duration::from_millis(300));
    c.release_held();
    c.pump(Duration::from_millis(300));
    let before = context_switches(pid);
    std::thread::sleep(IDLE_WINDOW);
    let after = context_switches(pid);
    assert_eq!(
        before,
        after,
        "{} switches while nobody touched the wheel",
        after - before
    );
    assert_eq!(children(pid), 0, "a child process is resident");
    let err = finish(c, child);
    let line = err
        .lines()
        .find(|l| l.contains("measure wheel "))
        .unwrap_or_else(|| panic!("no measure line: {err}"));
    assert!(line.contains(" idle_wakeups=0 "), "{line}");
    assert!(dead(pid as i32), "the wheel outlived its close");
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
    // The whole line, for benchmark runs that collect it.
    if let Some(line) = err.lines().find(|l| l.contains("measure wheel")) {
        eprintln!("{line}");
    }
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

/// The wait for the drag source's list takes buffer 1's first touch: the
/// level-1 wheel painted into it after the list pays no page faults for it
/// (a fresh 480x480 buffer is 225 of them). The list comes well after the
/// first commit, so the wheel is idle in between.
#[test]
fn level1_from_a_late_list_pays_no_first_touch() {
    let root = Root::new("level1-touch");
    root.markdown_action_cached();
    let list = root.files(1);
    let (mut c, child) = start(&root, true);
    let w = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("the early receive");
    // `Drop here` is on the screen; the source answers later.
    c.attached(1, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(50));
    source_writes(w, &list);
    c.attached(2, Duration::from_secs(5));
    let err = finish(c, child);
    if let Some(line) = err.lines().find(|l| l.contains("measure wheel")) {
        eprintln!("{line}");
    }
    let flt: i64 = err
        .split_whitespace()
        .find_map(|w| w.strip_prefix("list_to_level1_minflt="))
        .unwrap_or_else(|| panic!("no list_to_level1_minflt in {err:?}"))
        .parse()
        .unwrap();
    // Below one buffer's 225 pages: the paint's own allocations stay
    // under 20 on the reference machine.
    assert!(flt < 100, "{flt} faults: {err}");
}

// Bounded startup and teardown (hardening task 11). The fake compositor's
// script decides what arrives, and when; the wheel has no bus to notify on
// (`Root::cmd` removes the variable), so these see the exit code, the
// timing and the log line. The notification itself is in `notify.rs`.

/// The proposed `ui_startup_deadline` (`docs/resource-limits.md`), and the pair
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

/// Stops `pid` and waits (by its state, not by time) until it is stopped,
/// so what the compositor sends next is read by the client in one go.
fn freeze(pid: u32) {
    // SAFETY: a signal to our own child.
    unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
    let limit = Instant::now() + Duration::from_secs(5);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // The state letter follows the last `)` (the name may hold one).
        let state = stat.rsplit(')').next().unwrap().trim_start().as_bytes()[0];
        if state == b'T' {
            return;
        }
        assert!(Instant::now() < limit, "the wheel did not stop: {stat}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn thaw(pid: u32) {
    // SAFETY: as above.
    unsafe { libc::kill(pid as i32, libc::SIGCONT) };
}

/// The batch that shows the wheel (the keyboard arrives, the position is
/// known) also takes the keyboard away: the first commit happens inside
/// that dispatch, before the loop starts the user's stage, and the loss is
/// decided by what is on the screen, not by which clock runs. Without it
/// the wheel stays up with no Esc for the whole 30 s stage. The client is
/// stopped while both capabilities are sent, so it reads them together.
#[test]
fn keyboard_lost_in_the_batch_that_showed_the_wheel_closes_in_order() {
    let root = Root::new("startup-lostkbd-batch");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER);
    // The drag brings the position; its early read is the client's answer,
    // so the position is in the machine when the capabilities arrive.
    let _silent = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("early receive");
    assert_eq!(c.attaches(), 0, "no keyboard: no wheel yet");
    freeze(child.id());
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_caps(POINTER);
    thaw(child.id());
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(took < STARTUP, "{took:?}");
    assert!(
        c.layer_requests().contains(&7),
        "layer surface not destroyed"
    );
}

/// The keyboard goes away before the first commit: the wheel is never
/// shown and the startup names the keyboard, at the startup deadline.
#[test]
fn keyboard_lost_before_the_first_commit_fails_naming_the_keyboard() {
    let root = Root::new("startup-lostkbd-early");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.seat_caps(POINTER);
    c.pointer_enters();
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("keyboard was unavailable"), "{err}");
    assert!(took >= STARTUP - Duration::from_millis(100), "{took:?}");
    assert!(took < STARTUP + Duration::from_millis(1500), "{took:?}");
}

/// A keyboard that went away and came back is a new device: the client asks
/// for it again, and Esc on the new object closes the wheel. The old
/// object is inert, so without the new request Esc would never arrive.
#[test]
fn keyboard_back_after_a_loss_is_asked_for_again() {
    let root = Root::new("startup-backkbd");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.seat_caps(POINTER);
    c.seat_caps(POINTER | KEYBOARD);
    c.keyboards_requested(2, Duration::from_secs(2));
    c.pointer_enters();
    c.attached(1, Duration::from_secs(4));
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
}

/// Esc before the configure is a clean cancel too: exit 0, no
/// `ConfigureTimeout` notice for a compositor that never configured.
#[test]
fn startup_esc_before_the_configure_cancels_cleanly() {
    let root = Root::new("startup-esc-noconf");
    let t0 = Instant::now();
    let script = Script {
        configure: Delay::Never,
        ..Script::default()
    };
    let (mut c, child) = start_with(&root, script);
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(wheel_esc());
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(took < STARTUP, "{took:?}");
}

/// A drop in a seat with no keyboard must not start the user's 30 s: the
/// wheel is not on the screen, the startup clock is the only one, and the
/// keyboard is named at the startup deadline. (The machine arms its stage
/// on the drop whether or not the wheel is shown.)
#[test]
fn a_drop_before_the_wheel_is_shown_does_not_extend_the_startup() {
    let root = Root::new("startup-drop-nokbd");
    root.markdown_action_cached();
    let list = root.files(1);
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER);
    let _silent = c
        .drag_enters(0, Duration::from_secs(2))
        .expect("early receive");
    c.drop_it();
    // The client took the drop (its own read): the stage was armed by now.
    // The list arrives, so the machine goes on to the targets and waits
    // for a choice nobody can see.
    let own = c
        .next_receive(0, Duration::from_secs(2))
        .expect("the drop's own read");
    source_writes(own, &list);
    let (code, err, took) = until_gone(&mut c, child, t0);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("keyboard was unavailable"), "{err}");
    assert!(took < STARTUP + Duration::from_millis(1500), "{took:?}");
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
    let mut child = isolate(&mut Command::new(BIN), &rt)
        .env("XDG_RUNTIME_DIR", &rt)
        .env("WAYLAND_DISPLAY", "wayland-0")
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

/// The close waits for the answer to its own `sync`, not for any callback's
/// `done`: a `done` on another callback leaves it waiting until its
/// deadline (the fake never answers the `sync`). Kills `&&` -> `||` in the
/// callback handler. Only a lower bound is asserted, which load can only
/// help.
#[test]
fn startup_teardown_ignores_a_done_on_another_callback() {
    let root = Root::new("startup-td-decoy");
    let script = Script {
        teardown_ack: Delay::Never,
        teardown_decoy: true,
        ..Script::default()
    };
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, script);
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(!c.teardown_acked());
    let held = c.since_destroy().expect("destroyed");
    // The client's own deadline is counted from before its layer destroy
    // was sent, so allow the flush in between.
    assert!(held >= TEARDOWN - Duration::from_millis(100), "{held:?}");
}

/// The answer to the close's own `sync` ends the wait at once: the client
/// does not sit out its deadline. Kills `!=` -> `==` (the answer never
/// recognised). The bound is half the deadline: the deadline itself is the
/// failure being detected.
#[test]
fn startup_teardown_leaves_when_its_sync_is_answered() {
    let root = Root::new("startup-td-prompt");
    let t0 = Instant::now();
    let (mut c, child) = start_with(&root, Script::default());
    c.mapped(Duration::from_secs(2));
    seat_up(&mut c, POINTER | KEYBOARD);
    c.key(wheel_esc());
    let (code, err, _) = until_gone(&mut c, child, t0);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let after = c.since_ack().expect("the fake answered");
    assert!(after < TEARDOWN / 2, "{after:?}");
}

/// The largest of a `/proc` field's values seen so far (`None` while the
/// file is unreadable, e.g. the process just exited).
fn proc_field_kb(path: String, key: &str) -> Option<u64> {
    fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// Linux's `NAME_MAX`: the most bytes in one path component. A longer
/// one cannot be created, so the largest list is built from several.
const NAME_MAX: usize = 255;

/// Bytes of each line of the largest list the wheel accepts: the byte
/// limit shared out over the URI limit (1 MiB / 1024 = 1024 B, with its
/// `file://` and CRLF), so 1024 such lines fill the limit exactly.
const LARGEST_LINE: usize = action::INPUT_LIST_BYTES / action::INPUT_COUNT;

/// The shortest file name a line ends in: `0000.txt`, its index and type.
const SHORTEST_NAME: usize = "0000.txt".len();

impl Root {
    /// The largest list the wheel accepts: `action::INPUT_COUNT` text
    /// files under `f/`, each line `LARGEST_LINE` bytes, so the list is
    /// `action::INPUT_LIST_BYTES` to the byte (the wheel's own limit, which
    /// a unit test keeps equal). Every path component stays within
    /// `NAME_MAX` whatever the temp root's length. The files' directory
    /// and the list.
    fn largest_list(&self) -> (PathBuf, Vec<u8>) {
        let path_len = LARGEST_LINE - "file://".len() - "\r\n".len();
        let mut dir = self.0.join("f");
        assert!(dir.as_os_str().len() + 1 + SHORTEST_NAME <= path_len);
        loop {
            let name = path_len - dir.as_os_str().len() - 1;
            if name <= NAME_MAX {
                break;
            }
            // A directory, leaving at least the shortest name after it.
            dir = dir.join("d".repeat((name - 1 - SHORTEST_NAME).min(NAME_MAX)));
        }
        fs::create_dir_all(&dir).unwrap();
        let name = path_len - dir.as_os_str().len() - 1;
        let mut list = Vec::with_capacity(action::INPUT_LIST_BYTES);
        for i in 0..action::INPUT_COUNT {
            let pad = "_".repeat(name - SHORTEST_NAME);
            let p = dir.join(format!("{i:04}{pad}.txt"));
            fs::write(&p, "w\n").unwrap();
            assert!(p.components().all(|c| c.as_os_str().len() <= NAME_MAX));
            list.extend_from_slice(format!("file://{}\r\n", p.display()).as_bytes());
        }
        assert_eq!(list.len(), action::INPUT_LIST_BYTES);
        (dir, list)
    }
}

/// The phases of a wheel the memory tests tell apart: level 1 (from the
/// start through the dropped list's conversion), stage 2, and the run
/// after the click (the engine and the supervisor in the same process).
const PHASES: [&str; 3] = ["stage 1", "stage 2", "the run"];

/// Peak PSS of a live process per phase, from `smaps_rollup`, on a
/// thread of the test (the wheel pays nothing for it).
struct PssSampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    phase: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: std::thread::JoinHandle<Peaks>,
}

/// What a sampler saw: the peak PSS (kB) and the sample count per phase,
/// and `VmHWM` (kB), the exact resident peak a sample could miss.
struct Peaks {
    pss: [u64; PHASES.len()],
    samples: [u64; PHASES.len()],
    hwm: u64,
}

/// Time between two samples: 200 us. The wheel's steps on the largest
/// list (classifying 1024 paths, a paint, the run) take milliseconds, so
/// each is sampled several times; a shorter transient is bounded by
/// `VmHWM`, which is reported beside the samples.
const SAMPLE_EVERY: Duration = Duration::from_micros(200);

impl PssSampler {
    fn start(pid: u32) -> PssSampler {
        use std::sync::atomic::Ordering::Relaxed;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let phase = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread = std::thread::spawn({
            let (stop, phase) = (stop.clone(), phase.clone());
            move || {
                let mut p = Peaks {
                    pss: [0; PHASES.len()],
                    samples: [0; PHASES.len()],
                    hwm: 0,
                };
                while !stop.load(Relaxed) {
                    // Read before the sample: a sample taken after a phase
                    // began always counts for it.
                    let k = phase.load(Relaxed);
                    if let Some(kb) = proc_field_kb(format!("/proc/{pid}/smaps_rollup"), "Pss:") {
                        p.pss[k] = p.pss[k].max(kb);
                        p.samples[k] += 1;
                    }
                    let hwm = proc_field_kb(format!("/proc/{pid}/status"), "VmHWM:");
                    p.hwm = p.hwm.max(hwm.unwrap_or(0));
                    std::thread::sleep(SAMPLE_EVERY);
                }
                p
            }
        });
        PssSampler {
            stop,
            phase,
            thread,
        }
    }

    /// The samples from now on count for `PHASES[k]`.
    fn enter(&self, k: usize) {
        self.phase.store(k, std::sync::atomic::Ordering::Relaxed);
    }

    fn finish(self) -> Peaks {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.join().unwrap()
    }
}

impl Peaks {
    /// The gate's observations for the first `n` phases, each labelled;
    /// a phase without a sample is missing evidence: it fails here, in
    /// every build (the gate itself runs in the release build only).
    fn observations(&self, label: &str, n: usize) -> Vec<(String, u64)> {
        (0..n)
            .map(|k| {
                let s = self.samples[k];
                assert!(s > 0, "{label}: no PSS sample in phase {}", PHASES[k]);
                (format!("{label}, {} ({s} samples)", PHASES[k]), self.pss[k])
            })
            .collect()
    }
}

/// Where the Document category is: 150 px north of the center of the
/// 1920x1080 output, inside its slice's ring.
const DOCUMENT: (u32, u32) = (960, 390);

/// The most a test waits for the wheel to hang up after the click: 60 s.
/// The overlay goes before the run starts, so this is a stuck teardown's
/// limit, with room for a loaded machine running the whole suite; a stuck
/// wheel fails here, not in the harness.
const RUN_HANGUP: Duration = Duration::from_secs(60);

/// The most a test waits for the process to exit after the hang-up: 120 s
/// for the run of 1024 text files (well under a second here, release; a
/// debug build under a loaded suite is many times slower), its supervisor
/// teardown and the catalog refresh. A stuck run fails here, not in the
/// harness.
const RUN_EXIT: Duration = Duration::from_secs(120);

/// PSS of the wheel process through a whole choice (the drop, stage 2,
/// the click, the run) for the largest list it accepts (1024 paths of
/// ~1 KiB, the list at its 1 MiB limit), sampled per phase. Asserted: the
/// choice completes with one output group per path, and, in the optimized
/// build the budget is for, every phase stays within the 5,000,000 B PSS
/// budget: level 1 (where the list's bytes become paths), stage 2 and the
/// run. Before the list was given back as it was decoded, level 1 held
/// the bytes and the paths together and peaked at 4.9-5.0 MB.
#[test]
fn the_largest_drop_through_the_choice_stays_within_the_budget() {
    let root = Root::new("through1024");
    root.real_action_cached("inacircle-text", ("primer.txt", b"words\n"), "markdown");
    let (dir, list) = root.largest_list();
    let (mut c, mut child) = start(&root, false);
    let pid = child.id();
    let sampler = PssSampler::start(pid);
    c.seat_caps(POINTER | KEYBOARD);
    c.seat_devices(POINTER | KEYBOARD, Duration::from_secs(2));
    c.pointer_enters();
    drag_to_category(&mut c, &list, DOCUMENT);
    sampler.enter(1);
    let stage2 = pss_kb(pid);
    // Stage 2 settles (its buffers released, the wheel idle) before the
    // pointer moves: a few frames' time, far below the stage deadline.
    c.pump(Duration::from_millis(300));
    c.release_held();
    c.pointer_motion(DOCUMENT.0, DOCUMENT.1);
    // The motion is handled before the button (events are in order; this
    // is margin for the hover's repaint).
    c.pump(Duration::from_millis(50));
    sampler.enter(2);
    c.pointer_button(0x110);
    let _ = c.hangs_up(RUN_HANGUP);
    let st = wait_exit(&mut child, RUN_EXIT);
    let peaks = sampler.finish();
    let groups = fs::read_dir(&dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    let mut obs = peaks.observations("largest list", PHASES.len());
    obs.push(("largest list, at stage 2".into(), stage2));
    for (label, kb) in &obs {
        eprintln!("PSS {label}: {kb} kB");
    }
    eprintln!(
        "largest list: VmHWM {} kB; exit {:?}, {groups} output groups",
        peaks.hwm,
        st.code()
    );
    assert_eq!(st.code(), Some(0));
    assert_eq!(groups, action::INPUT_COUNT, "one output group per path");
    if !cfg!(debug_assertions) {
        let obs: Vec<(&str, u64)> = obs.iter().map(|(l, kb)| (l.as_str(), *kb)).collect();
        if let Err(why) = pss_gate(&obs) {
            panic!("{why}");
        }
    }
}
