//! `inacircle-testaction`: a synthetic action that can produce every element
//! of contract v0 on demand. Test-only, never packaged; the engine's
//! integration tests drive it to exercise event handling, exit codes,
//! cancellation, output size limits and the sandbox.
//!
//! Behavior is selected with `--opt key=value`, executed in command-line
//! order (all options are validated first, so a usage error has no side
//! effects). Events are always reported for the first input.
//!
//! A step that fails (a `fail` event from an I/O error, not `events=fail`)
//! ends the run at once with exit 1, regardless of a later `exit=`.
//!
//! - `events=<list>`: comma-separated `progress:<f>`, `done`, `skip`,
//!   `fail`, `warn`. `done` writes the output via `output_path` first.
//! - `outputs=<n>`: `n` outputs from the first input, `<work>/0/out-<k>.bin`.
//! - `sleep_ms=<n>`: sleep.
//! - `exit=<code>`: the exit code at the end of the run (default 0).
//! - `big_output=<bytes>`: write that many bytes to `<work>/big.bin`; a write
//!   error is reported as `fail` and ends the run with exit 1.
//! - `write_outside=<absolute path>`: try to create that file.
//! - `on_sigterm=exit|ignore`: SIGTERM disposition (default: kernel default).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use action::{Command, Events, Invocation, parse_args};

const MANIFEST: &str = "inacircle-manifest 0\n\
action\ttest.script\tfamily=test\tin=application/x-inacircle-test\tout=application/octet-stream\text=bin\tlabel=Test\tgroup=test\tcost=10\tlossy=no\tbatch=yes\n";

#[derive(Clone, Copy)]
enum Ev {
    Progress(f32),
    Done,
    Skip,
    Fail,
    Warn,
}

enum Step {
    Events(Vec<Ev>),
    Outputs(usize),
    SleepMs(u64),
    Exit(u8),
    BigOutput(u64),
    WriteOutside(PathBuf),
    IgnoreSigterm(bool),
}

fn parse_step(key: &str, value: &str) -> Result<Step, String> {
    let num = |what: &str| format!("{key}: invalid {what} `{value}`");
    match key {
        "events" => value
            .split(',')
            .map(|t| match t {
                "done" => Ok(Ev::Done),
                "skip" => Ok(Ev::Skip),
                "fail" => Ok(Ev::Fail),
                "warn" => Ok(Ev::Warn),
                _ => t
                    .strip_prefix("progress:")
                    .and_then(|f| f.parse().ok())
                    .map(Ev::Progress)
                    .ok_or_else(|| format!("events: unknown token `{t}`")),
            })
            .collect::<Result<_, _>>()
            .map(Step::Events),
        "outputs" => value.parse().map(Step::Outputs).map_err(|_| num("count")),
        "sleep_ms" => value.parse().map(Step::SleepMs).map_err(|_| num("number")),
        "exit" => value.parse().map(Step::Exit).map_err(|_| num("exit code")),
        "big_output" => value
            .parse()
            .map(Step::BigOutput)
            .map_err(|_| num("byte count")),
        "write_outside" => {
            let p = PathBuf::from(value);
            if p.is_absolute() {
                Ok(Step::WriteOutside(p))
            } else {
                Err(num("path (not absolute)"))
            }
        }
        "on_sigterm" => match value {
            "ignore" => Ok(Step::IgnoreSigterm(true)),
            "exit" => Ok(Step::IgnoreSigterm(false)),
            _ => Err(num("disposition")),
        },
        _ => Err(format!("unknown option `{key}`")),
    }
}

fn set_signal(sig: libc::c_int, ignore: bool) {
    let handler = if ignore { libc::SIG_IGN } else { libc::SIG_DFL };
    // SAFETY: `signal` is called with a valid signal number and one of the
    // two special dispositions (SIG_IGN/SIG_DFL), so no handler code runs
    // and there is no async-signal-safety concern.
    unsafe {
        libc::signal(sig, handler);
    }
}

/// Writes `size` bytes in chunks. Under `RLIMIT_FSIZE` the kernel sends
/// SIGXFSZ when the limit is exceeded; that signal is ignored first so the
/// write fails with EFBIG instead of killing the process.
fn write_big(path: &Path, size: u64) -> io::Result<()> {
    set_signal(libc::SIGXFSZ, true);
    let mut f = File::create(path)?;
    let chunk = [0u8; 64 * 1024];
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        f.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    f.flush()
}

fn run(inv: &Invocation, steps: Vec<Step>) -> u8 {
    let input = &inv.inputs[0];
    let mut ev = Events::new(io::stdout());
    let mut code = 0u8;
    for step in steps {
        match step {
            Step::Events(list) => {
                for e in list {
                    match e {
                        Ev::Progress(f) => ev.progress(f),
                        Ev::Done => {
                            let path = action::output_path(&inv.work, 0, input, "bin")
                                .and_then(|p| File::create(&p).map(|_| p));
                            match path {
                                Ok(p) => {
                                    let rel = p.strip_prefix(&inv.work).unwrap_or(&p);
                                    ev.done(input, rel);
                                }
                                Err(e) => {
                                    ev.fail(input, &e.to_string());
                                    return 1;
                                }
                            }
                        }
                        Ev::Skip => ev.skip(input, "skipped on request"),
                        Ev::Fail => ev.fail(input, "failed on request"),
                        Ev::Warn => ev.warn(input, "warning on request"),
                    }
                }
            }
            Step::Outputs(n) => {
                let dir = inv.work.join("0");
                if let Err(e) = fs::create_dir_all(&dir) {
                    ev.fail(input, &e.to_string());
                    return 1;
                }
                for k in 0..n {
                    let rel = PathBuf::from(format!("0/out-{k}.bin"));
                    if let Err(e) = File::create(inv.work.join(&rel)) {
                        ev.fail(input, &e.to_string());
                        return 1;
                    }
                    ev.done(input, &rel);
                }
            }
            Step::SleepMs(ms) => std::thread::sleep(Duration::from_millis(ms)),
            Step::Exit(c) => code = c,
            Step::BigOutput(size) => {
                if let Err(e) = write_big(&inv.work.join("big.bin"), size) {
                    ev.fail(input, &e.to_string());
                    return 1;
                }
            }
            Step::WriteOutside(path) => match fs::write(&path, b"x") {
                Ok(()) => ev.warn(input, "wrote outside work"),
                Err(e) => ev.fail(input, &e.to_string()),
            },
            Step::IgnoreSigterm(ignore) => set_signal(libc::SIGTERM, ignore),
        }
    }
    code
}

fn main() -> ExitCode {
    let inv = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Manifest) => {
            let _ = io::stdout().write_all(MANIFEST.as_bytes());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Run(inv)) => inv,
        Err(msg) => {
            eprintln!("inacircle-testaction: {msg}");
            return ExitCode::from(action::EXIT_USAGE as u8);
        }
    };
    if inv.action != "test.script" {
        eprintln!("inacircle-testaction: unknown action {}", inv.action);
        return ExitCode::from(action::EXIT_USAGE as u8);
    }
    let steps: Result<Vec<Step>, String> = inv.opts.iter().map(|(k, v)| parse_step(k, v)).collect();
    match steps {
        Ok(steps) => ExitCode::from(run(&inv, steps)),
        Err(msg) => {
            eprintln!("inacircle-testaction: {msg}");
            ExitCode::from(action::EXIT_USAGE as u8)
        }
    }
}
