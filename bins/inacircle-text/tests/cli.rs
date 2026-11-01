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
action\ttext.txt-to-html\tfamily=text\tin=text/plain\tout=text/html\text=html\tlabel=HTML\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.md-join-epub\tfamily=text\tin=text/markdown\tassets=image/jpeg,image/png\tout=application/epub+zip\text=epub\tlabel=EPUB\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\tjoin=yes\tmin=1\n";

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
fn manifest_lists_the_join_line() {
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

// ---------- text.md-join-epub (join) ----------

/// A small baseline JPEG (the image probe's fixture, not duplicated).
fn jpeg() -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/probe/tests/data/rgb.jpg");
    fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// A valid 8-bit RGB PNG whose pixels are all `shade`, so two shades differ.
fn png(shade: u8) -> Vec<u8> {
    let (w, h) = (2usize, 2usize);
    let mut rows = Vec::new();
    for _ in 0..h {
        rows.push(0);
        rows.extend(std::iter::repeat_n(shade, w * 3));
    }
    testzip::png(w as u32, h as u32, 2, None, false, false, &rows)
}

/// Runs the join on `inputs` (documents first, then assets, as the engine
/// passes them) without a language in the environment.
fn join(work: &Path, inputs: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
        .args(["run", "text.md-join-epub", "--work"])
        .arg(work)
        .arg("--")
        .args(inputs)
        .env_remove("INACIRCLE_LANG")
        .output()
        .unwrap()
}

/// Every entry of the book, in archive order.
fn entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let f = fs::File::open(path).unwrap();
    let limits = zip::Limits {
        entries: 4096,
        entry_bytes: 64 << 20,
        ratio: 1,
        name_bytes: 255,
    };
    let a = zip::open(&f, limits).unwrap();
    (0..a.entries().len())
        .map(|i| {
            let mut v = Vec::new();
            a.read(i, &mut v).unwrap();
            (String::from_utf8(a.entries()[i].name.clone()).unwrap(), v)
        })
        .collect()
}

fn entry<'a>(es: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    &es.iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no entry {name}"))
        .1
}

/// The text of every XHTML chapter (not the navigation) in one string, and
/// the names of the packaged images in archive order.
fn chapters(es: &[(String, Vec<u8>)]) -> Vec<(String, String)> {
    es.iter()
        .filter(|(n, _)| n.starts_with("OEBPS/ch") && n.ends_with(".xhtml"))
        .map(|(n, v)| (n.clone(), String::from_utf8(v.clone()).unwrap()))
        .collect()
}

fn images(es: &[(String, Vec<u8>)]) -> Vec<&str> {
    es.iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| n.starts_with("OEBPS/img/"))
        .collect()
}

/// The book a successful join announced.
fn joined_book(o: &Output, work: &Path) -> PathBuf {
    let out = stdout(o);
    let j = lines(&out, "joined");
    assert_eq!(j.len(), 1, "{out}");
    assert_eq!(j[0][1], "join/out.epub", "{out}");
    work.join(j[0][1])
}

fn takes(o: &Output) -> Vec<String> {
    lines(&stdout(o), "take")
        .iter()
        .map(|t| t[1].to_string())
        .collect()
}

#[test]
fn two_docs_one_book() {
    let (root, work) = workdir("join-two");
    let dir = root.join("kert");
    fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.md"), dir.join("b.md"));
    fs::write(
        &a,
        "# Alpha\n\nSee [the other](b.md#beta) and [gone](c.md).\n",
    )
    .unwrap();
    fs::write(&b, "# Beta\n\nBack to [alpha](a.md).\n").unwrap();
    let o = join(&work, &[&a, &b]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(takes(&o), [a.to_str().unwrap(), b.to_str().unwrap()]);
    let book = joined_book(&o, &work);
    let es = entries(&book);
    let opf = String::from_utf8(entry(&es, "OEBPS/content.opf").to_vec()).unwrap();
    // The title is the folder's name, the file name's source too.
    assert!(opf.contains("<dc:title>kert</dc:title>"), "{opf}");
    let nav = String::from_utf8(entry(&es, "OEBPS/nav.xhtml").to_vec()).unwrap();
    let (ia, ib) = (nav.find("Alpha").unwrap(), nav.find("Beta").unwrap());
    assert!(ia < ib, "{nav}");
    let ch = chapters(&es);
    assert_eq!(ch.len(), 2);
    // A link to the other file goes to its chapter file and heading.
    let name = |i: usize| ch[i].0.trim_start_matches("OEBPS/");
    assert!(
        ch[0].1.contains(&format!("href=\"{}#beta\"", name(1))),
        "{}",
        ch[0].1
    );
    assert!(
        ch[1].1.contains(&format!("href=\"{}\"", name(0))),
        "{}",
        ch[1].1
    );
    // A link to a file that is not in the book is its text and a warn.
    assert!(!ch[0].1.contains("c.md"), "{}", ch[0].1);
    assert!(
        lines(&stdout(&o), "warn")
            .iter()
            .any(|w| w[2] == "links flattened to text"),
        "{}",
        stdout(&o)
    );
    epubcheck(&book);
}

#[test]
fn doc_and_images_one_book() {
    let (root, work) = workdir("join-images");
    let md = root.join("jegyzet.md");
    fs::write(
        &md,
        "# Képek\n\n![egy](one.png)\n\n![kettő](two.jpg)\n\n![ismét](one.png)\n",
    )
    .unwrap();
    let (one, two, three) = (
        root.join("one.png"),
        root.join("two.jpg"),
        root.join("three.png"),
    );
    fs::write(&one, png(10)).unwrap();
    fs::write(&two, jpeg()).unwrap();
    fs::write(&three, png(200)).unwrap();
    let o = join(&work, &[&md, &one, &three, &two]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(takes(&o).len(), 4);
    let book = joined_book(&o, &work);
    let es = entries(&book);
    // Numbered by first reference; the unreferenced image comes last.
    assert_eq!(
        images(&es),
        [
            "OEBPS/img/0001.png",
            "OEBPS/img/0002.jpg",
            "OEBPS/img/0003.png"
        ]
    );
    assert_eq!(entry(&es, "OEBPS/img/0001.png"), &png(10)[..]);
    assert_eq!(entry(&es, "OEBPS/img/0002.jpg"), &jpeg()[..]);
    assert_eq!(entry(&es, "OEBPS/img/0003.png"), &png(200)[..]);
    let ch = chapters(&es);
    assert!(ch[0].1.contains("src=\"img/0001.png\""), "{}", ch[0].1);
    assert!(ch[0].1.contains("src=\"img/0002.jpg\""), "{}", ch[0].1);
    // One document: the title is its first H1.
    let opf = String::from_utf8(entry(&es, "OEBPS/content.opf").to_vec()).unwrap();
    assert!(opf.contains("<dc:title>Képek</dc:title>"), "{opf}");
    let last = &ch.last().unwrap().1;
    assert!(last.contains("<h1>Images</h1>"), "{last}");
    assert!(last.contains("src=\"img/0003.png\""), "{last}");
    assert!(last.contains("alt=\"three.png\""), "{last}");
    assert!(!last.contains("0001.png"), "{last}");
    assert!(lines(&stdout(&o), "warn").is_empty(), "{}", stdout(&o));
    epubcheck(&book);
}

#[test]
fn image_refs_resolve_by_file_identity() {
    use std::os::unix::fs::symlink;
    let (root, work) = workdir("join-identity");
    let sub = root.join("notes");
    fs::create_dir_all(sub.join("kepek")).unwrap();
    let md = sub.join("n.md");
    fs::write(
        &md,
        "![up](../kep.png)\n\n![sp](kepek/a%20b.jpg)\n\n![dot](./k.png)\n\n![plain](k.png)\n\n\
         ![link](sym.png)\n\n![abs](/abs/x.png)\n\n![url](http://example.com/y.png)\n\n\
         ![out](../other.png)\n",
    )
    .unwrap();
    let (kep, spaced, k) = (
        root.join("kep.png"),
        sub.join("kepek/a b.jpg"),
        sub.join("k.png"),
    );
    fs::write(&kep, png(1)).unwrap();
    fs::write(&spaced, jpeg()).unwrap();
    fs::write(&k, png(2)).unwrap();
    // Not an input: the reference must not pull it in.
    fs::write(root.join("other.png"), png(3)).unwrap();
    symlink("k.png", sub.join("sym.png")).unwrap();
    let o = join(&work, &[&md, &kep, &k, &spaced]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let book = joined_book(&o, &work);
    let es = entries(&book);
    // Three files, once each, though five references name them.
    assert_eq!(images(&es).len(), 3, "{:?}", images(&es));
    let ch = chapters(&es);
    let body = &ch[0].1;
    assert_eq!(body.matches("<img ").count(), 5, "{body}");
    // `./k.png`, `k.png` and the symlink are one image.
    assert_eq!(body.matches("src=\"img/0003.png\"").count(), 3, "{body}");
    // No local file, no image: the alt text stays.
    for alt in ["abs", "url", "out"] {
        assert!(body.contains(alt), "{body}");
    }
    assert!(!body.contains("example.com"), "{body}");
    assert!(
        lines(&stdout(&o), "warn")
            .iter()
            .any(|w| w[2] == "images replaced by their alt text"),
        "{}",
        stdout(&o)
    );
    epubcheck(&book);
}

#[test]
fn damaged_image_fails_alone() {
    let (root, work) = workdir("join-damaged");
    let md = root.join("a.md");
    let mut text = String::new();
    let mut inputs: Vec<PathBuf> = vec![md.clone()];
    for i in 0..12u8 {
        let p = root.join(format!("p{i:02}.png"));
        let mut bytes = png(i * 10);
        if i == 5 {
            // Cut inside the header.
            bytes.truncate(20);
        }
        fs::write(&p, bytes).unwrap();
        text.push_str(&format!(
            "![pic {i}]({})\n\n",
            p.file_name().unwrap().to_str().unwrap()
        ));
        inputs.push(p);
    }
    fs::write(&md, text).unwrap();
    let refs: Vec<&Path> = inputs.iter().map(PathBuf::as_path).collect();
    let o = join(&work, &refs);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert_eq!(fails[0][1], root.join("p05.png").to_str().unwrap());
    assert_eq!(takes(&o).len(), 12, "{out}");
    let book = joined_book(&o, &work);
    let es = entries(&book);
    assert_eq!(images(&es).len(), 11);
    let ch = chapters(&es);
    assert!(ch[0].1.contains("pic 5"), "{}", ch[0].1);
    assert_eq!(ch[0].1.matches("<img ").count(), 11);
    // The broken one is not parked in the `Images` chapter either.
    assert_eq!(ch.len(), 1);
    epubcheck(&book);
}

#[test]
fn missing_image_is_alt_text() {
    let (root, work) = workdir("join-missing");
    let md = root.join("a.md");
    fs::write(&md, "![nowhere](nope.png)\n\n![exists](there.png)\n").unwrap();
    // On disk but not among the inputs: never read, never packaged.
    fs::write(root.join("there.png"), png(9)).unwrap();
    let o = join(&work, &[&md]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(lines(&out, "fail").is_empty(), "{out}");
    let book = joined_book(&o, &work);
    let es = entries(&book);
    assert!(images(&es).is_empty());
    let ch = chapters(&es);
    assert!(ch[0].1.contains("nowhere") && ch[0].1.contains("exists"));
    assert!(!ch[0].1.contains("<img "), "{}", ch[0].1);
    assert_eq!(lines(&out, "warn").len(), 1, "{out}");
    epubcheck(&book);
}

#[test]
fn failed_doc_leaves_the_rest() {
    let (root, work) = workdir("join-faildoc");
    let dir = root.join("kert");
    fs::create_dir_all(&dir).unwrap();
    let (a, big, c) = (dir.join("a.md"), dir.join("b.md"), dir.join("c.md"));
    fs::write(&a, "# Alpha\n\n[to b](b.md)\n").unwrap();
    // One byte past the 8 MiB input budget.
    fs::write(&big, vec![b'x'; (8 << 20) + 1]).unwrap();
    fs::write(&c, "# Gamma\n").unwrap();
    let o = join(&work, &[&a, &big, &c]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert_eq!(fails[0][1], big.to_str().unwrap());
    assert_eq!(takes(&o), [a.to_str().unwrap(), c.to_str().unwrap()]);
    let book = joined_book(&o, &work);
    let es = entries(&book);
    let ch = chapters(&es);
    assert_eq!(ch.len(), 2);
    // The failed document is not in the book: its link is text.
    assert!(!ch[0].1.contains("b.md"), "{}", ch[0].1);
    epubcheck(&book);

    // No document left: no output at all, the image has nothing to join.
    let work2 = root.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let img = dir.join("p.png");
    fs::write(&img, png(1)).unwrap();
    let o = join(&work2, &[&big, &img]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(lines(&out, "joined").is_empty() && lines(&out, "take").is_empty());
    assert_eq!(lines(&out, "skip").len(), 1, "{out}");
    assert!(!work2.join("join").exists());
}

#[test]
fn chapter_limit_across_files() {
    let (root, work) = workdir("join-chapters");
    let dir = root.join("many");
    fs::create_dir_all(&dir).unwrap();
    let files: Vec<PathBuf> = (0..600)
        .map(|i| {
            let p = dir.join(format!("{i:03}.md"));
            fs::write(&p, "# a\n\n# b\n").unwrap();
            p
        })
        .collect();
    let refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let o = join(&work, &refs);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(lines(&out, "joined").is_empty() && lines(&out, "take").is_empty());
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 600, "every document fails");
    assert!(
        fails
            .iter()
            .all(|f| f[2] == "book would have more than 1024 chapters"),
        "{}",
        fails[0][2]
    );
    assert!(!work.join("join/out.epub").exists());
}

#[test]
fn output_ceiling_fails_alone() {
    let (root, work) = workdir("join-ceiling");
    let md = root.join("a.md");
    fs::write(&md, "# Alpha\n").unwrap();
    // 2.1 MB of pixels that do not compress: the stored image alone is
    // past the 1 MiB ceiling.
    let (w, h) = (1200usize, 600usize);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut rows = Vec::with_capacity(h * (1 + w * 3));
    for _ in 0..h {
        // Filter type 0, then the row.
        rows.push(0u8);
        rows.extend((0..w * 3).map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        }));
    }
    let big = root.join("big.png");
    fs::write(
        &big,
        testzip::png(w as u32, h as u32, 2, None, false, false, &rows),
    )
    .unwrap();
    let o = run_with_fsize("text.md-join-epub", &work, &[&md, &big]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(lines(&out, "joined").is_empty() && lines(&out, "take").is_empty());
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 2, "{out}");
    assert!(
        fails
            .iter()
            .all(|f| f[2] == "EPUB would be larger than 1 MiB"),
        "{out}"
    );
    // Nothing is left behind to publish.
    assert!(
        !work.join("join/out.epub").exists(),
        "{:?}",
        snapshot(&work)
            .into_iter()
            .map(|(p, _)| p)
            .collect::<Vec<_>>()
    );
}

#[test]
fn deterministic_join() {
    let (root, work) = workdir("join-det");
    let dir = root.join("kert");
    fs::create_dir_all(&dir).unwrap();
    let (a, b, img) = (dir.join("a.md"), dir.join("b.md"), dir.join("p.png"));
    fs::write(&a, "# Alpha\n\n![x](p.png)\n").unwrap();
    fs::write(&b, "# Beta\n").unwrap();
    fs::write(&img, png(7)).unwrap();
    let first = fs::read(joined_book(&join(&work, &[&a, &b, &img]), &work)).unwrap();
    let work2 = root.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let second = fs::read(joined_book(&join(&work2, &[&a, &b, &img]), &work2)).unwrap();
    assert_eq!(first, second);
    // A different image makes a different book (and a different identifier).
    fs::write(&img, png(8)).unwrap();
    let work3 = root.join("work3");
    fs::create_dir_all(&work3).unwrap();
    let third = fs::read(joined_book(&join(&work3, &[&a, &b, &img]), &work3)).unwrap();
    assert_ne!(first, third);
}

#[test]
fn sibling_folders_link_across() {
    let (root, work) = workdir("join-siblings");
    let (da, db) = (root.join("a"), root.join("b"));
    fs::create_dir_all(&da).unwrap();
    fs::create_dir_all(&db).unwrap();
    let (one, two) = (da.join("1.md"), db.join("2.md"));
    fs::write(&one, "# One\n\n[two](../b/2.md#two) [out](../../x.md)\n").unwrap();
    fs::write(&two, "# Two\n\n[one](../a/1.md)\n").unwrap();
    let o = join(&work, &[&one, &two]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let book = joined_book(&o, &work);
    let es = entries(&book);
    let ch = chapters(&es);
    let name = |i: usize| ch[i].0.trim_start_matches("OEBPS/");
    assert!(
        ch[0].1.contains(&format!("href=\"{}#two\"", name(1))),
        "{}",
        ch[0].1
    );
    assert!(
        ch[1].1.contains(&format!("href=\"{}\"", name(0))),
        "{}",
        ch[1].1
    );
    // A document outside every folder of the book is no target.
    assert!(!ch[0].1.contains("x.md"), "{}", ch[0].1);
    // Tie between two folders: the earlier input's folder names the book.
    let opf = String::from_utf8(entry(&es, "OEBPS/content.opf").to_vec()).unwrap();
    assert!(opf.contains("<dc:title>a</dc:title>"), "{opf}");
    epubcheck(&book);
}

/// Watches files for being opened (inotify `IN_OPEN`), so a test can prove
/// that the action never opened a file it must not touch.
struct OpenWatch(std::os::fd::OwnedFd);

impl OpenWatch {
    fn new(paths: &[&Path]) -> OpenWatch {
        use std::os::fd::FromRawFd;
        // SAFETY: no pointer is passed; a negative result is checked.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        // SAFETY: `fd` is a new descriptor owned by nothing else.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        for p in paths {
            let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c` is NUL-terminated and outlives the call.
            let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_OPEN) };
            assert!(
                wd >= 0,
                "{}: {}",
                p.display(),
                std::io::Error::last_os_error()
            );
        }
        OpenWatch(owned)
    }

    /// Whether any watched file was opened since the watch began.
    fn opened(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 4096];
        // SAFETY: `buf` is writable for its whole length; the descriptor is
        // non-blocking, so an empty queue returns -1 (EAGAIN) at once.
        let n = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        n > 0
    }
}

/// A referenced file that is not an input is never opened, only its
/// identity is looked at: neither a regular image nor a FIFO (whose open
/// could block or have effects) sees an open; both keep their alt text.
#[test]
fn referenced_non_input_is_never_opened() {
    let (root, work) = workdir("join-noopen");
    let md = root.join("a.md");
    fs::write(
        &md,
        "![plain](other.png)\n\n![pipe](pipe.png)\n\n![in](in.png)\n",
    )
    .unwrap();
    let (other, pipe, input) = (
        root.join("other.png"),
        root.join("pipe.png"),
        root.join("in.png"),
    );
    fs::write(&other, png(3)).unwrap();
    fs::write(&input, png(4)).unwrap();
    let c = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
    // SAFETY: c is NUL-terminated.
    assert_eq!(unsafe { libc_mkfifo(c.as_ptr(), 0o600) }, 0);
    let watch = OpenWatch::new(&[&other, &pipe]);
    let o = join(&work, &[&md, &input]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(!watch.opened(), "a non-input file was opened");
    let book = joined_book(&o, &work);
    let es = entries(&book);
    assert_eq!(images(&es).len(), 1);
    let ch = chapters(&es);
    assert!(ch[0].1.contains("plain") && ch[0].1.contains("pipe"));
    assert_eq!(ch[0].1.matches("<img ").count(), 1, "{}", ch[0].1);
    assert_eq!(lines(&out, "warn").len(), 1, "{out}");
}

/// An input with a non-UTF-8 path is skipped before anything opens it,
/// image or document, as in every other action.
#[test]
fn join_non_utf8_path_is_skipped_unread() {
    let (root, work) = workdir("join-badpath");
    let md = root.join("a.md");
    fs::write(&md, "# A\n").unwrap();
    let bad = root.join(OsStr::from_bytes(b"bad\xff.png"));
    fs::write(&bad, png(5)).unwrap();
    let watch = OpenWatch::new(&[&bad]);
    let o = join(&work, &[&md, &bad]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(!watch.opened(), "the non-UTF-8 input was opened");
    let skip = lines(&out, "skip");
    assert_eq!(skip.len(), 1, "{out}");
    assert_eq!(skip[0][2], "non-UTF-8 path");
    assert_eq!(takes(&o), [md.to_str().unwrap()]);
}

/// A colon in a file name is not a URL scheme: `a:b.png` (one letter
/// before the colon) is a sibling image like any other.
#[test]
fn colon_in_file_name_is_a_local_image() {
    let (root, work) = workdir("join-colon");
    let md = root.join("a.md");
    fs::write(&md, "![colon](a:b.png)\n").unwrap();
    let img = root.join("a:b.png");
    fs::write(&img, png(6)).unwrap();
    let o = join(&work, &[&md, &img]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let book = joined_book(&o, &work);
    let ch = chapters(&entries(&book));
    assert_eq!(ch.len(), 1, "referenced, so no Images chapter");
    assert!(ch[0].1.contains("src=\"img/0001.png\""), "{}", ch[0].1);
}

/// `lines` lines of `- a`, three nodes each (list item, paragraph, text):
/// a node-dense document of `4 * lines` bytes.
fn dense(path: &Path, lines: usize) {
    fs::write(path, "- a\n".repeat(lines)).unwrap();
}

/// The documents of one book share one document budget: two documents
/// that each fit it but together pass its 524,288 nodes fail the whole
/// book loudly (no file, every input it would take fails, the image too);
/// together under it they join.
#[test]
fn documents_share_one_budget() {
    let (root, work) = workdir("join-budget");
    let dir = root.join("dense");
    fs::create_dir_all(&dir).unwrap();
    let (a, b, img) = (dir.join("a.md"), dir.join("b.md"), dir.join("p.png"));
    // 100,000 items: ~300,000 nodes each, 600,000 together.
    dense(&a, 100_000);
    dense(&b, 100_000);
    fs::write(&img, png(7)).unwrap();
    let o = join(&work, &[&a, &b, &img]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(lines(&out, "joined").is_empty() && lines(&out, "take").is_empty());
    let fails = lines(&out, "fail");
    let failed: Vec<&str> = fails.iter().map(|f| f[1]).collect();
    assert_eq!(
        failed,
        [
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            img.to_str().unwrap()
        ],
        "{out}"
    );
    assert!(
        fails
            .iter()
            .all(|f| f[2] == "book is too large (documents exceed the document budget together)"),
        "{out}"
    );
    assert!(!work.join("join/out.epub").exists());

    // Each alone fits: the same document is a book of its own.
    let work1 = root.join("work1");
    fs::create_dir_all(&work1).unwrap();
    let o = join(&work1, &[&b]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));

    // 80,000 items each: ~480,000 nodes together, under the budget.
    let work2 = root.join("work2");
    fs::create_dir_all(&work2).unwrap();
    dense(&a, 80_000);
    dense(&b, 80_000);
    let o = join(&work2, &[&a, &b, &img]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(takes(&o).len(), 3);
    joined_book(&o, &work2);
}

/// A document past the node budget on its own fails alone when nothing was
/// spent before it, and the rest still join; after a document that spent
/// some of the shared budget, it fails the book.
#[test]
fn document_over_the_budget_alone() {
    let (root, work) = workdir("join-alone");
    let dir = root.join("d");
    fs::create_dir_all(&dir).unwrap();
    let (big, small) = (dir.join("a.md"), dir.join("b.md"));
    // 180,000 items: ~540,000 nodes, past 524,288 on its own.
    dense(&big, 180_000);
    fs::write(&small, "# Small\n").unwrap();
    let o = join(&work, &[&big, &small]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 1, "{out}");
    assert_eq!(fails[0][1], big.to_str().unwrap());
    assert!(fails[0][2].contains("too many text elements"), "{out}");
    assert_eq!(takes(&o), [small.to_str().unwrap()]);

    let work2 = root.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let o = join(&work2, &[&small, &big]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert_eq!(lines(&out, "fail").len(), 2, "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
}
