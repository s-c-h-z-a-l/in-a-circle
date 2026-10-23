//! End-to-end tests: the real `inacircle` binary driving the real action
//! binaries (`inacircle-text`, `inacircle-testaction`) under the sandbox.
//!
//! Every test gets its own root with `INACIRCLE_ACTIONS_DIR` pointing at
//! links to the built action binaries and private XDG directories and HOME.
//! When a test ends without panicking, no `.inacircle-*` work dir may be
//! left anywhere under its root.

use manifest::{Line, parse_line};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_inacircle");
const ACTIONS: [&str; 2] = ["inacircle-text", "inacircle-testaction"];

/// The action binaries live next to ours. `cargo test -p inacircle` alone
/// does not build them, so build them once into the same target dir.
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
        let st = c.status().expect("cannot run cargo");
        assert!(st.success(), "building the action binaries failed");
    });
    let p = dir.join(name);
    assert!(p.is_file(), "{} not built", p.display());
    p
}

struct Out {
    code: Option<i32>,
    lines: Vec<Line>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn of(kind: &str, lines: &[Line]) -> Vec<Line> {
        lines.iter().filter(|l| l.kind == kind).cloned().collect()
    }
    fn kind(&self, kind: &str) -> Vec<Line> {
        Out::of(kind, &self.lines)
    }
    fn job(&self) -> String {
        let j = self.kind("job");
        assert_eq!(j.len(), 1, "{}", self.stdout);
        j[0].bare_at(0).unwrap().to_owned()
    }
    /// `(input, second field)` of every line of `kind`.
    fn pairs(&self, kind: &str) -> Vec<(PathBuf, String)> {
        self.kind(kind)
            .iter()
            .map(|l| {
                (
                    PathBuf::from(l.bare_at(0).unwrap()),
                    l.bare_at(1).unwrap().to_owned(),
                )
            })
            .collect()
    }
    fn summary(&self) -> (usize, usize, usize) {
        let s = self.kind("summary");
        assert_eq!(s.len(), 1, "{}", self.stdout);
        let n = |k| s[0].get(k).unwrap().parse().unwrap();
        (n("done"), n("skipped"), n("failed"))
    }
    fn unsandboxed(&self) -> bool {
        self.pairs("warn")
            .iter()
            .any(|(_, m)| m == "ran without sandbox")
    }
}

fn parse_out(o: std::process::Output) -> Out {
    let stdout = String::from_utf8(o.stdout).unwrap();
    let lines = stdout
        .split_inclusive('\n')
        .map(|l| parse_line(l).unwrap_or_else(|e| panic!("bad line {l:?}: {e:?}")))
        .collect();
    Out {
        code: o.status.code(),
        lines,
        stdout,
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        Env::with_actions(name, &ACTIONS)
    }

    fn with_actions(name: &str, actions: &[&str]) -> Env {
        let root =
            std::env::temp_dir().join(format!("inacircle-cli-{}-{name}", std::process::id()));
        if root.exists() {
            make_writable(&root);
            fs::remove_dir_all(&root).unwrap();
        }
        for d in [
            "actions", "cache", "state", "runtime", "home", "config", "files",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        for a in actions {
            symlink(action_bin(a), root.join("actions").join(a)).unwrap();
        }
        Env { root }
    }

    fn files(&self) -> PathBuf {
        self.root.join("files")
    }

    /// Writes `content` to `files/<rel>`, creating folders.
    fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.files().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    }

    /// A test-type input whose content carries testaction options.
    fn test_input(&self, rel: &str, opts: &[&str]) -> PathBuf {
        let mut s = String::from("INACIRCLE-TEST\n");
        for o in opts {
            s.push_str(o);
            s.push('\n');
        }
        self.file(rel, s.as_bytes())
    }

    fn cmd<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("INACIRCLE_ACTIONS_DIR", self.root.join("actions"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("home/.local/share"))
            .env("HOME", self.root.join("home"))
            .current_dir(self.files())
            .stdin(Stdio::null());
        c
    }

    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Out {
        parse_out(self.cmd(args).output().unwrap())
    }

    fn history(&self) -> String {
        fs::read_to_string(self.root.join("state/inacircle/history")).unwrap()
    }
}

/// Paths under `root` whose name starts with `.inacircle-`.
fn work_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name().as_encoded_bytes().starts_with(b".inacircle-") {
                found.push(p.clone());
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p);
            }
        }
    }
    found
}

fn make_writable(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let left = work_dirs(&self.root);
        make_writable(&self.root);
        let _ = fs::remove_dir_all(&self.root);
        if !std::thread::panicking() {
            assert!(left.is_empty(), "work dirs left behind: {left:?}");
        }
    }
}

fn landlock_required() -> bool {
    std::env::var_os("INACIRCLE_REQUIRE_LANDLOCK").is_some_and(|v| v == "1")
}

/// True (and a note printed) when the run had no Landlock, so a sandbox
/// assertion cannot hold.
fn skip_unsandboxed(test: &str, o: &Out) -> bool {
    if o.unsandboxed() {
        assert!(
            !landlock_required(),
            "{test}: Landlock unavailable but INACIRCLE_REQUIRE_LANDLOCK=1"
        );
        eprintln!("{test}: skipped, Landlock unavailable");
        return true;
    }
    false
}

// ---- text conversions through the real inacircle-text ----

#[test]
fn txt_to_md_end_to_end() {
    let e = Env::new("txt2md");
    let src = e.file("jegyzet.txt", b"Hello *world*\n\n# not heading\n");
    // A relative path is taken against the cwd (`files/`).
    let o = e.run(&["run", "markdown", "jegyzet.txt"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.files().join("jegyzet.md");
    assert_eq!(o.pairs("done"), [(src.clone(), out.display().to_string())]);
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "Hello \\*world\\*\n\n\\# not heading\n"
    );
    assert_eq!(o.summary(), (1, 0, 0));
    assert_eq!(o.lines[0].kind, "job");
    assert_eq!(o.lines.last().unwrap().kind, "summary");
    // The source is untouched.
    assert_eq!(fs::read(&src).unwrap(), b"Hello *world*\n\n# not heading\n");
    let job = o.job();
    let (secs, pid) = job.split_once('-').unwrap();
    assert!(
        secs.parse::<u64>().is_ok() && pid.parse::<u32>().is_ok(),
        "{job}"
    );

    let history = e.history();
    let out_line = history
        .lines()
        .filter_map(|l| parse_line(l).ok())
        .find(|l| l.kind == "out" && l.get("job") == Some(&job))
        .unwrap_or_else(|| panic!("no out line for {job}: {history}"));
    assert_eq!(out_line.get("path"), Some(out.to_str().unwrap()));
    assert_eq!(out_line.get("in"), Some(src.to_str().unwrap()));
    assert!(
        history
            .lines()
            .any(|l| parse_line(l).is_ok_and(|l| l.kind == "job"
                && l.get("id") == Some(&job)
                && l.get("action") == Some("text.txt-to-md")
                && l.get("status") == Some("done"))),
        "{history}"
    );
}

#[test]
fn undo_end_to_end() {
    let e = Env::new("undo");
    let a = e.file("a.txt", b"a\n");
    let b = e.file("b.txt", b"b\n");
    let o = e.run(&["run", "markdown", "a.txt", "b.txt"]);
    assert_eq!(o.summary(), (2, 0, 0), "{}", o.stdout);
    let job = o.job();
    let (am, bm) = (e.files().join("a.md"), e.files().join("b.md"));
    // A file changed since the run is kept.
    fs::write(&bm, "edited\n").unwrap();

    let u = e.run(&["undo", &job]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    let removed: Vec<String> = u
        .kind("removed")
        .iter()
        .map(|l| l.bare_at(0).unwrap().to_owned())
        .collect();
    assert_eq!(removed, [am.display().to_string()]);
    assert_eq!(u.pairs("kept"), [(bm.clone(), "changed since".to_owned())]);
    assert!(!am.exists());
    assert_eq!(fs::read_to_string(&bm).unwrap(), "edited\n");
    assert!(a.exists() && b.exists());

    let again = e.run(&["undo", &job]);
    assert_eq!(again.code, Some(1));
    assert!(again.stdout.is_empty());
    assert!(again.stderr.contains("already undone"), "{}", again.stderr);

    let unknown = e.run(&["undo", "1-1"]);
    assert_eq!(unknown.code, Some(2));
    assert!(unknown.stderr.contains("unknown job"), "{}", unknown.stderr);
}

#[test]
fn review_focus_names() {
    let e = Env::new("names");
    let name = "a b\tc\nd=e -> é";
    let src = e.file(&format!("{name}.txt"), b"x\n");
    let o = e.run(&[OsStr::new("run"), OsStr::new("markdown"), src.as_os_str()]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.files().join(format!("{name}.md"));
    assert_eq!(o.pairs("done"), [(src.clone(), out.display().to_string())]);
    assert_eq!(fs::read_to_string(&out).unwrap(), "x\n");
    let job = o.job();
    let u = e.run(&["undo", &job]);
    assert_eq!(u.code, Some(0), "{}", u.stderr);
    let removed: Vec<String> = u
        .kind("removed")
        .iter()
        .map(|l| l.bare_at(0).unwrap().to_owned())
        .collect();
    assert_eq!(removed, [out.display().to_string()]);
    assert!(!out.exists() && src.exists());
}

#[test]
fn conflict_in_same_batch() {
    let e = Env::new("conflict");
    e.file("a.txt", b"one\n");
    e.file("a.text", b"two\n");
    let o = e.run(&["run", "markdown", "a.txt", "a.text"]);
    assert_eq!(o.summary(), (2, 0, 0), "{}", o.stdout);
    let mut outs: Vec<String> = o.pairs("done").into_iter().map(|(_, p)| p).collect();
    outs.sort();
    let f = |n: &str| e.files().join(n).display().to_string();
    assert_eq!(outs, [f("a (1).md"), f("a.md")]);
    let mut contents: Vec<String> = ["a.md", "a (1).md"]
        .iter()
        .map(|n| fs::read_to_string(e.files().join(n)).unwrap())
        .collect();
    contents.sort();
    assert_eq!(contents, ["one\n", "two\n"]);
}

#[test]
fn two_source_dirs_two_groups() {
    let e = Env::new("groups");
    let a1 = e.file("d1/a.txt", b"1\n");
    let a2 = e.file("d2/a.txt", b"2\n");
    let o = e.run(&["run", "markdown", "d1/a.txt", "d2/a.txt"]);
    assert_eq!(o.summary(), (2, 0, 0), "{}", o.stdout);
    let mut done = o.pairs("done");
    done.sort();
    let p = |n: &str| e.files().join(n).display().to_string();
    assert_eq!(done, [(a1, p("d1/a.md")), (a2, p("d2/a.md"))]);
    assert_eq!(
        fs::read_to_string(e.files().join("d1/a.md")).unwrap(),
        "1\n"
    );
    assert_eq!(
        fs::read_to_string(e.files().join("d2/a.md")).unwrap(),
        "2\n"
    );
}

#[test]
fn readonly_source_goes_to_downloads() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("readonly_source_goes_to_downloads: skipped as root");
        return;
    }
    let e = Env::new("readonly");
    let src = e.file("ro/a.txt", b"ro\n");
    let ro = e.files().join("ro");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let o = e.run(&["run", "markdown", "ro/a.txt"]);
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.root.join("home/Downloads/a.md");
    assert_eq!(o.pairs("done"), [(src, out.display().to_string())]);
    assert_eq!(fs::read_to_string(&out).unwrap(), "ro\n");
    assert!(!ro.join("a.md").exists());
}

#[test]
fn symlinks() {
    let e = Env::new("links");
    e.file("real/a.txt", b"linked\n");
    fs::create_dir_all(e.files().join("links")).unwrap();
    let l = e.files().join("links");
    symlink("../real/a.txt", l.join("la.txt")).unwrap();
    symlink("../real", l.join("ldir")).unwrap();
    symlink("../real/nothing.txt", l.join("broken.txt")).unwrap();
    let o = e.run(&[
        "run",
        "markdown",
        "links/la.txt",
        "links/ldir",
        "links/broken.txt",
    ]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.pairs("done"),
        [(l.join("la.txt"), l.join("la.md").display().to_string())]
    );
    assert_eq!(fs::read_to_string(l.join("la.md")).unwrap(), "linked\n");
    assert!(!e.files().join("real/a.md").exists());
    assert_eq!(
        o.pairs("skip"),
        [
            (l.join("ldir"), "not a regular file".to_owned()),
            (l.join("broken.txt"), "not a regular file".to_owned()),
        ]
    );
    assert_eq!(o.summary(), (1, 2, 0));
}

#[test]
fn same_file_given_twice_runs_once() {
    let e = Env::new("dedup");
    let a = e.file("a.txt", b"a\n");
    symlink("a.txt", e.files().join("link.txt")).unwrap();
    let o = e.run(&[
        "run",
        "markdown",
        "a.txt",
        "./a.txt",
        "sub/../a.txt",
        "link.txt",
    ]);
    fs::create_dir_all(e.files().join("sub")).unwrap();
    let o2 = e.run(&["run", "markdown", "a.txt", "sub/../a.txt"]);
    // `sub` did not exist on the first run: that path is a failure there.
    assert_eq!(o.pairs("done").len(), 1, "{}", o.stdout);
    assert_eq!(o.pairs("done")[0].0, a);
    assert_eq!(o2.pairs("done").len(), 1, "{}", o2.stdout);
    assert_eq!(o2.summary(), (1, 0, 0));
    assert!(e.files().join("a.md").exists() && e.files().join("a (1).md").exists());
    assert!(!e.files().join("link.md").exists());
}

#[test]
fn missing_input_fails() {
    let e = Env::new("missing");
    let o = e.run(&["run", "markdown", "nope.txt"]);
    assert_eq!(o.code, Some(1), "{}", o.stdout);
    let f = o.pairs("fail");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, e.files().join("nope.txt"));
    assert!(f[0].1.starts_with("cannot read: "), "{}", f[0].1);
    assert_eq!(o.summary(), (0, 0, 1));
}

#[test]
fn mixed_supported_and_unsupported_exit_0() {
    let e = Env::new("mixed");
    let a = e.file("a.txt", b"a\n");
    let b = e.file("b.pdf", b"%PDF-1.4\n");
    let c = e.file("c.md", b"# c\n");
    let o = e.run(&["run", "markdown", "a.txt", "b.pdf", "c.md"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.pairs("done").len(), 1);
    assert_eq!(o.pairs("done")[0].0, a);
    assert_eq!(
        o.pairs("skip"),
        [(b, "unsupported".to_owned()), (c, "unsupported".to_owned())]
    );
    assert_eq!(o.summary(), (1, 2, 0));
}

#[test]
fn all_unsupported_exit_3() {
    let e = Env::new("unsup");
    e.file("b.pdf", b"%PDF-1.4\n");
    e.file("c.bin", b"\x00\x01");
    let o = e.run(&["run", "markdown", "b.pdf", "c.bin"]);
    assert_eq!(o.code, Some(3), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.pairs("skip").len(), 2);
    assert_eq!(o.summary(), (0, 2, 0));
    // A direct action still requires its input type.
    e.file("d.md", b"d\n");
    let o = e.run(&["run", "--action", "text.txt-to-md", "d.md"]);
    assert_eq!(o.code, Some(3), "{}", o.stdout);
    assert_eq!(
        o.pairs("skip"),
        [(e.files().join("d.md"), "unsupported".to_owned())]
    );
}

#[test]
fn direct_action_runs() {
    let e = Env::new("direct");
    e.file("d.md", b"# Title\n\nbody\n");
    let o = e.run(&["run", "--action", "text.md-to-txt", "d.md"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.pairs("done"),
        [(
            e.files().join("d.md"),
            e.files().join("d.txt").display().to_string()
        )]
    );
    assert!(e.files().join("d.txt").is_file());
}

#[test]
fn catalog_from_actions_dir_list() {
    // Two folders in INACIRCLE_ACTIONS_DIR, one action binary in each.
    let e = Env::with_actions("catalog", &[]);
    let (d1, d2) = (e.root.join("a1"), e.root.join("a2"));
    fs::create_dir_all(&d1).unwrap();
    fs::create_dir_all(&d2).unwrap();
    symlink(action_bin("inacircle-text"), d1.join("inacircle-text")).unwrap();
    symlink(
        action_bin("inacircle-testaction"),
        d2.join("inacircle-testaction"),
    )
    .unwrap();
    e.file("a.txt", b"a\n");
    let t = e.test_input("t.test", &["events=skip"]);
    let dirs = format!("{}:{}", d1.display(), d2.display());
    let o = parse_out(
        e.cmd(&["run", "markdown", "a.txt"])
            .env("INACIRCLE_ACTIONS_DIR", &dirs)
            .output()
            .unwrap(),
    );
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let o = parse_out(
        e.cmd(&[
            OsStr::new("run"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            t.as_os_str(),
        ])
        .env("INACIRCLE_ACTIONS_DIR", &dirs)
        .output()
        .unwrap(),
    );
    assert_eq!(o.pairs("skip"), [(t, "skipped on request".to_owned())]);
    let cache = fs::read_to_string(e.root.join("cache/inacircle/catalog")).unwrap();
    assert!(cache.contains("text.txt-to-md") && cache.contains("test.script"));
    // Without the env list, nothing is found.
    let o = parse_out(
        e.cmd(&["run", "--action", "text.txt-to-md", "a.txt"])
            .env("INACIRCLE_ACTIONS_DIR", e.root.join("actions"))
            .output()
            .unwrap(),
    );
    assert_eq!(o.code, Some(2));
}

// ---- contract and sandbox through inacircle-testaction ----

fn run_test_action(e: &Env, inputs: &[&Path]) -> Out {
    let mut args: Vec<&OsStr> = vec!["run".as_ref(), "--action".as_ref(), "test.script".as_ref()];
    args.extend(inputs.iter().map(|p| p.as_os_str()));
    e.run(&args)
}

#[test]
fn sandbox_blocks_write_outside() {
    let e = Env::new("outside");
    let target = e.root.join("outside.txt");
    let t = e.test_input("t.test", &[&format!("write_outside={}", target.display())]);
    let o = run_test_action(&e, &[&t]);
    if skip_unsandboxed("sandbox_blocks_write_outside", &o) {
        let _ = fs::remove_file(&target);
        return;
    }
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    let f = o.pairs("fail");
    assert_eq!(f.len(), 1, "{}", o.stdout);
    assert!(f[0].1.contains("os error 13"), "{}", f[0].1);
    assert!(!target.exists());
}

#[test]
fn sandbox_blocks_network() {
    if !cfg!(target_arch = "x86_64") {
        eprintln!("sandbox_blocks_network: skipped, seccomp filter is x86_64 only");
        return;
    }
    let e = Env::new("network");
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    let t = e.test_input("t.test", &[&format!("connect=127.0.0.1:{port}")]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    let f = o.pairs("fail");
    assert_eq!(f.len(), 1, "{}", o.stdout);
    assert!(f[0].1.contains("os error 13"), "{}", f[0].1);
    assert!(o.pairs("warn").iter().all(|(_, m)| m != "connected"));
    assert_eq!(
        l.accept().map(|_| ()).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn fsize_limit_reports_output_too_large_or_fail() {
    let e = Env::new("fsize");
    // The limit is at least 256 MiB; write past it.
    let t = e.test_input("t.test", &[&format!("big_output={}", (256u64 + 1) << 20)]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    let f = o.pairs("fail");
    assert_eq!(f.len(), 1, "{}", o.stdout);
    assert!(
        f[0].1 == "output too large" || f[0].1.contains("File too large"),
        "{}",
        f[0].1
    );
}

#[test]
fn killed_by_signal_fails_inputs() {
    let e = Env::new("killed");
    let a = e.test_input("a.test", &["kill_self=9"]);
    let b = e.test_input("b.test", &["kill_self=9"]);
    let o = run_test_action(&e, &[&a, &b]);
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.pairs("fail"),
        [
            (a, "killed by signal 9".to_owned()),
            (b, "killed by signal 9".to_owned())
        ]
    );
    assert_eq!(o.summary(), (0, 0, 2));
}

#[test]
fn contract_violation_fails() {
    let e = Env::new("violation");
    let t = e.test_input("t.test", &["bad_done=../escape.bin"]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    let f = o.pairs("fail");
    assert_eq!(f.len(), 1, "{}", o.stdout);
    assert!(f[0].1.starts_with("contract violation: "), "{}", f[0].1);
    assert!(o.pairs("done").is_empty());
    assert!(!e.root.join("escape.bin").exists());
}

#[test]
fn progress_and_warn_passthrough() {
    let e = Env::new("progress");
    let t = e.test_input("t.test", &["events=progress:0.5,warn,done"]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let progress: Vec<f32> = o
        .kind("progress")
        .iter()
        .map(|l| l.bare_at(0).unwrap().parse().unwrap())
        .collect();
    assert!(progress.contains(&0.5), "{}", o.stdout);
    assert!(progress.windows(2).all(|w| w[0] < w[1]), "{progress:?}");
    assert_eq!(
        o.pairs("warn"),
        [(t.clone(), "warning on request".to_owned())]
    );
    assert_eq!(
        o.pairs("done"),
        [(t, e.files().join("t.bin").display().to_string())]
    );
    assert_eq!(o.summary(), (1, 0, 0));
}

#[test]
fn cancel_end_to_end() {
    let e = Env::new("cancel");
    let a = e.test_input("a.test", &["sleep_ms=10000", "events=done"]);
    let b = e.test_input("b.test", &[]);
    let mut child = e
        .cmd(&[
            OsStr::new("run"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            a.as_os_str(),
            b.as_os_str(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut rd = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    rd.read_line(&mut first).unwrap();
    let job = parse_line(&first).unwrap();
    assert_eq!(job.kind, "job", "{first}");
    let id = job.bare_at(0).unwrap().to_owned();

    let jobs = e.run(&["jobs"]);
    assert_eq!(jobs.code, Some(0));
    let mine = jobs.kind("job");
    assert_eq!(mine.len(), 1, "{}", jobs.stdout);
    assert_eq!(mine[0].get("id"), Some(id.as_str()));
    assert_eq!(mine[0].get("action"), Some("test.script"));
    assert_eq!(mine[0].get("pid"), Some(child.id().to_string().as_str()));
    assert!(mine[0].get("progress").is_some());

    let c = e.run(&["cancel", &id]);
    assert_eq!(c.code, Some(0), "{}", c.stderr);
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut rd, &mut rest).unwrap();
    let st = child.wait().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(st.code(), Some(1));
    let o = parse_out(std::process::Output {
        status: st,
        stdout: format!("{first}{rest}").into_bytes(),
        stderr: Vec::new(),
    });
    assert_eq!(
        o.pairs("fail"),
        [(a, "cancelled".to_owned()), (b, "cancelled".to_owned())]
    );
    assert_eq!(o.summary(), (0, 0, 2));
    assert!(e.run(&["jobs"]).lines.is_empty());
    assert!(e.history().contains(&format!("id={id}")));
    // Work dirs: checked by Env's drop.
}

#[test]
fn cancel_after_some_done_exits_0() {
    let e = Env::new("cancel-partial");
    // Two source folders: two processes, the first finishes before the cancel.
    let a = e.test_input("d1/a.test", &["events=done"]);
    let b = e.test_input("d2/b.test", &["sleep_ms=10000", "events=done"]);
    let mut child = e
        .cmd(&[
            OsStr::new("run"),
            OsStr::new("--action"),
            OsStr::new("test.script"),
            a.as_os_str(),
            b.as_os_str(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut rd = BufReader::new(child.stdout.take().unwrap());
    let mut seen = String::new();
    let mut id = None;
    loop {
        let mut l = String::new();
        assert!(rd.read_line(&mut l).unwrap() > 0, "eof before done: {seen}");
        seen.push_str(&l);
        let line = parse_line(&l).unwrap();
        match line.kind.as_str() {
            "job" => id = Some(line.bare_at(0).unwrap().to_owned()),
            "done" => break,
            _ => {}
        }
    }
    let c = e.run(&["cancel", &id.unwrap()]);
    assert_eq!(c.code, Some(0), "{}", c.stderr);
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut rd, &mut rest).unwrap();
    let st = child.wait().unwrap();
    let o = parse_out(std::process::Output {
        status: st,
        stdout: format!("{seen}{rest}").into_bytes(),
        stderr: Vec::new(),
    });
    assert_eq!(o.pairs("done").len(), 1, "{}", o.stdout);
    assert_eq!(o.pairs("fail"), [(b, "cancelled".to_owned())]);
    assert_eq!(o.summary(), (1, 0, 1));
    assert_eq!(o.code, Some(0), "{}", o.stdout);
}

#[test]
fn work_dirs_never_left_behind() {
    let e = Env::new("workdirs");
    let ok = e.test_input("ok/a.test", &["events=done"]);
    let killed = e.test_input("killed/a.test", &["events=progress:0.5", "kill_self=9"]);
    let bad = e.test_input("bad/a.test", &["events=done", "bad_done=0/none.bin"]);
    let exit2 = e.test_input("exit2/a.test", &["events=progress:0.1", "exit=2"]);
    let o = run_test_action(&e, &[&ok, &killed, &bad, &exit2]);
    assert_eq!(o.summary(), (1, 0, 3), "{}", o.stdout);
    assert!(work_dirs(&e.root).is_empty());
    for d in ["ok", "killed", "bad", "exit2"] {
        let names: Vec<_> = fs::read_dir(e.files().join(d))
            .unwrap()
            .map(|x| x.unwrap().file_name())
            .collect();
        let want = if d == "ok" { 2 } else { 1 };
        assert_eq!(names.len(), want, "{d}: {names:?}");
    }
}

// ---- the wheel offer ----

#[test]
fn offer_output() {
    let e = Env::new("offer");
    e.file("a.txt", b"a\n");
    e.file("b.md", b"b\n");
    e.file("c.md", b"c\n");
    let o = e.run(&["offer", "a.txt", "b.md", "c.md"]);
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "slice\tdir=4\tpreset=plain-text\tlabel=Plain text\tapplicable=2\nhub\t2 / 3 files\n"
    );
    let o = e.run(&["offer", "a.txt"]);
    assert_eq!(
        o.stdout,
        "slice\tdir=0\tpreset=markdown\tlabel=Markdown\tapplicable=1\nhub\ta.txt\n"
    );
}

#[test]
fn offer_counts_the_same_file_once() {
    let e = Env::new("offer-dedup");
    e.file("a.txt", b"a\n");
    let o = e.run(&["offer", "a.txt", "./a.txt"]);
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "slice\tdir=0\tpreset=markdown\tlabel=Markdown\tapplicable=1\nhub\ta.txt\n"
    );
}

#[test]
fn offer_pdfs_and_one_txt() {
    let e = Env::new("offer-pdf");
    let mut args = vec!["offer".to_owned()];
    for i in 0..5 {
        e.file(&format!("p{i}.pdf"), b"%PDF-1.7\n");
        args.push(format!("p{i}.pdf"));
    }
    e.file("n.txt", b"n\n");
    args.push("n.txt".into());
    let o = e.run(&args);
    assert_eq!(
        o.stdout,
        "slice\tdir=0\tpreset=markdown\tlabel=Markdown\tapplicable=1\nhub\t1 / 6 files\n"
    );
}

#[test]
fn offer_unknown_file() {
    let e = Env::new("offer-unknown");
    e.file("x.bin", b"\x00\x01\x02");
    let o = e.run(&["offer", "x.bin"]);
    assert_eq!(o.code, Some(0));
    assert_eq!(o.stdout, "hub\tno action for .bin file\n");
    fs::create_dir_all(e.files().join("dir")).unwrap();
    let o = e.run(&["offer", "dir"]);
    assert_eq!(o.stdout, "hub\tno action for folder\n");
}

#[test]
fn offer_without_actions_disables_slices() {
    let e = Env::with_actions("offer-none", &[]);
    e.file("a.txt", b"a\n");
    let o = e.run(&["offer", "a.txt"]);
    assert_eq!(
        o.stdout,
        "slice\tdir=0\tpreset=markdown\tlabel=Markdown\tapplicable=1\tdisabled=no action installed\nhub\ta.txt\n"
    );
    // Running the preset then has no route: the input is unsupported.
    let o = e.run(&["run", "markdown", "a.txt"]);
    assert_eq!(o.code, Some(3), "{}", o.stdout);
    assert_eq!(
        o.pairs("skip"),
        [(e.files().join("a.txt"), "unsupported".to_owned())]
    );
}

// ---- invocation ----

#[test]
fn invocation_errors_exit_2() {
    let e = Env::new("usage");
    e.file("a.txt", b"a\n");
    let cases: &[&[&str]] = &[
        &[],
        &["frobnicate"],
        &["run"],
        &["run", "markdown"],
        &["run", "no-such-preset", "a.txt"],
        &["run", "--action"],
        &["run", "--action", "no.such", "a.txt"],
        &["run", "--action", "text.txt-to-md"],
        &["offer"],
        &["undo"],
        &["undo", "1-1", "extra"],
        &["cancel"],
        &["cancel", "1-1"],
        &["jobs", "extra"],
        &["run", "markdown", ""],
        &["run", "markdown", "a.txt", ""],
        &["run", "--action", "text.txt-to-md", ""],
        &["offer", ""],
        &["offer", "a.txt", ""],
    ];
    for args in cases {
        let o = e.run(args);
        assert_eq!(o.code, Some(2), "{args:?}: {}", o.stdout);
        assert!(o.stdout.is_empty(), "{args:?}: {}", o.stdout);
        assert!(!o.stderr.is_empty(), "{args:?}");
    }
    assert!(!e.files().join("a.md").exists());
}

#[test]
fn jobs_empty_when_idle() {
    let e = Env::new("jobs");
    let o = e.run(&["jobs"]);
    assert_eq!(o.code, Some(0));
    assert!(o.stdout.is_empty());
}
