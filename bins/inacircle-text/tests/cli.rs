use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: &str = "inacircle-manifest 0\n\
action\ttext.txt-to-md\tfamily=text\tin=text/plain\tout=text/markdown\text=md\tlabel=Markdown\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.md-to-txt\tfamily=text\tin=text/markdown\tout=text/plain\text=txt\tlabel=Plain text\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.md-to-epub\tfamily=text\tin=text/markdown\tout=application/epub+zip\text=epub\tlabel=EPUB\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.txt-to-epub\tfamily=text\tin=text/plain\tout=application/epub+zip\text=epub\tlabel=EPUB\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.md-to-html\tfamily=text\tin=text/markdown\tout=text/html\text=html\tlabel=HTML\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.txt-to-html\tfamily=text\tin=text/plain\tout=text/html\text=html\tlabel=HTML\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n";

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
    let d = std::env::temp_dir().join(format!("inacircle-text-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn run_raw(id: &str, work: &Path, inputs: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
        .args(["run", id, "--work"])
        .arg(work)
        .arg("--")
        .args(inputs)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn lines<'a>(out: &'a str, kind: &str) -> Vec<Vec<&'a str>> {
    out.lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|f| f[0] == kind)
        .collect()
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            v.extend(snapshot(&p));
        } else {
            let c = fs::read(&p).unwrap();
            v.push((p, c));
        }
    }
    v.sort();
    v
}

#[test]
fn manifest_lists_six_actions() {
    let o = Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
        .arg("--manifest")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), MANIFEST);
}

#[test]
fn txt_to_md_writes_into_work_only() {
    let root = tmp("workonly");
    let inputs = root.join("in");
    let work = root.join("work");
    fs::create_dir_all(&inputs).unwrap();
    fs::create_dir_all(&work).unwrap();
    let a = inputs.join("a.txt");
    fs::write(&a, "hello *world*\n").unwrap();
    let before = snapshot(&inputs);
    let o = run_raw("text.txt-to-md", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(snapshot(&inputs), before);
    let out = stdout(&o);
    let done = lines(&out, "done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0][1], a.to_str().unwrap());
    let rel = Path::new(done[0][2]);
    assert!(rel.is_relative());
    let produced = fs::read_to_string(work.join(rel)).unwrap();
    assert!(produced.contains("hello"));
    // `progress 0` opens the run (the measured entry marker), then one per input.
    assert!(out.starts_with("progress\t0\n"), "{out}");
    assert_eq!(lines(&out, "progress").len(), 2);
    assert_eq!(lines(&out, "progress")[1][1], "1");
}

#[test]
fn md_to_txt_warns() {
    let root = tmp("warns");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.md");
    fs::write(&a, "# Title\n\nsome *emphasis* and [link](http://x)\n").unwrap();
    let o = run_raw("text.md-to-txt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert_eq!(lines(&out, "done").len(), 1);
    let warns: Vec<Vec<&str>> = lines(&out, "warn");
    let expected: Vec<Vec<&str>> = ["emphasis removed", "links flattened to text"]
        .iter()
        .map(|m| vec!["warn", a.to_str().unwrap(), m])
        .collect();
    assert_eq!(warns, expected);
}

#[test]
fn md_to_txt_reads_gfm() {
    let root = tmp("gfm");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.md");
    fs::write(&a, "| a | b |\n| - | - |\n| 1 | ~~2~~ |\n\n- [x] done\n").unwrap();
    let o = run_raw("text.md-to-txt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let warns: Vec<Vec<&str>> = lines(&out, "warn");
    assert_eq!(
        warns,
        vec![vec!["warn", a.to_str().unwrap(), "strikethrough removed"]]
    );
    let done = lines(&out, "done");
    let txt = fs::read_to_string(work.join(done[0][2])).unwrap();
    assert_eq!(txt, "a    b\n---  ---\n1    2\n\n- [x] done\n");
}

#[test]
fn md_to_txt_plain_input_unchanged() {
    let root = tmp("plain");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.md");
    fs::write(&a, "# Title\n\nSee ~this~ and a | b\n").unwrap();
    let o = run_raw("text.md-to-txt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert!(lines(&out, "warn").is_empty(), "{out}");
    let done = lines(&out, "done");
    let txt = fs::read_to_string(work.join(done[0][2])).unwrap();
    assert_eq!(txt, "Title\n\nSee ~this~ and a | b\n");
}

#[test]
fn each_loss_is_warned_once_in_stable_order() {
    let root = tmp("once");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.txt");
    fs::write(&a, "one\n\n\n\ntwo  spaced\n\n\n\nthree\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    let warns = lines(&out, "warn");
    let msgs: Vec<&str> = warns.iter().map(|f| f[2]).collect();
    // Several blank-line runs: still one warn; reader and writer losses both present.
    assert_eq!(
        msgs,
        [
            "blank lines merged or dropped",
            "runs of spaces or tabs may collapse when rendered"
        ]
    );
}

#[test]
fn batch_mixed_invalid_utf8() {
    let root = tmp("mixed");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let (a, b, c) = (root.join("a.txt"), root.join("b.txt"), root.join("c.txt"));
    fs::write(&a, "one\n").unwrap();
    fs::write(&b, b"\xff\xfe").unwrap();
    fs::write(&c, "three\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a, &b, &c]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert_eq!(lines(&out, "done").len(), 2);
    let skip = lines(&out, "skip");
    assert_eq!(skip.len(), 1);
    assert_eq!(skip[0][1..], [b.to_str().unwrap(), "unsupported"]);
    assert!(out.starts_with("progress\t0\n"), "{out}");
    assert_eq!(lines(&out, "progress").len(), 4);
    assert!(work.join("0").is_dir() && work.join("2").is_dir());
}

#[test]
fn all_unsupported_exit_3() {
    let root = tmp("unsup");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.txt");
    let b = root.join("b.txt");
    fs::write(&a, b"\xff").unwrap();
    fs::write(&b, b"nul\0byte").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a, &b]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(lines(&stdout(&o), "skip").len(), 2);
}

#[test]
fn non_utf8_path_is_skipped_unread() {
    let root = tmp("badpath");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let bad = root.join(OsStr::from_bytes(b"bad\xff.txt"));
    fs::write(&bad, "text\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&bad]);
    assert_eq!(o.status.code(), Some(3));
    let out = stdout(&o);
    let skip = lines(&out, "skip");
    assert_eq!(skip.len(), 1);
    assert_eq!(skip[0][2], "non-UTF-8 path");
    assert!(lines(&out, "done").is_empty());
}

#[test]
fn io_errors_fail_per_input() {
    let root = tmp("io");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let good = root.join("g.txt");
    fs::write(&good, "x\n").unwrap();
    let missing = root.join("missing.txt");
    let dir = root.join("adir");
    fs::create_dir_all(&dir).unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&missing, &dir, &good]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 2);
    assert_eq!(fails[0][1], missing.to_str().unwrap());
    // The real cause, not the "too large" copy of the bounded read. Kills
    // the `InvalidData` guard -> `true` in `process`.
    assert!(fails[0][2].contains("No such file"), "{:?}", fails[0]);
    assert!(fails[1][2].contains("not a regular file"), "{:?}", fails[1]);
    assert_eq!(lines(&out, "done").len(), 1);
}

#[test]
fn all_fail_exit_1() {
    let root = tmp("onlyfail");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let missing = root.join("missing.txt");
    let dir = root.join("adir");
    fs::create_dir_all(&dir).unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&missing, &dir]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(lines(&stdout(&o), "fail").len(), 2);
}

#[test]
fn done_plus_fail_exit_0() {
    let root = tmp("donefail");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let good = root.join("g.txt");
    fs::write(&good, "x\n").unwrap();
    let missing = root.join("missing.txt");
    let o = run_raw("text.txt-to-md", &work, &[&good, &missing]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert_eq!(lines(&out, "done").len(), 1);
    assert_eq!(lines(&out, "fail").len(), 1);
}

#[test]
fn unsupported_plus_unusable_output_exit_1() {
    let root = tmp("unsupfail");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    // Output dir "1" cannot be created: a file sits in its place.
    fs::write(work.join("1"), "block").unwrap();
    let bad = root.join("bad.txt");
    fs::write(&bad, b"\xff").unwrap();
    let good = root.join("g.txt");
    fs::write(&good, "x\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&bad, &good]);
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    assert_eq!(lines(&out, "skip").len(), 1);
    assert_eq!(lines(&out, "fail").len(), 1);
    assert!(lines(&out, "done").is_empty());
}

#[test]
fn work_missing_exits_1() {
    let root = tmp("nowork");
    let a = root.join("a.txt");
    fs::write(&a, "x\n").unwrap();
    let o = run_raw("text.txt-to-md", &root.join("does-not-exist"), &[&a]);
    assert_eq!(o.status.code(), Some(1));
    assert!(!o.stderr.is_empty());
    assert!(!root.join("does-not-exist").exists());
}

#[test]
fn work_not_writable_exits_1() {
    let root = tmp("rowork");
    let work = root.join("work-is-a-file");
    fs::write(&work, "x").unwrap();
    let a = root.join("a.txt");
    fs::write(&a, "x\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn empty_file_ok() {
    let root = tmp("empty");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("e.txt");
    fs::write(&a, "").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    let done = lines(&out, "done");
    assert_eq!(done.len(), 1);
    assert!(work.join(done[0][2]).is_file());
}

#[test]
fn unknown_action_exit_2() {
    let root = tmp("unknown");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.txt");
    fs::write(&a, "x\n").unwrap();
    let o = run_raw("text.nope", &work, &[&a]);
    assert_eq!(o.status.code(), Some(2));
    assert!(!o.stderr.is_empty());
    assert!(o.stdout.is_empty());
}

#[test]
fn usage_errors_exit_2() {
    let bin = env!("CARGO_BIN_EXE_inacircle-text");
    for args in [&["bogus"][..], &[][..], &["run", "text.txt-to-md"][..]] {
        let o = Command::new(bin).args(args).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(!o.stderr.is_empty());
    }
}

#[test]
fn inputs_from_file_with_tab_and_newline_in_paths() {
    let root = tmp("inputsfrom");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a\tb.txt");
    let b = root.join("c\nd.txt");
    fs::write(&a, "one\n").unwrap();
    fs::write(&b, "two\n").unwrap();
    let list = root.join("list.txt");
    let mut body = String::new();
    for p in [&a, &b] {
        body.push_str(&manifest::escape(p.to_str().unwrap()));
        body.push('\n');
    }
    fs::write(&list, body).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
        .args(["run", "text.txt-to-md", "--work"])
        .arg(&work)
        .arg("--inputs-from")
        .arg(&list)
        .arg("--")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let done: Vec<_> = out
        .lines()
        .filter(|l| l.starts_with("done\t"))
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .collect();
    assert_eq!(done.len(), 2, "{out:?}");
    for (d, p) in done.iter().zip([&a, &b]) {
        assert_eq!(manifest::unescape(d[1]).unwrap(), p.to_str().unwrap());
        let rel = manifest::unescape(d[2]).unwrap();
        assert!(work.join(rel).is_file());
    }
}

/// `document_input_bytes`: 8 MiB is read, 8 MiB + 1 is a `fail` with
/// nothing staged, and the next input of the batch still converts.
#[test]
fn text_input_is_bounded() {
    let root = tmp("bounded");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let at = root.join("at.txt");
    let over = root.join("over.txt");
    let small = root.join("small.txt");
    fs::write(&at, vec![b'a'; 8 * 1024 * 1024]).unwrap();
    fs::write(&over, vec![b'a'; 8 * 1024 * 1024 + 1]).unwrap();
    fs::write(&small, "x\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&at]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(lines(&stdout(&o), "done").len(), 1);
    let o = run_raw("text.txt-to-md", &work, &[&over, &small]);
    assert_eq!(o.status.code(), Some(0), "the small one is done");
    let out = stdout(&o);
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert!(fails[0][2].contains("larger than 8 MiB"), "{out}");
    // Nothing staged for the refused input (index 0 of that run).
    assert!(!work.join("0/over.md").exists());
    assert!(work.join("1/small.md").is_file());
}

/// txt -> md is `lossy=yes`: `alpha\n\n\nbeta` loses a blank line, and the
/// action says so (the manifest, the planner and the notice read the same
/// flag, so a lossless route can never be chosen through it).
#[test]
fn txt_to_md_reports_the_blank_line_loss() {
    let root = tmp("blanklines");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.txt");
    fs::write(&a, "alpha\n\n\nbeta\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let warns = lines(&out, "warn");
    assert_eq!(warns.len(), 1, "{out}");
    assert_eq!(warns[0][2], "blank lines merged or dropped");
    assert!(stdout(&Command::new(env!("CARGO_BIN_EXE_inacircle-text")).arg("--manifest").output().unwrap())
        .contains("text.txt-to-md\tfamily=text\tin=text/plain\tout=text/markdown\text=md\tlabel=Markdown\tgroup=document\tcost=10\tlossy=yes"));
}

/// `document_nodes`: `2n - 1 + 1` nodes for `n` lines in one paragraph; the
/// limit (524,288 = 2^19) converts, one more line is a `fail` that names the
/// limit, nothing is staged, and the next input of the batch still converts.
#[test]
fn text_node_budget_max_and_max_plus_one() {
    let root = tmp("nodes");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let at = root.join("at.txt");
    let over = root.join("over.txt");
    let small = root.join("small.txt");
    let n = (1 << 19) / 2;
    fs::write(&at, "a\n".repeat(n)).unwrap();
    fs::write(&over, "a\n".repeat(n + 1)).unwrap();
    fs::write(&small, "x\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&at]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(lines(&stdout(&o), "done").len(), 1);
    let o = run_raw("text.txt-to-md", &work, &[&over, &small]);
    assert_eq!(o.status.code(), Some(0), "the small one is done");
    let out = stdout(&o);
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert_eq!(
        fails[0][2],
        "This document has too many text elements (limit 524,288)."
    );
    assert!(!work.join("0/over.md").exists());
    assert!(work.join("1/small.md").is_file());
}

/// The md reader's budgets reach the action too: a hostile Markdown is a
/// `fail` with the budget's own words, not `unsupported`.
#[test]
fn markdown_node_budget_is_a_fail_not_unsupported() {
    let root = tmp("mdnodes");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.md");
    fs::write(&a, "> a\n".repeat(300_000)).unwrap();
    let o = run_raw("text.md-to-txt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "skip").len(), 0, "{out}");
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert!(fails[0][2].contains("too many text elements"), "{out}");
}

/// A FIFO as an input fails at once (never blocks); a symlink to a text
/// file still converts (positive control).
#[test]
fn text_input_fifo_fails_and_symlink_still_works() {
    let root = tmp("fifo");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let q = root.join("q.txt");
    let c = std::ffi::CString::new(q.as_os_str().as_bytes()).unwrap();
    // SAFETY: c is NUL-terminated.
    assert_eq!(unsafe { libc_mkfifo(c.as_ptr(), 0o600) }, 0);
    let real = root.join("real.txt");
    fs::write(&real, "hello\n").unwrap();
    let link = root.join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&q, &link]);
    let out = stdout(&o);
    assert_eq!(lines(&out, "fail").len(), 1, "{out}");
    assert_eq!(lines(&out, "done").len(), 1, "{out}");
}

unsafe extern "C" {
    #[link_name = "mkfifo"]
    fn libc_mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
}

// ---------- EPUB and HTML ----------

/// A tool on `PATH`; a missing one skips with a visible line, unless
/// `INACIRCLE_REQUIRE_BOOK_TOOLS=1` makes it a failure.
fn have(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false);
    if !found {
        if std::env::var_os("INACIRCLE_REQUIRE_BOOK_TOOLS").is_some_and(|v| v == "1") {
            panic!("{tool} is required (INACIRCLE_REQUIRE_BOOK_TOOLS=1) but not installed");
        }
        println!("skipped: {tool} not installed");
    }
    found
}

/// `epubcheck` finds no error and no warning in the book (when installed).
fn epubcheck(path: &Path) {
    if !have("epubcheck") {
        return;
    }
    let out = Command::new("epubcheck")
        .arg("--failonwarnings")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "epubcheck: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The Nu validator finds no error in the page (when installed).
fn vnu(path: &Path) {
    if !have("vnu") {
        return;
    }
    let out = Command::new("vnu").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "vnu: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Runs `id` on one input with `INACIRCLE_LANG` set (or removed); the
/// output file it announced.
fn convert(id: &str, input: &Path, work: &Path, lang: Option<&str>) -> (Output, PathBuf) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_inacircle-text"));
    c.args(["run", id, "--work"])
        .arg(work)
        .arg("--")
        .arg(input)
        .env_remove("INACIRCLE_LANG");
    if let Some(l) = lang {
        c.env("INACIRCLE_LANG", l);
    }
    let o = c.output().unwrap();
    let out = stdout(&o);
    let done = lines(&out, "done");
    let path = done
        .first()
        .map(|d| work.join(d[2]))
        .unwrap_or_else(|| work.join("none"));
    (o, path)
}

fn workdir(name: &str) -> (Tmp, PathBuf) {
    let root = tmp(name);
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    (root, work)
}

#[test]
fn md_to_epub_end_to_end() {
    let (root, work) = workdir("md2epub");
    let a = root.join("jegyzet.md");
    fs::write(
        &a,
        "Bevezető\n\n# Áttekintés\n\nSzöveg *kiemelés*. [Lásd](#2026-terv)\n\n# 2026 terv\n\n- egy\n- kettő\n",
    )
    .unwrap();
    let (o, book) = convert("text.md-to-epub", &a, &work, Some("hu"));
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(
        book.strip_prefix(&work).unwrap(),
        Path::new("0/jegyzet.epub")
    );
    let bytes = fs::read(&book).unwrap();
    assert!(bytes.starts_with(b"PK\x03\x04"));
    // The package document is stored, so its text is in the file as is.
    assert!(contains(&bytes, "<dc:title>Áttekintés</dc:title>"));
    assert!(contains(&bytes, "<dc:language>hu</dc:language>"));
    // No loss for plain Markdown: no warn.
    assert!(lines(&stdout(&o), "warn").is_empty(), "{}", stdout(&o));
    epubcheck(&book);
}

#[test]
fn txt_to_epub_title_is_stem() {
    let (root, work) = workdir("txt2epub");
    let a = root.join("Napló.txt");
    fs::write(&a, "# nem cím\n\nsima szöveg\n").unwrap();
    let (o, book) = convert("text.txt-to-epub", &a, &work, None);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let bytes = fs::read(&book).unwrap();
    assert!(contains(&bytes, "<dc:title>Napló</dc:title>"));
    // No language in the environment: undetermined.
    assert!(contains(&bytes, "<dc:language>und</dc:language>"));
    epubcheck(&book);
}

#[test]
fn md_to_html_end_to_end() {
    let (root, work) = workdir("md2html");
    let a = root.join("oldal.md");
    fs::write(
        &a,
        "# Üdv\n\nSzöveg [link](https://example.com/) és `kód`.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
    )
    .unwrap();
    let (o, page) = convert("text.md-to-html", &a, &work, Some("hu"));
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(page.strip_prefix(&work).unwrap(), Path::new("0/oldal.html"));
    let html = fs::read_to_string(&page).unwrap();
    assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
    assert!(html.contains("lang=\"hu\""), "{html}");
    assert!(html.contains("<title>Üdv</title>"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    vnu(&page);
}

#[test]
fn txt_to_html_title_is_stem() {
    let (root, work) = workdir("txt2html");
    let a = root.join("lista.txt");
    fs::write(&a, "egy\n\nkettő\n").unwrap();
    let (o, page) = convert("text.txt-to-html", &a, &work, Some("de-CH"));
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let html = fs::read_to_string(&page).unwrap();
    assert!(html.contains("<title>lista</title>"), "{html}");
    assert!(html.contains("lang=\"de-CH\""), "{html}");
    vnu(&page);
}

#[test]
fn invalid_lang_is_und() {
    let (root, _) = workdir("badlang");
    let a = root.join("a.md");
    fs::write(&a, "# T\n\nx\n").unwrap();
    // A fresh work dir per run: an output is never overwritten.
    let fresh = |n: &str| {
        let w = root.join(n);
        fs::create_dir_all(&w).unwrap();
        w
    };
    for (k, bad) in ["xx_YY", "hu_HU", "", "hu\"x", "a"].into_iter().enumerate() {
        let (o, page) = convert("text.md-to-html", &a, &fresh(&format!("h{k}")), Some(bad));
        assert_eq!(o.status.code(), Some(0), "{bad}: {}", stdout(&o));
        let html = fs::read_to_string(&page).unwrap();
        assert!(html.contains("lang=\"und\""), "{bad}: {html}");
        let (o, book) = convert("text.md-to-epub", &a, &fresh(&format!("e{k}")), Some(bad));
        assert_eq!(o.status.code(), Some(0), "{bad}: {}", stdout(&o));
        let bytes = fs::read(&book).unwrap();
        assert!(contains(&bytes, "<dc:language>und</dc:language>"), "{bad}");
    }
}

/// A small `RLIMIT_FSIZE` makes the book's write fail: the input fails and
/// no file is left in the work directory (an empty or half book must not
/// look like an output). `sh` sets the limit and ignores SIGXFSZ (an
/// ignored signal survives `exec`), so the write fails with EFBIG instead
/// of killing the process; `ulimit -f 1` is 512 B or 1 KiB by shell, less
/// than any book (six entries plus the headers).
#[test]
fn failure_leaves_no_partial_epub() {
    let (root, work) = workdir("fsize");
    let a = root.join("a.md");
    fs::write(&a, "# T\n\nx\n").unwrap();
    let o = Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 1; exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_inacircle-text"))
        .args(["run", "text.md-to-epub", "--work"])
        .arg(&work)
        .arg("--")
        .arg(&a)
        .output()
        .unwrap();
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert_eq!(lines(&out, "fail").len(), 1, "{out}");
    assert!(lines(&out, "done").is_empty(), "{out}");
    let left: Vec<_> = snapshot(&work).into_iter().map(|(p, _)| p).collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

#[test]
fn too_many_chapters_fails_without_a_file() {
    let (root, work) = workdir("chapters");
    let a = root.join("big.md");
    let src: String = (0..1025).map(|i| format!("# c{i}\n\nx\n\n")).collect();
    fs::write(&a, src).unwrap();
    let o = run_raw("text.md-to-epub", &work, &[&a]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert_eq!(fails[0][2], "book would have more than 1024 chapters");
    assert!(snapshot(&work).is_empty());
}

#[test]
fn losses_reported() {
    let (root, work) = workdir("losses");
    let a = root.join("a.md");
    fs::write(&a, "# T\n\n<div>raw</div>\n\n![pic](local.png)\n").unwrap();
    for id in ["text.md-to-epub", "text.md-to-html"] {
        let (o, _) = convert(id, &a, &work, None);
        assert_eq!(o.status.code(), Some(0), "{id}: {}", stdout(&o));
        let out = stdout(&o);
        let warns: Vec<Vec<&str>> = lines(&out, "warn");
        let expected: Vec<Vec<&str>> = ["images replaced by their alt text", "HTML dropped"]
            .iter()
            .map(|m| vec!["warn", a.to_str().unwrap(), m])
            .collect();
        assert_eq!(warns, expected, "{id}");
    }
}

/// `inacircle-text` run with its own `RLIMIT_FSIZE` at 1 MiB (soft and
/// hard), as the engine's sandbox bounds an action's output, but small
/// enough to reach with little data. SIGXFSZ is not ignored here: the
/// action must stay under the ceiling or fail the input itself.
fn run_with_fsize(id: &str, work: &Path, inputs: &[&Path]) -> Output {
    run_with_fsize_of(id, work, inputs, 1 << 20)
}

/// `run_with_fsize` with the ceiling `cap` bytes.
fn run_with_fsize_of(id: &str, work: &Path, inputs: &[&Path], cap: u64) -> Output {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new(env!("CARGO_BIN_EXE_inacircle-text"));
    c.args(["run", id, "--work"])
        .arg(work)
        .arg("--")
        .args(inputs);
    // SAFETY: the closure runs between fork and exec and only calls
    // `setrlimit`, which is async-signal-safe, on a local struct; it
    // touches no memory of the parent and takes no lock.
    unsafe {
        c.pre_exec(move || {
            let rl = libc::rlimit {
                rlim_cur: cap as libc::rlim_t,
                rlim_max: cap as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rl) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c.output().unwrap()
}

/// An output past the ceiling fails that input with the size copy and the
/// next input of the batch still converts. 2 MiB of paragraphs make an
/// EPUB (stored) or a page of more than 2 MiB; "x" makes a few KB.
#[test]
fn over_the_output_ceiling_fails_one_input_and_the_batch_goes_on() {
    for (id, what, ext) in [
        ("text.md-to-epub", "EPUB", "epub"),
        ("text.md-to-html", "HTML", "html"),
    ] {
        let (root, work) = workdir(&format!("ceiling-{ext}"));
        let big = root.join("big.md");
        fs::write(&big, "abcdefghi jklmnop\n\n".repeat(110_000)).unwrap();
        let small = root.join("small.md");
        fs::write(&small, "x\n").unwrap();
        let o = run_with_fsize(id, &work, &[&big, &small]);
        let out = stdout(&o);
        assert_eq!(o.status.code(), Some(0), "{id}: {out}");
        let fails = lines(&out, "fail");
        assert_eq!(fails.len(), 1, "{id}: {out}");
        assert_eq!(fails[0][1], big.to_str().unwrap());
        assert_eq!(fails[0][2], format!("{what} would be larger than 1 MiB"));
        let done = lines(&out, "done");
        assert_eq!(done.len(), 1, "{id}: {out}");
        assert_eq!(done[0][1], small.to_str().unwrap());
        assert!(work.join(done[0][2]).is_file());
        // Nothing of the big input is left behind.
        let left: Vec<_> = snapshot(&work).into_iter().map(|(p, _)| p).collect();
        assert_eq!(left.len(), 1, "{id}: {left:?}");
    }
}

/// Regression: a ceiling under 1 MiB was printed as "0 MiB"; it is KiB now.
#[test]
fn a_ceiling_under_one_mib_is_printed_in_kib() {
    let (root, work) = workdir("ceiling-kib");
    let big = root.join("big.md");
    fs::write(&big, "a".repeat(600 * 1024)).unwrap();
    let o = run_with_fsize_of("text.md-to-html", &work, &[&big], 512 * 1024);
    let out = stdout(&o);
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert_eq!(fails[0][2], "HTML would be larger than 512 KiB");
}

/// The page is created new, like the book: a file already at its path
/// fails the input instead of being written over.
#[test]
fn an_existing_html_is_never_overwritten() {
    let (root, work) = workdir("html-no-overwrite");
    let a = root.join("a.md");
    fs::write(&a, "x\n").unwrap();
    let first = run_raw("text.md-to-html", &work, &[&a]);
    let first_out = stdout(&first);
    let done = lines(&first_out, "done");
    assert_eq!(done.len(), 1);
    let page = work.join(done[0][2]);
    assert!(page.is_file());
    let second = stdout(&run_raw("text.md-to-html", &work, &[&a]));
    let fails = lines(&second, "fail");
    assert_eq!(fails.len(), 1, "{second}");
    assert!(fails[0][2].starts_with("cannot create output"), "{second}");
    assert!(lines(&second, "done").is_empty(), "{second}");
}

/// The book's modification time is the input's, in UTC, also before 1970.
#[test]
fn epub_modified_time_is_the_inputs() {
    use std::time::{Duration, UNIX_EPOCH};
    for (k, (mtime, want)) in [
        (
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            "2023-11-14T22:13:20Z",
        ),
        (
            UNIX_EPOCH - Duration::from_secs(90_000),
            "1969-12-30T23:00:00Z",
        ),
        (UNIX_EPOCH, "1970-01-01T00:00:00Z"),
    ]
    .into_iter()
    .enumerate()
    {
        let (root, work) = workdir(&format!("mtime{k}"));
        let a = root.join("a.md");
        fs::write(&a, "# T\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let (o, book) = convert("text.md-to-epub", &a, &work, None);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        let bytes = fs::read(&book).unwrap();
        assert!(
            contains(&bytes, &format!(">{want}</meta>")),
            "{want}: not in the book"
        );
    }
}

/// A page of exactly the size ceiling is written; one byte more is not.
#[test]
fn html_page_of_exactly_the_ceiling_is_written() {
    const CEILING: usize = 1 << 20; // `run_with_fsize`'s RLIMIT_FSIZE
    let page_size = |k: usize| {
        let (root, work) = workdir(&format!("exact{k}"));
        let a = root.join("big.md");
        fs::write(&a, format!("{}\n", "a".repeat(k))).unwrap();
        let (o, page) = convert("text.md-to-html", &a, &work, None);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        fs::metadata(&page).unwrap().len() as usize
    };
    // The page is the same around a paragraph of k letters.
    let k = CEILING - page_size(1) + 1;
    assert_eq!(page_size(k), CEILING);
    for (extra, fits) in [(0, true), (1, false)] {
        let (root, work) = workdir(&format!("ceiling-exact{extra}"));
        let a = root.join("big.md");
        fs::write(&a, format!("{}\n", "a".repeat(k + extra))).unwrap();
        let o = run_with_fsize("text.md-to-html", &work, &[&a]);
        let out = stdout(&o);
        if fits {
            assert_eq!(lines(&out, "done").len(), 1, "{out}");
            let done = lines(&out, "done");
            assert_eq!(
                fs::metadata(work.join(done[0][2])).unwrap().len() as usize,
                CEILING
            );
        } else {
            let fails = lines(&out, "fail");
            assert_eq!(fails.len(), 1, "{out}");
            assert_eq!(fails[0][2], "HTML would be larger than 1 MiB");
            assert!(snapshot(&work).is_empty());
        }
    }
}
