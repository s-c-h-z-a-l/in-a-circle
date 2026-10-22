use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A unique temporary directory, removed on drop.
struct Tmp(PathBuf);

impl std::ops::Deref for Tmp {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tmp(name: &str) -> Tmp {
    let d = std::env::temp_dir().join(format!(
        "inacircle-testaction-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

const BIN: &str = env!("CARGO_BIN_EXE_inacircle-testaction");

fn command(work: &Path, opts: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.args(["run", "test.script", "--work"]).arg(work);
    for o in opts {
        c.args(["--opt", o]);
    }
    c.args(["--", "/in/a.bin", "/in/b.bin"]);
    c
}

fn run(work: &Path, opts: &[&str]) -> Output {
    command(work, opts).output().unwrap()
}

fn out_lines(o: &Output) -> Vec<String> {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn manifest_describes_test_script() {
    let o = Command::new(BIN).arg("--manifest").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        "inacircle-manifest 0\n\
action\ttest.script\tfamily=test\tin=application/x-inacircle-test\tout=application/octet-stream\text=bin\tlabel=Test\tgroup=test\tcost=10\tlossy=no\tbatch=yes\n"
    );
}

#[test]
fn events_in_order() {
    let w = tmp("events");
    let o = run(&w, &["events=progress:0.5,done,skip,fail,warn,progress:1"]);
    assert_eq!(o.status.code(), Some(0));
    let l = out_lines(&o);
    assert_eq!(l.len(), 6, "{l:?}");
    assert_eq!(l[0], "progress\t0.5");
    assert!(l[1].starts_with("done\t/in/a.bin\t0/a.bin"), "{}", l[1]);
    assert!(l[2].starts_with("skip\t/in/a.bin\t"), "{}", l[2]);
    assert!(l[3].starts_with("fail\t/in/a.bin\t"), "{}", l[3]);
    assert!(l[4].starts_with("warn\t/in/a.bin\t"), "{}", l[4]);
    assert_eq!(l[5], "progress\t1");
    assert!(w.join("0/a.bin").is_file());
}

#[test]
fn exit_code_passthrough() {
    let w = tmp("exit");
    for code in [0, 1, 2, 3, 42] {
        let o = run(&w, &[&format!("exit={code}")]);
        assert_eq!(o.status.code(), Some(code));
    }
}

#[test]
fn multiple_outputs() {
    let w = tmp("outputs");
    let o = run(&w, &["outputs=3"]);
    assert_eq!(o.status.code(), Some(0));
    let l = out_lines(&o);
    assert_eq!(l.len(), 3, "{l:?}");
    for (k, line) in l.iter().enumerate() {
        assert_eq!(*line, format!("done\t/in/a.bin\t0/out-{k}.bin"));
        assert!(w.join(format!("0/out-{k}.bin")).is_file());
    }
}

#[test]
fn options_run_in_command_line_order() {
    let w = tmp("order");
    let o = run(&w, &["events=warn", "exit=1", "events=skip"]);
    assert_eq!(o.status.code(), Some(1));
    let l = out_lines(&o);
    assert!(
        l[0].starts_with("warn") && l[1].starts_with("skip"),
        "{l:?}"
    );
}

#[test]
fn unknown_key_is_usage_error() {
    let w = tmp("unknown");
    let o = run(&w, &["bogus=1"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("bogus"));
}

#[test]
fn bad_value_is_usage_error() {
    let w = tmp("badvalue");
    for opt in [
        "exit=abc",
        "sleep_ms=-1",
        "big_output=abc",
        "outputs=-1",
        "events=nope",
        "on_sigterm=maybe",
    ] {
        assert_eq!(run(&w, &[opt]).status.code(), Some(2), "{opt}");
    }
}

/// Spawns the action with `opts`, then `events=progress:0` and a long sleep,
/// and returns once the first event line has arrived: every option before it
/// (e.g. the signal disposition) has been applied by then.
fn spawn_ready(
    work: &Path,
    opts: &[&str],
) -> (std::process::Child, BufReader<std::process::ChildStdout>) {
    let mut all = opts.to_vec();
    all.extend(["events=progress:0", "sleep_ms=5000"]);
    let mut c = command(work, &all).stdout(Stdio::piped()).spawn().unwrap();
    let mut r = BufReader::new(c.stdout.take().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert_eq!(line, "progress\t0\n");
    (c, r)
}

fn sigterm(c: &std::process::Child) {
    let pid = i32::try_from(c.id()).unwrap();
    // SAFETY: `kill` only takes integers; `pid` is the id of our own live,
    // not yet reaped child, so it cannot name an unrelated process.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(rc, 0);
}

#[test]
fn sigterm_exits() {
    let w = tmp("sigterm");
    let (mut c, _r) = spawn_ready(&w, &[]);
    let t = Instant::now();
    sigterm(&c);
    let st = c.wait().unwrap();
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(st.signal(), Some(libc::SIGTERM));
}

#[test]
fn sigterm_ignored_on_request() {
    let w = tmp("sigign");
    let (mut c, _r) = spawn_ready(&w, &["on_sigterm=ignore"]);
    sigterm(&c);
    // Still alive after the signal; stop it for real.
    std::thread::sleep(Duration::from_millis(200));
    assert!(c.try_wait().unwrap().is_none());
    c.kill().unwrap();
    assert_eq!(c.wait().unwrap().signal(), Some(libc::SIGKILL));
}

#[test]
fn sigterm_exit_resets_ignore() {
    let w = tmp("sigreset");
    let (mut c, _r) = spawn_ready(&w, &["on_sigterm=ignore", "on_sigterm=exit"]);
    sigterm(&c);
    assert_eq!(c.wait().unwrap().signal(), Some(libc::SIGTERM));
}

#[test]
fn bad_options_are_validated_before_side_effects() {
    let w = tmp("validate");
    let o = run(&w, &["events=done", "bogus=1"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert_eq!(fs::read_dir(&*w).unwrap().count(), 0);
}

#[test]
fn relative_write_outside_is_usage_error() {
    let w = tmp("reloutside");
    let o = run(&w, &["write_outside=rel/file"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(!Path::new("rel/file").exists());
}

#[test]
fn write_outside_reports() {
    let w = tmp("outside");
    let target = w.join("outside.txt");
    let o = run(&w, &[&format!("write_outside={}", target.display())]);
    assert_eq!(o.status.code(), Some(0));
    let l = out_lines(&o);
    assert_eq!(l, ["warn\t/in/a.bin\twrote outside work"]);
    assert!(target.is_file());

    let bad = "/nonexistent-dir-xyz/f";
    let o = run(&w, &[&format!("write_outside={bad}")]);
    assert_eq!(o.status.code(), Some(0));
    let l = out_lines(&o);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(l[0].starts_with("fail\t/in/a.bin\t"), "{}", l[0]);
    assert!(l[0].to_lowercase().contains("no such file"), "{}", l[0]);
}

#[test]
fn big_output_writes_requested_size() {
    let w = tmp("big");
    let o = run(&w, &["big_output=300000"]);
    assert_eq!(o.status.code(), Some(0));
    let total: u64 = fs::read_dir(&*w)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert_eq!(total, 300000);
}

#[test]
fn big_output_fails_under_fsize_limit() {
    let w = tmp("fsize");
    // ulimit -f counts 512-byte blocks: 8 blocks = 4096 bytes.
    let o = Command::new("sh")
        .arg("-c")
        .arg("ulimit -f 8; exec \"$0\" run test.script --work \"$1\" --opt big_output=1000000 -- /in/a.bin")
        .arg(BIN)
        .arg(&*w)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "{:?}", o.status);
    let l = out_lines(&o);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(l[0].starts_with("fail\t/in/a.bin\t"), "{}", l[0]);
}
