//! The action catalog: which actions exist, found by asking every action
//! binary for its manifest (`<bin> --manifest`) and cached so that a normal
//! start costs a few `stat` calls.
//!
//! Cache format (`$XDG_CACHE_HOME/inacircle/catalog`): header
//! `inacircle-catalog 0`; per binary a `bin` line (`path`, `mtime` in ns,
//! `size`, `builtin`) followed by that binary's `action` lines. The cache is
//! valid only when the current set of binaries and their (mtime, size) match
//! it exactly; otherwise everything is rebuilt. A binary whose manifest is
//! broken is cached with no actions, so it is not rerun until it changes.

use manifest::Line;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MANIFEST_MAGIC: &str = "inacircle-manifest";
const CACHE_MAGIC: &str = "inacircle-catalog";
const DEADLINE: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub id: String,
    pub family: String,
    pub bin: PathBuf,
    pub input: String,
    pub output: String,
    pub ext: String,
    pub label: String,
    pub group: String,
    pub cost: u32,
    pub lossy: bool,
    pub batch: bool,
    pub requires: Vec<String>,
    pub builtin: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub actions: Vec<Action>,
}

/// One action binary as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bin {
    path: PathBuf,
    mtime: i128,
    size: u64,
    builtin: bool,
}

/// A binary plus its (validated) `action` lines.
struct Entry {
    bin: Bin,
    lines: Vec<Line>,
}

impl Catalog {
    /// Loads from the standard directories and cache location.
    pub fn load() -> Catalog {
        let cache = crate::xdg::cache_home().join("inacircle/catalog");
        Catalog::load_from(&default_dirs(), &cache)
    }

    /// Testable core. `dirs` is in priority order; the bool marks built-in.
    pub fn load_from(dirs: &[(PathBuf, bool)], cache: &Path) -> Catalog {
        let bins = scan(dirs);
        let entries = match read_cache(cache) {
            Some(cached) if cached.iter().map(|e| &e.bin).eq(bins.iter()) => cached,
            _ => {
                let entries: Vec<Entry> = bins
                    .into_iter()
                    .map(|bin| {
                        let lines = run_manifest(&bin.path);
                        Entry { bin, lines }
                    })
                    .collect();
                write_cache(cache, &entries);
                entries
            }
        };
        let mut actions: Vec<Action> = Vec::new();
        for e in &entries {
            for l in &e.lines {
                let Some(a) = to_action(l, &e.bin) else {
                    continue;
                };
                if actions.iter().any(|x| x.id == a.id) {
                    eprintln!(
                        "inacircle: duplicate action {} in {} ignored",
                        a.id,
                        e.bin.path.display()
                    );
                } else {
                    actions.push(a);
                }
            }
        }
        Catalog { actions }
    }

    pub fn get(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.id == id)
    }
}

/// `requires=` tools that are not executable files on `PATH`.
pub fn missing_tools(a: &Action) -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    a.requires
        .iter()
        .filter(|t| {
            !std::env::split_paths(&path).any(|d| {
                std::fs::metadata(d.join(t)).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
            })
        })
        .cloned()
        .collect()
}

fn default_dirs() -> Vec<(PathBuf, bool)> {
    if let Some(v) = std::env::var_os("INACIRCLE_ACTIONS_DIR") {
        return std::env::split_paths(&v)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| (std::path::absolute(&p).unwrap_or(p), true))
            .collect();
    }
    vec![
        (PathBuf::from("/usr/lib/inacircle/actions"), true),
        (PathBuf::from("/usr/local/lib/inacircle/actions"), true),
        (crate::xdg::data_home().join("inacircle/actions"), false),
    ]
}

/// Executable regular files (links resolved) in each dir, sorted by name.
fn scan(dirs: &[(PathBuf, bool)]) -> Vec<Bin> {
    let mut out = Vec::new();
    for (dir, builtin) in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<Bin> = rd
            .flatten()
            .filter_map(|e| {
                let m = std::fs::metadata(e.path()).ok()?;
                if !m.is_file() || m.mode() & 0o111 == 0 {
                    return None;
                }
                Some(Bin {
                    path: e.path(),
                    mtime: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                    size: m.len(),
                    builtin: *builtin,
                })
            })
            .collect();
        found.sort_by(|a, b| a.path.cmp(&b.path));
        out.extend(found);
    }
    out
}

fn to_action(l: &Line, bin: &Bin) -> Option<Action> {
    if l.kind != "action" {
        return None;
    }
    let yes_no = |k: &str| match l.get(k)? {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    };
    let text = |k: &str| l.get(k).filter(|v| !v.is_empty()).map(str::to_owned);
    Some(Action {
        id: l.bare_at(0).filter(|v| !v.is_empty())?.to_owned(),
        family: text("family")?,
        bin: bin.path.clone(),
        input: text("in")?,
        output: text("out")?,
        ext: text("ext")?,
        label: text("label")?,
        group: text("group")?,
        cost: l.get("cost")?.parse().ok()?,
        lossy: yes_no("lossy")?,
        batch: yes_no("batch")?,
        requires: l
            .get("requires")
            .map(|r| {
                r.split(',')
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        builtin: bin.builtin,
    })
}

/// Runs `<bin> --manifest` and returns its valid `action` lines; a broken
/// binary yields none (and a message on stderr).
fn run_manifest(bin: &Path) -> Vec<Line> {
    let skip = |why: &str| {
        eprintln!("inacircle: {}: {why}; skipped", bin.display());
        Vec::new()
    };
    let out = match capture(bin) {
        Ok(o) => o,
        Err(e) => return skip(&e),
    };
    let Ok(text) = String::from_utf8(out) else {
        return skip("manifest is not UTF-8");
    };
    match manifest::parse(&text, MANIFEST_MAGIC) {
        Ok((0, lines)) => lines
            .into_iter()
            .filter(|l| {
                let ok = to_action(
                    l,
                    &Bin {
                        path: PathBuf::new(),
                        mtime: 0,
                        size: 0,
                        builtin: false,
                    },
                )
                .is_some();
                if !ok {
                    eprintln!("inacircle: {}: invalid action line ignored", bin.display());
                }
                ok
            })
            .collect(),
        Ok((v, _)) => skip(&format!("unknown manifest version {v}")),
        Err(e) => skip(&format!("bad manifest: line {}: {}", e.line, e.msg)),
    }
}

/// Stdout of `<bin> --manifest` within the deadline and size limit; the child
/// is killed and reaped on any failure.
fn capture(bin: &Path) -> Result<Vec<u8>, String> {
    let mut child = Command::new(bin)
        .arg("--manifest")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run: {e}"))?;
    let result = read_limited(&mut child);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = wait_until(&mut child, Instant::now() + DEADLINE);
    let out = result?;
    match status {
        Some(s) if s.success() => Ok(out),
        Some(s) => Err(format!("exited with {s}")),
        None => Err("did not exit in time".to_owned()),
    }
}

fn read_limited(child: &mut std::process::Child) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + DEADLINE;
    let stdout = child.stdout.as_ref().ok_or("no stdout")?;
    let fd = stdout.as_raw_fd();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out".to_owned());
        }
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as i32 + 1;
        // SAFETY: `p` is a valid pollfd and nfds is 1.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("poll failed".to_owned());
        }
        if r == 0 {
            continue; // re-check the deadline
        }
        // SAFETY: `buf` is valid for `buf.len()` bytes; `fd` is open (owned
        // by `child.stdout`, which outlives this call).
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("read failed".to_owned());
        }
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n as usize]);
        if out.len() > MAX_OUTPUT {
            return Err("manifest larger than 64 KB".to_owned());
        }
    }
}

/// Waits for exit until `deadline`, then kills; always reaps.
fn wait_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn read_cache(path: &Path) -> Option<Vec<Entry>> {
    let text = std::fs::read_to_string(path).ok()?;
    let (0, lines) = manifest::parse(&text, CACHE_MAGIC).ok()? else {
        return None;
    };
    let mut entries: Vec<Entry> = Vec::new();
    for l in lines {
        match l.kind.as_str() {
            "bin" => entries.push(Entry {
                bin: Bin {
                    path: PathBuf::from(l.get("path")?),
                    mtime: l.get("mtime")?.parse().ok()?,
                    size: l.get("size")?.parse().ok()?,
                    builtin: match l.get("builtin")? {
                        "yes" => true,
                        "no" => false,
                        _ => return None,
                    },
                },
                lines: Vec::new(),
            }),
            "action" => entries.last_mut()?.lines.push(l),
            _ => return None,
        }
    }
    Some(entries)
}

/// Atomic (temp file + rename); any failure just means no cache.
fn write_cache(path: &Path, entries: &[Entry]) {
    let mut s = manifest::header(CACHE_MAGIC, 0);
    for e in entries {
        let b = &e.bin;
        let line = Line::new("bin")
            .pair("path", &b.path.to_string_lossy())
            .pair("mtime", &b.mtime.to_string())
            .pair("size", &b.size.to_string())
            .pair("builtin", if b.builtin { "yes" } else { "no" });
        s.push_str(&manifest::write_line(&line));
        for l in &e.lines {
            s.push_str(&manifest::write_line(l));
        }
    }
    let _ = (|| -> std::io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".catalog.{}.tmp", std::process::id()));
        let r = std::fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(s.as_bytes()))
            .and_then(|()| std::fs::rename(&tmp, path));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    })();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Serializes tests: writing then exec'ing scripts from parallel threads
    /// races with fork (ETXTBSY).
    static LOCK: Mutex<()> = Mutex::new(());

    struct Fix {
        root: PathBuf,
        _g: std::sync::MutexGuard<'static, ()>,
    }

    impl Fix {
        fn new(name: &str) -> Fix {
            let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let root =
                std::env::temp_dir().join(format!("engine-cat-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Fix { root, _g: g }
        }
        fn dir(&self, name: &str) -> PathBuf {
            let d = self.root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        }
        fn cache(&self) -> PathBuf {
            self.root.join("cache/inacircle/catalog")
        }
    }

    impl Drop for Fix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn act(id: &str, label: &str) -> String {
        format!(
            "action\t{id}\tfamily=f\tin=text/plain\tout=text/markdown\text=md\tlabel={label}\tgroup=g\tcost=10\tlossy=no\tbatch=yes"
        )
    }

    fn manifest_script(path: &Path, lines: &[String]) {
        let mut body =
            String::from("[ \"$1\" = --manifest ] || exit 1\nprintf 'inacircle-manifest 0\\n'\n");
        for l in lines {
            body.push_str(&format!(
                "printf '%s\\n' '{}'\n",
                l.replace('\t', "'\"\t\"'")
            ));
        }
        script(path, &body);
    }

    #[test]
    fn loads_actions_from_dirs() {
        let f = Fix::new("loads");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A"), act("x.b", "B")]);
        std::fs::write(d.join("notexec"), "data").unwrap();
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        let ids: Vec<_> = c.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["x.a", "x.b"]);
        let a = c.get("x.a").unwrap();
        assert_eq!(a.family, "f");
        assert_eq!(a.bin, d.join("one"));
        assert_eq!(a.input, "text/plain");
        assert_eq!(a.output, "text/markdown");
        assert_eq!(a.ext, "md");
        assert_eq!(a.label, "A");
        assert_eq!(a.group, "g");
        assert_eq!(a.cost, 10);
        assert!(!a.lossy && a.batch && a.builtin);
        assert!(a.requires.is_empty());
        assert!(c.get("nope").is_none());
    }

    #[test]
    fn requires_and_missing_fields() {
        let f = Fix::new("req");
        let d = f.dir("a");
        manifest_script(
            &d.join("one"),
            &[
                format!("{}\trequires=foo,bar", act("x.a", "A")),
                "action\tx.bad\tfamily=f".to_owned(),
            ],
        );
        let c = Catalog::load_from(&[(d, false)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        assert_eq!(c.actions[0].requires, ["foo", "bar"]);
        assert!(!c.actions[0].builtin);
    }

    #[test]
    fn cache_reused_when_unchanged() {
        let f = Fix::new("reuse");
        let d = f.dir("a");
        let marker = f.root.join("marker");
        let mut body = act("x.a", "A");
        body = format!(
            "echo run >> {}\nprintf 'inacircle-manifest 0\\n%s\\n' '{}'",
            marker.display(),
            body.replace('\t', "'\"\t\"'")
        );
        script(&d.join("one"), &body);
        let first = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(first.actions.len(), 1);
        assert!(f.cache().exists());
        let second = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(second.actions, first.actions);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "run\n");
    }

    #[test]
    fn cache_invalidated_on_mtime_change_and_on_new_binary() {
        let f = Fix::new("inval");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "A");
        // Same size, different mtime.
        manifest_script(&d.join("one"), &[act("x.a", "B")]);
        let t = std::fs::metadata(d.join("one"))
            .unwrap()
            .modified()
            .unwrap()
            + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(d.join("one"))
            .unwrap()
            .set_modified(t)
            .unwrap();
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "B");
        manifest_script(&d.join("two"), &[act("y.a", "Y")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert!(c.get("y.a").is_some() && c.get("x.a").is_some());
        std::fs::remove_file(d.join("two")).unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(c.get("y.a").is_none());
    }

    #[test]
    fn unwritable_cache_does_not_fail_load() {
        let f = Fix::new("nocache");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        std::fs::write(f.root.join("blocker"), "").unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.root.join("blocker/inacircle/catalog"));
        assert_eq!(c.actions.len(), 1);
    }

    #[test]
    fn corrupt_cache_is_rebuilt() {
        let f = Fix::new("corrupt");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        std::fs::create_dir_all(f.cache().parent().unwrap()).unwrap();
        std::fs::write(f.cache(), "garbage\n").unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
    }

    #[test]
    fn broken_manifest_skipped_others_kept() {
        let f = Fix::new("broken");
        let d = f.dir("a");
        script(&d.join("a-garbage"), "echo nonsense");
        script(&d.join("b-fail"), "exit 3");
        script(&d.join("c-version"), "printf 'inacircle-manifest 9\\n'");
        script(
            &d.join("d-huge"),
            "printf 'inacircle-manifest 0\\n'; yes '# padding padding padding' | head -c 200000",
        );
        manifest_script(&d.join("e-good"), &[act("x.a", "A")]);
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        assert_eq!(c.actions[0].id, "x.a");
    }

    #[test]
    fn hanging_manifest_killed_after_deadline() {
        let f = Fix::new("hang");
        let d = f.dir("a");
        script(&d.join("hang"), "exec sleep 10");
        let t = Instant::now();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(c.actions.is_empty());
    }

    #[test]
    fn duplicate_id_first_dir_wins() {
        let f = Fix::new("dup");
        let (d1, d2) = (f.dir("a"), f.dir("b"));
        manifest_script(&d1.join("one"), &[act("x.a", "first")]);
        manifest_script(&d2.join("two"), &[act("x.a", "second"), act("x.b", "B")]);
        let c = Catalog::load_from(&[(d1, true), (d2, false)], &f.cache());
        assert_eq!(c.actions.len(), 2);
        assert_eq!(c.get("x.a").unwrap().label, "first");
        assert!(c.get("x.b").is_some());
    }

    #[test]
    fn missing_tools_checks_path() {
        let a = Catalog::load_from(&[], Path::new("/nonexistent/c")).actions;
        assert!(a.is_empty());
        let act = Action {
            id: "i".into(),
            family: "f".into(),
            bin: "/b".into(),
            input: "a/b".into(),
            output: "a/b".into(),
            ext: "x".into(),
            label: "l".into(),
            group: "g".into(),
            cost: 1,
            lossy: false,
            batch: false,
            requires: vec!["sh".into(), "no-such-tool-inacircle".into()],
            builtin: true,
        };
        assert_eq!(missing_tools(&act), ["no-such-tool-inacircle"]);
    }
}
