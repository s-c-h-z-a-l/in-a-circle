use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: &str = "inacircle-manifest 0\n\
action\ttext.txt-to-md\tfamily=text\tin=text/plain\tout=text/markdown\text=md\tlabel=Markdown\tgroup=document\tcost=10\tlossy=no\tbatch=yes\n\
action\ttext.md-to-txt\tfamily=text\tin=text/markdown\tout=text/plain\text=txt\tlabel=Plain text\tgroup=document\tcost=10\tlossy=yes\tbatch=yes\n";

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
fn manifest_exact() {
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
    assert_eq!(lines(&out, "progress").len(), 1);
    assert_eq!(lines(&out, "progress")[0][1], "1");
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
        ["repeated blank lines merged", "whitespace normalized"]
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
    assert_eq!(lines(&out, "progress").len(), 3);
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
    assert!(!fails[0][2].is_empty());
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
