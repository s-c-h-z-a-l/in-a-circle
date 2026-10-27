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
    /// Whether the sandbox gate refused the run (no Landlock ABI >= 6).
    fn unsandboxed(&self) -> bool {
        self.pairs("fail")
            .iter()
            .any(|(_, m)| m.starts_with("sandbox unavailable"))
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
    let parts: Vec<&str> = job.split('-').collect();
    assert!(
        matches!(parts[..], [secs, pid, rand]
            if secs.parse::<u64>().is_ok()
                && pid.parse::<u32>().is_ok()
                && rand.len() == 8
                && u32::from_str_radix(rand, 16).is_ok()),
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
    // One cache per set of action dirs: `catalog-<hash>` for this list.
    let cache = fs::read_dir(e.root.join("cache/inacircle"))
        .unwrap()
        .map(|d| d.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("catalog-")
        })
        .map(|p| fs::read_to_string(p).unwrap())
        .expect("a catalog-<hash> cache");
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

// ---- two-step routes through plugin scripts ----

/// A `#!/bin/sh` plugin in `actions/`: `--manifest` prints one action
/// `in` -> `out`, `run` executes `body` (`$work`, `$list` set). Written
/// under a temporary name by `cp` and renamed, so no fd of this process
/// is open on it when another test's child execs it (ETXTBSY).
fn plugin(e: &Env, name: &str, input: &str, output: &str, ext: &str, body: &str) {
    let src = e.root.join(format!("{name}.src"));
    fs::write(
        &src,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --manifest ]; then\n\
             printf 'inacircle-manifest 0\\naction\\tp.{name}\\tfamily=p\\tin={input}\\tout={output}\\text={ext}\\tlabel={name}\\tgroup=p\\tcost=1\\tlossy=no\\tbatch=yes\\n'\n\
             exit 0\nfi\n\
             work=\"$4\"; list=\"$6\"\n{body}\n"
        ),
    )
    .unwrap();
    let tmp = e.root.join("actions").join(format!(".{name}.tmp"));
    assert!(
        Command::new("cp")
            .arg(&src)
            .arg(&tmp)
            .status()
            .unwrap()
            .success()
    );
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, e.root.join("actions").join(name)).unwrap();
}

/// Step 1 of the route text/plain -> text/x-mid -> text/markdown: copies
/// the first input to `0/a.mid`, reports it, then runs `after`.
fn mid_step(e: &Env, after: &str) {
    plugin(
        e,
        "s1",
        "text/plain",
        "text/x-mid",
        "mid",
        &format!(
            "in=$(head -n1 \"$list\"); mkdir -p \"$work/0\"; cat \"$in\" > \"$work/0/a.mid\"\n\
             printf 'done\\t%s\\t0/a.mid\\n' \"$in\"\n{after}"
        ),
    );
    plugin(
        e,
        "s2",
        "text/x-mid",
        "text/markdown",
        "md",
        "in=$(head -n1 \"$list\"); mkdir -p \"$work/0\"; { cat \"$in\"; echo +; } > \"$work/0/out.md\"\n\
         printf 'done\\t%s\\t0/out.md\\n' \"$in\"",
    );
}

/// True (and a note printed) when plugins could not run for lack of Landlock.
fn skip_no_sandbox(test: &str, o: &Out) -> bool {
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

#[test]
fn two_step_route_end_to_end() {
    let e = Env::with_actions("twostep", &[]);
    mid_step(&e, "");
    let a = e.file("doc.txt", b"text\n");
    let o = e.run(&["run", "markdown", "doc.txt"]);
    if skip_no_sandbox("two_step_route_end_to_end", &o) {
        return;
    }
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.files().join("doc.md");
    assert_eq!(o.pairs("done"), [(a, out.display().to_string())]);
    assert_eq!(fs::read_to_string(&out).unwrap(), "text\n+\n");
}

#[test]
fn two_step_intermediate_symlink_swap_is_refused() {
    let e = Env::with_actions("swap", &[]);
    let secret = e.root.join("secret/key");
    fs::create_dir_all(secret.parent().unwrap()).unwrap();
    fs::write(&secret, b"SECRET\n").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    mid_step(
        &e,
        &format!(
            "rm \"$work/0/a.mid\"; ln -s '{}' \"$work/0/a.mid\"",
            secret.display()
        ),
    );
    let a = e.file("doc.txt", b"text\n");
    let o = e.run(&["run", "markdown", "doc.txt"]);
    if skip_no_sandbox("two_step_intermediate_symlink_swap_is_refused", &o) {
        return;
    }
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.pairs("fail"),
        [(a, "intermediate output changed".to_owned())]
    );
    assert!(!e.files().join("doc.md").exists());
    for f in fs::read_dir(e.files()).unwrap().flatten() {
        let body = fs::read_to_string(f.path()).unwrap_or_default();
        assert!(
            !body.contains("SECRET"),
            "leaked into {}",
            f.path().display()
        );
    }
}

/// Neither a manifest nor an action sees the parent's environment: no
/// session socket, bus, display, loader switch or token, only the fixed
/// PATH and locale, and for an action its own TMPDIR (its work dir).
#[test]
fn sandbox_children_get_a_clean_environment() {
    let e = Env::with_actions("cleanenv", &[]);
    let src = e.root.join("envdump.src");
    // The manifest has no file to write: its verdict goes into the label.
    fs::write(
        &src,
        "#!/bin/sh\n\
         if [ \"$1\" = --manifest ]; then\n\
         l=clean\n\
         for v in SSH_AUTH_SOCK DBUS_SESSION_BUS_ADDRESS WAYLAND_DISPLAY LD_PRELOAD \\\n\
           INACIRCLE_FAKE_TOKEN HOME XDG_RUNTIME_DIR; do\n\
           eval \"[ -z \\\"\\${$v+x}\\\" ]\" || l=leaked-$v\n\
         done\n\
         [ \"$PATH\" = /usr/bin:/bin ] && [ \"$LC_ALL\" = C.UTF-8 ] && [ \"$LANG\" = C.UTF-8 ] \\\n\
           && [ \"$TMPDIR\" = /nonexistent ] || l=badfixed\n\
         printf 'inacircle-manifest 0\\naction\\tp.envdump\\tfamily=p\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=env-%s\\tgroup=p\\tcost=1\\tlossy=no\\tbatch=yes\\n' \"$l\"\n\
         exit 0\nfi\n\
         work=\"$4\"; list=\"$6\"\n\
         in=$(head -n1 \"$list\"); mkdir -p \"$work/0\"; env > \"$work/0/env.md\"\n\
         printf 'done\\t%s\\t0/env.md\\n' \"$in\"\n",
    )
    .unwrap();
    let tmp = e.root.join("actions/.envdump.tmp");
    assert!(
        Command::new("cp")
            .arg(&src)
            .arg(&tmp)
            .status()
            .unwrap()
            .success()
    );
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, e.root.join("actions/envdump")).unwrap();
    let a = e.file("doc.txt", b"text\n");
    let o = parse_out(
        e.cmd(&["run", "--action", "p.envdump", "doc.txt"])
            .env("SSH_AUTH_SOCK", "/run/user/1000/ssh-canary")
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/canary-bus")
            .env("WAYLAND_DISPLAY", "wayland-canary")
            // A real library, so the loader of inacircle itself stays quiet.
            .env("LD_PRELOAD", "libc.so.6")
            .env("INACIRCLE_FAKE_TOKEN", "token-canary-4711")
            .env("TMPDIR", "/tmp/parent-canary")
            .output()
            .unwrap(),
    );
    if skip_unsandboxed("sandbox_children_get_a_clean_environment", &o) {
        return;
    }
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.files().join("doc.md");
    assert_eq!(o.pairs("done"), [(a, out.display().to_string())]);
    let cache = fs::read_dir(e.root.join("cache/inacircle"))
        .unwrap()
        .map(|d| fs::read_to_string(d.unwrap().path()).unwrap_or_default())
        .collect::<String>();
    assert!(cache.contains("label=env-clean"), "manifest env: {cache}");
    let env = fs::read_to_string(&out).unwrap();
    for canary in [
        "SSH_AUTH_SOCK",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "LD_PRELOAD",
        "token-canary-4711",
        "parent-canary",
        "HOME=",
        "XDG_",
        "INACIRCLE_",
    ] {
        assert!(!env.contains(canary), "{canary} leaked: {env}");
    }
    let vars: Vec<&str> = env.lines().collect();
    for want in ["PATH=/usr/bin:/bin", "LC_ALL=C.UTF-8", "LANG=C.UTF-8"] {
        assert!(vars.contains(&want), "{want} missing: {env}");
    }
    let tmpdir = vars
        .iter()
        .find_map(|v| v.strip_prefix("TMPDIR="))
        .unwrap_or_else(|| panic!("no TMPDIR: {env}"));
    let tmpdir = Path::new(tmpdir);
    assert!(
        tmpdir.ends_with("tmp")
            && tmpdir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n.to_string_lossy().starts_with("step")),
        "TMPDIR is not <work>/tmp: {}",
        tmpdir.display()
    );
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

/// Unix sockets, pathname and abstract, and socket pairs: the listeners
/// take a connection from this (unsandboxed) process, the action gets
/// EACCES from seccomp for each, with or without Landlock.
#[test]
fn sandbox_blocks_unix_sockets() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    let e = Env::new("unix");
    let path = e.root.join("listener.sock");
    let named = UnixListener::bind(&path).unwrap();
    let name = format!("inacircle-cli-test-{}", std::process::id());
    let sa = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let abstract_ = UnixListener::bind_addr(&sa).unwrap();
    // Control: both listeners work.
    UnixStream::connect(&path).unwrap();
    UnixStream::connect_addr(&sa).unwrap();
    named.accept().unwrap();
    abstract_.accept().unwrap();
    named.set_nonblocking(true).unwrap();
    abstract_.set_nonblocking(true).unwrap();
    for (i, opt) in [
        format!("unix_connect={}", path.display()),
        format!("unix_connect=@{name}"),
        "socketpair=stream".to_owned(),
        "socketpair=dgram".to_owned(),
    ]
    .iter()
    .enumerate()
    {
        let t = e.test_input(&format!("t{i}.test"), &[opt]);
        let o = run_test_action(&e, &[&t]);
        assert_eq!(o.code, Some(1), "{opt}: {}{}", o.stdout, o.stderr);
        let f = o.pairs("fail");
        assert_eq!(f.len(), 1, "{opt}: {}", o.stdout);
        assert!(f[0].1.contains("os error 13"), "{opt}: {}", f[0].1);
        assert!(o.pairs("warn").iter().all(|(_, m)| m != "connected"));
    }
    for l in [&named, &abstract_] {
        assert_eq!(
            l.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
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

// ---- the RLIMIT_DATA memory net ----

const MIB: u64 = 1 << 20;

/// This machine's `RLIMIT_DATA` for an action.
fn data_cap() -> u64 {
    engine::sandbox::data_cap(engine::sandbox::total_ram())
}

#[test]
fn alloc_over_cap_fails() {
    let e = Env::new("alloc-over");
    // 256 MiB past the cap: the allocation must fail, the machine must not
    // notice (this test itself still runs).
    let t = e.test_input("t.test", &[&format!("alloc={}", data_cap() + 256 * MIB)]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    let f = o.pairs("fail");
    assert_eq!(f.len(), 1, "{}", o.stdout);
    assert_eq!(
        f[0].1,
        format!("out of memory (limit {} MiB)", data_cap() / MIB)
    );
    assert!(o.pairs("done").is_empty());
}

#[test]
fn alloc_under_cap_passes() {
    let e = Env::new("alloc-under");
    let t = e.test_input("t.test", &[&format!("alloc={}", 64 * MIB), "events=done"]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.pairs("done").len(), 1, "{}", o.stdout);
}

/// A multi-threaded action allocating under the cap passes.
#[test]
fn threaded_small_alloc_passes() {
    let e = Env::new("alloc-threads");
    let t = e.test_input(
        "t.test",
        &["threads=8", &format!("alloc={}", 64 * MIB), "events=done"],
    );
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.pairs("done").len(), 1, "{}", o.stdout);
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

    // A running job cannot be undone.
    let u = e.run(&["undo", &id]);
    assert_eq!(u.code, Some(1), "{}{}", u.stdout, u.stderr);
    assert!(u.stdout.is_empty(), "{}", u.stdout);
    assert!(
        u.stderr.contains("is running; cancel it first"),
        "{}",
        u.stderr
    );

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
fn undo_removes_a_multi_output_folder() {
    let e = Env::new("undo-many");
    let t = e.test_input("t.test", &["outputs=3"]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.summary(), (1, 0, 0), "{}", o.stdout);
    let folder = e.files().join("t");
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 3);
    assert!(e.history().contains("dir\tjob="), "{}", e.history());
    let u = e.run(&["undo", &o.job()]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    assert_eq!(u.kind("removed").len(), 3, "{}", u.stdout);
    assert!(!folder.exists());
}

/// Peak RSS stays within the 5 MB budget however many outcomes a run has:
/// 64 processes (source folders) × 1024 outputs each.
#[test]
fn many_groups_many_outputs_stay_within_rss_budget() {
    let e = Env::new("rss");
    let mut args: Vec<PathBuf> = Vec::new();
    for i in 0..64 {
        args.push(e.test_input(&format!("d{i}/o.test"), &["outputs=1024"]));
    }
    let mut c = e.cmd(&["run", "--action", "test.script"]);
    c.args(&args).env("INACIRCLE_MEASURE", "1");
    let o = parse_out(c.output().unwrap());
    assert_eq!(o.summary(), (64, 0, 0), "{}", o.stderr);
    assert_eq!(o.kind("done").len(), 64 * 1024);
    let kb: u64 = o
        .stderr
        .split_once("vm_hwm=")
        .and_then(|(_, r)| r.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no measure line: {}", o.stderr));
    assert!(kb <= 5 * 1024, "VmHWM {kb} kB > 5 MB");
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
fn offer_groups_output() {
    let e = Env::new("offer-groups");
    e.file("a.txt", b"a\n");
    e.file("b.md", b"b\n");
    e.file("x.bin", b"\x00\x01\x02");
    let o = e.run(&["offer", "--groups", "a.txt"]);
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "group\tdir=0\tid=document\tlabel=Document\tlit=yes\nhub\ta.txt\n"
    );
    let o = e.run(&["offer", "--groups", "a.txt", "b.md"]);
    assert_eq!(
        o.stdout,
        "group\tdir=0\tid=document\tlabel=Document\tlit=yes\nhub\t2 files\n"
    );
    // Nothing applies: the category stays, dim.
    let o = e.run(&["offer", "--groups", "x.bin"]);
    assert_eq!(
        o.stdout,
        "group\tdir=0\tid=document\tlabel=Document\tlit=no\nhub\tx.bin\n"
    );
    // The plain offer output is unchanged and `--groups` needs files.
    let o = e.run(&["offer", "--groups"]);
    assert_eq!(o.code, Some(2), "{}", o.stderr);
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
    // No arguments is the wheel (tests/wheel.rs), not an error.
    let cases: &[&[&str]] = &[
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

#[test]
fn unwritable_history_warns_once_and_the_run_proceeds() {
    let e = Env::new("nohistory");
    e.file("a.txt", b"hi\n");
    // The state home is a regular file: no history can be opened under it.
    let state = e.root.join("state-file");
    fs::write(&state, b"").unwrap();
    let o = parse_out(
        e.cmd(&["run", "markdown", "a.txt"])
            .env("XDG_STATE_HOME", &state)
            .output()
            .unwrap(),
    );
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (1, 0, 0));
    assert!(e.files().join("a.md").exists());
    let warns = o
        .stderr
        .lines()
        .filter(|l| l.starts_with("inacircle: history not written: "))
        .count();
    assert_eq!(warns, 1, "{}", o.stderr);
}

/// The value of `key` in the first `measure <what>` stderr line.
fn measure_field(stderr: &str, what: &str, key: &str) -> Option<i64> {
    let line = stderr
        .lines()
        .find(|l| l.starts_with(&format!("inacircle: measure {what} ")))?;
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))?
        .parse()
        .ok()
}

/// The measurement log carries the moment the first action event was
/// observed (CPU and `CLOCK_MONOTONIC`, absolute, with the process id the
/// wheel's report also carries); a child that emits no event leaves the
/// markers out, never 0.
#[test]
fn action_entry_marker_is_observed_once_and_absent_without_events() {
    let e = Env::new("entry");
    let ok = e.test_input("ok/a.test", &["events=progress:0,progress:0.5,done"]);
    let mut c = e.cmd(&["run", "--action", "test.script"]);
    c.arg(&ok).env("INACIRCLE_MEASURE", "1");
    let o = parse_out(c.output().unwrap());
    assert_eq!(o.summary(), (1, 0, 0), "{}", o.stderr);
    let cpu = measure_field(&o.stderr, "run", "action_entry_observed_abs_cpu_us");
    let wall = measure_field(&o.stderr, "run", "action_entry_observed_abs_wall_us");
    let (cpu, wall) = (cpu.expect("cpu marker"), wall.expect("wall marker"));
    let start = measure_field(&o.stderr, "run", "start_abs_cpu_us").unwrap();
    assert!(cpu >= start, "{cpu} < {start}");
    assert!(wall > 0);
    assert_eq!(
        measure_field(&o.stderr, "run", "action_entries"),
        Some(1),
        "{}",
        o.stderr
    );
    assert!(measure_field(&o.stderr, "run", "pid").is_some_and(|p| p > 1));

    let silent = e.test_input("silent/a.test", &["kill_self=9"]);
    let mut c = e.cmd(&["run", "--action", "test.script"]);
    c.arg(&silent).env("INACIRCLE_MEASURE", "1");
    let o = parse_out(c.output().unwrap());
    assert_eq!(
        measure_field(&o.stderr, "run", "action_entry_observed_abs_cpu_us"),
        None,
        "{}",
        o.stderr
    );
    assert_eq!(
        measure_field(&o.stderr, "run", "action_entry_observed_abs_wall_us"),
        None
    );
    assert_eq!(measure_field(&o.stderr, "run", "action_entries"), Some(0));

    // Without the switch, no marker line at all.
    let o = run_test_action(&e, &[&ok]);
    assert!(!o.stderr.contains("action_entry"), "{}", o.stderr);
}
