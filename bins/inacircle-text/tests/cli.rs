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
action\ttext.md-join-epub\tfamily=text\tin=text/markdown\tassets=image/jpeg,image/png\tout=application/epub+zip\text=epub\tlabel=EPUB\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\tjoin=yes\tmin=1\n\
action\ttext.epub-to-txt\tfamily=text\tin=application/epub+zip\tout=text/plain\text=txt\tlabel=Plain text\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n\
action\ttext.epub-to-md\tfamily=text\tin=application/epub+zip\tout=text/markdown\text=md\tlabel=Markdown\tgroup=document\tcost=10\tlossy=yes\tbatch=no\n\
action\ttext.cp1250-to-utf8\tfamily=text\tin=text/x-8bit\tout=text/plain\text=same\tlabel=Windows-1250\tgroup=encoding\tcost=10\tlossy=no\tbatch=yes\n\
action\ttext.iso88592-to-utf8\tfamily=text\tin=text/x-8bit\tout=text/plain\text=same\tlabel=ISO-8859-2\tgroup=encoding\tcost=10\tlossy=no\tbatch=yes\n";

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

/// A text line made only of `:` and dashes under another line would be
/// a table's delimiter row for a GFM viewer (the line above its header):
/// its `:` is escaped, which no CommonMark viewer shows.
#[test]
fn txt_to_md_never_writes_a_delimiter_row() {
    let root = tmp("delimrow");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join("a.txt");
    fs::write(&a, "Name\n:--\n\nx\ny\n:-:\n").unwrap();
    let o = run_raw("text.txt-to-md", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let done = lines(&out, "done");
    let produced = fs::read_to_string(work.join(done[0][2])).unwrap();
    assert_eq!(produced, "Name\\\n\\:--\n\nx\\\ny\\\n\\:-:\n");
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

/// Items of a document of exactly the node budget (524,288): the list is
/// one node, each item three, and a closing thematic break one more.
const BUDGET_ITEMS: usize = 174_762;

/// After a document that left no node, a document of only blank lines
/// fails the book (it could be parsed without a node, but fails closed),
/// while an empty file still joins; a document at the budget alone joins.
#[test]
fn spent_budget_admits_only_an_empty_file() {
    let (root, work) = workdir("join-spent");
    let dir = root.join("d");
    fs::create_dir_all(&dir).unwrap();
    let (a, blank, empty) = (dir.join("a.md"), dir.join("b.md"), dir.join("c.md"));
    fs::write(&a, format!("{}\n***\n", "- a\n".repeat(BUDGET_ITEMS))).unwrap();
    fs::write(&blank, "\n\n").unwrap();
    fs::write(&empty, "").unwrap();

    let o = join(&work, &[&a, &blank]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
    assert_eq!(lines(&out, "fail").len(), 2, "{out}");

    let work2 = root.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let o = join(&work2, &[&a, &empty]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_eq!(takes(&o), [a.to_str().unwrap(), empty.to_str().unwrap()]);
    joined_book(&o, &work2);
}

/// Progress is the share of inputs passed, exactly: 0, then k/n.
#[test]
fn join_progress_counts_the_inputs() {
    let (root, work) = workdir("join-progress");
    let dir = root.join("d");
    fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.md"), dir.join("b.md"));
    fs::write(&a, "# A\n").unwrap();
    fs::write(&b, "# B\n").unwrap();
    let o = join(&work, &[&a, &b]);
    let out = stdout(&o);
    let p: Vec<&str> = lines(&out, "progress").iter().map(|f| f[1]).collect();
    assert_eq!(p, ["0", "0.5", "1"], "{out}");
}

/// The identifier follows the content of every input, not only its size
/// or its name.
#[test]
fn join_identifier_follows_the_inputs() {
    let identifier = |text: &str, k: usize| {
        let (root, work) = workdir(&format!("join-id{k}"));
        let dir = root.join("d");
        fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.md"), dir.join("b.md"));
        fs::write(&a, text).unwrap();
        fs::write(&b, "# B\n").unwrap();
        let o = join(&work, &[&a, &b]);
        let es = entries(&joined_book(&o, &work));
        let opf = String::from_utf8(entry(&es, "OEBPS/content.opf").to_vec()).unwrap();
        let from = opf.find("<dc:identifier").unwrap();
        let to = opf.find("</dc:identifier>").unwrap();
        opf[from..to].to_string()
    };
    let (x, y, z) = (
        identifier("# A1\n", 0),
        identifier("# A2\n", 1),
        identifier("# A1\n", 2),
    );
    assert_eq!(x, z);
    assert_ne!(x, y);
}

/// The book is as old as its newest document.
#[test]
fn join_modified_is_the_newest_input() {
    use std::time::{Duration, UNIX_EPOCH};
    let (root, work) = workdir("join-mtime");
    let dir = root.join("d");
    fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.md"), dir.join("b.md"));
    for (p, secs) in [(&a, 1_700_000_000u64), (&b, 1_000_000_000)] {
        fs::write(p, "# T\n").unwrap();
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
    let o = join(&work, &[&a, &b]);
    let es = entries(&joined_book(&o, &work));
    let opf = String::from_utf8(entry(&es, "OEBPS/content.opf").to_vec()).unwrap();
    assert!(
        opf.contains(">2023-11-14T22:13:20Z</meta>"),
        "newest of the two: {opf}"
    );
}

/// An image is an input too: a newer image makes the book newer than its
/// documents.
#[test]
fn join_modified_counts_the_images() {
    use std::time::{Duration, UNIX_EPOCH};
    let (root, work) = workdir("join-mtime-image");
    let dir = root.join("d");
    fs::create_dir_all(&dir).unwrap();
    let (a, img) = (dir.join("a.md"), dir.join("pic.png"));
    fs::write(&a, "# T\n").unwrap();
    fs::write(&img, png(7)).unwrap();
    for (p, secs) in [(&a, 1_000_000_000u64), (&img, 1_700_000_000)] {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
    let o = join(&work, &[&a, &img]);
    let es = entries(&joined_book(&o, &work));
    let opf = String::from_utf8(entry(&es, "OEBPS/content.opf").to_vec()).unwrap();
    assert!(
        opf.contains(">2023-11-14T22:13:20Z</meta>"),
        "the newer image sets the date: {opf}"
    );
}

/// A document over the input limit and one that cannot be read fail with
/// their own reasons; a non-text file is a `skip`, and a join that took
/// nothing and failed nothing exits 3.
#[test]
fn join_failure_reasons_and_exit_codes() {
    let (root, work) = workdir("join-reasons");
    let dir = root.join("d");
    fs::create_dir_all(&dir).unwrap();
    let (a, big, gone) = (dir.join("a.md"), dir.join("big.md"), dir.join("gone.md"));
    fs::write(&a, "# A\n").unwrap();
    fs::write(&big, vec![b'x'; (8 << 20) + 1]).unwrap();
    let o = join(&work, &[&a, &big, &gone]);
    let out = stdout(&o);
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 2, "{out}");
    assert_eq!(fails[0][1], big.to_str().unwrap());
    assert_eq!(fails[0][2], "This text file is larger than 8 MiB.");
    assert_eq!(fails[1][1], gone.to_str().unwrap());
    assert!(fails[1][2].contains("No such file"), "{out}");

    let work2 = root.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let bin = dir.join("bin.md");
    fs::write(&bin, [0xffu8, 0xfe, 0, 1, 2]).unwrap();
    let o = join(&work2, &[&bin]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(3), "{out}");
    assert_eq!(lines(&out, "skip").len(), 1, "{out}");
    assert!(lines(&out, "fail").is_empty(), "{out}");
    assert!(!work2.join("join").exists());
}

/// A damaged image says so, and a header past the pixel limit says that.
#[test]
fn join_image_failure_reasons() {
    let (root, work) = workdir("join-imgreasons");
    let md = root.join("a.md");
    fs::write(&md, "# A\n").unwrap();
    let (cut, wide) = (root.join("cut.png"), root.join("wide.png"));
    let mut bytes = png(1);
    bytes.truncate(20);
    fs::write(&cut, bytes).unwrap();
    fs::write(
        &wide,
        testzip::png(20_000, 20_000, 2, None, false, false, &[]),
    )
    .unwrap();
    let o = join(&work, &[&md, &cut, &wide]);
    let out = stdout(&o);
    let fails = lines(&out, "fail");
    assert_eq!(fails.len(), 2, "{out}");
    assert_eq!(fails[0][2], "damaged image data", "{out}");
    assert_eq!(fails[1][2], "image is larger than 180 megapixels", "{out}");
}

/// A destination that looks like a URL or an absolute path is no local
/// file, even when a file of that name is an input: `http://x/y.png` is
/// not the folder `http:` and `%2F…` is not the root.
#[test]
fn urls_and_encoded_absolute_paths_are_not_files() {
    let (root, work) = workdir("join-notlocal");
    let md = root.join("a.md");
    let url_dir = root.join("http:/example.com");
    fs::create_dir_all(&url_dir).unwrap();
    let (url_img, abs_img) = (url_dir.join("y.png"), root.join("k.png"));
    fs::write(&url_img, png(1)).unwrap();
    fs::write(&abs_img, png(2)).unwrap();
    let abs = format!("%2F{}", abs_img.to_str().unwrap().trim_start_matches('/'));
    fs::write(
        &md,
        format!("![url](http://example.com/y.png)\n\n![abs]({abs})\n"),
    )
    .unwrap();
    let o = join(&work, &[&md, &url_img, &abs_img]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let es = entries(&joined_book(&o, &work));
    let ch = chapters(&es);
    // The inputs are in the book, but the document shows alt text only.
    assert_eq!(images(&es).len(), 2);
    assert_eq!(ch[0].1.matches("<img ").count(), 0, "{}", ch[0].1);
    assert!(ch[0].1.contains("url") && ch[0].1.contains("abs"));
}

/// `text.epub-to-txt`: EPUB books (built here with `testzip`, independent
/// of the crate's own ZIP code) to one plain-text file each.
mod epub_to_txt {
    use super::*;
    use testzip::Store::Stored;

    const ID: &str = "text.epub-to-txt";
    const STYLES: &str = "book styles (fonts, colours, layout) not carried over";
    const IMAGES: &str = "images replaced by their alt text";
    const LINKS: &str = "links flattened to text";
    const MERGED: &str = "merged table cells split";
    const XHTML: &str = "application/xhtml+xml";
    const AES: &str = "http://www.w3.org/2001/04/xmlenc#aes128-cbc";
    const IDPF: &str = "http://www.idpf.org/2008/embedding";

    /// One manifest item: its href (relative to `OEBPS/`), media type,
    /// data (`None`: not in the ZIP), whether the spine names it and
    /// whether it is linear there.
    struct Ch {
        href: String,
        media: &'static str,
        data: Option<Vec<u8>>,
        spine: bool,
        linear: bool,
    }

    fn ch(href: &str, body: &str) -> Ch {
        Ch {
            href: href.to_string(),
            media: XHTML,
            data: Some(xhtml(body)),
            spine: true,
            linear: true,
        }
    }

    /// An XHTML chapter: the declaration on line 1, `body` on line 2.
    fn xhtml(body: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" \
             xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>t</title></head><body>{body}</body></html>"
        )
        .into_bytes()
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    /// A book at `path`: package `OEBPS/content.opf` (EPUB `version`) with
    /// `meta`, the manifest items `chs` (each named `iN`; those in the
    /// spine in order, `repeat` itemrefs each) and `extra` entries as
    /// they are.
    fn book_at(
        path: &Path,
        version: &str,
        meta: &str,
        chs: &[Ch],
        repeat: usize,
        extra: &[(&str, &[u8])],
    ) {
        let extra: Vec<_> = extra.iter().map(|&(n, d)| (n, d, Stored)).collect();
        fs::write(path, book_zip(version, meta, chs, repeat, &extra)).unwrap();
    }

    /// The archive of [`book_at`], each `extra` entry stored as it says.
    fn book_zip(
        version: &str,
        meta: &str,
        chs: &[Ch],
        repeat: usize,
        extra: &[(&str, &[u8], testzip::Store)],
    ) -> Vec<u8> {
        let manifest: String = chs
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    r#"<item id="i{i}" href="{}" media-type="{}"/>"#,
                    c.href, c.media
                )
            })
            .collect();
        let spine: String = chs
            .iter()
            .enumerate()
            .filter(|(_, c)| c.spine)
            .map(|(i, c)| {
                let linear = if c.linear { "" } else { r#" linear="no""# };
                format!(r#"<itemref idref="i{i}"{linear}/>"#).repeat(repeat)
            })
            .collect();
        let opf = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="{version}" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">{meta}</metadata>
  <manifest>{manifest}</manifest>
  <spine>{spine}</spine>
</package>"#
        );
        let names: Vec<String> = chs.iter().map(|c| format!("OEBPS/{}", c.href)).collect();
        let mut entries: Vec<(&[u8], &[u8], testzip::Store)> = vec![
            (b"mimetype", b"application/epub+zip", Stored),
            (b"META-INF/container.xml", CONTAINER.as_bytes(), Stored),
            (b"OEBPS/content.opf", opf.as_bytes(), Stored),
        ];
        for (c, n) in chs.iter().zip(&names) {
            if let Some(d) = &c.data {
                entries.push((n.as_bytes(), d, Stored));
            }
        }
        for &(n, d, store) in extra {
            entries.push((n.as_bytes(), d, store));
        }
        testzip::zip(&entries)
    }

    /// An EPUB 3 book of `chs` at `root/<name>`.
    fn book(root: &Path, name: &str, meta: &str, chs: &[Ch]) -> PathBuf {
        let p = root.join(name);
        book_at(&p, "3.0", meta, chs, 1, &[]);
        p
    }

    /// `encryption.xml` listing each `(algorithm, uri)` as `EncryptedData`.
    fn encryption(list: &[(&str, &str)]) -> Vec<u8> {
        let data: String = list
            .iter()
            .map(|(alg, uri)| {
                format!(
                    r#"<enc:EncryptedData><enc:EncryptionMethod Algorithm="{alg}"/><enc:CipherData><enc:CipherReference URI="{uri}"/></enc:CipherData></enc:EncryptedData>"#
                )
            })
            .collect();
        format!(
            r#"<?xml version="1.0"?>
<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#">{data}</encryption>"#
        )
        .into_bytes()
    }

    /// Runs the action on one book: its output's text (empty when none),
    /// the `warn` messages, and the output.
    fn txt(book: &Path, work: &Path) -> (String, Vec<String>, Output) {
        let o = run_raw(ID, work, &[book]);
        let out = stdout(&o);
        let warns = lines(&out, "warn")
            .iter()
            .map(|w| w[2].to_string())
            .collect();
        let text = lines(&out, "done")
            .first()
            .map(|d| fs::read_to_string(work.join(d[2])).unwrap())
            .unwrap_or_default();
        (text, warns, o)
    }

    /// The input failed with `reason` and nothing is left in `work`.
    fn fails_alone(o: &Output, work: &Path, reason: &str) {
        let out = stdout(o);
        assert_eq!(o.status.code(), Some(1), "{out}");
        let fails = lines(&out, "fail");
        assert_eq!(fails.len(), 1, "{out}");
        assert_eq!(fails[0][2], reason, "{out}");
        assert!(lines(&out, "done").is_empty(), "{out}");
        let left: Vec<_> = snapshot(work).into_iter().map(|(p, _)| p).collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[test]
    fn manifest_lists_epub_to_txt() {
        let o = Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
            .arg("--manifest")
            .output()
            .unwrap();
        assert!(stdout(&o).lines().any(|l| l
            == "action\ttext.epub-to-txt\tfamily=text\tin=application/epub+zip\t\
                out=text/plain\text=txt\tlabel=Plain text\tgroup=document\tcost=10\t\
                lossy=yes\tbatch=yes"));
    }

    /// The title and the authors head the file, then every spine item in
    /// spine order (`linear="no"` in its place), a blank line between
    /// chapters; each chapter's own heading is its title, none is added.
    #[test]
    fn epub3_to_txt_header_and_order() {
        let (root, work) = workdir("epub-order");
        let mut note = ch("notes.xhtml", "<p>Note.</p>");
        note.linear = false;
        let b = book(
            &root,
            "novel.epub",
            "<dc:title> The   Book </dc:title><dc:creator>Ann</dc:creator>\
             <dc:creator>Bob</dc:creator>",
            &[
                ch("c1.xhtml", "<h1>One</h1><p>First.</p>"),
                note,
                ch("c2.xhtml", "<h1>Two</h1><p>Second.</p>"),
            ],
        );
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(
            text,
            "The Book\nAnn, Bob\n\nOne\n\nFirst.\n\nNote.\n\nTwo\n\nSecond.\n"
        );
        assert_eq!(warns, [STYLES]);
        let out = stdout(&o);
        assert_eq!(lines(&out, "done")[0][2], "0/novel.txt");
    }

    /// What is missing of the header is left out; with neither, the
    /// header and its blank line are.
    #[test]
    fn header_parts_are_left_out_when_missing() {
        let (root, work) = workdir("epub-header");
        let body = [ch("c1.xhtml", "<p>Text.</p>")];
        for (k, (meta, want)) in [
            ("<dc:title>T</dc:title>", "T\n\nText.\n"),
            ("<dc:creator>A</dc:creator>", "A\n\nText.\n"),
            ("<dc:title> </dc:title>", "Text.\n"),
        ]
        .into_iter()
        .enumerate()
        {
            let b = book(&root, &format!("b{k}.epub"), meta, &body);
            let w = work.join(k.to_string());
            fs::create_dir_all(&w).unwrap();
            let (text, _, o) = txt(&b, &w);
            assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
            assert_eq!(text, want, "{meta}");
        }
    }

    /// A real EPUB 2 chapter: the XHTML 1.1 DOCTYPE and HTML named
    /// characters; the NCX is in the manifest, not read.
    #[test]
    fn epub2_to_txt() {
        let (root, work) = workdir("epub2");
        let chapter = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
            <!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \
            \"http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd\">\n\
            <html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>c</title>\
            <link rel=\"stylesheet\" href=\"s.css\"/></head><body>\
            <h1>Caf&eacute;</h1><p class=\"x\">Au&nbsp;lait &amp; cr&egrave;me.</p></body></html>";
        let ncx = Ch {
            href: "toc.ncx".into(),
            media: "application/x-dtbncx+xml",
            data: Some(b"<ncx/>".to_vec()),
            spine: false,
            linear: true,
        };
        let chapter = Ch {
            href: "ch.xhtml".into(),
            media: XHTML,
            data: Some(chapter.as_bytes().to_vec()),
            spine: true,
            linear: true,
        };
        let p = root.join("old.epub");
        book_at(
            &p,
            "2.0",
            "<dc:title>Old</dc:title>",
            &[ncx, chapter],
            1,
            &[],
        );
        let (text, warns, o) = txt(&p, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(text, "Old\n\nCaf\u{e9}\n\nAu\u{a0}lait & cr\u{e8}me.\n");
        assert_eq!(warns, [STYLES]);
    }

    /// Chapter 29 of 30 is not well-formed: its line stands in its place,
    /// the rest converts and one counted `warn` says so.
    #[test]
    fn bad_late_chapter_marked_in_place() {
        let (root, work) = workdir("epub-bad29");
        let chs: Vec<Ch> = (1..=30)
            .map(|i| {
                if i == 29 {
                    ch("c29.xhtml", "<p>broken</b></p>")
                } else {
                    ch(&format!("c{i}.xhtml"), &format!("<p>chapter {i}</p>"))
                }
            })
            .collect();
        let b = book(&root, "b.epub", "", &chs);
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert!(text.starts_with("chapter 1\n\nchapter 2\n"), "{text}");
        assert!(
            text.ends_with(
                "chapter 28\n\n[Chapter 29 could not be read: not well-formed XML (line 2)]\
                 \n\nchapter 30\n"
            ),
            "{text}"
        );
        assert_eq!(warns, ["1 chapter(s) could not be read", STYLES]);
    }

    /// Every chapter's reason in its place: a missing entry, another
    /// type, an unsupported encoding, a DTD entity, nesting past the XML
    /// depth, the node budget.
    #[test]
    fn chapter_reasons_in_place() {
        let (root, work) = workdir("epub-reasons");
        let mut missing = ch("gone.xhtml", "");
        missing.data = None;
        let html = Ch {
            href: "p.html".into(),
            media: "text/html",
            data: Some(b"<p>x</p>".to_vec()),
            spine: true,
            linear: true,
        };
        let mut latin = ch("latin.xhtml", "");
        latin.data =
            Some(b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><html><p>x</p></html>".to_vec());
        let mut entity = ch("ent.xhtml", "");
        entity.data = Some(b"<!DOCTYPE html [<!ENTITY e \"x\">]><html><p>&e;</p></html>".to_vec());
        let deep = ch("deep.xhtml", &"<div>".repeat(300));
        let nodes = ch("nodes.xhtml", &format!("<p>{}</p>", "<b/>".repeat(1 << 19)));
        let b = book(
            &root,
            "b.epub",
            "",
            &[
                ch("ok.xhtml", "<p>fine</p>"),
                missing,
                html,
                latin,
                entity,
                deep,
                nodes,
            ],
        );
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(
            text,
            "fine\n\n\
             [Chapter 2 could not be read: missing]\n\n\
             [Chapter 3 could not be read: unsupported type text/html]\n\n\
             [Chapter 4 could not be read: unsupported encoding ISO-8859-1]\n\n\
             [Chapter 5 could not be read: entity declarations are not supported]\n\n\
             [Chapter 6 could not be read: too large (nesting limit)]\n\n\
             [Chapter 7 could not be read: too large (nodes limit)]\n"
        );
        assert_eq!(warns, ["6 chapter(s) could not be read", STYLES]);
    }

    #[test]
    fn all_chapters_bad_is_no_output() {
        let (root, work) = workdir("epub-allbad");
        let b = book(
            &root,
            "b.epub",
            "<dc:title>T</dc:title>",
            &[ch("a.xhtml", "<p>x</b>"), ch("b.xhtml", "<p>y</i>")],
        );
        let o = run_raw(ID, &work, &[&b]);
        fails_alone(&o, &work, "no chapter could be read");
    }

    #[test]
    fn drm_book_fails_with_reason() {
        let (root, work) = workdir("epub-drm");
        let p = root.join("drm.epub");
        let enc = encryption(&[(AES, "OEBPS/a.xhtml"), (AES, "OEBPS/b.xhtml")]);
        book_at(
            &p,
            "3.0",
            "",
            &[ch("a.xhtml", "<p>x</p>"), ch("b.xhtml", "<p>y</p>")],
            1,
            &[("META-INF/encryption.xml", &enc)],
        );
        let o = run_raw(ID, &work, &[&p]);
        fails_alone(&o, &work, "book is DRM-protected");
    }

    /// One encrypted chapter among readable ones is not DRM: its line
    /// stands in its place and is counted once.
    #[test]
    fn partially_encrypted_chapter_in_place() {
        let (root, work) = workdir("epub-enc1");
        let p = root.join("enc.epub");
        let enc = encryption(&[(AES, "OEBPS/b.xhtml")]);
        book_at(
            &p,
            "3.0",
            "",
            &[
                ch("a.xhtml", "<p>a</p>"),
                ch("b.xhtml", "<p>b</p>"),
                ch("c.xhtml", "<p>c</p>"),
            ],
            1,
            &[("META-INF/encryption.xml", &enc)],
        );
        let (text, warns, o) = txt(&p, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(text, "a\n\n[Chapter 2 could not be read: encrypted]\n\nc\n");
        assert_eq!(warns, ["1 chapter(s) could not be read", STYLES]);
    }

    /// A chapter in UTF-16 (little-endian, with its byte order mark) is
    /// decoded: its text comes out as UTF-8, characters intact.
    #[test]
    fn utf16le_bom_chapter() {
        let (root, work) = workdir("epub-utf16");
        let src = String::from_utf8(xhtml("<p>Árvíztűrő tükörfúrógép ✓</p>"))
            .unwrap()
            .replace("UTF-8", "UTF-16");
        let mut data = vec![0xFF, 0xFE];
        data.extend(src.encode_utf16().flat_map(u16::to_le_bytes));
        let b = book(
            &root,
            "b.epub",
            "",
            &[
                Ch {
                    href: "u.xhtml".into(),
                    media: XHTML,
                    data: Some(data),
                    spine: true,
                    linear: true,
                },
                ch("a.xhtml", "<p>next</p>"),
            ],
        );
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(text, "Árvíztűrő tükörfúrógép ✓\n\nnext\n");
        assert_eq!(warns, [STYLES]);
    }

    /// Font obfuscation is not DRM: the book converts and only the styles
    /// loss is reported.
    #[test]
    fn font_obfuscated_book_converts_without_warning() {
        let (root, work) = workdir("epub-font");
        let p = root.join("font.epub");
        let enc = encryption(&[(IDPF, "OEBPS/f.otf")]);
        book_at(
            &p,
            "3.0",
            "",
            &[ch("a.xhtml", "<p>x</p>")],
            1,
            &[
                ("META-INF/encryption.xml", &enc),
                ("OEBPS/f.otf", b"scrambled font"),
            ],
        );
        let (text, warns, o) = txt(&p, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(text, "x\n");
        assert_eq!(warns, [STYLES]);
    }

    /// Pictures only (an image page and an SVG page): no text, no file.
    #[test]
    fn no_text_book_fails() {
        let (root, work) = workdir("epub-notext");
        let svg = Ch {
            href: "p.svg".into(),
            media: "image/svg+xml",
            data: Some(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec()),
            spine: true,
            linear: true,
        };
        let b = book(
            &root,
            "pics.epub",
            "<dc:title>Pictures</dc:title>",
            &[
                ch("c.xhtml", "<p><img src=\"a.jpg\" alt=\"Cover\"/></p>"),
                svg,
            ],
        );
        let o = run_raw(ID, &work, &[&b]);
        fails_alone(&o, &work, "book has no text");
    }

    /// An SVG page in the spine has no text: it is left out and reported
    /// as an image loss.
    #[test]
    fn svg_page_is_an_image_loss() {
        let (root, work) = workdir("epub-svg");
        let svg = Ch {
            href: "p.svg".into(),
            media: "image/svg+xml",
            data: Some(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec()),
            spine: true,
            linear: true,
        };
        let b = book(&root, "b.epub", "", &[svg, ch("c.xhtml", "<p>x</p>")]);
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(text, "x\n");
        assert_eq!(warns, [IMAGES, STYLES]);
    }

    /// A cover page of one image and the navigation document in the
    /// spine: the cover is its alt text (an image loss), the table of
    /// contents a list of text (book-internal links), the book has text.
    #[test]
    fn cover_and_toc_pages() {
        let (root, work) = workdir("epub-cover");
        let b = book(
            &root,
            "b.epub",
            "",
            &[
                ch(
                    "cover.xhtml",
                    "<div class=\"cover\"><img src=\"cover.jpg\" alt=\"Cover\"/></div>",
                ),
                ch(
                    "nav.xhtml",
                    "<nav epub:type=\"toc\"><h1>Contents</h1><ol>\
                     <li><a href=\"c1.xhtml\">One</a></li>\
                     <li><a href=\"c1.xhtml#s2\">Two</a></li></ol></nav>",
                ),
                ch("c1.xhtml", "<h1>One</h1><p>Text.</p><h1 id=\"s2\">Two</h1>"),
            ],
        );
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(
            text,
            "Cover\n\nContents\n\n1. One\n2. Two\n\nOne\n\nText.\n\nTwo\n"
        );
        assert_eq!(warns, [LINKS, IMAGES, STYLES]);
    }

    /// A book-internal link is its text only; an external one keeps its
    /// destination after the text. Either is a links loss.
    #[test]
    fn internal_links_are_text_only() {
        let (root, work) = workdir("epub-links");
        let b = book(
            &root,
            "b.epub",
            "",
            &[ch(
                "c.xhtml",
                "<p>See<a href=\"notes.xhtml#fn1\">1</a>, <a href=\"#x\">here</a>, \
                 <a href=\"../Text/c2.xhtml\">there</a>, \
                 <a href=\"https://example.org/\">site</a>, \
                 <a href=\"http://example.org/a\">old</a>, \
                 <a href=\"//example.org/b\">net</a> and \
                 <a href=\"mailto:a@example.org\">mail</a>.</p>",
            )],
        );
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(
            text,
            "See1, here, there, site (https://example.org/), \
             old (http://example.org/a), net (//example.org/b) and \
             mail (mailto:a@example.org).\n"
        );
        assert_eq!(warns, [LINKS, STYLES]);
    }

    /// The chapters to read are over 64 MiB in all (one chapter of
    /// nearly 8 MiB, nine times in the spine): nothing is read or left.
    #[test]
    fn book_too_large_leaves_nothing() {
        let (root, work) = workdir("epub-toolarge");
        let p = root.join("big.epub");
        let body = format!("<p>{}</p>", "word ".repeat((8 << 20) / 5 - 100));
        book_at(&p, "3.0", "", &[ch("c.xhtml", &body)], 9, &[]);
        let o = run_raw(ID, &work, &[&p]);
        fails_alone(&o, &work, "book is too large");
    }

    /// The text would pass the 32 MiB output budget (a chapter of nearly
    /// 8 MiB of text, five times in the spine: 40 MiB in, below the 64 MiB
    /// book limit): the file is removed and the input fails.
    #[test]
    fn output_budget_is_book_too_large() {
        let (root, work) = workdir("epub-outbudget");
        let p = root.join("big.epub");
        let para = format!("<p>{}</p>", "word ".repeat(200));
        let body = para.repeat((8 << 20) / para.len() - 1);
        book_at(&p, "3.0", "", &[ch("c.xhtml", &body)], 5, &[]);
        let o = run_raw(ID, &work, &[&p]);
        fails_alone(&o, &work, "book is too large");
    }

    /// Past the action's own `RLIMIT_FSIZE` (1 MiB here) the book fails
    /// with the size copy, nothing of it is left and the next book of the
    /// batch still converts.
    #[test]
    fn output_ceiling_fails_alone() {
        let (root, work) = workdir("epub-ceiling");
        let para = format!("<p>{}</p>", "abcdefghi ".repeat(100));
        let chs: Vec<Ch> = (0..3)
            .map(|i| ch(&format!("c{i}.xhtml"), &para.repeat(600)))
            .collect();
        let big = book(&root, "big.epub", "", &chs);
        let small = book(&root, "small.epub", "", &[ch("c.xhtml", "<p>x</p>")]);
        let o = run_with_fsize(ID, &work, &[&big, &small]);
        let out = stdout(&o);
        assert_eq!(o.status.code(), Some(0), "{out}");
        let fails = lines(&out, "fail");
        assert_eq!(fails.len(), 1, "{out}");
        assert_eq!(fails[0][1], big.to_str().unwrap());
        assert_eq!(fails[0][2], "Plain text would be larger than 1 MiB");
        let done = lines(&out, "done");
        assert_eq!(done.len(), 1, "{out}");
        assert_eq!(done[0][1], small.to_str().unwrap());
        let left: Vec<_> = snapshot(&work).into_iter().map(|(p, _)| p).collect();
        assert_eq!(left, [work.join(done[0][2])]);
    }

    /// A 100 KB encoding name in the package document or in
    /// `container.xml` fails that book with the name cut to 40 characters
    /// plus `...`: its `fail` line stays far under the supervisor's 64 KiB
    /// event-line limit (past it the whole batch would stop), and the next
    /// book of the batch still converts.
    #[test]
    fn long_skeleton_encoding_name_is_cut() {
        let (root, work) = workdir("epub-long-encoding");
        let name = "a".repeat(100_000);
        let decl = format!("<?xml version=\"1.0\" encoding=\"{name}\"?>");
        let opf = format!("{decl}<package/>");
        let opf_book = root.join("opf.epub");
        fs::write(
            &opf_book,
            testzip::zip(&[
                (b"mimetype", b"application/epub+zip", Stored),
                (b"META-INF/container.xml", CONTAINER.as_bytes(), Stored),
                (b"OEBPS/content.opf", opf.as_bytes(), Stored),
            ]),
        )
        .unwrap();
        let container = format!("{decl}<container/>");
        let container_book = root.join("container.epub");
        fs::write(
            &container_book,
            testzip::zip(&[
                (b"mimetype", b"application/epub+zip", Stored),
                (b"META-INF/container.xml", container.as_bytes(), Stored),
            ]),
        )
        .unwrap();
        let good = book(&root, "good.epub", "", &[ch("c.xhtml", "<p>x</p>")]);
        let o = run_raw(ID, &work, &[&opf_book, &container_book, &good]);
        let out = stdout(&o);
        let forty = "a".repeat(40);
        let fails = lines(&out, "fail");
        assert_eq!(fails.len(), 2, "{}", &out[..out.len().min(2000)]);
        assert_eq!(fails[0][1], opf_book.to_str().unwrap());
        assert_eq!(
            fails[0][2],
            format!("not an EPUB (unsupported encoding {forty}... in OEBPS/content.opf)")
        );
        assert_eq!(fails[1][1], container_book.to_str().unwrap());
        assert_eq!(
            fails[1][2],
            format!("not an EPUB (unsupported encoding {forty}... in META-INF/container.xml)")
        );
        for l in out.lines() {
            assert!(l.len() < 1024, "line of {} bytes", l.len());
        }
        let done = lines(&out, "done");
        assert_eq!(done.len(), 1, "{out}");
        assert_eq!(done[0][1], good.to_str().unwrap());
        assert_eq!(fs::read_to_string(work.join(done[0][2])).unwrap(), "x\n");
    }

    /// A book of nothing but picture pages has no text, and it read: the
    /// failure is "no text", not "no chapter could be read".
    #[test]
    fn picture_only_book_has_no_text() {
        let (root, work) = workdir("epub-svg-only");
        let svg = Ch {
            href: "p.svg".into(),
            media: "image/svg+xml",
            data: Some(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec()),
            spine: true,
            linear: true,
        };
        let b = book(&root, "pics.epub", "", &[svg]);
        let o = run_raw(ID, &work, &[&b]);
        fails_alone(&o, &work, "book has no text");
    }

    /// The whole book is held to the 32 MiB output budget, separators and
    /// head included: a book of exactly 32 MiB converts, one byte more
    /// fails (the last chapter alone still fits what was left before its
    /// separator, so only the file's own check can refuse it). Two
    /// chapters of 5,592,000 bytes of text, three times each in the
    /// spine: 3 * 11,184,000 + 5 blank-line separators + the head (title
    /// of `len` bytes, its line feed and the blank line).
    #[test]
    fn output_budget_boundary_is_exact() {
        const LIMIT: usize = 32 << 20;
        let (root, work) = workdir("epub-outbudget-exact");
        for (len, ok) in [(2425usize, true), (2426, false)] {
            let p = root.join(format!("b{len}.epub"));
            let para = format!("<p>{}</p>", "a".repeat(5_591_999));
            let chs = [ch("c0.xhtml", &para), ch("c1.xhtml", &para)];
            let meta = format!("<dc:title>{}</dc:title>", "t".repeat(len));
            book_at(&p, "3.0", &meta, &chs, 3, &[]);
            assert_eq!((len + 2) + 3 * 11_184_000 + 5, LIMIT - 2425 + len);
            let o = run_raw(ID, &work, &[&p]);
            if ok {
                let out = stdout(&o);
                assert_eq!(o.status.code(), Some(0), "{out}");
                let done = lines(&out, "done");
                let size = fs::metadata(work.join(done[0][2])).unwrap().len();
                assert_eq!(size, LIMIT as u64);
                fs::remove_file(work.join(done[0][2])).unwrap();
            } else {
                fails_alone(&o, &work, "book is too large");
            }
        }
    }

    /// An output of exactly `RLIMIT_FSIZE` bytes is written (the file may
    /// reach the ceiling); one byte more fails with the size copy.
    #[test]
    fn output_of_exactly_the_ceiling_is_written() {
        const CAP: u64 = 1 << 20;
        let (root, work) = workdir("epub-ceiling-exact");
        // One paragraph of `n` bytes is `n + 1` bytes of text; no head.
        let at = book(
            &root,
            "at.epub",
            "",
            &[ch(
                "c.xhtml",
                &format!("<p>{}</p>", "a".repeat(CAP as usize - 1)),
            )],
        );
        let over = book(
            &root,
            "over.epub",
            "",
            &[ch(
                "c.xhtml",
                &format!("<p>{}</p>", "a".repeat(CAP as usize)),
            )],
        );
        let o = run_with_fsize_of(ID, &work, &[&at, &over], CAP);
        let out = stdout(&o);
        assert_eq!(o.status.code(), Some(0), "{out}");
        let done = lines(&out, "done");
        assert_eq!(done.len(), 1, "{out}");
        assert_eq!(done[0][1], at.to_str().unwrap());
        assert_eq!(fs::metadata(work.join(done[0][2])).unwrap().len(), CAP);
        let fails = lines(&out, "fail");
        assert_eq!(fails.len(), 1, "{out}");
        assert_eq!(fails[0][1], over.to_str().unwrap());
        assert_eq!(fails[0][2], "Plain text would be larger than 1 MiB");
    }

    #[test]
    fn deterministic_output() {
        let (root, work) = workdir("epub-det");
        let b = book(
            &root,
            "b.epub",
            "<dc:title>T</dc:title><dc:creator>A</dc:creator>",
            &[
                ch(
                    "a.xhtml",
                    "<h1>A</h1><p>x <em>y</em></p><table><tr><td>1</td></tr></table>",
                ),
                ch("b.xhtml", "<p>z</p>"),
            ],
        );
        let (w1, w2) = (work.join("1"), work.join("2"));
        fs::create_dir_all(&w1).unwrap();
        fs::create_dir_all(&w2).unwrap();
        let (t1, warns1, _) = txt(&b, &w1);
        let (t2, warns2, _) = txt(&b, &w2);
        assert!(!t1.is_empty());
        assert_eq!(t1, t2);
        assert_eq!(warns1, warns2);
    }

    /// Losses of several chapters are listed once each, after the one
    /// counted line for the chapters that could not be read, well under
    /// the supervisor's 16 `warn` per input.
    #[test]
    fn losses_listed_once() {
        let (root, work) = workdir("epub-losses");
        let body = "<p><img src=\"i.png\" alt=\"pic\"/> <a href=\"https://x.org/\">x</a></p>\
                    <table><tr><td colspan=\"2\">wide</td></tr><tr><td>a</td><td>b</td></tr></table>";
        let b = book(
            &root,
            "b.epub",
            "",
            &[
                ch("a.xhtml", body),
                ch("b.xhtml", "<p>bad</i>"),
                ch("c.xhtml", body),
                ch("d.xhtml", "<p>bad</i>"),
            ],
        );
        let (text, warns, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert!(text.contains("pic x (https://x.org/)"), "{text}");
        assert_eq!(
            warns,
            [
                "2 chapter(s) could not be read",
                LINKS,
                IMAGES,
                STYLES,
                MERGED
            ]
        );
        assert!(warns.len() <= 16);
    }

    /// The headerless table of a book (the reader's empty header) is
    /// written without an empty line over a row of dashes.
    #[test]
    fn headerless_table_has_no_empty_header() {
        let (root, work) = workdir("epub-table");
        let b = book(
            &root,
            "b.epub",
            "",
            &[ch(
                "c.xhtml",
                "<table><tr><td>apple</td><td>3</td></tr><tr><td>fig</td><td>12</td></tr></table>",
            )],
        );
        let (text, _, o) = txt(&b, &work);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(text, "apple  3\nfig    12\n");
    }

    /// Not a ZIP at all: the reader's reason, no file.
    #[test]
    fn not_an_epub_fails() {
        let (root, work) = workdir("epub-notzip");
        let p = root.join("x.epub");
        fs::write(&p, "just text").unwrap();
        let o = run_raw(ID, &work, &[&p]);
        let out = stdout(&o);
        assert_eq!(o.status.code(), Some(1), "{out}");
        assert!(
            lines(&out, "fail")[0][2].starts_with("damaged EPUB: "),
            "{out}"
        );
        assert!(snapshot(&work).is_empty());
    }

    // ---------- text.epub-to-md ----------

    mod md {
        use super::*;
        use testzip::Store::Deflated;

        const MD: &str = "text.epub-to-md";

        /// A book at `root/<name>` (EPUB 3) with `extra` entries (images).
        fn book_with(
            root: &Path,
            name: &str,
            meta: &str,
            chs: &[Ch],
            extra: &[(&str, &[u8], testzip::Store)],
        ) -> PathBuf {
            let p = root.join(name);
            fs::write(&p, book_zip("3.0", meta, chs, 1, extra)).unwrap();
            p
        }

        /// What one run on one book gave: the md (empty when none), every
        /// `done` path in order, the `warn` messages, and the output.
        struct Md {
            text: String,
            done: Vec<String>,
            warns: Vec<String>,
            o: Output,
        }

        fn md(book: &Path, work: &Path) -> Md {
            let o = run_raw(MD, work, &[book]);
            let out = stdout(&o);
            let done: Vec<String> = lines(&out, "done")
                .iter()
                .map(|d| d[2].to_string())
                .collect();
            let warns = lines(&out, "warn")
                .iter()
                .map(|w| w[2].to_string())
                .collect();
            let text = done
                .last()
                .filter(|d| d.ends_with(".md"))
                .map(|d| fs::read_to_string(work.join(d)).unwrap())
                .unwrap_or_default();
            Md {
                text,
                done,
                warns,
                o,
            }
        }

        fn ok(r: &Md) {
            assert_eq!(r.o.status.code(), Some(0), "{}", stdout(&r.o));
            assert!(lines(&stdout(&r.o), "fail").is_empty(), "{}", stdout(&r.o));
        }

        /// The fixture JPEG with `pad` bytes of zeros in comment segments
        /// after its SOI: still a valid JPEG, `pad` bytes larger, and
        /// nearly nothing more once deflated in the book.
        fn padded_jpeg(pad: usize) -> Vec<u8> {
            let j = jpeg();
            let mut out = j[..2].to_vec();
            let mut left = pad;
            while left > 0 {
                // `n` bytes in all: the marker, then a length that counts
                // itself and the data (at most 65535).
                let n = left.clamp(4, 65_537);
                out.extend_from_slice(&[0xFF, 0xFE]);
                out.extend_from_slice(&((n - 2) as u16).to_be_bytes());
                out.extend(std::iter::repeat_n(0u8, n - 4));
                left = left.saturating_sub(n);
            }
            out.extend_from_slice(&j[2..]);
            out
        }

        #[test]
        fn manifest_lists_epub_to_md() {
            let o = Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
                .arg("--manifest")
                .output()
                .unwrap();
            assert!(stdout(&o).lines().any(|l| l
                == "action\ttext.epub-to-md\tfamily=text\tin=application/epub+zip\t\
                    out=text/markdown\text=md\tlabel=Markdown\tgroup=document\tcost=10\t\
                    lossy=yes\tbatch=no"));
        }

        /// `# <title>`, the authors as a paragraph, then every spine item in
        /// spine order a blank line apart; a table is a GFM pipe table.
        #[test]
        fn epub_to_md_header_order_and_gfm_table() {
            let (root, work) = workdir("md-order");
            let mut note = ch("notes.xhtml", "<p>Note.</p>");
            note.linear = false;
            let b = book(
                &root,
                "novel.epub",
                "<dc:title> The   Book </dc:title><dc:creator>Ann</dc:creator>\
                 <dc:creator>Bob</dc:creator>",
                &[
                    ch("c1.xhtml", "<h1>One</h1><p>First <em>word</em>.</p>"),
                    note,
                    ch(
                        "c2.xhtml",
                        "<h1>Two</h1><table><thead><tr><th>a</th><th style=\"text-align:right\">b</th></tr></thead>\
                         <tbody><tr><td>1 | 2</td><td>3<br/>4</td></tr></tbody></table>",
                    ),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "# The Book\n\nAnn, Bob\n\n# One\n\nFirst *word*.\n\nNote.\n\n# Two\n\n\
                 | a | b |\n| --- | --- |\n| 1 \\| 2 | 3 4 |\n"
            );
            assert_eq!(r.done, ["0/novel.md"]);
            assert_eq!(r.warns, [STYLES, "table cell line breaks became spaces"]);
        }

        /// The images a chapter shows go beside the md byte for byte, named
        /// by the input's stem in the order of their first reference, and
        /// reported before the md; an image no chapter shows is not
        /// written.
        #[test]
        fn images_written_beside_the_md() {
            let (root, work) = workdir("md-images");
            let (j, p) = (jpeg(), png(7));
            let b = book_with(
                &root,
                "novel.epub",
                "",
                &[ch(
                    "c1.xhtml",
                    "<p>See <em><img src=\"img/b.png\" alt=\"B\"/></em> and <img src=\"img/a.jpg\" alt=\"A\" title=\"t\"/>.</p>",
                )],
                &[
                    ("OEBPS/img/a.jpg", &j, Stored),
                    ("OEBPS/img/b.png", &p, Deflated),
                    ("OEBPS/img/unused.jpg", &j, Stored),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.done,
                ["0/novel-0001.png", "0/novel-0002.jpg", "0/novel.md"]
            );
            assert_eq!(fs::read(work.join("0/novel-0001.png")).unwrap(), p);
            assert_eq!(fs::read(work.join("0/novel-0002.jpg")).unwrap(), j);
            assert_eq!(
                r.text,
                "See *![B](novel-0001.png)* and ![A](novel-0002.jpg \"t\").\n"
            );
            assert_eq!(snapshot(&work).len(), 3);
            assert_eq!(r.warns, [STYLES]);
        }

        /// One image named by two paths from two chapters is one file, and
        /// both references point to it.
        #[test]
        fn same_image_two_paths_one_file() {
            let (root, work) = workdir("md-same-image");
            let j = jpeg();
            let b = book_with(
                &root,
                "b.epub",
                "",
                &[
                    ch(
                        "Text/c1.xhtml",
                        "<p><img src=\"../Images/a.jpg\" alt=\"one\"/></p>",
                    ),
                    ch(
                        "Text/c2.xhtml",
                        "<p><img src=\"../Images/./a.jpg\" alt=\"two\"/></p>",
                    ),
                ],
                &[("OEBPS/Images/a.jpg", &j, Stored)],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done, ["0/b-0001.jpg", "0/b.md"]);
            assert_eq!(r.text, "![one](b-0001.jpg)\n\n![two](b-0001.jpg)\n");
        }

        /// GIF, WebP and SVG are never written: their places keep the alt
        /// text and one counted `warn` says so.
        #[test]
        fn gif_webp_svg_become_alt_text() {
            let (root, work) = workdir("md-gif");
            let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
            let webp = b"RIFF\x1a\x00\x00\x00WEBPVP8L\x0d\x00\x00\x00/\x00\x00\x00\x10\x07\x10\x11\x11\x88\x88\xfe\x07\x00".to_vec();
            let svg =
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>x</script></svg>".to_vec();
            let b = book_with(
                &root,
                "b.epub",
                "",
                &[ch(
                    "c.xhtml",
                    "<p>a <img src=\"a.gif\" alt=\"G\"/> <img src=\"a.webp\" alt=\"W\"/> <img src=\"a.svg\" alt=\"S\"/></p>",
                )],
                &[
                    ("OEBPS/a.gif", &gif, Stored),
                    ("OEBPS/a.webp", &webp, Stored),
                    ("OEBPS/a.svg", &svg, Stored),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done, ["0/b.md"]);
            assert_eq!(r.text, "a G W S\n");
            assert_eq!(r.warns, ["3 image(s) replaced by alt text", STYLES]);
            assert_eq!(snapshot(&work).len(), 1);
        }

        /// 1023 images are written; the 1024th keeps its alt text, so the
        /// md and its images are at most 1024 `done` lines.
        #[test]
        fn image_limit_1023() {
            let (root, work) = workdir("md-1023");
            let p = png(1);
            let names: Vec<String> = (1..=1024).map(|k| format!("OEBPS/i/{k}.png")).collect();
            let extra: Vec<(&str, &[u8], testzip::Store)> =
                names.iter().map(|n| (n.as_str(), &p[..], Stored)).collect();
            let body: String = (1..=1024)
                .map(|k| format!("<p><img src=\"i/{k}.png\" alt=\"i{k}\"/></p>"))
                .collect();
            let b = book_with(&root, "b.epub", "", &[ch("c.xhtml", &body)], &extra);
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done.len(), 1024);
            assert_eq!(r.done[1022], "0/b-1023.png");
            assert_eq!(r.done[1023], "0/b.md");
            assert!(
                r.text.contains("![i1023](b-1023.png)\n\ni1024\n"),
                "{}",
                &r.text[r.text.len() - 200..]
            );
            assert_eq!(r.warns, ["1 image(s) replaced by alt text", STYLES]);
        }

        /// The images read are at most twice the book file in all: two
        /// images that each nearly match the book fit, a third does not
        /// and keeps its alt text (they deflate to almost nothing, so the
        /// book stays small).
        #[test]
        fn image_total_over_twice_the_book() {
            let (root, work) = workdir("md-budget");
            let build = |pad: usize| {
                let j = padded_jpeg(pad);
                let b = book_with(
                    &root,
                    "b.epub",
                    "",
                    &[ch(
                        "c.xhtml",
                        "<p>x <img src=\"1.jpg\" alt=\"one\"/> <img src=\"2.jpg\" alt=\"two\"/> <img src=\"3.jpg\" alt=\"three\"/></p>",
                    )],
                    &[
                        ("OEBPS/1.jpg", &j, Deflated),
                        ("OEBPS/2.jpg", &j, Deflated),
                        ("OEBPS/3.jpg", &j, Deflated),
                    ],
                );
                (b, j.len() as u64)
            };
            let (b, plain) = build(0);
            let size = fs::metadata(&b).unwrap().len();
            // Each image about 85% of the book.
            let (b, image) = build((size * 85 / 100 - plain) as usize);
            let size = fs::metadata(&b).unwrap().len();
            // The premise: two fit in twice the book, three do not.
            assert!(
                2 * image <= 2 * size && 3 * image > 2 * size,
                "{image} {size}"
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done, ["0/b-0001.jpg", "0/b-0002.jpg", "0/b.md"]);
            assert_eq!(r.text, "x ![one](b-0001.jpg) ![two](b-0002.jpg) three\n");
            assert_eq!(r.warns, ["1 image(s) replaced by alt text", STYLES]);
        }

        /// A damaged image is charged its declared size too: the ZIP
        /// reader inflates it whole before the CRC tells it is wrong.
        /// Four damaged images of about 45% of the book each spend 180%
        /// of the twice-the-book budget, so a valid fifth of the same size
        /// no longer fits and keeps its alt text. Without the charge, such
        /// a book inflates about a thousand times its size (the ZIP ratio
        /// limit) for nothing; with it, never more than twice.
        #[test]
        fn damaged_images_charge_the_budget() {
            let (root, work) = workdir("md-damaged-budget");
            let build = |n: usize| {
                let bad: Vec<Vec<u8>> = (1..=4u8).map(|k| vec![k; n]).collect();
                let j = padded_jpeg(n.saturating_sub(jpeg().len()));
                let names = ["OEBPS/1.png", "OEBPS/2.png", "OEBPS/3.png", "OEBPS/4.png"];
                let mut extra: Vec<(&str, &[u8], testzip::Store)> = names
                    .iter()
                    .zip(&bad)
                    .map(|(n, d)| (*n, &d[..], Deflated))
                    .collect();
                extra.push(("OEBPS/ok.jpg", &j, Deflated));
                let mut z = book_zip(
                    "3.0",
                    "",
                    &[ch(
                        "c.xhtml",
                        "<p>x <img src=\"1.png\" alt=\"A\"/> <img src=\"2.png\" alt=\"B\"/> \
                         <img src=\"3.png\" alt=\"C\"/> <img src=\"4.png\" alt=\"D\"/> \
                         <img src=\"ok.jpg\" alt=\"K\"/></p>",
                    )],
                    1,
                    &extra,
                );
                // Each damaged entry's CRC, in its local and central
                // header, made wrong.
                for d in &bad {
                    let crc = testzip::crc32(d).to_le_bytes();
                    let at: Vec<usize> = z
                        .windows(4)
                        .enumerate()
                        .filter(|(_, w)| *w == crc)
                        .map(|(k, _)| k)
                        .collect();
                    assert_eq!(at.len(), 2);
                    for k in at {
                        z[k] ^= 1;
                    }
                }
                let b = root.join("b.epub");
                fs::write(&b, &z).unwrap();
                (b, z.len() as u64, j.len() as u64)
            };
            let (_, size, _) = build(1000);
            let n = (size * 45 / 100) as usize;
            let (b, size, image) = build(n);
            // The premise: the four damaged reads fit in twice the book,
            // and the valid image would too on its own, not after them.
            let n = n as u64;
            assert!(
                4 * n <= 2 * size && image <= 2 * size && 4 * n + image > 2 * size,
                "{n} {image} {size}"
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done, ["0/b.md"]);
            assert_eq!(r.text, "x A B C D K\n");
            assert_eq!(r.warns, ["5 image(s) replaced by alt text", STYLES]);
        }

        /// A damaged PNG (a CRC wrong), a truncated JPEG, a missing image
        /// and one outside the book keep their alt text; the book is made.
        #[test]
        fn damaged_image_is_alt_text() {
            let (root, work) = workdir("md-damaged");
            let mut bad = png(3);
            let at = bad.len() - 20;
            bad[at] ^= 0xFF;
            let j = jpeg();
            let cut = &j[..j.len() / 2];
            let b = book_with(
                &root,
                "b.epub",
                "",
                &[ch(
                    "c.xhtml",
                    "<p>t <img src=\"bad.png\" alt=\"P\"/> <img src=\"cut.jpg\" alt=\"J\"/> \
                     <img src=\"none.jpg\" alt=\"M\"/> <img src=\"../../x.jpg\" alt=\"O\"/> \
                     <img src=\"ok.jpg\" alt=\"K\"/></p>",
                )],
                &[
                    ("OEBPS/bad.png", &bad, Stored),
                    ("OEBPS/cut.jpg", cut, Stored),
                    ("OEBPS/ok.jpg", &j, Stored),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done, ["0/b-0001.jpg", "0/b.md"]);
            assert_eq!(r.text, "t P J M O ![K](b-0001.jpg)\n");
            assert_eq!(r.warns, ["4 image(s) replaced by alt text", STYLES]);
        }

        /// Three chapters with a `Notes` heading: the md's headings get
        /// `notes`, `notes-1`, `notes-2` (numbered over the whole book, as
        /// a viewer and the join number them), and a link to each lands on
        /// its own one, read back from the md.
        #[test]
        fn duplicate_headings_across_chapters_link_right() {
            let (root, work) = workdir("md-dup-headings");
            let notes = |k: usize| {
                ch(
                    &format!("c{k}.xhtml"),
                    &format!(
                        "<section id=\"s{k}\"><h2 id=\"n\">Notes</h2><p>note {k}</p></section>"
                    ),
                )
            };
            let b = book(
                &root,
                "b.epub",
                "<dc:title>Book</dc:title>",
                &[
                    ch(
                        "c0.xhtml",
                        "<p><a href=\"c1.xhtml#n\">one</a> <a href=\"c2.xhtml#s2\">two</a> \
                         <a href=\"c3.xhtml\">three</a></p>",
                    ),
                    notes(1),
                    notes(2),
                    notes(3),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "# Book\n\n[one](#notes) [two](#notes-1) [three](#notes-2)\n\n\
                 ## Notes\n\nnote 1\n\n## Notes\n\nnote 2\n\n## Notes\n\nnote 3\n"
            );
            // Read back: each link's id is the heading right above its note.
            let d = doc::md::read_limited_with(
                r.text.as_bytes(),
                doc::Limits::DEFAULT,
                doc::Dialect::Gfm,
            )
            .unwrap();
            let ch = doc::xhtml::Chapter {
                blocks: &d.blocks,
                file: "b.md".into(),
            };
            let a = doc::xhtml::Anchors::build(std::slice::from_ref(&ch));
            let ids = a.top_level_ids(0, &d.blocks);
            for (k, id) in [(1, "notes"), (2, "notes-1"), (3, "notes-2")] {
                let (at, _) = ids.iter().find(|(_, i)| *i == id).unwrap();
                let doc::Block::Paragraph(p) = &d.blocks[at + 1] else {
                    panic!("{id}")
                };
                assert_eq!(p, &[doc::Inline::Text(format!("note {k}"))]);
            }
        }

        /// A chapter that starts with a list of the kind the chapter before
        /// ends with takes the other marker, so a reader keeps them two
        /// lists, and an ordered one keeps its start.
        #[test]
        fn lists_across_chapters_stay_apart() {
            let (root, work) = workdir("md-lists");
            let b = book(
                &root,
                "b.epub",
                "",
                &[
                    ch("c1.xhtml", "<p>x</p><ul><li>end one</li></ul>"),
                    ch("c2.xhtml", "<ul><li>start two</li></ul>"),
                    ch("c3.xhtml", "<ul><li>three</li></ul><ol><li>one</li></ol>"),
                    ch("c4.xhtml", "<ol start=\"5\"><li>five</li></ol>"),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "x\n\n- end one\n\n* start two\n\n- three\n\n1. one\n\n5) five\n"
            );
            let d = doc::md::read_limited_with(
                r.text.as_bytes(),
                doc::Limits::DEFAULT,
                doc::Dialect::Gfm,
            )
            .unwrap();
            let lists: Vec<Option<u64>> = d
                .blocks
                .iter()
                .filter_map(|b| match b {
                    doc::Block::List { start, .. } => Some(*start),
                    _ => None,
                })
                .collect();
            assert_eq!(lists, [None, None, None, Some(1), Some(5)]);
        }

        /// A heading that starts with a digit is linked by GitHub's anchor,
        /// with no `h-` before it (`#1984`, then `#1984-1`), which the join
        /// still finds; a heading with no anchor text (`!!!`) has no
        /// anchor, so its link keeps only its text.
        #[test]
        fn digit_led_headings_link_github_form() {
            let (root, work) = workdir("md-digit-heading");
            let b = book(
                &root,
                "b.epub",
                "<dc:title>Book</dc:title>",
                &[
                    ch(
                        "c0.xhtml",
                        "<p><a href=\"c1.xhtml\">a</a> <a href=\"c2.xhtml#x\">b</a> \
                         <a href=\"c3.xhtml#e\">c</a></p>",
                    ),
                    ch("c1.xhtml", "<h2>1984</h2><p>one</p>"),
                    ch("c2.xhtml", "<h2 id=\"x\">1984</h2><p>two</p>"),
                    ch("c3.xhtml", "<h2 id=\"e\">!!!</h2><p>three</p>"),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "# Book\n\n[a](#1984) [b](#1984-1) c\n\n## 1984\n\none\n\n## 1984\n\ntwo\n\n\
                 ## \\!\\!\\!\n\nthree\n"
            );
            assert_eq!(r.warns, [LINKS, STYLES]);
            // Read back as the join does: the fragment through the slug
            // rule finds the heading right above its note.
            let d = doc::md::read_limited_with(
                r.text.as_bytes(),
                doc::Limits::DEFAULT,
                doc::Dialect::Gfm,
            )
            .unwrap();
            let ch = doc::xhtml::Chapter {
                blocks: &d.blocks,
                file: "b.md".into(),
            };
            let a = doc::xhtml::Anchors::build(std::slice::from_ref(&ch));
            let ids = a.top_level_ids(0, &d.blocks);
            for (fragment, note) in [("1984", "one"), ("1984-1", "two")] {
                let id = doc::xhtml::slug(fragment);
                let (at, _) = ids.iter().find(|(_, i)| *i == id).unwrap();
                let doc::Block::Paragraph(p) = &d.blocks[at + 1] else {
                    panic!("{id}")
                };
                assert_eq!(p, &[doc::Inline::Text(note.into())]);
            }
        }

        /// A table of contents links to an anchor inside the heading
        /// (`<h2><a id="x"/>...</h2>`, as Project Gutenberg's books do) or
        /// to a span around its text: the link lands on that heading.
        #[test]
        fn toc_anchor_inside_heading_links() {
            let (root, work) = workdir("md-toc-anchor");
            let b = book(
                &root,
                "b.epub",
                "",
                &[
                    ch(
                        "toc.xhtml",
                        "<p><a href=\"c1.xhtml#chap01\">I</a> <a href=\"c2.xhtml#t2\">II</a></p>",
                    ),
                    ch(
                        "c1.xhtml",
                        "<h2><a id=\"chap01\"/>CHAPTER I.</h2><p>one</p>",
                    ),
                    ch(
                        "c2.xhtml",
                        "<h2><span id=\"t2\">CHAPTER II.</span></h2><p>two</p>",
                    ),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "[I](#chapter-i) [II](#chapter-ii)\n\n## CHAPTER I.\n\none\n\n\
                 ## CHAPTER II.\n\ntwo\n"
            );
            assert_eq!(r.warns, [STYLES]);
        }

        /// A link to a footnote anchor (not a heading), to a chapter with
        /// no heading, and with a scheme other than the web's or mail's
        /// keeps only its text; a web link stays.
        #[test]
        fn link_to_footnote_anchor_is_text() {
            let (root, work) = workdir("md-footnote");
            let b = book(
                &root,
                "b.epub",
                "",
                &[
                    ch(
                        "c1.xhtml",
                        "<h1 id=\"top\">Top</h1><p>Text<a id=\"r1\" href=\"#fn1\">1</a>. \
                         <a href=\"#top\">up</a> <a href=\"c2.xhtml\">plain</a></p>\
                         <p><a href=\"https://example.org/x\">web</a> <a href=\"javascript:alert(1)\">js</a></p>\
                         <p id=\"fn1\">Note.</p>",
                    ),
                    ch("c2.xhtml", "<p>No heading.</p>"),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "# Top\n\nText1. [up](#top) plain\n\n[web](https://example.org/x) js\n\nNote.\n\n\
                 No heading.\n"
            );
            assert_eq!(r.warns, [LINKS, STYLES]);
        }

        /// A book named with a space and accents names its images the same
        /// way; the md links to them in `<...>`, and the link read back is
        /// the file's name.
        #[test]
        fn image_name_with_space_and_accent() {
            let (root, work) = workdir("md-name");
            let j = jpeg();
            let b = book_with(
                &root,
                "Árvíztűrő tükör.epub",
                "",
                &[ch("c.xhtml", "<p>Kép: <img src=\"a.jpg\" alt=\"x\"/></p>")],
                &[("OEBPS/a.jpg", &j, Stored)],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.done,
                ["0/Árvíztűrő tükör-0001.jpg", "0/Árvíztűrő tükör.md"]
            );
            assert_eq!(r.text, "Kép: ![x](<Árvíztűrő tükör-0001.jpg>)\n");
            let d = doc::md::read_limited_with(
                r.text.as_bytes(),
                doc::Limits::DEFAULT,
                doc::Dialect::Gfm,
            )
            .unwrap();
            let doc::Block::Paragraph(p) = &d.blocks[0] else {
                panic!()
            };
            let doc::Inline::Image { dest, .. } = &p[1] else {
                panic!("{p:?}")
            };
            assert_eq!(dest, "Árvíztűrő tükör-0001.jpg");
            assert_eq!(fs::read(work.join("0").join(dest)).unwrap(), j);
        }

        /// A stem so long that the md's name fits the file system but an
        /// image's (6 bytes longer) does not: the images keep their alt
        /// text and the book is made.
        #[test]
        fn long_stem_images_keep_alt_text() {
            let (root, work) = workdir("md-long-stem");
            let j = jpeg();
            // 250 + `.epub` is the longest file name (255 bytes); the md's
            // is 253, an image's 259.
            let stem = "s".repeat(250);
            let b = book_with(
                &root,
                &format!("{stem}.epub"),
                "",
                &[ch(
                    "c.xhtml",
                    "<p>t <img src=\"a.jpg\" alt=\"A\"/> <img src=\"b.jpg\" alt=\"B\"/></p>",
                )],
                &[("OEBPS/a.jpg", &j, Stored), ("OEBPS/b.jpg", &j, Stored)],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.done, [format!("0/{stem}.md")]);
            assert_eq!(r.text, "t A B\n");
            assert_eq!(r.warns, ["2 image(s) replaced by alt text", STYLES]);
            assert_eq!(snapshot(&work).len(), 1);
            // A picture book under such a stem fails for its names, not
            // for having no text, and leaves nothing.
            let (root, work) = workdir("md-long-stem-pictures");
            let b = book_with(
                &root,
                &format!("{stem}.epub"),
                "",
                &[ch("c.xhtml", "<p><img src=\"a.jpg\" alt=\"\"/></p>")],
                &[("OEBPS/a.jpg", &j, Stored)],
            );
            fails_alone(
                &run_raw(MD, &work, &[&b]),
                &work,
                "image names too long for this folder",
            );
        }

        /// A control character in the stem (a line feed) is percent-encoded
        /// in the image's link, so the link read back and decoded is the
        /// file's name.
        #[test]
        fn control_character_in_stem_links_the_image() {
            let (root, work) = workdir("md-newline-stem");
            let j = jpeg();
            let b = book_with(
                &root,
                "new\nline.epub",
                "",
                &[ch("c.xhtml", "<p>K <img src=\"a.jpg\" alt=\"x\"/></p>")],
                &[("OEBPS/a.jpg", &j, Stored)],
            );
            // The `done` lines carry the name escaped: read the md itself.
            let o = run_raw(MD, &work, &[&b]);
            assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
            assert_eq!(lines(&stdout(&o), "done").len(), 2, "{}", stdout(&o));
            let text = fs::read_to_string(work.join("0/new\nline.md")).unwrap();
            assert_eq!(text, "K ![x](new%0Aline-0001.jpg)\n");
            let d = doc::md::read_limited_with(
                text.as_bytes(),
                doc::Limits::DEFAULT,
                doc::Dialect::Gfm,
            )
            .unwrap();
            let doc::Block::Paragraph(p) = &d.blocks[0] else {
                panic!()
            };
            let doc::Inline::Image { dest, .. } = &p[1] else {
                panic!("{p:?}")
            };
            let name = doc::xhtml::percent_decode(dest);
            assert_eq!(name, "new\nline-0001.jpg");
            assert_eq!(fs::read(work.join("0").join(&name)).unwrap(), j);
        }

        /// A picture book (each chapter one image, no text) is an md of its
        /// images; with no image written either, there is no book.
        #[test]
        fn picture_book_to_md() {
            let (root, work) = workdir("md-pictures");
            let (j, p) = (jpeg(), png(9));
            let b = book_with(
                &root,
                "comic.epub",
                "",
                &[
                    ch("p1.xhtml", "<div><img src=\"p1.jpg\" alt=\"\"/></div>"),
                    ch("p2.xhtml", "<div><img src=\"p2.png\" alt=\"\"/></div>"),
                ],
                &[("OEBPS/p1.jpg", &j, Stored), ("OEBPS/p2.png", &p, Stored)],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.done,
                ["0/comic-0001.jpg", "0/comic-0002.png", "0/comic.md"]
            );
            assert_eq!(r.text, "![](comic-0001.jpg)\n\n![](comic-0002.png)\n");

            let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
            let b = book_with(
                &root,
                "gifs.epub",
                "<dc:title>Gifs</dc:title>",
                &[ch(
                    "p1.xhtml",
                    "<div><img src=\"p1.gif\" alt=\"Cover\"/></div>",
                )],
                &[("OEBPS/p1.gif", &gif, Stored)],
            );
            let w = work.join("x");
            fs::create_dir_all(&w).unwrap();
            let o = run_raw(MD, &w, &[&b]);
            fails_alone(&o, &w, "book has no text");
        }

        /// A late chapter that cannot be read is an escaped paragraph in its
        /// place (never a link), counted once.
        #[test]
        fn bad_late_chapter_marked_in_md() {
            let (root, work) = workdir("md-bad");
            let b = book(
                &root,
                "b.epub",
                "",
                &[
                    ch("c1.xhtml", "<p>one</p>"),
                    ch("c2.xhtml", "<p>two</p>"),
                    ch("c3.xhtml", "<p>broken</b></p>"),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "one\n\ntwo\n\n\\[Chapter 3 could not be read: not well-formed XML (line 2)\\]\n"
            );
            assert_eq!(r.warns, ["1 chapter(s) could not be read", STYLES]);
        }

        #[test]
        fn drm_book_fails() {
            let (root, work) = workdir("md-drm");
            let p = root.join("drm.epub");
            let enc = encryption(&[(AES, "OEBPS/a.xhtml")]);
            book_at(
                &p,
                "3.0",
                "",
                &[ch("a.xhtml", "<p>x<img src=\"a.jpg\" alt=\"\"/></p>")],
                1,
                &[("META-INF/encryption.xml", &enc), ("OEBPS/a.jpg", &jpeg())],
            );
            let o = run_raw(MD, &work, &[&p]);
            fails_alone(&o, &work, "book is DRM-protected");
        }

        /// The md passing the output ceiling after an image was written
        /// fails the book with no `done` at all, and nothing of it is left:
        /// neither the md nor the image.
        #[test]
        fn failure_leaves_no_images() {
            let (root, work) = workdir("md-fail");
            let j = jpeg();
            let para = format!("<p>{}</p>", "abcdefghi ".repeat(100));
            let b = book_with(
                &root,
                "b.epub",
                "",
                &[
                    ch("c1.xhtml", "<p><img src=\"a.jpg\" alt=\"a\"/></p>"),
                    ch("c2.xhtml", &para.repeat(20)),
                ],
                &[("OEBPS/a.jpg", &j, Stored)],
            );
            let o = run_with_fsize_of(MD, &work, &[&b], 8192);
            fails_alone(&o, &work, "Markdown would be larger than 8 KiB");
        }

        /// An image of exactly the budget (twice the book file) is read and
        /// written; one byte more keeps its alt text.
        #[test]
        fn image_of_exactly_the_budget_is_placed() {
            // `pad` and `fill` (a byte of text) found so that the deflated
            // book is half the image's size to the byte: the image is
            // 3818 B, the book 1909 B (asserted below).
            let build = |name: &str, pad: usize| {
                let (root, work) = workdir(name);
                let j = padded_jpeg(pad);
                let b = book_with(
                    &root,
                    "b.epub",
                    "",
                    &[ch("c.xhtml", "<p>xy <img src=\"1.jpg\" alt=\"one\"/></p>")],
                    &[("OEBPS/1.jpg", &j, Deflated)],
                );
                let size = fs::metadata(&b).unwrap().len();
                (md(&b, &work), j.len() as u64, size, (root, work))
            };
            let (r, image, size, (_root, work)) = build("md-budget-exact", 3033);
            assert_eq!(image, 2 * size, "{image} {size}");
            ok(&r);
            assert_eq!(r.done, ["0/b-0001.jpg", "0/b.md"]);
            assert_eq!(r.text, "xy ![one](b-0001.jpg)\n");
            assert_eq!(
                fs::read(work.join("0/b-0001.jpg")).unwrap().len() as u64,
                image
            );
            // One byte more than the budget.
            let (r, image, size, _) = build("md-budget-over", 3034);
            assert_eq!(image, 2 * size + 1, "{image} {size}");
            ok(&r);
            assert_eq!(r.done, ["0/b.md"]);
            assert_eq!(r.text, "xy one\n");
            assert_eq!(r.warns, ["1 image(s) replaced by alt text", STYLES]);
        }

        /// An image larger than the process's `RLIMIT_FSIZE` keeps its alt
        /// text (a write of it would fail); one of exactly the ceiling is
        /// written.
        #[test]
        fn image_past_the_file_size_ceiling_is_alt_text() {
            const CAP: usize = 4096;
            let (root, work) = workdir("md-image-ceiling");
            // Incompressible filler, so the budget (twice the book file)
            // is far above two images of the ceiling's size.
            let mut x = 1u32;
            let filler: Vec<u8> = (0..20_000)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 24) as u8
                })
                .collect();
            let (big, exact) = (
                padded_jpeg(CAP + 1 - jpeg().len()),
                padded_jpeg(CAP - jpeg().len()),
            );
            assert_eq!((big.len(), exact.len()), (CAP + 1, CAP));
            let b = book_with(
                &root,
                "b.epub",
                "",
                &[ch(
                    "c.xhtml",
                    "<p>x <img src=\"big.jpg\" alt=\"big\"/> <img src=\"exact.jpg\" alt=\"ok\"/></p>",
                )],
                &[
                    ("OEBPS/big.jpg", &big, Deflated),
                    ("OEBPS/exact.jpg", &exact, Deflated),
                    ("OEBPS/filler.bin", &filler, Stored),
                ],
            );
            let o = run_with_fsize_of(MD, &work, &[&b], CAP as u64);
            let out = stdout(&o);
            assert_eq!(o.status.code(), Some(0), "{out}");
            assert!(lines(&out, "fail").is_empty(), "{out}");
            let done: Vec<&str> = lines(&out, "done").iter().map(|d| d[2]).collect();
            assert_eq!(done, ["0/b-0001.jpg", "0/b.md"], "{out}");
            assert_eq!(fs::read(work.join("0/b-0001.jpg")).unwrap(), exact);
            assert_eq!(
                fs::read_to_string(work.join("0/b.md")).unwrap(),
                "x big ![ok](b-0001.jpg)\n"
            );
            let warns: Vec<&str> = lines(&out, "warn").iter().map(|w| w[2]).collect();
            assert_eq!(warns, ["1 image(s) replaced by alt text", STYLES]);
        }

        fn svg_page(data: &[u8]) -> Ch {
            Ch {
                href: "p.svg".into(),
                media: "image/svg+xml",
                data: Some(data.to_vec()),
                spine: true,
                linear: true,
            }
        }

        /// An SVG page is read and left out as an image loss (not parsed as
        /// a chapter: no "could not be read" line, no text of its own); a
        /// book of nothing else has no text, and is not a book with no
        /// chapter read.
        #[test]
        fn svg_page_in_an_md_book() {
            let raw = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
            let (root, work) = workdir("md-svg");
            let b = book_with(
                &root,
                "b.epub",
                "",
                &[svg_page(raw), ch("c.xhtml", "<p>x</p>")],
                &[],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.text, "x\n");
            assert_eq!(r.warns, [IMAGES, STYLES]);
            let (root, work) = workdir("md-svg-only");
            let b = book_with(&root, "pics.epub", "", &[svg_page(raw)], &[]);
            fails_alone(&run_raw(MD, &work, &[&b]), &work, "book has no text");
        }

        /// A spine item missing from the ZIP is a paragraph in its place
        /// and is counted.
        #[test]
        fn missing_chapter_is_marked_and_counted() {
            let (root, work) = workdir("md-missing");
            let mut gone = ch("c2.xhtml", "<p>never</p>");
            gone.data = None;
            let b = book(
                &root,
                "b.epub",
                "",
                &[
                    ch("c1.xhtml", "<p>one</p>"),
                    gone,
                    ch("c3.xhtml", "<p>three</p>"),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(
                r.text,
                "one\n\n\\[Chapter 2 could not be read: missing\\]\n\nthree\n"
            );
            assert_eq!(r.warns, ["1 chapter(s) could not be read", STYLES]);
        }

        /// The heading index charges a chapter's bytes as sums: the path
        /// (300 B), the heading's anchor and its id (30,000 B each) are
        /// about 90 KB, far under the 8 MiB of text it holds. Products of
        /// these would be past it (path times anchor 9 MB, id times
        /// anchor 900 MB) and refuse the book.
        #[test]
        fn long_heading_and_id_are_charged_as_sums() {
            let (root, work) = workdir("md-index-bytes");
            let name = "a".repeat(300);
            let text = "b".repeat(30_000);
            let id = "i".repeat(30_000);
            let b = book(
                &root,
                "b.epub",
                "",
                &[
                    ch(
                        &format!("{name}.xhtml"),
                        &format!("<h1 id=\"{id}\">{text}</h1>"),
                    ),
                    ch(
                        "c.xhtml",
                        &format!("<p><a href=\"{name}.xhtml#{id}\">go</a></p>"),
                    ),
                ],
            );
            let r = md(&b, &work);
            ok(&r);
            assert_eq!(r.text, format!("# {text}\n\n[go](#{text})\n"));
        }

        /// The heading index holds at most 2^19 entries, and each chapter
        /// with headings counts itself too: 1024 chapters of 256 headings,
        /// each with an id, are 513 entries a chapter, 525,312 in all,
        /// 1,024 past the limit. (Counting a chapter's `id`s alone,
        /// without the chapter, would give exactly the limit.) Nothing is
        /// left behind.
        #[test]
        fn index_entries_count_each_chapter() {
            let (root, work) = workdir("md-index-entries");
            let chs: Vec<Ch> = (0..1024)
                .map(|c| {
                    let body: String = (0..256)
                        .map(|k| format!("<h2 id=\"i{c}-{k}\">h{c}-{k}</h2>"))
                        .collect();
                    ch(&format!("c{c}.xhtml"), &body)
                })
                .collect();
            let b = book(&root, "b.epub", "", &chs);
            let o = run_raw(MD, &work, &[&b]);
            fails_alone(&o, &work, "book is too large");
        }

        /// The same book gives the same md and images, byte for byte.
        #[test]
        fn deterministic_md_and_images() {
            let (root, work) = workdir("md-determ");
            let (j, p) = (jpeg(), png(5));
            let b = book_with(
                &root,
                "b.epub",
                "<dc:title>T</dc:title>",
                &[
                    ch(
                        "c1.xhtml",
                        "<h1 id=\"a\">A</h1><p><img src=\"a.jpg\" alt=\"a\"/></p>",
                    ),
                    ch(
                        "c2.xhtml",
                        "<h1>A</h1><p><a href=\"c1.xhtml#a\">back</a><img src=\"b.png\" alt=\"b\"/></p>",
                    ),
                ],
                &[("OEBPS/a.jpg", &j, Stored), ("OEBPS/b.png", &p, Stored)],
            );
            let runs: Vec<Vec<(PathBuf, Vec<u8>)>> = (0..2)
                .map(|k| {
                    let w = work.join(k.to_string());
                    fs::create_dir_all(&w).unwrap();
                    ok(&md(&b, &w));
                    snapshot(&w)
                        .into_iter()
                        .map(|(p, c)| (p.strip_prefix(&w).unwrap().to_path_buf(), c))
                        .collect()
                })
                .collect();
            assert_eq!(runs[0].len(), 3);
            assert_eq!(runs[0], runs[1]);
        }

        /// The words of a book, lower-cased letters and digits, leaving out
        /// what the plain-text writer adds for an in-book link of the md
        /// (` (#id)`): the md read back as GFM and written as plain text
        /// has the same word sequence as the book's own plain text, so the
        /// two readings of a book agree.
        fn words(text: &str) -> Vec<String> {
            text.split_whitespace()
                .filter(|w| !w.starts_with("(#"))
                .map(|w| {
                    w.chars()
                        .filter(|c| c.is_alphanumeric())
                        .flat_map(char::to_lowercase)
                        .collect::<String>()
                })
                .filter(|w| !w.is_empty())
                .collect()
        }

        #[test]
        fn md_reads_back_to_the_txt_words() {
            let (root, work) = workdir("md-words");
            let j = jpeg();
            let b = book_with(
                &root,
                "b.epub",
                "<dc:title>The Title</dc:title><dc:creator>Ann</dc:creator>",
                &[
                    ch(
                        "c1.xhtml",
                        "<h1 id=\"one\">Chapter <em>one</em></h1><p>Some <strong>bold</strong> and \
                         <code>code</code> text<a href=\"#fn\">1</a>, a <a href=\"https://example.org\">site</a> \
                         and <a href=\"c2.xhtml\">the next</a>.</p><ul><li>first</li><li>second <del>gone</del></li></ul>\
                         <blockquote><p>quoted * star _ under</p></blockquote><pre>pre  text</pre>\
                         <table><tr><th>h1</th><th>h2</th></tr><tr><td>c1</td><td>c2<br/>more</td></tr></table>\
                         <p><img src=\"a.jpg\" alt=\"picture\"/> <img src=\"none.png\" alt=\"lost\"/></p>\
                         <p id=\"fn\">1. The note.</p>",
                    ),
                    ch(
                        "c2.xhtml",
                        "<h2>Two</h2><ol start=\"3\"><li>three</li></ol><p>[brackets] # hash</p>",
                    ),
                    ch("c3.xhtml", "<p>broken</b></p>"),
                ],
                &[("OEBPS/a.jpg", &j, Stored)],
            );
            let txt_work = work.join("txt");
            fs::create_dir_all(&txt_work).unwrap();
            let (plain, _, o) = txt(&b, &txt_work);
            assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
            let r = md(&b, &work);
            ok(&r);
            let d = doc::md::read_limited_with(
                r.text.as_bytes(),
                doc::Limits::DEFAULT,
                doc::Dialect::Gfm,
            )
            .unwrap();
            let (back, _) = doc::txt::write(&d);
            assert_eq!(
                words(&back),
                words(&plain),
                "md:\n{}\ntxt:\n{plain}",
                r.text
            );
            assert!(words(&plain).len() > 30);
        }
    }
}
