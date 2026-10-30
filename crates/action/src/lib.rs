//! Shared side of the action invocation contract (v0).
//!
//! Non-UTF-8 paths are out of scope for v0 (the manifest line format is
//! `String`-based). They do not panic: `parse_args` keeps such an input as a
//! `PathBuf`, and `Events` writes paths with `to_string_lossy()`, so an action
//! can report it, e.g. `skip <lossy path> non-UTF-8 path`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use manifest::Line;

pub mod order;
pub use order::natural_cmp;

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
            if work.is_some() {
                return Err("duplicate --work".to_owned());
            }
            work = Some(PathBuf::from(args.next().ok_or("--work needs a value")?));
        } else if a == "--opt" {
            let kv = utf8(args.next().ok_or("--opt needs a value")?, "--opt")?;
            let (k, v) = kv.split_once('=').ok_or("--opt needs key=value")?;
            opts.push((k.to_owned(), v.to_owned()));
        } else if a == "--inputs-from" {
            if from_file.is_some() {
                return Err("duplicate --inputs-from".to_owned());
            }
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
    if !work.is_absolute() {
        return Err(format!(
            "--work is not absolute: {}",
            work.to_string_lossy()
        ));
    }
    let mut inputs = Vec::new();
    let mut budget = InputBudget::default();
    if let Some(f) = from_file {
        let bytes = read_file_bounded(&f, INPUT_LIST_BYTES).map_err(|e| {
            if e.kind() == io::ErrorKind::InvalidData {
                INPUT_REFUSAL.to_owned()
            } else {
                format!("cannot read {}: {e}", f.to_string_lossy())
            }
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8", f.to_string_lossy()))?;
        for line in text.lines().filter(|l| !l.is_empty()) {
            if line.len() > INPUT_ENCODED_LINE_BYTES {
                return Err(INPUT_REFUSAL.to_owned());
            }
            let path = manifest::unescape(line)?;
            budget.add(path.as_bytes())?;
            inputs.push(PathBuf::from(path));
        }
    }
    for a in args {
        budget.add(a.as_encoded_bytes())?;
        inputs.push(PathBuf::from(a));
    }
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

/// Makes room for `extra` more bytes; a failed reservation is
/// `OutOfMemory`, not an abort.
fn grow(v: &mut Vec<u8>, extra: usize) -> io::Result<()> {
    v.try_reserve(extra)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
}

/// Reads `reader` to its end, but at most `max_bytes` bytes: the real size
/// is enforced while reading (one guard byte past the limit is the most
/// ever read), so a file that is small when stat'ed and grows afterwards,
/// a pipe or an endless source costs at most `max_bytes + 1` bytes.
/// `InvalidData` past the limit, `OutOfMemory` when the buffer cannot be
/// reserved, `InvalidInput` for `usize::MAX` (no room for the guard byte).
/// Interrupted reads are retried; short reads are fine.
pub fn read_bounded(reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    read_bounded_hint(reader, max_bytes, 0)
}

fn read_bounded_hint(mut reader: impl Read, max_bytes: usize, hint: usize) -> io::Result<Vec<u8>> {
    let limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut out = Vec::new();
    // A size hint from fstat only pre-sizes; it never raises the limit.
    if hint <= max_bytes {
        grow(&mut out, hint)?;
    }
    // Stack chunk: the buffer grows with the data actually read.
    let mut chunk = [0u8; 8192];
    loop {
        // out.len() <= max_bytes here, so room >= 1.
        let room = limit - out.len();
        let want = room.min(chunk.len());
        match reader.read(&mut chunk[..want]) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                grow(&mut out, n)?;
                out.extend_from_slice(&chunk[..n]);
                if out.len() > max_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("more than {max_bytes} bytes"),
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// A regular file read at most `max_bytes` (see [`read_bounded`]). Opened
/// with `O_NONBLOCK` (a FIFO never waits for a writer) and followed through a symlink (an input may be one);
/// a directory, FIFO or device is `InvalidInput`. The size `fstat` shows is
/// a pre-filter and a buffer hint only.
pub fn read_file_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let len = usize::try_from(m.len()).unwrap_or(usize::MAX);
    if len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("more than {max_bytes} bytes"),
        ));
    }
    read_bounded_hint(f, max_bytes, len)
}

/// The `--inputs-from` limits (see `docs/resource-limits.md`).
/// `input_list_bytes`: 1024 paths at ~1 KiB; past it the list is refused.
pub const INPUT_LIST_BYTES: usize = 1_048_576;
/// `input_count`: one output per input at least and an action takes at
/// most 1024 outputs (engine `MAX_OUTPUTS`); argv and list share it.
pub const INPUT_COUNT: usize = 1024;
/// `input_encoded_line_bytes`: 2 x 4096, the worst case of the manifest
/// escape for a 4096 B path.
pub const INPUT_ENCODED_LINE_BYTES: usize = 8192;
/// `input_path_bytes`: Linux PATH_MAX.
pub const INPUT_PATH_BYTES: usize = 4096;
/// `input_total_path_bytes`: as the list size (escapes only shrink it);
/// argv paths count too.
pub const INPUT_TOTAL_PATH_BYTES: usize = 1_048_576;
/// Shown for every input-list refusal (rejection copy of the rows).
const INPUT_REFUSAL: &str = "Too many input paths or input path data exceeds the safety limit.";

/// Counts inputs against the shared limits.
#[derive(Default)]
struct InputBudget {
    count: usize,
    bytes: usize,
}

impl InputBudget {
    fn add(&mut self, path: &[u8]) -> Result<(), String> {
        let refuse = || INPUT_REFUSAL.to_owned();
        if path.len() > INPUT_PATH_BYTES || self.count >= INPUT_COUNT {
            return Err(refuse());
        }
        let bytes = self.bytes.checked_add(path.len()).ok_or_else(refuse)?;
        if bytes > INPUT_TOTAL_PATH_BYTES {
            return Err(refuse());
        }
        self.count += 1;
        self.bytes = bytes;
        Ok(())
    }
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

/// `<work>/<index>/<stem>.<ext>`; creates the directory. A path without a
/// stem uses `output`.
pub fn output_path(work: &Path, index: usize, input: &Path, ext: &str) -> io::Result<PathBuf> {
    let dir = work.join(index.to_string());
    fs::create_dir_all(&dir)?;
    let stem = input
        .file_stem()
        .map_or_else(|| "output".into(), |s| s.to_os_string());
    let mut name = stem;
    name.push(".");
    name.push(ext);
    Ok(dir.join(name))
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
        let a = output_path(&w, 0, Path::new("/x/notes.md"), "html").unwrap();
        let b = output_path(&w, 1, Path::new("/y/notes.md"), "html").unwrap();
        assert_eq!(a, w.join("0/notes.html"));
        assert_eq!(b, w.join("1/notes.html"));
        assert!(w.join("0").is_dir() && w.join("1").is_dir());
        assert_eq!(
            output_path(&w, 2, Path::new("/"), "txt").unwrap(),
            w.join("2/output.txt")
        );
        assert_eq!(
            output_path(&w, 3, Path::new("/a/b.tar.gz"), "md").unwrap(),
            w.join("3/b.tar.md")
        );
    }

    #[test]
    fn output_path_reports_create_error() {
        let w = tmp("blocked");
        fs::write(w.join("0"), "a file where the dir should be").unwrap();
        assert!(output_path(&w, 0, Path::new("/x/a.md"), "txt").is_err());
    }

    #[test]
    fn parse_rejects_duplicates_and_relative_work() {
        assert!(
            parse_args(os(&[
                "run", "a", "--work", "/w", "--work", "/v", "--", "/x"
            ]))
            .is_err()
        );
        assert!(
            parse_args(os(&[
                "run",
                "a",
                "--work",
                "/w",
                "--inputs-from",
                "/f",
                "--inputs-from",
                "/g",
                "--",
                "/x"
            ]))
            .is_err()
        );
        assert!(parse_args(os(&["run", "a", "--work", "w", "--", "/x"])).is_err());
    }

    // ---- read_bounded ----

    /// Hands out at most `step` bytes per read, and fails every other call
    /// with `Interrupted`: a short, interrupted reader.
    struct Choppy<'a> {
        data: &'a [u8],
        step: usize,
        flip: bool,
    }
    impl Read for Choppy<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.flip = !self.flip;
            if self.flip {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = self.step.min(self.data.len()).min(buf.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// An endless reader that counts what it handed out.
    struct Endless(usize);
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Test guard: a broken bound must fail here, not eat the machine.
            assert!(self.0 < 4 << 20, "read far past the bound");
            buf.fill(b'x');
            self.0 += buf.len();
            Ok(buf.len())
        }
    }

    #[test]
    fn bounded_read_accepts_max_and_refuses_max_plus_one() {
        for max in [0usize, 1, 7, 8191, 8192, 8193, 20_000] {
            let at = vec![b'a'; max];
            assert_eq!(read_bounded(&at[..], max).unwrap(), at, "max {max} exact");
            if max > 0 {
                let below = &at[..max - 1];
                assert_eq!(read_bounded(below, max).unwrap(), below, "max {max} - 1");
            }
            let over = vec![b'a'; max + 1];
            let e = read_bounded(&over[..], max).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "max {max} + 1");
        }
    }

    #[test]
    fn bounded_read_survives_short_and_interrupted_reads() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        for step in [1, 3, 4096, 9000] {
            let r = Choppy {
                data: &data,
                step,
                flip: false,
            };
            assert_eq!(read_bounded(r, 5000).unwrap(), data, "step {step}");
            let r = Choppy {
                data: &data,
                step,
                flip: false,
            };
            let e = read_bounded(r, 4999).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "step {step}");
        }
    }

    /// A source that never ends (a file that grows while it is read) costs
    /// the guard byte at most: the read stops at max + 1.
    #[test]
    fn bounded_read_stops_a_growing_source_at_max_plus_one() {
        let mut e = Endless(0);
        let err = read_bounded(&mut e, 100_000).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(e.0, 100_001, "never reads past max + 1");
    }

    #[test]
    fn bounded_read_refuses_a_max_that_cannot_hold_the_guard_byte() {
        let e = read_bounded(&b"x"[..], usize::MAX).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        // One below is a limit like any other (the source is short).
        assert_eq!(read_bounded(&b"x"[..], usize::MAX - 1).unwrap(), b"x");
    }

    #[test]
    fn bounded_read_reports_a_failed_reservation_as_out_of_memory() {
        let mut v: Vec<u8> = Vec::new();
        let e = grow(&mut v, usize::MAX / 2).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        assert!(v.is_empty());
    }

    #[test]
    fn bounded_read_passes_a_reader_error_through() {
        struct Bad;
        impl Read for Bad {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
        let e = read_bounded(Bad, 10).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn bounded_file_read_checks_type_size_and_follows_a_link() {
        let d = tmp("bfile");
        let f = d.join("f");
        fs::write(&f, [b'a'; 10]).unwrap();
        assert_eq!(read_file_bounded(&f, 10).unwrap().len(), 10);
        let e = read_file_bounded(&f, 9).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        // Positive control: an input that is a symlink still reads.
        let l = d.join("l");
        std::os::unix::fs::symlink(&f, &l).unwrap();
        assert_eq!(read_file_bounded(&l, 10).unwrap().len(), 10);
        // A directory is not a regular file.
        let e = read_file_bounded(&d, 10).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        // A FIFO neither, and opening it does not block.
        let q = d.join("q");
        let c = std::ffi::CString::new(q.to_str().unwrap()).unwrap();
        // SAFETY: c is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_file_bounded(&q, 10));
        });
        let e = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("blocked on a FIFO")
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    /// A file whose `stat` size is smaller than what a read returns (a
    /// file that grows while read; `/proc` files show 0): the read itself
    /// is cut at max + 1, the size is only a pre-filter.
    #[test]
    fn bounded_file_read_cuts_a_file_longer_than_its_stat() {
        let p = Path::new("/proc/self/maps");
        assert_eq!(fs::metadata(p).unwrap().len(), 0);
        // Positive control: there is more than 64 B to read.
        assert!(read_file_bounded(p, 1 << 20).unwrap().len() > 64);
        let e = read_file_bounded(p, 64).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    // ---- --inputs-from limits ----

    fn from_file(d: &Path, body: &[u8], argv: &[&str]) -> Result<Invocation, String> {
        let f = d.join("in.txt");
        fs::write(&f, body).unwrap();
        let mut a = vec![
            "run",
            "a",
            "--work",
            "/w",
            "--inputs-from",
            f.to_str().unwrap(),
            "--",
        ];
        a.extend_from_slice(argv);
        parse_args(os(&a)).map(run)
    }

    fn lines_of(n: usize, len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            let mut p = format!("/{i:06}").into_bytes();
            p.resize(len, b'p');
            v.extend_from_slice(&p);
            v.push(b'\n');
        }
        v
    }

    #[test]
    fn inputs_from_is_bounded_count() {
        let d = tmp("incount");
        assert_eq!(
            from_file(&d, &lines_of(1024, 8), &[]).unwrap().inputs.len(),
            1024
        );
        let e = from_file(&d, &lines_of(1025, 8), &[]).unwrap_err();
        assert!(e.contains("safety limit"), "{e}");
        // One counter for the list and argv together.
        assert!(from_file(&d, &lines_of(1023, 8), &["/a"]).is_ok());
        assert!(from_file(&d, &lines_of(1023, 8), &["/a", "/b"]).is_err());
        let argv: Vec<String> = (0..1025).map(|i| format!("/a{i}")).collect();
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        assert!(from_file(&d, b"/x\n", &refs[..1023]).is_ok());
        assert!(from_file(&d, b"/x\n", &refs[..1024]).is_err());
    }

    #[test]
    fn inputs_from_is_bounded_bytes() {
        let d = tmp("inbytes");
        // 256 paths of 4095 B + newline = 1 MiB exactly.
        let at = lines_of(256, 4095);
        assert_eq!(at.len(), INPUT_LIST_BYTES);
        assert_eq!(from_file(&d, &at, &[]).unwrap().inputs.len(), 256);
        let mut over = at.clone();
        over.push(b'\n');
        let e = from_file(&d, &over, &[]).unwrap_err();
        assert!(e.contains("safety limit"), "{e}");
        // A list that grows past the limit while it is read: see
        // `bounded_file_read_cuts_a_file_longer_than_its_stat`.
    }

    #[test]
    fn inputs_from_is_bounded_line_and_path() {
        let d = tmp("inline");
        // A decoded path of 4096 B is the largest; 4097 is refused.
        let p4096 = lines_of(1, 4096);
        assert!(from_file(&d, &p4096, &[]).is_ok());
        let p4097 = lines_of(1, 4097);
        let e = from_file(&d, &p4097, &[]).unwrap_err();
        assert!(e.contains("safety limit"), "{e}");
        // The same limit holds for a path given in argv.
        let long = format!("/{}", "a".repeat(4095));
        assert!(from_file(&d, b"/x\n", &[&long]).is_ok());
        let long = format!("/{}", "a".repeat(4096));
        assert!(from_file(&d, b"/x\n", &[&long]).is_err());
        // The longest valid encoded line is 8191 B ("/" and 4095 escaped
        // tabs: 4096 decoded; 8192 would need a decoded path over 4096).
        let mut enc = String::from("/");
        enc.push_str(&"\\t".repeat(4095));
        assert_eq!(enc.len(), 8191);
        let mut body = enc.into_bytes();
        body.push(b'\n');
        assert_eq!(
            from_file(&d, &body, &[]).unwrap().inputs[0]
                .as_os_str()
                .len(),
            4096
        );
        // A line past INPUT_ENCODED_LINE_BYTES is refused before it is
        // decoded. Seen through a line that does not decode (a bad escape
        // at its end): at 8192 B the decoder's error comes back, at 8193 B
        // the limit's, so the decoder never ran on it.
        let bad = |n: usize| {
            let mut body = vec![b'/'];
            body.resize(n - 2, b'p');
            body.extend_from_slice(b"\\q\n");
            assert_eq!(body.len(), n + 1);
            from_file(&d, &body, &[]).unwrap_err()
        };
        let e = bad(INPUT_ENCODED_LINE_BYTES);
        assert!(e.contains("unknown escape"), "{e}");
        let e = bad(INPUT_ENCODED_LINE_BYTES + 1);
        assert_eq!(e, INPUT_REFUSAL);
    }

    #[test]
    fn inputs_from_is_bounded_total_path_bytes() {
        let d = tmp("intotal");
        // 255 paths of 4096 B in the list; the sum may reach 1 MiB exactly
        // (one more 4096 B path in argv) and not one byte more, while no
        // single path, line or count limit is hit.
        let mut body = Vec::new();
        for i in 0..255 {
            let mut p = format!("/{i:04}").into_bytes();
            p.resize(4096, b'p');
            body.extend_from_slice(&p);
            body.push(b'\n');
        }
        let full = format!("/{}", "a".repeat(4095));
        assert_eq!(255 * 4096 + full.len(), INPUT_TOTAL_PATH_BYTES);
        assert!(from_file(&d, &body, &[&full]).is_ok());
        let e = from_file(&d, &body, &[&full, "/c"]).unwrap_err();
        assert!(e.contains("safety limit"), "{e}");
    }

    #[test]
    fn inputs_from_non_utf8_list_is_refused_not_lossy() {
        let d = tmp("innonutf");
        assert!(from_file(&d, b"/ok\n/\xff\n", &[]).is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            [EXIT_OK, EXIT_FAILED, EXIT_USAGE, EXIT_UNSUPPORTED],
            [0, 1, 2, 3]
        );
    }
}
