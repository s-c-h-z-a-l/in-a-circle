//! End-to-end tests: the real `inacircle` binary driving the real action
//! binaries (`inacircle-text`, `inacircle-testaction`) under the sandbox.
//!
//! Every test gets its own root with `INACIRCLE_ACTIONS_DIR` pointing at
//! links to the built action binaries and private XDG directories and HOME.
//! When a test ends without panicking, no `.inacircle-*` work dir may be
//! left anywhere under its root.

mod isolate;

use isolate::isolate;
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
const ACTIONS: [&str; 3] = ["inacircle-text", "inacircle-testaction", "inacircle-pages"];

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
        isolate(&mut c, &self.root)
            .args(args)
            .env("INACIRCLE_ACTIONS_DIR", self.root.join("actions"))
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

// ---- comic book to PDF through the real inacircle-pages ----

/// A two-page CBZ of generated 8x8 grayscale PNGs.
fn comic_book() -> Vec<u8> {
    let rows: Vec<u8> = (0..8).flat_map(|_| [0u8; 9]).collect();
    let page = testzip::png(8, 8, 0, None, false, false, &rows);
    testzip::zip(&[
        (b"page2.png", &page, testzip::Store::Stored),
        (b"page1.png", &page, testzip::Store::Deflated),
    ])
}

#[test]
fn cbz_to_pdf_end_to_end() {
    let e = Env::with_actions("cbz2pdf", &["inacircle-pages"]);
    let src = e.file("képregény.cbz", &comic_book());
    let o = e.run(&["run", "pdf", "képregény.cbz"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.files().join("képregény/képregény.pdf");
    assert_eq!(o.pairs("done"), [(src.clone(), out.display().to_string())]);
    assert_eq!(o.summary(), (1, 0, 0));
    assert!(fs::read(&out).unwrap().starts_with(b"%PDF-"));
    // The source is untouched.
    assert_eq!(fs::read(&src).unwrap(), comic_book());
    let job = o.job();
    let history = e.history();
    let commit = history
        .lines()
        .filter_map(|l| parse_line(l).ok())
        .find(|l| l.kind == "publish-commit" && l.get("job") == Some(&job))
        .unwrap_or_else(|| panic!("no publish-commit in {history}"));
    // The history percent-encodes non-ASCII bytes of a path.
    assert_eq!(
        commit.get("actual"),
        Some(format!("{}/k%C3%A9preg%C3%A9ny", e.files().display()).as_str())
    );
    assert!(
        history
            .lines()
            .any(|l| parse_line(l).is_ok_and(|l| l.kind == "job"
                && l.get("id") == Some(&job)
                && l.get("action") == Some("pages.cbz-to-pdf")
                && l.get("status") == Some("done"))),
        "{history}"
    );

    // The wheel's offer for the same file.
    let o = e.run(&["offer", "képregény.cbz"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("preset=pdf"), "{}", o.stdout);
}

/// A PNG page for the image-join tests: 8x8, 8-bit gray, no alpha.
fn gray_png() -> Vec<u8> {
    let rows: Vec<u8> = (0..8).flat_map(|_| [0u8; 9]).collect();
    testzip::png(8, 8, 0, None, false, false, &rows)
}

/// A JPEG page for the image-join tests (a probe fixture, not duplicated).
fn rgb_jpeg() -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/probe/tests/data/rgb.jpg");
    fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Two images and a text file through the real `inacircle-pages` under the
/// real sandbox: one PDF of two pages in the folder-named wrapper, one
/// history group, the text file skipped, and Undo trashes the wrapper.
#[test]
fn images_to_pdf_end_to_end() {
    let e = Env::with_actions("img2pdf", &["inacircle-pages"]);
    let jpg = rgb_jpeg();
    let png = gray_png();
    e.file("Nyaralás/1.jpg", &jpg);
    e.file("Nyaralás/2.png", &png);
    e.file("Nyaralás/jegyzet.txt", b"nem kep\n");
    let o = e.run(&[
        "run",
        "pdf",
        "Nyaralás/1.jpg",
        "Nyaralás/2.png",
        "Nyaralás/jegyzet.txt",
    ]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let wrapper = e.files().join("Nyaralás/Nyaralás");
    let out = wrapper.join("Nyaralás.pdf");
    assert_eq!(o.summary(), (2, 1, 0), "{}", o.stdout);
    let done = o.pairs("done");
    assert_eq!(done.len(), 2, "{}", o.stdout);
    assert!(done.iter().all(|(_, p)| Path::new(p) == out), "{done:?}");
    assert_eq!(
        o.pairs("skip"),
        [(
            e.files().join("Nyaralás/jegyzet.txt"),
            "unsupported".to_owned()
        )]
    );
    let pdf = fs::read(&out).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    // Two pages: one `/Type /Page ` object per image (`/Pages` is the tree).
    let pages = pdf.windows(12).filter(|w| w == b"/Type /Page ").count();
    assert_eq!(pages, 2, "page objects in the PDF");

    let job = o.job();
    let h = e.history();
    let mine: Vec<&str> = h.lines().filter(|l| l.contains(&job)).collect();
    let count = |kind: &str| {
        mine.iter()
            .filter(|l| l.split('\t').next() == Some(kind))
            .count()
    };
    assert_eq!(count("group-begin"), 1, "{h}");
    assert_eq!(count("publish-commit"), 1, "{h}");
    assert_eq!(count("skip"), 1, "{h}");
    assert!(
        mine.iter()
            .any(|l| l.starts_with("skip\t") && l.contains("reason=unsupported")),
        "{h}"
    );

    let u = e.run(&["undo", &job]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    let trash = e.root.join("home/.local/share/Trash/files");
    assert!(!wrapper.exists(), "the wrapper is still there");
    assert!(trash.join("Nyaralás/Nyaralás.pdf").is_file());
    // The inputs are untouched.
    assert_eq!(fs::read(e.files().join("Nyaralás/1.jpg")).unwrap(), jpg);
    assert_eq!(fs::read(e.files().join("Nyaralás/2.png")).unwrap(), png);
}

/// `run --action <id>` reaches a join action too: the same single PDF the
/// preset makes, with its inputs as one group; a preset id is not an action
/// id, and a bare `--action` is a usage error.
#[test]
fn direct_action_runs_a_join_action() {
    let e = Env::with_actions("direct-join", &["inacircle-pages"]);
    e.file("a/1.jpg", &rgb_jpeg());
    e.file("a/2.png", &gray_png());
    let o = e.run(&[
        "run",
        "--action",
        "pages.images-to-pdf",
        "a/1.jpg",
        "a/2.png",
    ]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (2, 0, 0), "{}", o.stdout);
    let pdf = fs::read(e.files().join("a/a/a.pdf")).expect("no PDF written");
    assert_eq!(
        pdf.windows(12).filter(|w| w == b"/Type /Page ").count(),
        2,
        "page objects in the PDF"
    );
    // A preset id after `--action` is no action: nothing runs.
    let o = e.run(&["run", "--action", "pdf", "a/1.jpg"]);
    assert_ne!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    assert!(o.pairs("done").is_empty(), "{}", o.stdout);
    // The flag without an id is a usage error, not a preset named "--action".
    let o = e.run(&["run", "--action"]);
    assert!(
        o.stderr.contains("--action needs an action id"),
        "{}",
        o.stderr
    );
    // A lone preset id is a preset with no files, not a bare flag.
    let o = e.run(&["run", "pdf"]);
    assert!(
        o.stderr.contains("no files") && !o.stderr.contains("needs an action id"),
        "{}",
        o.stderr
    );
}

/// An action whose tool is missing fails the inputs it would take, with the
/// missing tool named; an input it does not take is still skipped.
#[test]
fn missing_tool_fails_matching_inputs() {
    let e = Env::with_actions("needy", &[]);
    let bin = e.root.join("actions/needy");
    fs::write(
        &bin,
        "#!/bin/sh\n\
         printf 'inacircle-manifest 0\\naction\\tp.needy\\tfamily=p\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=n\\tgroup=p\\tcost=1\\tlossy=no\\tbatch=yes\\trequires=no-such-tool-inacircle\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let t = e.file("doc.txt", b"text\n");
    let o = e.run(&["run", "--action", "p.needy", "doc.txt"]);
    if skip_unsandboxed("missing_tool_fails_matching_inputs", &o) {
        return;
    }
    assert_eq!(
        o.pairs("fail"),
        [(t, "needs no-such-tool-inacircle".to_owned())],
        "{}{}",
        o.stdout,
        o.stderr
    );
    assert!(o.pairs("skip").is_empty(), "{}", o.stdout);
}

// ---- text conversions through the real inacircle-text ----

#[test]
fn txt_to_md_end_to_end() {
    let e = Env::new("txt2md");
    let src = e.file("jegyzet.txt", b"Hello *world*\n\n# not heading\n");
    // A relative path is taken against the cwd (`files/`).
    let o = e.run(&["run", "markdown", "jegyzet.txt"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    // Every input gets its own folder, a single output too.
    let out = e.files().join("jegyzet/jegyzet.md");
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

    // The job's start first; the group is recorded with its baseline
    // before the publishing rename and its committed place after it.
    let history = e.history();
    assert!(history.starts_with("inacircle-history 1\n"), "{history}");
    let records: Vec<Line> = history
        .lines()
        .filter_map(|l| parse_line(l).ok())
        .filter(|l| l.get("job") == Some(&job))
        .collect();
    let kinds: Vec<&str> = records.iter().map(|l| l.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "job-start",
            "group-begin",
            "child",
            "group-end",
            "publish-intent",
            "publish-commit"
        ],
        "{history}"
    );
    assert_eq!(records[2].get("rel"), Some("jegyzet.md"));
    let folder = e.files().join("jegyzet");
    assert_eq!(records[4].get("target"), Some(folder.to_str().unwrap()));
    assert_eq!(records[5].get("actual"), Some(folder.to_str().unwrap()));
    assert_eq!(records[5].get("durability"), Some("synced"));
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

/// Undo moves each input's group whole into the Trash (the test's own
/// `XDG_DATA_HOME`), with its `.trashinfo`; a group with a changed file is
/// kept whole, and the CLI prints one line per group.
#[test]
fn undo_end_to_end() {
    let e = Env::new("undo");
    let a = e.file("a.txt", b"a\n");
    let b = e.file("b.txt", b"b\n");
    let o = e.run(&["run", "markdown", "a.txt", "b.txt"]);
    assert_eq!(o.summary(), (2, 0, 0), "{}", o.stdout);
    let job = o.job();
    let (am, bm) = (e.files().join("a/a.md"), e.files().join("b/b.md"));
    // A file changed since the run keeps its group.
    fs::write(&bm, "edited\n").unwrap();

    let u = e.run(&["undo", &job]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    let trash = e.root.join("home/.local/share/Trash");
    let moved: Vec<String> = u
        .kind("moved")
        .iter()
        .map(|l| l.bare_at(0).unwrap().to_owned())
        .collect();
    assert_eq!(moved, [trash.join("files/a").display().to_string()]);
    assert_eq!(
        u.pairs("kept"),
        [(e.files().join("b"), "Changed".to_owned())]
    );
    assert!(!am.exists() && !e.files().join("a").exists());
    assert_eq!(
        fs::read_to_string(trash.join("files/a/a.md")).unwrap(),
        "a\n"
    );
    let info = fs::read_to_string(trash.join("info/a.trashinfo")).unwrap();
    assert!(info.starts_with("[Trash Info]\nPath=/"), "{info}");
    assert!(info.contains("/files/a\nDeletionDate="), "{info}");
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

/// Without a safe Trash (a symlink in place of the home Trash) nothing
/// moves: the group is kept and the link's target untouched.
#[test]
fn undo_without_a_safe_trash_keeps_the_outputs() {
    let e = Env::new("undo-notrash");
    e.file("a.txt", b"a\n");
    let o = e.run(&["run", "markdown", "a.txt"]);
    assert_eq!(o.summary(), (1, 0, 0), "{}", o.stdout);
    let share = e.root.join("home/.local/share");
    fs::create_dir_all(&share).unwrap();
    let elsewhere = e.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, share.join("Trash")).unwrap();
    let u = e.run(&["undo", &o.job()]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    assert_eq!(
        u.pairs("kept"),
        [(e.files().join("a"), "NoTrash".to_owned())]
    );
    assert!(e.files().join("a/a.md").exists());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

/// A group whose folder is gone from where it was saved is kept, but not
/// provably in place: the line has no path and the exit is 1.
#[test]
fn undo_of_a_group_not_found_exits_1() {
    let e = Env::new("undo-missing");
    e.file("a.txt", b"a\n");
    let o = e.run(&["run", "markdown", "a.txt"]);
    assert_eq!(o.summary(), (1, 0, 0), "{}", o.stdout);
    fs::rename(e.files().join("a"), e.root.join("a-moved")).unwrap();
    let u = e.run(&["undo", &o.job()]);
    assert_eq!(u.code, Some(1), "{}{}", u.stdout, u.stderr);
    assert!(u.stdout.contains("kept\t-\tMissing"), "{}", u.stdout);
    assert!(e.root.join("a-moved/a.md").exists());
}

#[test]
fn review_focus_names() {
    let e = Env::new("names");
    let name = "a b\tc\nd=e -> é";
    let src = e.file(&format!("{name}.txt"), b"x\n");
    let o = e.run(&[OsStr::new("run"), OsStr::new("markdown"), src.as_os_str()]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let out = e.files().join(name).join(format!("{name}.md"));
    assert_eq!(o.pairs("done"), [(src.clone(), out.display().to_string())]);
    assert_eq!(fs::read_to_string(&out).unwrap(), "x\n");
    let job = o.job();
    let u = e.run(&["undo", &job]);
    assert_eq!(u.code, Some(0), "{}", u.stderr);
    let moved: Vec<String> = u
        .kind("moved")
        .iter()
        .map(|l| l.bare_at(0).unwrap().to_owned())
        .collect();
    let trash = e.root.join("home/.local/share/Trash");
    assert_eq!(
        moved,
        [trash.join("files").join(name).display().to_string()]
    );
    assert!(!out.exists() && src.exists());
    assert!(!e.files().join(name).exists());
    // The Trash spec's `Path=`: every byte outside the unreserved set
    // percent-encoded.
    let info = fs::read_to_string(trash.join("info").join(format!("{name}.trashinfo"))).unwrap();
    assert!(
        info.contains("/files/a%20b%09c%0Ad%3De%20-%3E%20%C3%A9\nDeletionDate="),
        "{info}"
    );
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
    assert_eq!(outs, [f("a (1)/a.md"), f("a/a.md")]);
    let mut contents: Vec<String> = ["a/a.md", "a (1)/a.md"]
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
    assert_eq!(done, [(a1, p("d1/a/a.md")), (a2, p("d2/a/a.md"))]);
    assert_eq!(
        fs::read_to_string(e.files().join("d1/a/a.md")).unwrap(),
        "1\n"
    );
    assert_eq!(
        fs::read_to_string(e.files().join("d2/a/a.md")).unwrap(),
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
    let out = e.root.join("home/Downloads/a/a.md");
    assert_eq!(o.pairs("done"), [(src, out.display().to_string())]);
    assert_eq!(fs::read_to_string(&out).unwrap(), "ro\n");
    assert!(!ro.join("a").exists());
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
        [(l.join("la.txt"), l.join("la/la.md").display().to_string())]
    );
    assert_eq!(fs::read_to_string(l.join("la/la.md")).unwrap(), "linked\n");
    assert!(!e.files().join("real/a").exists());
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
    assert!(e.files().join("a/a.md").exists() && e.files().join("a (1)/a.md").exists());
    assert!(!e.files().join("link").exists());
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
            e.files().join("d/d.txt").display().to_string()
        )]
    );
    assert!(e.files().join("d/d.txt").is_file());
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

/// The catalog cache file(s) of `e`, as (path, text).
fn catalog_caches(e: &Env) -> Vec<(PathBuf, String)> {
    fs::read_dir(e.root.join("cache/inacircle"))
        .unwrap()
        .map(|d| d.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("catalog")
        })
        .map(|p| {
            let t = fs::read_to_string(&p).unwrap();
            (p, t)
        })
        .collect()
}

/// `text.txt-to-md` is `lossy=yes` everywhere the catalog is seen (spec
/// 5): the shipped manifest, the cache, and so the planner (no lossless
/// intermediate) and the notice. A cache written by the old binary (said
/// `lossy=no`) cannot stay active: its stamp (size, mtime) is the old
/// binary's, so the next run rescans and rewrites it.
#[test]
fn txt_to_md_is_lossy_in_the_catalog_and_a_stale_cache_is_replaced() {
    let e = Env::new("lossy-catalog");
    e.file("a.txt", b"alpha\n\n\nbeta\n");
    let o = e.run(&["run", "--action", "text.txt-to-md", "a.txt"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let action_line = |t: &str| {
        t.lines()
            .find(|l| l.starts_with("action\ttext.txt-to-md\t"))
            .map(str::to_owned)
    };
    let caches = catalog_caches(&e);
    assert_eq!(caches.len(), 1);
    let (path, text) = &caches[0];
    assert!(
        action_line(text).unwrap().contains("\tlossy=yes\t"),
        "{text}"
    );
    // The previous binary's cache: lossless claim, other size stamp.
    let stale: String = text
        .lines()
        .map(|l| {
            if l.starts_with("action\ttext.txt-to-md\t") {
                l.replace("\tlossy=yes\t", "\tlossy=no\t")
            } else if l.starts_with("bin\t") && l.contains("inacircle-text") {
                l.split('\t')
                    .map(|f| if f.starts_with("size=") { "size=1" } else { f })
                    .collect::<Vec<_>>()
                    .join("\t")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_ne!(&stale, text);
    fs::write(path, &stale).unwrap();
    let o = e.run(&["run", "--action", "text.txt-to-md", "a.txt"]);
    assert_eq!(o.code, Some(0), "{}{}", o.stdout, o.stderr);
    let fresh = catalog_caches(&e);
    let line = action_line(&fresh[0].1).unwrap();
    assert!(
        line.contains("\tlossy=yes\t"),
        "stale lossless cache stayed: {line}"
    );
    // The notice says so: the run is reported lossy.
    assert!(
        o.pairs("warn")
            .iter()
            .any(|(_, m)| m.contains("blank lines")),
        "{}",
        o.stdout
    );
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
    let out = e.files().join("doc/doc.md");
    assert_eq!(o.pairs("done"), [(a, out.display().to_string())]);
    assert_eq!(fs::read_to_string(&out).unwrap(), "text\n+\n");
}

/// THP: `inacircle` runs with transparent huge pages disabled
/// (`THP_enabled: 0`, so a small heap is not backed by a 2 MiB page). The
/// action child gets the default back; that half is tested in the
/// engine's sandbox tests, where the child can read `/proc`.
#[test]
fn thp_is_disabled_in_the_main_process() {
    // The child inherits the harness's flag and restores 1 before main; if
    // the harness itself had THP disabled, "child has 0" would prove nothing.
    let own = fs::read_to_string("/proc/self/status").unwrap_or_default();
    if !own
        .lines()
        .any(|l| l.split_whitespace().eq(["THP_enabled:", "1"]))
    {
        let msg = "thp test: the harness has THP disabled (or /proc unreadable), the check would be vacuous";
        assert!(
            !landlock_required() && std::env::var_os("CI").is_none(),
            "{msg}"
        );
        eprintln!("{msg}: skipped");
        return;
    }
    let e = Env::with_actions("thp", &[]);
    // A plugin that keeps the engine alive for a second while its status is
    // read; it then ends normally so the engine cleans its work directory.
    plugin(&e, "thp", "text/plain", "text/markdown", "md", "sleep 1");
    e.file("doc.txt", b"text\n");
    let child = e
        .cmd(&["run", "markdown", "doc.txt"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = format!("/proc/{}/status", child.id());
    // Between fork and main the process still has the test's value (1), so
    // poll until main has run; 800 ms is far above a start (~1 ms) and
    // under the plugin's 1 s, so if the call is ever missing the loop ends
    // with the engine still alive and the assert fails.
    let deadline = Instant::now() + Duration::from_millis(800);
    let mut line = String::new();
    while Instant::now() < deadline {
        line = fs::read_to_string(&status)
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("THP_enabled:"))
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if line == "THP_enabled: 0" {
            break;
        }
        // 10 ms: ten starts (~1 ms), so the flag is seen within one tick
        // of main; 80 polls fit under the deadline at negligible CPU.
        std::thread::sleep(Duration::from_millis(10));
    }
    // Without Landlock the engine refuses the plugin and exits at once, so
    // the status read above raced; follow the sandbox skip/require rule.
    let o = parse_out(child.wait_with_output().unwrap());
    if skip_no_sandbox("thp_is_disabled_in_the_main_process", &o) {
        return;
    }
    assert_eq!(line, "THP_enabled: 0");
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
    // The swap runs right after step 1 prints `done`: the engine refuses
    // it either when it checks the `done` output (the name is gone or is
    // the link by then) or when it rechecks it before step 2. Both are
    // refusals; which one depends only on scheduling.
    let fails = o.pairs("fail");
    assert_eq!(fails.len(), 1, "{}{}", o.stdout, o.stderr);
    assert_eq!(fails[0].0, a);
    let why = &fails[0].1;
    assert!(
        why == "intermediate output changed"
            || why.starts_with("contract violation: done output 0/a.mid: ")
            || why
                == "contract violation: done output 0/a.mid is not a regular file in the work directory",
        "{why}"
    );
    assert!(!e.files().join("doc").exists());
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
    let out = e.files().join("doc/doc.md");
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
        [(t, e.files().join("t/t.bin").display().to_string())]
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
fn undo_moves_a_multi_output_folder_whole() {
    let e = Env::new("undo-many");
    let t = e.test_input("t.test", &["outputs=3"]);
    let o = run_test_action(&e, &[&t]);
    assert_eq!(o.summary(), (1, 0, 0), "{}", o.stdout);
    let folder = e.files().join("t");
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 3);
    assert!(e.history().contains("\tchildren=3\n"), "{}", e.history());
    let u = e.run(&["undo", &o.job()]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    assert_eq!(u.kind("moved").len(), 1, "{}", u.stdout);
    assert!(!folder.exists());
    let moved = e.root.join("home/.local/share/Trash/files/t");
    assert_eq!(fs::read_dir(moved).unwrap().count(), 3);
}

/// The budget is the shipped `inacircle`'s peak PSS <= 5 MB, however many
/// outcomes a run has: 64 processes (source folders) x 1024 outputs each.
/// Gated on the release binary only (`cargo test --release`, a CI step of
/// its own): peak RSS (`VmHWM`) <= 5 000 000 B. RSS counts shared pages
/// whole, so RSS >= PSS and this is stricter than the budget. A debug
/// build is not the shipped artifact: its 15 MB binary's code pages
/// (kernel fault-around) dominate its RSS (5048-5140 kB here,
/// 5428 kB on ARM CI, with the heap, RssAnon ~850 kB, unchanged; release
/// ~3.9 MB), so there the number is printed, not judged, unless
/// `INACIRCLE_RSS_GATE` is set (CI), which makes a non-release build a
/// failure.
#[test]
fn many_groups_many_outputs_stay_within_rss_budget() {
    let release = Path::new(BIN)
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|d| d == "release");
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
    // CI sets the gate: that step must measure the shipped binary, so a
    // changed profile or target layout fails it instead of passing as a
    // diagnostic.
    assert!(
        release || std::env::var_os("INACIRCLE_RSS_GATE").is_none(),
        "INACIRCLE_RSS_GATE is set but {BIN} is not a release build"
    );
    if !release {
        eprintln!("diagnostic only (debug build, not gated): VmHWM {kb} kB");
        return;
    }
    eprintln!("release VmHWM {kb} kB");
    assert!(
        kb * 1024 <= 5_000_000,
        "VmHWM {kb} kB > 5 000 000 B (release)"
    );
}

/// The largest of a `/proc` field's values (`None` while the file is
/// unreadable, e.g. the process just exited).
fn proc_field_kb(path: &str, key: &str) -> Option<u64> {
    fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// One path of the invocation's worst realistic case: ~1 KiB, so 1024 of
/// them fill the 1 MiB input list limit (`action::INPUT_LIST_BYTES`) with
/// the escape's newline; the same length as the wheel's large-drop test
/// (a 1023-byte `file://` URI line).
const LONG_PATH: usize = 1014;
/// The invocation's own input count limit (`action::INPUT_COUNT`).
const MANY_INPUTS: usize = 1024;

/// The budget holds through a whole run at the invocation's limits:
/// `inacircle run` over 1024 inputs whose paths are ~1 KiB each (~1 MiB
/// of paths), one batch process, one output group per input. Peak PSS of
/// the `inacircle` process (engine and supervisor) is sampled from
/// `smaps_rollup` every 200 us until exit (the run takes about 100 ms, so
/// a few hundred samples; a sample costs about as long) and must stay within
/// 5,000,000 B in the release build the budget is for; a debug build
/// prints it.
#[test]
fn many_long_paths_stay_within_the_pss_budget() {
    let release = Path::new(BIN)
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|d| d == "release");
    let e = Env::new("longpaths");
    let mut dir = e.files();
    for c in ['a', 'b', 'c', 'd'] {
        dir = dir.join(c.to_string().repeat(240));
    }
    fs::create_dir_all(&dir).unwrap();
    let mut paths = Vec::with_capacity(MANY_INPUTS);
    for i in 0..MANY_INPUTS {
        let plen = dir.join(format!("{i:04}.txt")).as_os_str().len();
        let pad = LONG_PATH.saturating_sub(plen);
        let p = dir.join(format!("{i:04}{}.txt", "_".repeat(pad)));
        fs::write(&p, "w\n").unwrap();
        paths.push(p);
    }
    let bytes: usize = paths.iter().map(|p| p.as_os_str().len() + 1).sum();
    assert!(bytes <= 1_048_576, "{bytes} bytes of paths");
    let stdout = e.root.join("stdout");
    let mut c = e.cmd(&["run", "markdown"]);
    c.args(&paths)
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(Stdio::piped());
    let child = c.spawn().unwrap();
    let pid = child.id();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = std::thread::spawn({
        let stop = stop.clone();
        move || {
            let (mut pss, mut hwm, mut n) = (0u64, 0u64, 0u64);
            let (roll, status) = (
                format!("/proc/{pid}/smaps_rollup"),
                format!("/proc/{pid}/status"),
            );
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(kb) = proc_field_kb(&roll, "Pss:") {
                    pss = pss.max(kb);
                    n += 1;
                }
                hwm = hwm.max(proc_field_kb(&status, "VmHWM:").unwrap_or(0));
                std::thread::sleep(Duration::from_micros(200));
            }
            (pss, hwm, n)
        }
    });
    let o = child.wait_with_output().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (pss, hwm, samples) = sampler.join().unwrap();
    let mut out = parse_out(o);
    out.stdout = fs::read_to_string(&stdout).unwrap();
    out.lines = out
        .stdout
        .split_inclusive('\n')
        .map(|l| parse_line(l).unwrap_or_else(|e| panic!("bad line {l:?}: {e:?}")))
        .collect();
    if skip_unsandboxed("many_long_paths_stay_within_the_pss_budget", &out) {
        return;
    }
    eprintln!(
        "{MANY_INPUTS} x ~1 KiB paths, CLI run: sampled max PSS {pss} kB, VmHWM {hwm} kB, \
         {samples} samples"
    );
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.summary(), (MANY_INPUTS, 0, 0), "{}", out.stderr);
    assert!(samples > 0, "no PSS sample: incomplete evidence");
    if release {
        // 5,000,000 B (CLAUDE.md); `Pss` is in KiB: multiply by 1024.
        assert!(pss * 1024 <= 5_000_000, "PSS {pss} kB over the budget");
    }
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
        "group\tdir=0\tid=document\tlabel=Document\tlit=yes\ngroup\tdir=2\tid=book\tlabel=Book\tlit=no\nhub\ta.txt\n"
    );
    let o = e.run(&["offer", "--groups", "a.txt", "b.md"]);
    assert_eq!(
        o.stdout,
        "group\tdir=0\tid=document\tlabel=Document\tlit=yes\ngroup\tdir=2\tid=book\tlabel=Book\tlit=no\nhub\t2 files\n"
    );
    // Nothing applies: the category stays, dim.
    let o = e.run(&["offer", "--groups", "x.bin"]);
    assert_eq!(
        o.stdout,
        "group\tdir=0\tid=document\tlabel=Document\tlit=no\ngroup\tdir=2\tid=book\tlabel=Book\tlit=no\nhub\tx.bin\n"
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

/// A `#!/bin/sh` plugin that declares the image join action (its manifest
/// line is the shipped one) and runs [`JOIN_ALL`]. Installed like `plugin`.
fn join_plugin(e: &Env) {
    join_plugin_with(e, JOIN_ALL);
}

/// Takes every input into `<work>/join/out.pdf` and reports `joined` once.
const JOIN_ALL: &str = "work=$4; mkdir -p \"$work/join\"; : > \"$work/join/out.pdf\"\n\
     while IFS= read -r in; do cat \"$in\" >> \"$work/join/out.pdf\"; printf 'take\\t%s\\n' \"$in\"; done < \"$6\"\n\
     printf 'joined\\tjoin/out.pdf\\n'";

/// [`join_plugin`] running `body` (`$4` the work dir, `$6` the input list).
fn join_plugin_with(e: &Env, body: &str) {
    let src = e.root.join("joiner.src");
    fs::write(
        &src,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --manifest ]; then\n\
             printf 'inacircle-manifest 0\\naction\\tp.images-to-pdf\\tfamily=p\\tin=image/jpeg,image/png\\tout=application/pdf\\text=pdf\\tlabel=PDF\\tgroup=book\\tcost=10\\tlossy=yes\\tbatch=yes\\tjoin=yes\\tmin=1\\n'\n\
             exit 0\nfi\n{body}\n"
        ),
    )
    .unwrap();
    let tmp = e.root.join("actions").join(".joiner.tmp");
    assert!(
        Command::new("cp")
            .arg(&src)
            .arg(&tmp)
            .status()
            .unwrap()
            .success()
    );
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, e.root.join("actions").join("joiner")).unwrap();
}

#[test]
fn offer_images_shows_join_count() {
    let e = Env::with_actions("offer-images", &[]);
    join_plugin(&e);
    e.file("a.jpg", b"\xff\xd8\xff\xe0 jpeg");
    e.file("b.png", b"\x89PNG\r\n\x1a\n....");
    e.file("c.txt", b"c\n");
    let o = e.run(&["offer", "a.jpg", "b.png", "c.txt"]);
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "slice\tdir=0\tpreset=pdf\tlabel=PDF\tapplicable=2\n\
         slice\tdir=4\tpreset=cbz\tlabel=CBZ\tapplicable=2\tdisabled=no action installed\n\
         hub\t2 / 3 files\n",
        "the CBZ slice is on the wheel, dim: only the PDF join is installed"
    );
}

/// The preset runs the join action once over every image of its set, in
/// natural order, and publishes one result; the text file is unsupported.
#[test]
fn run_join_preset_publishes_one_result() {
    let e = Env::with_actions("run-join", &[]);
    join_plugin(&e);
    e.file("b.png", b"\x89PNG\r\n\x1a\n....");
    e.file("a.jpg", b"\xff\xd8\xff\xe0 jpeg");
    e.file("c.txt", b"c\n");
    let o = e.run(&["run", "pdf", "b.png", "a.jpg", "c.txt"]);
    assert_eq!(o.code, Some(0), "{}\n{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (2, 1, 0));
    let done = o.pairs("done");
    let ins: Vec<_> = done.iter().map(|(i, _)| i.clone()).collect();
    assert_eq!(ins, [e.files().join("a.jpg"), e.files().join("b.png")]);
    assert_eq!(done[0].1, done[1].1, "one result for both");
    let mut want = b"\xff\xd8\xff\xe0 jpeg".to_vec();
    want.extend_from_slice(b"\x89PNG\r\n\x1a\n....");
    assert_eq!(fs::read(&done[0].1).unwrap(), want);
    assert_eq!(
        o.pairs("skip"),
        [(e.files().join("c.txt"), "unsupported".to_owned())]
    );
}

/// `run --action` with a join action takes the join path too.
#[test]
fn run_join_action_by_id() {
    let e = Env::with_actions("run-join-id", &[]);
    join_plugin(&e);
    e.file("a.jpg", b"\xff\xd8\xff\xe0 jpeg");
    e.file("b.jpg", b"\xff\xd8\xff\xe0 two");
    let o = e.run(&["run", "--action", "p.images-to-pdf", "a.jpg", "b.jpg"]);
    assert_eq!(o.code, Some(0), "{}\n{}", o.stdout, o.stderr);
    let done = o.pairs("done");
    assert_eq!(done.len(), 2, "{}", o.stdout);
    assert_eq!(done[0].1, done[1].1, "one result for both");
}

/// A join run exits 0 once its result is published (one input skipped
/// by the action or not), 3 when every input was skipped, 1 when one
/// failed and nothing was published.
#[test]
fn images_to_pdf_exit_codes() {
    let e = Env::with_actions("join-exit", &[]);
    e.file("a.jpg", b"\xff\xd8\xff\xe0 jpeg");
    e.file("b.png", b"\x89PNG\r\n\x1a\n....");
    let run = || e.run(&["run", "--action", "p.images-to-pdf", "a.jpg", "b.png"]);
    join_plugin(&e);
    let o = run();
    if skip_no_sandbox("images_to_pdf_exit_codes", &o) {
        return;
    }
    assert_eq!(o.code, Some(0), "{}\n{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (2, 0, 0));
    // Every input skipped by the action: nothing published, exit 3.
    join_plugin_with(
        &e,
        "while IFS= read -r in; do printf 'skip\\t%s\\thas alpha\\n' \"$in\"; done < \"$6\"; exit 3",
    );
    let o = run();
    assert_eq!(o.code, Some(3), "{}\n{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (0, 2, 0));
    // One skipped, one failed, no result: exit 1.
    join_plugin_with(
        &e,
        "{ read -r a; read -r b; } < \"$6\"\n\
         printf 'skip\\t%s\\thas alpha\\n' \"$a\"; printf 'fail\\t%s\\tbroken\\n' \"$b\"; exit 1",
    );
    let o = run();
    assert_eq!(o.code, Some(1), "{}\n{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (0, 1, 1));
}

/// A join is one group in the history; the inputs it did not take keep
/// their `skip` lines. Undo moves the one wrapper to the Trash and leaves
/// every input as it was.
#[test]
fn join_history_has_one_group_and_undoes() {
    let e = Env::with_actions("join-history", &[]);
    join_plugin_with(
        &e,
        "work=$4; mkdir -p \"$work/join\"; : > \"$work/join/out.pdf\"\n\
         while IFS= read -r in; do case \"$in\" in\n\
         *b.png) printf 'skip\\t%s\\thas alpha\\n' \"$in\";;\n\
         *) cat \"$in\" >> \"$work/join/out.pdf\"; printf 'take\\t%s\\n' \"$in\";;\n\
         esac; done < \"$6\"\n\
         printf 'joined\\tjoin/out.pdf\\n'",
    );
    let ins = [
        e.file("a.jpg", b"\xff\xd8\xff\xe0 a"),
        e.file("b.png", b"\x89PNG\r\n\x1a\n...."),
        e.file("c.jpg", b"\xff\xd8\xff\xe0 c"),
    ];
    let o = e.run(&["run", "pdf", "a.jpg", "b.png", "c.jpg"]);
    if skip_no_sandbox("join_history_has_one_group_and_undoes", &o) {
        return;
    }
    assert_eq!(o.code, Some(0), "{}\n{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (2, 1, 0));
    // Named after the folder of the inputs (`files`).
    let wrapper = e.files().join("files");
    let result = wrapper.join("files.pdf");
    let done = o.pairs("done");
    assert_eq!(done.len(), 2, "{}", o.stdout);
    assert!(done.iter().all(|(_, p)| Path::new(p) == result), "{done:?}");
    let job = o.job();
    let h = e.history();
    let mine: Vec<&str> = h.lines().filter(|l| l.contains(&job)).collect();
    let count = |kind: &str| {
        mine.iter()
            .filter(|l| l.split('\t').next() == Some(kind))
            .count()
    };
    assert_eq!(count("group-begin"), 1, "{h}");
    assert_eq!(count("publish-commit"), 1, "{h}");
    assert_eq!(count("skip"), 1, "{h}");
    assert!(
        mine.iter().any(|l| l.starts_with("skip\t")
            && l.contains("b.png")
            && l.contains("reason=has alpha")),
        "{h}"
    );
    let before: Vec<Vec<u8>> = ins.iter().map(|p| fs::read(p).unwrap()).collect();

    let u = e.run(&["undo", &job]);
    assert_eq!(u.code, Some(0), "{}{}", u.stdout, u.stderr);
    let trash = e.root.join("home/.local/share/Trash/files");
    let moved: Vec<String> = u
        .kind("moved")
        .iter()
        .map(|l| l.bare_at(0).unwrap().to_owned())
        .collect();
    assert_eq!(moved, [trash.join("files").display().to_string()]);
    assert!(!wrapper.exists());
    assert!(trash.join("files/files.pdf").is_file());
    let after: Vec<Vec<u8>> = ins.iter().map(|p| fs::read(p).unwrap()).collect();
    assert_eq!(before, after, "an input changed");
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
    assert!(!e.files().join("a").exists());
}

#[test]
fn jobs_empty_when_idle() {
    let e = Env::new("jobs");
    let o = e.run(&["jobs"]);
    assert_eq!(o.code, Some(0));
    assert!(o.stdout.is_empty());
}

/// A registry that cannot be read in full is a failure of `jobs`,
/// `cancel` and `undo`, never "no job runs"; idle stays quiet and exit 0
/// (the control above).
#[test]
fn jobs_cancel_undo_fail_on_an_unreadable_registry() {
    let e = Env::new("jobs-over");
    let dir = e.root.join("runtime/inacircle/jobs");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(dir.join("9-9"), vec![b'j'; 4097]).unwrap();
    for args in [&["jobs"][..], &["cancel", "9-9"][..], &["undo", "9-9"][..]] {
        let o = e.run(args);
        assert_eq!(o.code, Some(1), "{args:?}: {} {}", o.stdout, o.stderr);
        assert!(o.stderr.contains("safety limits"), "{args:?}: {}", o.stderr);
        assert!(o.stdout.is_empty(), "{args:?}");
    }
    fs::remove_file(dir.join("9-9")).unwrap();
    assert_eq!(e.run(&["jobs"]).code, Some(0));
}

/// Without a history nothing can be recorded before the publishing rename,
/// so nothing is published: the input fails with the reason, the source
/// stays, and the warning comes once.
#[test]
fn unwritable_history_refuses_publication_and_warns_once() {
    let e = Env::new("nohistory");
    let src = e.file("a.txt", b"hi\n");
    // The state home is a regular file: no history can be opened under it.
    let state = e.root.join("state-file");
    fs::write(&state, b"").unwrap();
    let o = parse_out(
        e.cmd(&["run", "markdown", "a.txt"])
            .env("XDG_STATE_HOME", &state)
            .output()
            .unwrap(),
    );
    assert_eq!(o.code, Some(1), "{}{}", o.stdout, o.stderr);
    assert_eq!(o.summary(), (0, 0, 1));
    let fails = o.pairs("fail");
    assert!(fails[0].1.contains("history not written"), "{fails:?}");
    assert!(!e.files().join("a").exists());
    assert_eq!(fs::read(&src).unwrap(), b"hi\n");
    let warns = o
        .stderr
        .lines()
        .filter(|l| l.contains("history not written"))
        .count();
    assert_eq!(warns, 1, "{}", o.stderr);
    // The warning says what happened: nothing was published, not "kept".
    assert!(o.stderr.contains("; nothing was published"), "{}", o.stderr);
    assert!(!o.stderr.contains("kept"), "{}", o.stderr);
}

/// A history of the previous format is archived on the first run, never
/// overwritten; its jobs cannot be undone (with the reason), and their
/// outputs stay.
#[test]
fn v0_history_is_archived_and_its_jobs_refused() {
    let e = Env::new("v0");
    let old_out = e.file("old.md", b"old\n");
    let v0 = format!(
        "inacircle-history 0\nout\tjob=1-1\tin=/i\tpath={}\tsize=4\tmtime=1\tctime=1\tdev=1\tino=1\njob\tid=1-1\ttime=1\taction=a\tstatus=done\tsandbox=yes\n",
        old_out.display()
    );
    let dir = e.root.join("state/inacircle");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("history"), &v0).unwrap();
    e.file("a.txt", b"a\n");
    let o = e.run(&["run", "markdown", "a.txt"]);
    assert_eq!(o.summary(), (1, 0, 0), "{}{}", o.stdout, o.stderr);
    assert_eq!(fs::read_to_string(dir.join("history.v0")).unwrap(), v0);
    assert!(e.history().starts_with("inacircle-history 1\n"));
    let u = e.run(&["undo", "1-1"]);
    assert_eq!(u.code, Some(1), "{}{}", u.stdout, u.stderr);
    assert!(u.stderr.contains("older version"), "{}", u.stderr);
    assert!(old_out.exists());
    // The new job undoes as usual.
    let u = e.run(&["undo", &o.job()]);
    assert_eq!(u.code, Some(0), "{}", u.stderr);
    assert!(!e.files().join("a").exists());
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
