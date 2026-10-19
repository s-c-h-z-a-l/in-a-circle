//! Shared side of the action invocation contract (v0).
//!
//! Non-UTF-8 paths are out of scope for v0 (the manifest line format is
//! `String`-based). They do not panic: `parse_args` keeps such an input as a
//! `PathBuf`, and `Events` writes paths with `to_string_lossy()`, so an action
//! can report it, e.g. `skip <lossy path> non-UTF-8 path`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use manifest::Line;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
/// Contract or invocation error (ours, not the file's).
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNSUPPORTED: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub action: String,
    pub work: PathBuf,
    pub opts: Vec<(String, String)>,
    pub inputs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Manifest,
    Run(Invocation),
}

/// Parses the arguments after `argv[0]`:
/// `--manifest` | `run <id> --work <dir> [--opt k=v]... [--inputs-from <file>] -- <paths>...`
///
/// Inputs must be absolute. `--inputs-from` paths (one per line, manifest
/// escapes) come before the paths after `--`. At least one input is required.
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args;
    let first = args.next().ok_or("missing command")?;
    if first == "--manifest" {
        return match args.next() {
            None => Ok(Command::Manifest),
            Some(_) => Err("--manifest takes no arguments".to_owned()),
        };
    }
    if first != "run" {
        return Err(format!("unknown command {}", first.to_string_lossy()));
    }
    let action = utf8(args.next().ok_or("missing action id")?, "action id")?;
    let mut work = None;
    let mut opts = Vec::new();
    let mut from_file: Option<PathBuf> = None;
    let mut separated = false;
    while let Some(a) = args.next() {
        if a == "--" {
            separated = true;
            break;
        }
        if a == "--work" {
            work = Some(PathBuf::from(args.next().ok_or("--work needs a value")?));
        } else if a == "--opt" {
            let kv = utf8(args.next().ok_or("--opt needs a value")?, "--opt")?;
            let (k, v) = kv.split_once('=').ok_or("--opt needs key=value")?;
            opts.push((k.to_owned(), v.to_owned()));
        } else if a == "--inputs-from" {
            from_file = Some(PathBuf::from(
                args.next().ok_or("--inputs-from needs a value")?,
            ));
        } else {
            return Err(format!("unknown argument {}", a.to_string_lossy()));
        }
    }
    if !separated {
        return Err("missing `--` before input paths".to_owned());
    }
    let work = work.ok_or("missing --work")?;
    let mut inputs = Vec::new();
    if let Some(f) = from_file {
        let text = fs::read_to_string(&f)
            .map_err(|e| format!("cannot read {}: {e}", f.to_string_lossy()))?;
        for line in text.lines().filter(|l| !l.is_empty()) {
            inputs.push(PathBuf::from(manifest::unescape(line)?));
        }
    }
    inputs.extend(args.map(PathBuf::from));
    if inputs.is_empty() {
        return Err("no input paths".to_owned());
    }
    if let Some(p) = inputs.iter().find(|p| !p.is_absolute()) {
        return Err(format!(
            "input path is not absolute: {}",
            p.to_string_lossy()
        ));
    }
    Ok(Command::Run(Invocation {
        action,
        work,
        opts,
        inputs,
    }))
}

fn utf8(s: OsString, what: &str) -> Result<String, String> {
    s.into_string()
        .map_err(|_| format!("{what} is not valid UTF-8"))
}

/// Writes event lines, flushing after each. Write errors (e.g. a closed
/// stdout) are ignored: a broken pipe is the engine's problem.
pub struct Events<W: Write> {
    out: W,
}

impl<W: Write> Events<W> {
    pub fn new(out: W) -> Events<W> {
        Events { out }
    }

    fn emit(&mut self, kind: &str, fields: &[&str]) {
        let line = fields.iter().fold(Line::new(kind), |l, f| l.bare(f));
        let _ = self.out.write_all(manifest::write_line(&line).as_bytes());
        let _ = self.out.flush();
    }

    pub fn progress(&mut self, fraction: f32) {
        self.emit("progress", &[&fraction.to_string()]);
    }

    pub fn done(&mut self, input: &Path, output_rel: &Path) {
        self.emit(
            "done",
            &[&input.to_string_lossy(), &output_rel.to_string_lossy()],
        );
    }

    pub fn skip(&mut self, input: &Path, reason: &str) {
        self.emit("skip", &[&input.to_string_lossy(), reason]);
    }

    pub fn fail(&mut self, input: &Path, reason: &str) {
        self.emit("fail", &[&input.to_string_lossy(), reason]);
    }

    pub fn warn(&mut self, input: &Path, message: &str) {
        self.emit("warn", &[&input.to_string_lossy(), message]);
    }
}

/// `<work>/<index>/<stem>.<ext>`; creates the directory. A creation error is
/// ignored here: the action's own write to the returned path will fail and be
/// reported. A path without a stem uses `output`.
pub fn output_path(work: &Path, index: usize, input: &Path, ext: &str) -> PathBuf {
    let dir = work.join(index.to_string());
    let _ = fs::create_dir_all(&dir);
    let stem = input
        .file_stem()
        .map_or_else(|| "output".into(), |s| s.to_os_string());
    let mut name = stem;
    name.push(".");
    name.push(ext);
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::Item;
    use std::os::unix::ffi::OsStringExt;

    fn os(v: &[&str]) -> impl Iterator<Item = OsString> {
        v.iter().map(OsString::from).collect::<Vec<_>>().into_iter()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("action-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(c: Command) -> Invocation {
        match c {
            Command::Run(i) => i,
            Command::Manifest => panic!("expected run"),
        }
    }

    #[test]
    fn parse_manifest_flag() {
        assert_eq!(parse_args(os(&["--manifest"])), Ok(Command::Manifest));
        assert!(parse_args(os(&["--manifest", "x"])).is_err());
    }

    #[test]
    fn parse_errors() {
        assert!(parse_args(os(&[])).is_err());
        assert!(parse_args(os(&["bogus"])).is_err());
        assert!(parse_args(os(&["run"])).is_err());
        assert!(parse_args(os(&["run", "a", "--", "/x"])).is_err()); // no --work
        assert!(parse_args(os(&["run", "a", "--work", "/w", "--",])).is_err()); // no inputs
        assert!(parse_args(os(&["run", "a", "--work"])).is_err());
        assert!(
            parse_args(os(&[
                "run", "a", "--work", "/w", "--opt", "novalue", "--", "/x"
            ]))
            .is_err()
        );
        assert!(parse_args(os(&["run", "a", "--work", "/w", "--zzz", "--", "/x"])).is_err());
        assert!(
            parse_args(os(&[
                "run",
                "a",
                "--work",
                "/w",
                "--inputs-from",
                "/nonexistent/f",
                "--",
            ]))
            .is_err()
        );
    }

    #[test]
    fn parse_run_full() {
        let d = tmp("full");
        let f = d.join("in.txt");
        fs::write(&f, "/from/file\n").unwrap();
        let args = [
            "run",
            "md-to-html",
            "--work",
            "/tmp/w",
            "--opt",
            "a=b",
            "--opt",
            "k=v=w",
            "--inputs-from",
            f.to_str().unwrap(),
            "--",
            "/a/b.md",
            "/c.md",
        ];
        let i = run(parse_args(os(&args)).unwrap());
        assert_eq!(i.action, "md-to-html");
        assert_eq!(i.work, PathBuf::from("/tmp/w"));
        assert_eq!(
            i.opts,
            vec![("a".into(), "b".into()), ("k".into(), "v=w".into())]
        );
        assert_eq!(
            i.inputs,
            vec![
                PathBuf::from("/from/file"),
                PathBuf::from("/a/b.md"),
                PathBuf::from("/c.md")
            ]
        );
    }

    #[test]
    fn relative_input_rejected() {
        let e = parse_args(os(&["run", "a", "--work", "/w", "--", "/ok", "rel/x.md"])).unwrap_err();
        assert!(e.contains("rel/x.md"), "{e}");
    }

    #[test]
    fn missing_separator_rejected() {
        assert!(parse_args(os(&["run", "a", "--work", "/w"])).is_err());
    }

    #[test]
    fn inputs_from_file() {
        let d = tmp("from");
        let f = d.join("in.txt");
        fs::write(
            &f,
            "/with space/a b.md\n/tab\\tname/x\\ny.md\n\n/#hash=eq\n",
        )
        .unwrap();
        let i = run(parse_args(os(&[
            "run",
            "a",
            "--work",
            "/w",
            "--inputs-from",
            f.to_str().unwrap(),
            "--",
        ]))
        .unwrap());
        assert_eq!(
            i.inputs,
            vec![
                PathBuf::from("/with space/a b.md"),
                PathBuf::from("/tab\tname/x\ny.md"),
                PathBuf::from("/#hash=eq"),
            ]
        );
    }

    #[test]
    fn inputs_from_bad_escape_rejected() {
        let d = tmp("badesc");
        let f = d.join("in.txt");
        fs::write(&f, "/a\\q\n").unwrap();
        assert!(
            parse_args(os(&[
                "run",
                "a",
                "--work",
                "/w",
                "--inputs-from",
                f.to_str().unwrap(),
                "--"
            ]))
            .is_err()
        );
    }

    #[test]
    fn non_utf8_input_kept_not_panicking() {
        let mut args: Vec<OsString> = os(&["run", "a", "--work", "/w", "--"]).collect();
        args.push(OsString::from_vec(b"/bad/\xff.md".to_vec()));
        let i = run(parse_args(args.into_iter()).unwrap());
        assert_eq!(i.inputs[0].as_os_str().as_encoded_bytes(), b"/bad/\xff.md");
        let mut buf = Vec::new();
        Events::new(&mut buf).skip(&i.inputs[0], "non-UTF-8 path");
        assert!(
            String::from_utf8(buf)
                .unwrap()
                .starts_with("skip\t/bad/\u{fffd}.md\t")
        );
        // non-UTF-8 action id is an error, not a panic
        let bad = vec![OsString::from("run"), OsString::from_vec(vec![0xff])];
        assert!(parse_args(bad.into_iter()).is_err());
    }

    fn bares(l: &Line) -> Vec<&str> {
        l.items
            .iter()
            .map(|i| match i {
                Item::Bare(s) => s.as_str(),
                Item::Pair(..) => panic!("pair"),
            })
            .collect()
    }

    #[test]
    fn events_are_escaped_lines() {
        let weird = Path::new("/d/a -> b\tc\\d.md");
        let mut buf = Vec::new();
        {
            let mut ev = Events::new(&mut buf);
            ev.progress(0.42);
            ev.done(weird, Path::new("0/a -> b.html"));
            ev.skip(weird, "unsupported");
            ev.fail(weird, "read error\nat line 12");
            ev.warn(weird, "x=y");
        }
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<_> = text.split_inclusive('\n').collect();
        assert_eq!(lines.len(), 5);
        let p: Vec<Line> = lines
            .iter()
            .map(|l| manifest::parse_line(l).unwrap())
            .collect();
        assert_eq!(p[0].kind, "progress");
        assert_eq!(bares(&p[0]), ["0.42"]);
        assert_eq!(p[1].kind, "done");
        assert_eq!(bares(&p[1]), [weird.to_str().unwrap(), "0/a -> b.html"]);
        assert_eq!(p[2].kind, "skip");
        assert_eq!(bares(&p[2]), [weird.to_str().unwrap(), "unsupported"]);
        assert_eq!(p[3].kind, "fail");
        assert_eq!(
            bares(&p[3]),
            [weird.to_str().unwrap(), "read error\nat line 12"]
        );
        assert_eq!(p[4].kind, "warn");
        assert_eq!(bares(&p[4]), [weird.to_str().unwrap(), "x=y"]);
    }

    struct Counting {
        flushes: usize,
        buf: Vec<u8>,
    }
    impl Write for Counting {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn events_flush_every_line() {
        let mut w = Counting {
            flushes: 0,
            buf: Vec::new(),
        };
        {
            let mut ev = Events::new(&mut w);
            ev.progress(0.0);
            ev.progress(1.0);
        }
        assert_eq!(w.flushes, 2);
    }

    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn events_survive_closed_stdout() {
        let mut ev = Events::new(Broken);
        ev.progress(0.5);
        ev.done(Path::new("/a"), Path::new("b"));
    }

    #[test]
    fn output_path_per_index() {
        let w = tmp("out");
        let a = output_path(&w, 0, Path::new("/x/notes.md"), "html");
        let b = output_path(&w, 1, Path::new("/y/notes.md"), "html");
        assert_eq!(a, w.join("0/notes.html"));
        assert_eq!(b, w.join("1/notes.html"));
        assert!(w.join("0").is_dir() && w.join("1").is_dir());
        assert_eq!(
            output_path(&w, 2, Path::new("/"), "txt"),
            w.join("2/output.txt")
        );
        assert_eq!(
            output_path(&w, 3, Path::new("/a/b.tar.gz"), "md"),
            w.join("3/b.tar.md")
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            [EXIT_OK, EXIT_FAILED, EXIT_USAGE, EXIT_UNSUPPORTED],
            [0, 1, 2, 3]
        );
    }
}
