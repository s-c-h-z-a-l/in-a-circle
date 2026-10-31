//! The action catalog: which actions exist, found by asking every action
//! binary for its manifest (`<bin> --manifest`) and cached so that a normal
//! start costs a few `stat` calls.
//!
//! Cache format (`$XDG_CACHE_HOME/inacircle/catalog`): header
//! `inacircle-catalog 1`; per binary a `bin` line (`path`, `mtime` in ns,
//! `size`, `builtin`, and `fails`/`retry` while it backs off) followed by
//! that binary's `action` lines. The cache is used as it is only when the
//! current set of binaries and their (mtime, size) match it exactly and no
//! binary is due for a retry; otherwise the binaries that are new, changed
//! or due rerun their manifest and the rest come from the cache. A binary
//! whose manifest is broken is cached with no actions, so it is not rerun
//! until it changes. A binary whose run fails by its own fault (it hangs
//! past its own deadline, crashes, exits non-zero, leaves processes or its
//! output open) backs off: it keeps its old actions, if any, and is not
//! rerun before `retry` (unix seconds), 60 s after the first failure in a
//! row, doubling up to a day; a success or a change of the binary ends it.

use crate::exec;
use manifest::Line;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MANIFEST_MAGIC: &str = "inacircle-manifest";
const CACHE_MAGIC: &str = "inacircle-catalog";
/// 1: a `bin` line may carry `fails` and `retry`. A version-0 reader would
/// ignore them and take a backed-off entry (the binary's current identity,
/// its old or no actions) as final, never rerunning it; so each version
/// rejects the other's cache, which is then rebuilt.
const CACHE_VERSION: u32 = 1;
/// The wait after the first failure in a row that is the binary's own: a
/// one-off hang (a loaded disk at login) recovers within a minute, while a
/// binary that hangs at every start costs a start 2 s at most once a minute.
const BACKOFF_FIRST: u64 = 60;
/// The longest wait (24 h), reached at the 12th failure in a row
/// (60 x 2^11 > 86 400): a binary broken for good is tried once a day, so a
/// system repaired meanwhile gets its actions back within a day. A `retry`
/// further ahead than this cannot be ours (the clock went back), so it is
/// tried at once instead of waiting for days.
const BACKOFF_MAX: u64 = 86_400;
/// The failure counter stops here. From the 12th failure on the wait is
/// already `BACKOFF_MAX`, so a higher count changes nothing; the ceiling
/// keeps `2^(fails - 1)` within a `u64` shift and is what the cache reader
/// accepts (1..=64, past it the cache is rejected).
const BACKOFF_FAILS_MAX: u32 = 64;
/// `retry` in the cache: `u64::MAX` has 20 decimal digits, so a longer
/// value is no `u64` and is refused before it is parsed.
const RETRY_DIGITS_MAX: usize = 20;
/// One manifest's deadline: it prints a few lines and exits in milliseconds,
/// so 2 s is far above a healthy run and short enough to cut a hang.
const DEADLINE: Duration = Duration::from_secs(2);
/// The whole refresh's budget, shared by all manifests it runs. Only new or
/// changed binaries run a manifest, each in milliseconds, so a healthy
/// refresh uses a tiny part of it; 10 s cuts in after five hanging binaries
/// in a row (5 x `DEADLINE`) instead of N x 2 s. What is left unrun is
/// transient: a binary with an old cache entry keeps it (so its family stays
/// on the wheel), one without stays out, and either is retried at the next
/// start. Not `alarm`: SIGALRM would kill only our process, and the manifest
/// child, in its own process group, would be orphaned; each step's deadline
/// instead ends in `kill_group`.
const REFRESH_BUDGET: Duration = Duration::from_secs(10);
const MAX_OUTPUT: usize = 64 * 1024;

// Cache limits (`cache_*`). The cache is ours and rebuildable,
// so a cache past any of them is no cache: it is rejected loudly and
// rebuilt, never half-used. The real cache is about 0.4 KB for three
// records; the limits leave 1.6x to 100x room over the largest shape the
// other limits allow.
/// `cache_bytes`: 1024 actions x ~133 B + 128 bins x ~235 B (with a
/// backoff) is ~166 KB; 256 KiB is 1.58x. Checked on the bytes actually
/// read (max + 1).
const CACHE_BYTES: usize = 262_144;
/// `cache_bins`: ~100x the handful of shipped binaries; each bin is one
/// spawn on a rebuild, so this bounds the rebuild cost too.
const CACHE_BINS: usize = 128;
/// `cache_actions`: ~100x the dozen families x presets planned.
const CACHE_ACTIONS: usize = 1024;
/// `cache_records`: `CACHE_BINS + CACHE_ACTIONS`, no own basis.
const CACHE_RECORDS: usize = CACHE_BINS + CACHE_ACTIONS;
/// `cache_fields_per_record`: the widest real record has 10 fields
/// (columns after the kind); 32 is 3.2x. Counted before any allocation.
const CACHE_FIELDS_PER_RECORD: usize = 32;
/// `cache_total_fields`: 1024 x 10 + 128 x 6 (a bin with a backoff) =
/// 11 008 real-shaped fields; 12 288 is 1.12x (the earlier 8192 refused a
/// valid full cache).
const CACHE_TOTAL_FIELDS: usize = 12_288;
/// `cache_line_bytes`: a 4096 B path escaped twice is 8 KiB plus ~200 B
/// of other fields; 32 KiB is 3.9x.
const CACHE_LINE_BYTES: usize = 32_768;
// Manifest limits (`manifest_*`), applied to the captured text
// (already <= `MAX_OUTPUT`) before `manifest::parse` allocates per record.
/// `manifest_records`: a real manifest has 8 action lines; 256 is 32x.
const MANIFEST_RECORDS: usize = 256;
/// `manifest_fields_per_record`: the widest real action has 10; 32 is 3.2x.
const MANIFEST_FIELDS_PER_RECORD: usize = 32;
/// `manifest_total_fields`: 256 x 10 = 2560 real-shaped; 4096 is 1.6x.
const MANIFEST_TOTAL_FIELDS: usize = 4096;
/// `manifest_line_bytes`: ~130 B real, 8 KiB with an escaped 4096 B path.
const MANIFEST_LINE_BYTES: usize = 32_768;

/// A text format's per-line, record and field limits.
struct Shape {
    line: usize,
    records: usize,
    fields_per_record: usize,
    total_fields: usize,
}

const CACHE_SHAPE: Shape = Shape {
    line: CACHE_LINE_BYTES,
    records: CACHE_RECORDS,
    fields_per_record: CACHE_FIELDS_PER_RECORD,
    total_fields: CACHE_TOTAL_FIELDS,
};
const MANIFEST_SHAPE: Shape = Shape {
    line: MANIFEST_LINE_BYTES,
    records: MANIFEST_RECORDS,
    fields_per_record: MANIFEST_FIELDS_PER_RECORD,
    total_fields: MANIFEST_TOTAL_FIELDS,
};

/// Checks `text` against `shape` before `manifest::parse` allocates: the
/// header and every line stay within `shape.line` bytes; every record
/// (a line that is not blank or a `#` comment, after the header) has at
/// most `fields_per_record` fields (tab-separated columns after the kind)
/// and all together at most `total_fields`, in at most `records` records.
/// Counts only; allocates nothing.
fn preflight(text: &str, shape: &Shape) -> Result<(), &'static str> {
    let (mut records, mut total) = (0usize, 0usize);
    for (i, l) in text.lines().enumerate() {
        if l.len() > shape.line {
            return Err("a line is too long");
        }
        if i == 0 || l.is_empty() || l.starts_with('#') {
            continue;
        }
        records += 1;
        if records > shape.records {
            return Err("too many records");
        }
        let fields = l.bytes().filter(|&b| b == b'\t').count();
        if fields > shape.fields_per_record {
            return Err("a record has too many fields");
        }
        total = total.saturating_add(fields);
        if total > shape.total_fields {
            return Err("too many fields in all");
        }
    }
    Ok(())
}

/// What the cache would hold, counted as entries are accepted, so the
/// state a rebuild collects obeys the same limits as the file the parser
/// accepts (each manifest can be valid alone and the sum still too big).
struct CacheBudget {
    bins: usize,
    actions: usize,
    fields: usize,
    bytes: usize,
}

impl CacheBudget {
    fn new() -> CacheBudget {
        CacheBudget {
            bins: 0,
            actions: 0,
            fields: 0,
            bytes: manifest::header(CACHE_MAGIC, CACHE_VERSION).len(),
        }
    }

    /// Takes `e` in, or says which limit it would cross (and takes nothing).
    fn take(&mut self, e: &Entry) -> Result<(), &'static str> {
        let bin_l = bin_line(e);
        let bin = manifest::write_line(&bin_l);
        let mut next = CacheBudget {
            bins: self.bins + 1,
            actions: self.actions,
            fields: self.fields + bin_l.items.len(),
            bytes: self.bytes.saturating_add(bin.len()),
        };
        if bin.len() - 1 > CACHE_LINE_BYTES {
            return Err("a binary's path is too long");
        }
        for l in &e.lines {
            let w = manifest::write_line(l);
            if w.len() - 1 > CACHE_LINE_BYTES || l.items.len() > CACHE_FIELDS_PER_RECORD {
                return Err("an action line is too big");
            }
            next.actions += 1;
            next.fields = next.fields.saturating_add(l.items.len());
            next.bytes = next.bytes.saturating_add(w.len());
        }
        if next.bins > CACHE_BINS {
            return Err("more binaries than the cache holds");
        }
        if next.actions > CACHE_ACTIONS {
            return Err("more actions than the cache holds");
        }
        if next.bins + next.actions > CACHE_RECORDS {
            return Err("more records than the cache holds");
        }
        if next.fields > CACHE_TOTAL_FIELDS {
            return Err("more fields than the cache holds");
        }
        if next.bytes > CACHE_BYTES {
            return Err("more bytes than the cache holds");
        }
        *self = next;
        Ok(())
    }
}

fn bin_line(e: &Entry) -> Line {
    let b = &e.bin;
    let l = Line::new("bin")
        .pair("path", &b.path.to_string_lossy())
        .pair("mtime", &b.mtime.to_string())
        .pair("size", &b.size.to_string())
        .pair("builtin", if b.builtin { "yes" } else { "no" });
    match &e.backoff {
        Some(o) => l
            .pair("fails", &o.fails.to_string())
            .pair("retry", &o.retry.to_string()),
        None => l,
    }
}

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
    pub join: Option<Join>,
    pub requires: Vec<String>,
    pub builtin: bool,
}

/// A many-inputs-to-one-output action: the MIME types it takes and the
/// fewest accepted inputs a run needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub types: Vec<String>,
    pub min: u32,
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

/// A binary plus its (validated) `action` lines, and its backoff while
/// its last runs failed by its own fault.
#[cfg_attr(fuzzing, derive(PartialEq))]
struct Entry {
    bin: Bin,
    lines: Vec<Line>,
    backoff: Option<Backoff>,
}

/// A binary's backoff: `fails` failures of its own in a row (1 to
/// `BACKOFF_FAILS_MAX`), its manifest not rerun before `retry` (unix
/// seconds, `CLOCK_REALTIME`, which survives a reboot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    fails: u32,
    retry: u64,
}

impl Backoff {
    /// The backoff after one more failure of its own, `prev` before it
    /// (0: none), at `now`: `BACKOFF_FIRST x 2^(n - 1)`, at most
    /// `BACKOFF_MAX`, with `n` the new count.
    fn after(prev: u32, now: u64) -> Backoff {
        let fails = prev.saturating_add(1).clamp(1, BACKOFF_FAILS_MAX);
        let wait = 1u64
            .checked_shl(fails - 1)
            .and_then(|m| m.checked_mul(BACKOFF_FIRST))
            .map_or(BACKOFF_MAX, |w| w.min(BACKOFF_MAX));
        Backoff {
            fails,
            retry: now.saturating_add(wait),
        }
    }

    /// Whether the manifest reruns at `now`: its time has come, or `retry`
    /// is further ahead than any wait we set (the clock went back).
    fn due(&self, now: u64) -> bool {
        now >= self.retry || self.retry > now.saturating_add(BACKOFF_MAX)
    }
}

impl Entry {
    /// Whether this entry's binary reruns its manifest at `now` even
    /// though it did not change.
    fn due(&self, now: u64) -> bool {
        self.backoff.is_some_and(|b| b.due(now))
    }
}

/// The wall clock in unix seconds (`CLOCK_REALTIME`); before the epoch, 0
/// (every backoff is then further ahead than `BACKOFF_MAX`: retried).
fn wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Catalog {
    /// Loads from the standard directories and cache location.
    /// A refused cache or data dir (see [`crate::xdg`]) is reported and
    /// not used.
    pub fn load() -> Catalog {
        let (dirs, cache) = standard();
        Catalog::load_with(&dirs, cache.as_deref(), wall_secs())
    }

    /// Testable core. `dirs` is in priority order; the bool marks built-in.
    pub fn load_from(dirs: &[(PathBuf, bool)], cache: &Path) -> Catalog {
        Catalog::load_with(dirs, Some(cache), wall_secs())
    }

    /// The catalog from the cache alone, for the wheel: no manifest is run
    /// while the overlay is up. A cached binary still found in the action
    /// directories is used as cached, even if it changed since; one the
    /// cache does not know is left out until the next [`Catalog::load`].
    /// `None` without a readable cache.
    pub fn load_cached() -> Option<Catalog> {
        let (dirs, cache) = standard();
        Catalog::load_cached_from(&dirs, cache.as_deref()?)
    }

    /// Testable core of [`Catalog::load_cached`].
    pub fn load_cached_from(dirs: &[(PathBuf, bool)], cache: &Path) -> Option<Catalog> {
        let bins = scan(dirs);
        let mut entries = read_cache(cache)?;
        entries.retain(|e| bins.iter().any(|b| b.path == e.bin.path));
        Some(Catalog::of(&entries))
    }

    /// The catalog at wall-clock time `now` (unix seconds): see the module
    /// doc for when a manifest reruns.
    fn load_with(dirs: &[(PathBuf, bool)], cache: Option<&Path>, now: u64) -> Catalog {
        let refresh_end = Instant::now() + REFRESH_BUDGET;
        let mut bins = scan(dirs);
        if bins.len() > CACHE_BINS {
            eprintln!(
                "inacircle: more than {CACHE_BINS} action binaries found; only the first {CACHE_BINS} are used (the action catalog's limit)"
            );
            bins.truncate(CACHE_BINS);
        }
        let cached = cache.and_then(read_cache).unwrap_or_default();
        // A hit: every binary as cached and none due for a retry.
        let hit =
            cached.iter().map(|e| &e.bin).eq(bins.iter()) && !cached.iter().any(|e| e.due(now));
        let entries: Vec<Entry> = if hit {
            cached
        } else {
            // Rerun only binaries that are new, changed or due. When the
            // rerun fails transiently, the binary's old entry (same path,
            // old mtime and size or a due backoff) stays as it was, so its
            // family stays on the wheel, which reads the cache alone, and
            // the next start reruns it. A binary with no old entry stays
            // out. A failure of its own backs the binary off instead.
            let mut cached = cached;
            let mut budget = CacheBudget::new();
            let mut entries: Vec<Entry> = Vec::new();
            for bin in bins {
                let mut entry = if let Some(i) =
                    cached.iter().position(|e| e.bin == bin && !e.due(now))
                {
                    cached.swap_remove(i)
                } else {
                    let old = cached
                        .iter()
                        .position(|e| e.bin.path == bin.path)
                        .map(|i| cached.swap_remove(i));
                    match run_manifest(&bin.path, refresh_end) {
                        Run::Lines(lines) => Entry {
                            bin,
                            lines,
                            backoff: None,
                        },
                        Run::Penalized(why) => {
                            // The same binary failing again counts on; a
                            // changed one starts over. Its old actions, if
                            // any, stay on the wheel meanwhile.
                            let prev = match &old {
                                Some(o) if o.bin == bin => o.backoff.map_or(0, |b| b.fails),
                                _ => 0,
                            };
                            let backoff = Backoff::after(prev, now);
                            eprintln!(
                                "inacircle: {}: {why}; skipped, retried in {} s",
                                bin.path.display(),
                                backoff.retry - now
                            );
                            Entry {
                                bin,
                                lines: old.map(|o| o.lines).unwrap_or_default(),
                                backoff: Some(backoff),
                            }
                        }
                        Run::Transient => {
                            // The old entry stays as it was (old bin),
                            // unless it no longer fits: then it goes,
                            // loudly, like any binary over the limits.
                            if let Some(old) = old {
                                match budget.take(&old) {
                                    Ok(()) => entries.push(old),
                                    Err(why) => eprintln!(
                                        "inacircle: {}: {why}; its actions are refused (the action catalog's limits)",
                                        old.bin.path.display()
                                    ),
                                }
                            }
                            continue;
                        }
                    }
                };
                // Valid alone is not enough: the sum of all manifests must
                // fit what the cache can hold. A binary that would cross a
                // limit is refused whole and loudly (no action of it is
                // dropped alone), and cached without actions so it is not
                // rerun until it changes.
                if let Err(why) = budget.take(&entry) {
                    eprintln!(
                        "inacircle: {}: {why}; its actions are refused (the action catalog's limits)",
                        entry.bin.path.display()
                    );
                    entry.lines = Vec::new();
                    // A bare bin line always fits when bins < CACHE_BINS.
                    if budget.take(&entry).is_err() {
                        continue;
                    }
                }
                entries.push(entry);
            }
            if let Some(cache) = cache {
                write_cache(cache, &entries);
            }
            entries
        };
        Catalog::of(&entries)
    }

    /// The actions of `entries`; a duplicate id after the first is ignored.
    fn of(entries: &[Entry]) -> Catalog {
        let mut actions: Vec<Action> = Vec::new();
        for e in entries {
            for l in &e.lines {
                let Some(a) = parse_action(l) else {
                    continue;
                };
                let a = Action {
                    bin: e.bin.path.clone(),
                    builtin: e.bin.builtin,
                    ..a
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

    /// The join actions (many inputs, one output).
    pub fn joins(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| a.join.is_some())
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

/// The standard action directories and cache path.
fn standard() -> (Vec<(PathBuf, bool)>, Option<PathBuf>) {
    let over = std::env::var_os("INACIRCLE_ACTIONS_DIR");
    let cache = match crate::xdg::cache_home() {
        Ok(c) => Some(c.join("inacircle").join(cache_name(over.as_deref()))),
        Err(e) => {
            eprintln!("inacircle: no catalog cache: {e}");
            None
        }
    };
    let data = crate::xdg::data_home()
        .inspect_err(|e| eprintln!("inacircle: no user actions: {e}"))
        .ok();
    (dirs_from(over, data), cache)
}

/// The cache file for a set of action directories: `catalog` for the
/// standard ones, `catalog-<hash>` for an `INACIRCLE_ACTIONS_DIR` override.
/// One file per set: a run with another set (a test, a shell without the
/// variable) must not overwrite the catalog the wheel reads from the cache.
fn cache_name(over: Option<&std::ffi::OsStr>) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Some(v) = over else {
        return "catalog".to_owned();
    };
    // FNV-1a, 64 bit: stable across runs and builds (unlike std's hasher).
    let h = v.as_bytes().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("catalog-{h:016x}")
}

/// Action directories: the override (colon-separated, all built-in; empty
/// segments dropped, relative ones made absolute) or the standard three
/// (the user's only with a data home).
fn dirs_from(over: Option<std::ffi::OsString>, data_home: Option<PathBuf>) -> Vec<(PathBuf, bool)> {
    if let Some(v) = over {
        return std::env::split_paths(&v)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| (std::path::absolute(&p).unwrap_or(p), true))
            .collect();
    }
    let mut dirs = vec![
        (PathBuf::from("/usr/lib/inacircle/actions"), true),
        (PathBuf::from("/usr/local/lib/inacircle/actions"), true),
    ];
    dirs.extend(data_home.map(|d| (d.join("inacircle/actions"), false)));
    dirs
}

/// Entries examined per action folder, on the wheel's path too (not a
/// table row; derived): 4x `CACHE_BINS`, room for the binaries plus unrelated
/// files beside them. Each entry costs one `stat`, ~0.3 us (a name lookup
/// measured at 17.7 us for 64, measured while bounding the scan), so a full
/// folder is ~0.15 ms. A folder with more is no action folder we know
/// (a wrong `INACIRCLE_ACTIONS_DIR`, a broken install): the scan stops at
/// entry 513 and the folder is refused whole, loudly, rather than used in
/// part (readdir order would decide which part).
const SCAN_DIR_ENTRIES: usize = 512;

/// The executable regular files (links resolved) of `dir` with the `room`
/// smallest paths, sorted. Holds at most `room` binaries while it runs and
/// examines at most `SCAN_DIR_ENTRIES` + 1 entries (`examined`); past that,
/// `Err`. An unreadable folder has none.
fn scan_dir(dir: &Path, builtin: &bool, room: usize, examined: &mut usize) -> Result<Vec<Bin>, ()> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut found: Vec<Bin> = Vec::new();
    for e in rd {
        *examined += 1;
        if *examined > SCAN_DIR_ENTRIES {
            return Err(());
        }
        let Ok(e) = e else {
            continue;
        };
        if e.file_name().as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let path = e.path();
        if found.len() == room && found.last().is_none_or(|b| b.path <= path) {
            continue;
        }
        let Ok(m) = std::fs::metadata(&path) else {
            continue;
        };
        if !m.is_file() || m.mode() & 0o111 == 0 {
            continue;
        }
        let at = found.partition_point(|b| b.path < path);
        found.insert(
            at,
            Bin {
                path,
                mtime: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                size: m.len(),
                builtin: *builtin,
            },
        );
        found.truncate(room);
    }
    Ok(found)
}

/// Executable regular files (links resolved) in each dir, sorted by name,
/// in folder order: at most `CACHE_BINS` + 1 in all (one over says the
/// catalog's limit was crossed). A folder past `SCAN_DIR_ENTRIES` is
/// refused with a message.
fn scan(dirs: &[(PathBuf, bool)]) -> Vec<Bin> {
    let mut out = Vec::new();
    for (dir, builtin) in dirs {
        let room = (CACHE_BINS + 1).saturating_sub(out.len());
        if room == 0 {
            break;
        }
        match scan_dir(dir, builtin, room, &mut 0) {
            Ok(found) => out.extend(found),
            Err(()) => eprintln!(
                "inacircle: {}: more than {SCAN_DIR_ENTRIES} entries; its actions are not used (the action catalog's limit)",
                dir.display()
            ),
        }
    }
    out
}

/// A valid `action` line as an `Action` (with `bin` and `builtin` unset).
fn parse_action(l: &Line) -> Option<Action> {
    if l.kind != "action" {
        return None;
    }
    let yes_no = |k: &str| match l.get(k)? {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    };
    let text = |k: &str| l.get(k).filter(|v| !v.is_empty()).map(str::to_owned);
    let input = text("in")?;
    // `join` is absent (= no), `yes` or `no`; anything else rejects the line.
    let is_join = match l.get("join") {
        None => false,
        Some(_) => yes_no("join")?,
    };
    let join = if is_join {
        let types = mime_set(&input)?;
        // `min` is the fewest accepted inputs a run needs; absent means 1.
        // Its ceiling is the invocation's input limit: a larger `min` could
        // never be met.
        let min = match l.get("min") {
            None => 1,
            // Plain canonical decimal only: no sign, no leading zero (`u32`
            // parsing alone takes `+1` and `01`).
            Some(v) if v.bytes().all(|b| b.is_ascii_digit()) && !v.starts_with('0') => {
                v.parse::<u32>().ok()?
            }
            Some(_) => return None,
        };
        if min == 0 || min as usize > action::INPUT_COUNT {
            return None;
        }
        Some(Join { types, min })
    } else if l.get("min").is_some() || input.contains(',') {
        // `min` and a MIME set are meaningful only for a join action.
        return None;
    } else {
        None
    };
    // A join action's `input` is the set's first member, for display only.
    let input = match &join {
        Some(j) => j.types[0].clone(),
        None => input,
    };
    Some(Action {
        id: l.bare_at(0).filter(|v| !v.is_empty())?.to_owned(),
        family: text("family")?,
        bin: PathBuf::new(),
        input,
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
        join,
        builtin: false,
    })
}

/// The most members an `in=` set may have. Derived from the formats the
/// roadmap names for a joined document (JPEG, PNG, GIF, TIFF, HEIC, WebP)
/// plus a document-and-assets role, both far below 16; the menu matches an
/// input against the set member by member, so the cost is linear in it.
/// Past it the line is rejected whole (a set that wide is a malformed line,
/// not an action we know).
const JOIN_TYPES_MAX: usize = 16;

/// A comma-separated MIME set: 1 to `JOIN_TYPES_MAX` members, none empty,
/// none repeated.
fn mime_set(v: &str) -> Option<Vec<String>> {
    let types: Vec<String> = v.split(',').map(str::to_owned).collect();
    if types.len() > JOIN_TYPES_MAX
        || types.iter().any(String::is_empty)
        || (1..types.len()).any(|i| types[..i].contains(&types[i]))
    {
        return None;
    }
    Some(types)
}

/// Why a manifest run failed.
enum Fail {
    /// Not the binary's fault (the refresh budget, the sandbox, the
    /// supervision, a system call): not cached, retried at the next start.
    Transient(String),
    /// The binary's own run failed (it hung past its own deadline, crashed,
    /// exited non-zero, left processes or its output open, or outlived
    /// SIGKILL): it backs off (see [`Backoff`]).
    Penalized(String),
    /// The manifest itself is broken (not UTF-8, too large, malformed):
    /// cached without actions until the binary changes.
    Deterministic(String),
}

/// The outcome of one manifest run, for the catalog.
enum Run {
    /// Its valid `action` lines; none for a broken manifest.
    Lines(Vec<Line>),
    /// A failure of the binary's own (message: why).
    Penalized(String),
    /// A transient failure: nothing new is cached, and the caller keeps the
    /// binary's old entry, if any.
    Transient,
}

/// Runs `<bin> --manifest` (messages on stderr, except for a penalized
/// failure, which the caller reports with its backoff).
fn run_manifest(bin: &Path, refresh_end: Instant) -> Run {
    match manifest_lines(bin, refresh_end) {
        Ok(lines) => Run::Lines(lines),
        Err(Fail::Deterministic(why)) => {
            eprintln!("inacircle: {}: {why}; skipped", bin.display());
            Run::Lines(Vec::new())
        }
        Err(Fail::Penalized(why)) => Run::Penalized(why),
        Err(Fail::Transient(why)) => {
            eprintln!("inacircle: {}: {why}; skipped, will retry", bin.display());
            Run::Transient
        }
    }
}

/// The deadline of one manifest: `DEADLINE`, but not past the refresh's end;
/// `None` when the refresh has no time left. The bool says whether it is
/// the step's own `DEADLINE` (true) or the refresh's end cut it short
/// (false): only a hang past its own deadline is the binary's fault.
fn step_deadline(now: Instant, refresh_end: Instant) -> Option<(Instant, bool)> {
    (now < refresh_end).then(|| {
        let own = now + DEADLINE;
        if own <= refresh_end {
            (own, true)
        } else {
            (refresh_end, false)
        }
    })
}

fn manifest_lines(bin: &Path, refresh_end: Instant) -> Result<Vec<Line>, Fail> {
    let (deadline, own) = step_deadline(Instant::now(), refresh_end)
        .ok_or_else(|| Fail::Transient("refresh budget spent".to_owned()))?;
    let out = capture(bin, deadline, own)?;
    let bad = |why: String| Fail::Deterministic(why);
    let text = String::from_utf8(out).map_err(|_| bad("manifest is not UTF-8".to_owned()))?;
    preflight(&text, &MANIFEST_SHAPE).map_err(|why| {
        bad(format!(
            "The action manifest exceeds its safety limits ({why}). This action binary was refused."
        ))
    })?;
    match manifest::parse(&text, MANIFEST_MAGIC) {
        Ok((0, lines)) => Ok(lines
            .into_iter()
            .filter(|l| {
                // Other kinds are ignored silently (forward compatibility).
                if l.kind != "action" || parse_action(l).is_some() {
                    return l.kind == "action";
                }
                eprintln!("inacircle: {}: invalid action line ignored", bin.display());
                false
            })
            .collect()),
        Ok((v, _)) => Err(bad(format!("unknown manifest version {v}"))),
        Err(e) => Err(bad(format!("bad manifest: line {}: {}", e.line, e.msg))),
    }
}

/// Stdout of `<bin> --manifest` within `deadline` and the size limit, run
/// in the sandbox of `run` but narrower: Landlock lets it read and execute
/// only the system directories and the binary, with no input, no writable
/// directory and a clean environment. Without the whole sandbox (see
/// [`sandbox::ready`](crate::sandbox::ready)) no manifest runs, built-in
/// or not (transient: a later start retries). See [`collect`] for how the
/// child's whole process group is finished before its output counts.
fn capture(bin: &Path, deadline: Instant, own: bool) -> Result<Vec<u8>, Fail> {
    let mut child = spawn_manifest(bin)?;
    collect(&mut child, deadline, own)
}

/// Starts `<bin> --manifest` in the manifest sandbox (see [`capture`]).
fn spawn_manifest(bin: &Path) -> Result<std::process::Child, Fail> {
    use crate::sandbox::{self, Limits, Policy};
    use std::os::unix::process::CommandExt;
    // Paths that do not exist are skipped by the ruleset builder.
    let mut exec: Vec<PathBuf> = sandbox::SYSTEM_EXEC_PATHS
        .iter()
        .map(PathBuf::from)
        .collect();
    exec.push(bin.to_path_buf());
    let policy = Policy {
        exec: &exec,
        read: &[],
        read_exact: &[],
        write: None,
    };
    let ruleset = sandbox::build(&policy)
        .map_err(|e| Fail::Transient(format!("sandbox unavailable: {e}")))?;
    // A manifest writes only to its stdout pipe, to which RLIMIT_FSIZE does
    // not apply, and may write no file: 0 makes any file write fail.
    let limits = Limits {
        fsize: 0,
        data: sandbox::data_cap(sandbox::total_ram()),
    };
    let fd = ruleset.as_raw_fd();
    let mut cmd = Command::new(bin);
    // No writable place, so no usable TMPDIR.
    sandbox::configure_child_env(&mut cmd, None);
    cmd.arg("--manifest")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // No `process_group(0)`: std applies it before pre_exec, and the
    // child's `setsid` in `apply_in_child` (which also makes it a group
    // leader, so `kill_group` still reaches its descendants) would fail.
    // The probe dies with us if we die abnormally (see `die_with_parent`).
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: the closure makes only the async-signal-safe calls of
    // `die_with_parent` and `apply_in_child`; the ruleset fd stays open
    // until spawn returns (`ruleset` is dropped only after the spawn).
    unsafe {
        cmd.pre_exec(move || {
            sandbox::die_with_parent(parent)?;
            sandbox::apply_in_child(fd, &limits)
        });
    }
    // Orphans of the manifest come back to us (see `exec::reap_group`).
    exec::become_subreaper().map_err(|e| Fail::Transient(format!("cannot supervise: {e}")))?;
    let child = cmd
        .spawn()
        .map_err(|e| Fail::Transient(format!("cannot run: {e}")))?;
    drop(ruleset);
    Ok(child)
}

/// Kills the child's whole process group. Only while the child is
/// unreaped: its pid, which is the group id, is then still its own.
fn kill_group(child: &std::process::Child) {
    // SAFETY: kill has no memory-safety preconditions. The child leads its own
    // process group (it calls `setsid` in `apply_in_child`; seccomp denies
    // leaving it) and is not reaped, so -pid addresses only its group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
}

/// Reads the child's stdout until the child exits, `deadline` passes, the
/// output passes `MAX_OUTPUT` or reading fails. Then kills its whole
/// group (the child still unreaped), reaps it, and reaps every member
/// until none is left; if one was alive at the kill, the manifest is
/// rejected. Then reads the pipe to EOF: the kernel's word that no
/// write end is open, so no descendant can add to the output any more.
/// Only then does the output count, if the child exited with success.
/// The cleanup has its own `exec::GROUP_GONE`; past it the manifest fails.
/// The child is always reaped unless it never exits. `own` says whether
/// `deadline` is the step's own (see [`step_deadline`]): a hang past it is
/// the binary's fault, one cut by the refresh's end is not. Every failure
/// of the run itself is [`Fail::Penalized`]; one of ours (a system call,
/// the budget) is [`Fail::Transient`].
fn collect(child: &mut std::process::Child, deadline: Instant, own: bool) -> Result<Vec<u8>, Fail> {
    let t = |s: &str| Fail::Transient(s.to_owned());
    let p = |s: &str| Fail::Penalized(s.to_owned());
    let pid = child.id() as libc::pid_t;
    let pidfd = exec::pidfd_open(pid);
    let Some(stdout) = child.stdout.take() else {
        kill_group(child);
        let until = Instant::now() + exec::GROUP_GONE;
        if exec::wait_exited(pid, pidfd.as_ref(), until) {
            let _ = child.wait();
            // Its killed members came back to us (subreaper): reaped too,
            // not left as zombies. The verdict is the same either way.
            let _ = exec::reap_group(pid, until);
        }
        return Err(t("no stdout"));
    };
    let fd = stdout.as_raw_fd();
    exec::set_nonblocking(fd);
    let mut out = Vec::new();
    let mut open = true;
    let mut result = Ok(());
    // While the child runs. Without a pidfd its exit is polled every
    // `NO_PIDFD_POLL`, like the action supervisor does.
    while result.is_ok() {
        if pidfd.is_none() && exec::exited_unreaped(pid) {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            result = Err(if own {
                p("timed out")
            } else {
                t("timed out (refresh budget spent)")
            });
            break;
        }
        let mut fds = Vec::with_capacity(2);
        if open {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(p) = &pidfd {
            fds.push(libc::pollfd {
                fd: p.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let wait = if pidfd.is_some() {
            left
        } else {
            left.min(exec::NO_PIDFD_POLL)
        };
        let ms = wait.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // SAFETY: fds is a valid array of fds.len() pollfds (possibly 0).
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                result = Err(t("poll failed"));
            }
            continue;
        }
        // The exit first: once it is seen, nothing more is read until the
        // group is gone.
        if pidfd.is_some() && fds.last().is_some_and(|p| p.revents != 0) {
            break;
        }
        if open && fds[0].revents != 0 {
            match read_some(fd, &mut out) {
                Ok(true) => {}
                Ok(false) => open = false,
                Err(e) => result = Err(e),
            }
        }
    }
    // The whole group is killed first, whatever happens next: the
    // unreaped leader keeps `pid` the manifest's group id, so the kill
    // cannot reach another group, and nothing of it outlives us even if
    // its members cannot be counted.
    kill_group(child);
    let until = Instant::now() + exec::GROUP_GONE;
    // Not exited: left unreaped (its group id stays its own), nothing
    // accepted (see `leader_failure`).
    if let Some(f) = leader_failure(exec::wait_for_exit(pid, pidfd.as_ref(), until)) {
        return Err(f);
    }
    let status = child.wait();
    // A descendant alive at the leader's exit rejects the manifest: its
    // output is not finished. The kernel's child list tells
    // (`exec::reap_group`, see `group_failure`).
    let left_behind = exec::reap_group(pid, until);
    // The reason the run itself failed (timed out, too large, unreadable)
    // is the cause; the descendants that outlived it are its consequence.
    // Either way nothing is accepted.
    result?;
    if let Some(f) = group_failure(left_behind) {
        return Err(f);
    }
    while open {
        let left = until.saturating_duration_since(Instant::now());
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // SAFETY: `pfd` is a valid pollfd and nfds is 1.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if r < 0 {
            return Err(t("poll failed"));
        }
        if r == 0 {
            return Err(p("output still open"));
        }
        match read_some(fd, &mut out) {
            Ok(true) => {}
            Ok(false) => open = false,
            Err(e) => return Err(e),
        }
    }
    drop(stdout);
    match status {
        Ok(s) if s.success() => Ok(out),
        Ok(s) => Err(Fail::Penalized(format!("exited with {s}"))),
        Err(_) => Err(t("wait failed")),
    }
}

/// The killed leader's exit wait (`exec::wait_for_exit`), as a failure.
/// Still running at the cleanup's deadline: stuck in the kernel after
/// SIGKILL, the binary's run (penalized). The last `waitid` failed: ours,
/// not the binary's (transient).
fn leader_failure(w: exec::Waited) -> Option<Fail> {
    match w {
        exec::Waited::Exited => None,
        exec::Waited::Running => Some(Fail::Penalized("did not exit after SIGKILL".to_owned())),
        exec::Waited::Unknown => Some(Fail::Transient("cannot tell if it exited".to_owned())),
    }
}

/// The group's reap (`exec::reap_group`), as a failure. A member alive at
/// the kill, or one still there at the cleanup's deadline (`TimedOut`):
/// the binary left processes behind (penalized). Any other error: the
/// child list could not be read, ours (transient).
fn group_failure(r: std::io::Result<bool>) -> Option<Fail> {
    match r {
        Ok(false) => None,
        Ok(true) => Some(Fail::Penalized("left processes behind".to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            Some(Fail::Penalized("left processes behind".to_owned()))
        }
        Err(_) => Some(Fail::Transient("cannot count its processes".to_owned())),
    }
}

/// One read from the non-blocking `fd` into `out`: true while open, false
/// at EOF; an error past `MAX_OUTPUT`.
fn read_some(fd: RawFd, out: &mut Vec<u8>) -> Result<bool, Fail> {
    let mut buf = [0u8; 4096];
    // SAFETY: `buf` is valid for `buf.len()` bytes; `fd` is open (owned by
    // the caller's ChildStdout, which outlives this call).
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        return match e.kind() {
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock => Ok(true),
            _ => Err(Fail::Transient("read failed".to_owned())),
        };
    }
    if n == 0 {
        return Ok(false);
    }
    out.extend_from_slice(&buf[..n as usize]);
    if out.len() > MAX_OUTPUT {
        return Err(Fail::Deterministic("manifest larger than 64 KB".to_owned()));
    }
    Ok(true)
}

/// The cache, read through its folder's descriptor: never through a
/// symlink, never blocking on a FIFO, only a regular file of this user.
/// Anything else is no cache.
fn read_cache(path: &Path) -> Option<Vec<Entry>> {
    let (dir, name) = (path.parent()?, path.file_name()?.to_str()?);
    let dir = match crate::journal::StateDir::open(dir) {
        Ok(d) => d,
        Err(e) => {
            // A folder that is a symlink is refused on purpose: no cache is
            // read or written. Say why once; any other failure is just no cache.
            if e.get_ref()
                .is_some_and(|i| i.is::<crate::journal::LinkRefused>())
            {
                // Straight to fd 2 (not `eprintln!`), so a test can capture it.
                let _ = std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!("inacircle: no action catalog cache: {e}.\n"),
                );
            }
            return None;
        }
    };
    let file = dir.open_file(name, libc::O_RDONLY, 0).ok()?;
    // The size `fstat` shows is a pre-filter; the bytes read are the
    // authority (a file may grow while it is read): at most max + 1.
    if file.metadata().ok()?.len() > CACHE_BYTES as u64 {
        cache_rejected("the cache file is too big");
        return None;
    }
    read_cache_from(file)
}

fn cache_rejected(why: &str) {
    eprintln!(
        "inacircle: The action catalog was rejected and will be rebuilt ({why}). Its actions were not silently discarded."
    );
}

/// The cache from an open reader, read bounded ([`CACHE_BYTES`] + 1 at
/// most, whatever the file showed when opened).
fn read_cache_from(file: impl std::io::Read) -> Option<Vec<Entry>> {
    let bytes = match action::read_bounded(file, CACHE_BYTES) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            cache_rejected("the cache file is too big");
            return None;
        }
        Err(_) => return None,
    };
    if let Err(why) = check_cache(&bytes) {
        cache_rejected(why);
        return None;
    }
    // Within the limits but not what `write_cache` writes (another
    // version, an unknown kind, a bad field): rejected as loudly.
    let entries = parse_cache(&bytes);
    if entries.is_none() {
        cache_rejected("the cache file is malformed");
    }
    entries
}

/// The cache's limits on the whole file (bytes, lines, records, fields,
/// bins, actions), counted without allocating. `Err` names the limit.
/// Non-UTF-8 or a bad header is not a limit: [`parse_cache`] says `None`.
fn check_cache(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > CACHE_BYTES {
        return Err("the cache file is too big");
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(());
    };
    preflight(text, &CACHE_SHAPE)?;
    let (mut bins, mut actions) = (0usize, 0usize);
    for l in text.lines().skip(1) {
        match l.split('\t').next() {
            Some("bin") => bins += 1,
            Some("action") => actions += 1,
            _ => {}
        }
    }
    if bins > CACHE_BINS {
        return Err("too many binaries");
    }
    if actions > CACHE_ACTIONS {
        return Err("too many actions");
    }
    Ok(())
}

/// The cache file's content as entries; `None` for anything that is not
/// exactly what `write_cache` writes (over a limit, not UTF-8, wrong
/// header or version, a line of an unknown kind or with a field missing or
/// malformed, an action before any bin). Pure: no file.
fn parse_cache(bytes: &[u8]) -> Option<Vec<Entry>> {
    check_cache(bytes).ok()?;
    let text = std::str::from_utf8(bytes).ok()?;
    let (CACHE_VERSION, lines) = manifest::parse(text, CACHE_MAGIC).ok()? else {
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
                backoff: match (l.get("fails"), l.get("retry")) {
                    (None, None) => None,
                    (Some(f), Some(r)) => Some(Backoff {
                        // 2: the digits of `BACKOFF_FAILS_MAX` (64).
                        fails: decimal(f, 2)
                            .filter(|f| (1..=u64::from(BACKOFF_FAILS_MAX)).contains(f))?
                            as u32,
                        retry: decimal(r, RETRY_DIGITS_MAX)?,
                    }),
                    _ => return None,
                },
            }),
            "action" => entries.last_mut()?.lines.push(l),
            _ => return None,
        }
    }
    Some(entries)
}

/// A plain canonical decimal of at most `digits` digits (no sign, space or
/// leading zero, as `to_string` writes it) that fits a `u64`.
fn decimal(v: &str, digits: usize) -> Option<u64> {
    if v.is_empty()
        || v.len() > digits
        || !v.bytes().all(|b| b.is_ascii_digit())
        || (v.len() > 1 && v.starts_with('0'))
    {
        return None;
    }
    v.parse().ok()
}

/// Fuzz entry point: the cache parser on arbitrary bytes. What it accepts
/// is written back as the same entries (when the text written fits the
/// limits), and every backoff it accepts obeys the retry rule at any time.
#[cfg(fuzzing)]
pub fn fuzz_read_cache(bytes: &[u8]) {
    let Some(entries) = parse_cache(bytes) else {
        return;
    };
    let text = cache_text(&entries);
    if check_cache(text.as_bytes()).is_ok() {
        assert!(parse_cache(text.as_bytes()).as_deref() == Some(&entries[..]));
    }
    for b in entries.iter().filter_map(|e| e.backoff) {
        assert!((1..=BACKOFF_FAILS_MAX).contains(&b.fails));
        let top = u64::MAX - BACKOFF_MAX;
        for now in [0, b.retry.saturating_sub(1), b.retry, top, u64::MAX] {
            let _ = b.due(now);
            let n = Backoff::after(b.fails, now);
            assert!(n.fails >= b.fails && n.fails <= BACKOFF_FAILS_MAX);
            // A fresh backoff is never due at once, nor further than a day.
            if now <= top {
                assert!(n.retry > now && n.retry - now <= BACKOFF_MAX && !n.due(now));
            }
        }
    }
}

/// The cache file's text for `entries`.
fn cache_text(entries: &[Entry]) -> String {
    let mut s = manifest::header(CACHE_MAGIC, CACHE_VERSION);
    for e in entries {
        s.push_str(&manifest::write_line(&bin_line(e)));
        for l in &e.lines {
            s.push_str(&manifest::write_line(l));
        }
    }
    s
}

/// Atomic (temp file + rename).
fn write_cache(path: &Path, entries: &[Entry]) {
    let s = cache_text(entries);
    // Any failure just means no cache. A private folder (opened once,
    // checked), a fresh temp file under a random name (exclusive, never
    // through a symlink), renamed over the cache relative to the folder.
    let _ = (|| -> std::io::Result<()> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        crate::journal::StateDir::create(dir)?.replace(name, s.as_bytes(), false)?;
        Ok(())
    })();
}

#[cfg(test)]
pub(crate) mod tests {

    /// Regression: a run without `INACIRCLE_ACTIONS_DIR` (other dirs, no
    /// binaries) wrote an empty catalog over the one the wheel reads from
    /// the cache, so its Markdown slice came up dim.
    #[test]
    fn each_set_of_action_dirs_has_its_own_cache() {
        use std::ffi::OsStr;
        assert_eq!(cache_name(None), "catalog");
        let a = cache_name(Some(OsStr::new("/a/actions")));
        let b = cache_name(Some(OsStr::new("/b/actions")));
        assert!(
            a.starts_with("catalog-") && a.len() == "catalog-".len() + 16,
            "{a}"
        );
        assert_ne!(a, b);
        assert_ne!(a, "catalog");
        assert_eq!(a, cache_name(Some(OsStr::new("/a/actions"))), "stable");
    }

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Serializes the catalog tests (they share nothing else, but keep
    /// timing-sensitive cache checks apart).
    static LOCK: Mutex<()> = Mutex::new(());

    /// An entry for the budget tests: a short binary path and `lines`.
    fn entry(lines: Vec<Line>) -> Entry {
        Entry {
            bin: Bin {
                path: PathBuf::from("/b"),
                mtime: 1,
                size: 1,
                builtin: false,
            },
            lines,
            backoff: None,
        }
    }

    fn action_line(items: usize, pad: usize) -> Line {
        let mut l = Line::new("action");
        for _ in 0..items {
            l = l.bare(&"a".repeat(pad));
        }
        l
    }

    /// Largest `n` in `0..=hi` for which `ok(n)` holds (`ok` is monotone).
    fn largest(hi: usize, ok: impl Fn(usize) -> bool) -> usize {
        let (mut lo, mut hi) = (0, hi);
        assert!(ok(lo));
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if ok(mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// The cleanup's verdicts on the killed leader and its group: a run that
    /// outlives SIGKILL or leaves members behind (also when they are still
    /// there at the deadline) is the binary's; a system call that fails is
    /// ours.
    #[test]
    fn cleanup_failures_are_penalized_or_transient() {
        let pen = |f: Option<Fail>, why: &str| matches!(f, Some(Fail::Penalized(w)) if w == why);
        let tra = |f: Option<Fail>| matches!(f, Some(Fail::Transient(_)));
        assert!(leader_failure(exec::Waited::Exited).is_none());
        assert!(pen(
            leader_failure(exec::Waited::Running),
            "did not exit after SIGKILL"
        ));
        assert!(tra(leader_failure(exec::Waited::Unknown)));

        let err = |k| std::io::Error::new(k, "x");
        assert!(group_failure(Ok(false)).is_none());
        assert!(pen(group_failure(Ok(true)), "left processes behind"));
        assert!(pen(
            group_failure(Err(err(std::io::ErrorKind::TimedOut))),
            "left processes behind"
        ));
        assert!(tra(group_failure(Err(std::io::Error::from_raw_os_error(
            libc::EINVAL
        )))));
        assert!(tra(group_failure(Err(err(std::io::ErrorKind::Other)))));
    }

    /// Every cache limit is exact: the value at the limit is taken, one
    /// more is refused, and a refusal takes nothing.
    #[test]
    fn cache_budget_limits_are_exact() {
        // Binaries: CACHE_BINS fit, the next one does not.
        let mut b = CacheBudget::new();
        for _ in 0..CACHE_BINS {
            b.take(&entry(vec![])).unwrap();
        }
        assert_eq!(b.bins, CACHE_BINS);
        assert!(b.take(&entry(vec![])).unwrap_err().contains("binaries"));
        assert_eq!(b.bins, CACHE_BINS, "a refusal takes nothing");

        // Actions, and the records total at its exact maximum.
        let mut b = CacheBudget::new();
        for _ in 0..CACHE_BINS - 1 {
            b.take(&entry(vec![])).unwrap();
        }
        b.take(&entry(vec![action_line(0, 0); CACHE_ACTIONS]))
            .unwrap();
        assert_eq!(b.bins + b.actions, CACHE_RECORDS);
        let mut b = CacheBudget::new();
        let over = entry(vec![action_line(0, 0); CACHE_ACTIONS + 1]);
        assert!(b.take(&over).unwrap_err().contains("actions"));
        assert_eq!((b.bins, b.actions), (0, 0));

        // Fields: the bin's 4 plus the items, exactly CACHE_TOTAL_FIELDS.
        let per = CACHE_FIELDS_PER_RECORD;
        let mut full = vec![action_line(per, 1); (CACHE_TOTAL_FIELDS - 4) / per];
        let rest = (CACHE_TOTAL_FIELDS - 4) % per;
        full.push(action_line(rest, 1));
        let mut b = CacheBudget::new();
        b.take(&entry(full.clone())).unwrap();
        assert_eq!(b.fields, CACHE_TOTAL_FIELDS);
        full.push(action_line(1, 1));
        let mut b = CacheBudget::new();
        assert!(b.take(&entry(full)).unwrap_err().contains("fields"));

        // The same with a backoff on the bin, whose line then has 6 fields:
        // the writer counts them as the reader does, so a cache written at
        // the limit is read back, and one field more is refused by both.
        let fields_with_backoff = |extra: usize| {
            let mut lines = vec![action_line(per, 1); (CACHE_TOTAL_FIELDS - 6) / per];
            lines.push(action_line((CACHE_TOTAL_FIELDS - 6) % per, 1));
            if extra > 0 {
                lines.push(action_line(extra, 1));
            }
            let mut e = entry(lines);
            e.backoff = Some(Backoff {
                fails: 3,
                retry: 1_000,
            });
            assert_eq!(bin_line(&e).items.len(), 6);
            let mut b = CacheBudget::new();
            (b.take(&e).map(|()| b.fields), cache_text(&[e]))
        };
        let (taken, text) = fields_with_backoff(0);
        assert_eq!(taken, Ok(CACHE_TOTAL_FIELDS));
        assert_eq!(check_cache(text.as_bytes()), Ok(()));
        assert!(parse_cache(text.as_bytes()).is_some());
        let (taken, text) = fields_with_backoff(1);
        assert!(taken.unwrap_err().contains("fields"));
        assert!(check_cache(text.as_bytes()).is_err());

        // Fields of one record: 32 fit, 33 do not.
        let mut b = CacheBudget::new();
        b.take(&entry(vec![action_line(per, 1)])).unwrap();
        let mut b = CacheBudget::new();
        let e = b.take(&entry(vec![action_line(per + 1, 1)])).unwrap_err();
        assert!(e.contains("too big"));

        // One line's bytes: the longest accepted line is exactly the limit.
        let fits = |n: usize| {
            CacheBudget::new()
                .take(&entry(vec![action_line(1, n)]))
                .is_ok()
        };
        let n = largest(CACHE_LINE_BYTES + 1, fits);
        let w = manifest::write_line(&action_line(1, n));
        assert_eq!(w.len() - 1, CACHE_LINE_BYTES);

        // A binary path at the line limit is accepted, one byte more is not.
        let long = |n: usize| {
            let mut e = entry(vec![]);
            e.bin.path = PathBuf::from("/".to_owned() + &"p".repeat(n));
            CacheBudget::new().take(&e).is_ok()
        };
        let n = largest(CACHE_LINE_BYTES + 1, long);
        let mut e = entry(vec![]);
        e.bin.path = PathBuf::from("/".to_owned() + &"p".repeat(n));
        assert_eq!(
            manifest::write_line(&bin_line(&e)).len() - 1,
            CACHE_LINE_BYTES
        );

        // Total bytes: filler lines, then the last line sized to land on
        // CACHE_BYTES exactly.
        let filler = vec![action_line(1, 20_000); 13];
        let fits = |n: usize| {
            let mut lines = filler.clone();
            lines.push(action_line(1, n));
            CacheBudget::new().take(&entry(lines)).is_ok()
        };
        let n = largest(20_000, fits);
        assert!(n > 0 && n < 20_000);
        let mut lines = filler;
        lines.push(action_line(1, n));
        let mut b = CacheBudget::new();
        b.take(&entry(lines)).unwrap();
        assert_eq!(b.bytes, CACHE_BYTES);
    }

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

    /// Installs an executable script without this process ever holding a
    /// writable fd on it. Other tests fork at any time; a child that
    /// inherited such an fd until its own exec would make our exec of the
    /// script fail with ETXTBSY. So `cp` (its own process) writes the file
    /// under a temporary name, which is then renamed into place.
    fn script(path: &Path, body: &str) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let src = std::env::temp_dir().join(format!("engine-cat-src-{}-{n}", std::process::id()));
        std::fs::write(&src, format!("#!/bin/sh\n{body}\n")).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        let tmp = path.with_file_name(format!(".{name}.tmp"));
        let ok = Command::new("cp").arg(&src).arg(&tmp).status().unwrap();
        assert!(ok.success());
        std::fs::remove_file(&src).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    /// Plays a supervisor for `a_killed_supervisor_takes_its_manifest_probe_with_it`
    /// (only with `INACIRCLE_PDEATH_BIN` set): starts the manifest probe
    /// with [`spawn_manifest`], prints `child <pid>`, then waits for it
    /// (killed first, or at most its 60 s).
    #[test]
    fn helper_manifest_pdeath() {
        use std::io::Write;
        let Some(bin) = std::env::var_os("INACIRCLE_PDEATH_BIN") else {
            return;
        };
        let mut c = spawn_manifest(Path::new(&bin))
            .ok()
            .expect("sandboxed spawn");
        let mut out = std::io::stdout();
        // On a line of its own: libtest has printed `test <name> ... `.
        writeln!(out, "\nchild {}", c.id()).unwrap();
        out.flush().unwrap();
        let _ = c.wait();
    }

    /// `spawn_manifest` wires `die_with_parent`: when the process that
    /// started the probe is killed with `SIGKILL`, the probe (here
    /// `exec sleep 60`, so its leader is that process) dies too.
    #[test]
    fn a_killed_supervisor_takes_its_manifest_probe_with_it() {
        use std::io::BufRead;
        use std::process::Stdio;
        let fix = Fix::new("pdeath");
        let bin = fix.root.join("probe");
        script(&bin, "exec sleep 60");
        let mut sup = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "catalog::tests::helper_manifest_pdeath",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("INACIRCLE_PDEATH_BIN", &bin)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let rd = std::io::BufReader::new(sup.stdout.take().unwrap());
        let pid: libc::pid_t = rd
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.strip_prefix("child ").map(|p| p.parse().unwrap()))
            .expect("the helper names the probe");
        sup.kill().unwrap();
        sup.wait().unwrap();
        // Whether `pid` has ended (gone, or a zombie), polled every 20 ms
        // for at most 10 s: a kill-on-death is immediate, the rest is
        // slack for a loaded machine.
        let ended = |wait: std::time::Duration| {
            let until = Instant::now() + wait;
            loop {
                // SAFETY: WNOHANG never blocks; not our child: ECHILD.
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|s| {
                        s.rsplit_once(')')
                            .map(|(_, r)| r.trim_start().chars().next())
                    });
                if !matches!(state, Some(Some(c)) if c != 'Z') {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let died = ended(std::time::Duration::from_secs(10));
        if !died {
            // SAFETY: kill has no memory preconditions; the pid is the
            // sleep just seen alive.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            assert!(ended(std::time::Duration::from_secs(10)));
        }
        assert!(died, "the probe outlived its supervisor");
    }

    /// Sets the mtime by path (no writable fd on an executable, see `script`).
    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap();
        let ts = libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        };
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c is a valid path; times points to two timespecs.
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [ts, ts].as_ptr(), 0) };
        assert_eq!(r, 0);
    }

    fn act(id: &str, label: &str) -> String {
        format!(
            "action\t{id}\tfamily=f\tin=text/plain\tout=text/markdown\text=md\tlabel={label}\tgroup=g\tcost=10\tlossy=no\tbatch=yes"
        )
    }

    fn manifest_script(path: &Path, lines: &[String]) {
        script(path, &manifest_body("", lines));
    }

    /// The shell body of a manifest: `pre` (shell), then the header and `lines`.
    fn manifest_body(pre: &str, lines: &[String]) -> String {
        let mut body = format!("[ \"$1\" = --manifest ] || exit 1\n{pre}\n");
        body.push_str("printf 'inacircle-manifest 0\\n'\n");
        for l in lines {
            body.push_str(&format!(
                "printf '%s\\n' '{}'\n",
                l.replace('\t', "'\"\t\"'")
            ));
        }
        body
    }

    // The sandbox is applied by the same `apply_in_child` and ruleset
    // builder as `run`, whose tests cover the network denial; denied writes
    // and reads here prove it is applied to the manifest too.
    #[test]
    fn manifest_cannot_write_outside() {
        if crate::sandbox::tests::abi_or_skip("manifest_cannot_write_outside", 1).is_none() {
            return;
        }
        let f = Fix::new("mf-write");
        let d = f.dir("a");
        let outside = f.root.join("outside");
        let pre = format!("echo x > '{}' 2>/dev/null", outside.display());
        script(&d.join("one"), &manifest_body(&pre, &[act("x.a", "A")]));
        let c = Catalog::load_from(&[(d, false)], &f.cache());
        assert!(c.get("x.a").is_some(), "the manifest itself still loads");
        assert!(!outside.exists(), "the manifest wrote outside the sandbox");
    }

    #[test]
    fn manifest_cannot_read_outside() {
        if crate::sandbox::tests::abi_or_skip("manifest_cannot_read_outside", 1).is_none() {
            return;
        }
        let f = Fix::new("mf-read");
        let d = f.dir("a");
        let secret = f.root.join("secret");
        std::fs::write(&secret, "topsecret-4711").unwrap();
        // The secret goes into a label, so any leak shows in the catalog.
        let body = format!(
            "[ \"$1\" = --manifest ] || exit 1\nprintf 'inacircle-manifest 0\\n'\n\
             printf 'action\\tx.s\\tfamily=f\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=%s\\tgroup=g\\tcost=10\\tlossy=no\\tbatch=yes\\n' \"$(cat '{}' 2>/dev/null)\"\n\
             printf 'action\\tx.a\\tfamily=f\\tin=text/plain\\tout=text/markdown\\text=md\\tlabel=A\\tgroup=g\\tcost=10\\tlossy=no\\tbatch=yes\\n'\n",
            secret.display()
        );
        script(&d.join("one"), &body);
        let c = Catalog::load_from(&[(d, false)], &f.cache());
        assert!(c.get("x.a").is_some(), "the manifest itself still loads");
        for a in &c.actions {
            assert!(!a.label.contains("topsecret"), "read outside: {a:?}");
        }
    }

    /// Every manifest, built-in or a plugin's, runs only behind the whole
    /// sandbox gate: no Landlock, ABI below the floor, a setup error or a
    /// privileged parent each keep it from running (its actions do not
    /// load). Positive control: the same manifest loads once the gate
    /// passes again (the refusal is transient, not cached).
    #[test]
    fn every_manifest_needs_the_sandbox_gate() {
        use crate::sandbox::tests::{GATE_FAILURES, abi_or_skip, inject_failure};
        if abi_or_skip("every_manifest_needs_the_sandbox_gate", 0).is_none() {
            return;
        }
        for builtin in [true, false] {
            for name in GATE_FAILURES {
                let f = Fix::new("mf-gate");
                let d = f.dir("a");
                manifest_script(&d.join("one"), &[act("x.a", "A")]);
                {
                    let _g = inject_failure(name);
                    let c = Catalog::load_from(&[(d.clone(), builtin)], &f.cache());
                    assert!(
                        c.get("x.a").is_none(),
                        "builtin={builtin}, {name}: the manifest ran"
                    );
                }
                let c = Catalog::load_from(&[(d, builtin)], &f.cache());
                assert!(c.get("x.a").is_some(), "builtin={builtin}, {name}: control");
            }
        }
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
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        assert_eq!(c.actions[0].requires, ["foo", "bar"]);
    }

    #[test]
    fn cache_reused_when_unchanged() {
        let f = Fix::new("reuse");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        let first = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(first.actions.len(), 1);
        assert!(f.cache().exists());
        // The manifest cannot leave a run marker (it is sandboxed): change
        // its output behind the cache's back instead (same size and mtime).
        let mtime = std::fs::metadata(d.join("one"))
            .unwrap()
            .modified()
            .unwrap();
        manifest_script(&d.join("one"), &[act("x.a", "B")]);
        set_mtime(&d.join("one"), mtime);
        let second = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(second.actions, first.actions, "the manifest ran again");
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
        set_mtime(&d.join("one"), t);
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

    /// Symlinks planted at the temp name the old code used and at the cache
    /// name itself are never followed: their targets' bytes stay, and the
    /// cache becomes a regular file of our own.
    #[test]
    fn cache_temp_and_final_symlinks_are_not_followed() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let f = Fix::new("cachesym");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        let sentinel = f.root.join("sentinel");
        std::fs::write(&sentinel, CACHE_OK).unwrap();
        let cache = f.cache();
        let dir = cache.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let tmp = dir.join(format!(".catalog.{}.tmp", std::process::id()));
        symlink(&sentinel, &tmp).unwrap();
        symlink(&sentinel, &cache).unwrap();
        let c = Catalog::load_from(&[(d, true)], &cache);
        assert_eq!(c.actions.len(), 1);
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), CACHE_OK);
        let m = std::fs::symlink_metadata(&cache).unwrap();
        assert!(m.file_type().is_file());
        assert_eq!(m.mode() & 0o777, 0o600);
        assert!(read_cache(&cache).is_some());
    }

    /// Runs `f` with stderr (fd 2) redirected to a file; returns what was
    /// written to it meanwhile.
    pub(crate) fn stderr_of(root: &Path, f: impl FnOnce()) -> String {
        use std::os::fd::AsRawFd;
        let file = root.join("stderr");
        let out = std::fs::File::create(&file).unwrap();
        // SAFETY: dup on the always-open fd 2; `saved` is checked below.
        let saved = unsafe { libc::dup(2) };
        assert!(saved >= 0);
        // SAFETY: both fds are open; fd 2 is restored right after `f`.
        assert_eq!(unsafe { libc::dup2(out.as_raw_fd(), 2) }, 2);
        f();
        // SAFETY: `saved` is the open copy made above.
        assert_eq!(unsafe { libc::dup2(saved, 2) }, 2);
        // SAFETY: `saved` is open and not used after this.
        unsafe { libc::close(saved) };
        std::fs::read_to_string(file).unwrap()
    }

    /// A cache folder that is a symlink is refused on purpose: the user is
    /// told in plain words, no cache is read or written through it, and the
    /// catalog is still built.
    #[test]
    fn a_symlinked_cache_folder_is_refused_with_a_plain_message() {
        use std::os::unix::fs::symlink;
        let f = Fix::new("cachedirlink");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        let real = f.dir("real-cache");
        std::fs::write(real.join("catalog"), CACHE_OK).unwrap();
        let cache = f.cache();
        std::fs::create_dir_all(cache.parent().unwrap().parent().unwrap()).unwrap();
        symlink(&real, cache.parent().unwrap()).unwrap();
        let mut c = None;
        let err = stderr_of(&f.root, || {
            c = Some(Catalog::load_from(&[(d, true)], &cache));
        });
        assert!(err.contains("symbolic link"), "{err}");
        assert!(err.contains("refused on purpose"), "{err}");
        assert_eq!(c.unwrap().actions.len(), 1);
        // Nothing read from the target, nothing written into it.
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 1);
        assert_eq!(
            std::fs::read_to_string(real.join("catalog")).unwrap(),
            CACHE_OK
        );
    }

    /// A FIFO at the cache's name must not block the wheel's read (a full
    /// screen overlay waiting on it would lock the desktop): no cache, and
    /// a full load still builds the catalog.
    #[test]
    fn a_fifo_in_place_of_the_cache_does_not_block() {
        use crate::sys::tests::{mkfifo, within};
        let f = Fix::new("cache-fifo");
        let d = f.dir("actions");
        manifest_script(&d.join("inacircle-one"), &[act("one.a", "A")]);
        let cache = f.cache();
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        mkfifo(&cache);
        let (dirs, c2) = (vec![(d, true)], cache.clone());
        assert!(within(5, move || Catalog::load_cached_from(&dirs, &c2)).is_none());
        let dirs = vec![(f.root.join("actions"), true)];
        let c = within(20, move || Catalog::load_from(&dirs, &cache));
        assert_eq!(c.actions.len(), 1);
    }

    const CACHE_OK: &str = "inacircle-catalog 1\nbin\tpath=/a/one\tmtime=5\tsize=7\tbuiltin=yes\n";

    #[test]
    fn parse_cache_reads_what_write_cache_writes() {
        let text = format!("{CACHE_OK}{}\n", act("x.a", "A"));
        let e = parse_cache(text.as_bytes()).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].bin.path, Path::new("/a/one"));
        assert_eq!(
            (e[0].bin.mtime, e[0].bin.size, e[0].bin.builtin),
            (5, 7, true)
        );
        assert_eq!(e[0].lines.len(), 1);
        assert_eq!(parse_cache(b"inacircle-catalog 1\n").unwrap().len(), 0);
    }

    #[test]
    fn parse_cache_refuses_truncated_and_bad_input() {
        // Cut mid-field and mid-header.
        assert!(parse_cache(&CACHE_OK.as_bytes()[..CACHE_OK.len() - 9]).is_none());
        assert!(parse_cache(b"inacircle-cat").is_none());
        assert!(parse_cache(b"").is_none());
        // Wrong magic, unsupported version, not UTF-8, an action before any
        // bin, an unknown line kind.
        assert!(parse_cache(b"other 0\n").is_none());
        assert!(parse_cache(b"inacircle-catalog 2\n").is_none());
        assert!(parse_cache(b"inacircle-catalog 0\n").is_none());
        assert!(parse_cache(b"inacircle-catalog 1\n\xff\n").is_none());
        let orphan = format!("inacircle-catalog 1\n{}\n", act("x.a", "A"));
        assert!(parse_cache(orphan.as_bytes()).is_none());
        assert!(parse_cache(b"inacircle-catalog 1\nzzz\n").is_none());
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
    fn step_deadline_takes_the_smaller() {
        let now = Instant::now();
        assert_eq!(
            step_deadline(now, now + Duration::from_secs(9)),
            Some((now + DEADLINE, true))
        );
        // Exactly the step's own deadline left: still its own.
        assert_eq!(
            step_deadline(now, now + DEADLINE),
            Some((now + DEADLINE, true))
        );
        let end = now + Duration::from_secs(1);
        assert_eq!(step_deadline(now, end), Some((end, false)));
        assert_eq!(step_deadline(now, now), None);
        assert_eq!(step_deadline(now + Duration::from_secs(1), now), None);
    }

    /// The pids still running a command line with any of `markers`, after
    /// a bounded wait for 3 empty samples in a row 50 ms apart (5 s at most).
    /// A process that was sent SIGKILL stays in /proc until the kernel has
    /// torn it down, which takes longer on a loaded machine; one that
    /// escaped the kill never goes, and shows in the result.
    fn left_after_kill(markers: &[&str]) -> Vec<u32> {
        let until = Instant::now() + Duration::from_secs(5);
        let (mut quiet, mut left) = (0, Vec::new());
        while quiet < 3 && Instant::now() < until {
            left = markers.iter().flat_map(|m| procs_with(m)).collect();
            quiet = if left.is_empty() { quiet + 1 } else { 0 };
            std::thread::sleep(Duration::from_millis(50));
        }
        if quiet >= 3 { Vec::new() } else { left }
    }

    /// Pids whose command line mentions `marker` (a unique fixture path).
    pub(crate) fn procs_with(marker: &str) -> Vec<u32> {
        let mut found = Vec::new();
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Ok(cmd) = std::fs::read(e.path().join("cmdline"))
                && String::from_utf8_lossy(&cmd).contains(marker)
            {
                found.push(pid);
            }
        }
        found
    }

    #[test]
    fn refresh_budget_leaves_no_child() {
        let f = Fix::new("budget");
        let d = f.dir("a");
        // Eight manifests of 1.5 s each: every one fits its own 2 s with
        // margin, six fit the 10 s budget, the seventh is cut while running
        // and the eighth finds the budget spent.
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        // A sleep duration unique to this process (1.50<pid> s), so the
        // sleep itself, not just its shell, can be found in /proc.
        let sleep_marker = format!("1.50{}", std::process::id());
        for (i, n) in names.iter().enumerate() {
            let pre = format!("sleep {sleep_marker}");
            let body = manifest_body(&pre, &[act(&format!("x.{n}"), "L")]);
            script(&d.join(format!("s{i}")), &body);
        }
        let dirs = [(d.clone(), true)];
        let t = Instant::now();
        let c = Catalog::load_from(&dirs, &f.cache());
        assert!(t.elapsed() < Duration::from_secs(11), "{:?}", t.elapsed());
        for n in &names[..6] {
            assert!(c.get(&format!("x.{n}")).is_some(), "x.{n} missing");
        }
        for n in &names[6..] {
            assert!(
                c.get(&format!("x.{n}")).is_none(),
                "x.{n} is in the catalog"
            );
        }
        // The sleep, not just the shell, must be gone: the shell's command
        // line holds the fixture dir (scripts run by path), the sleep's its
        // unique duration.
        let left = left_after_kill(&[&d.to_string_lossy(), &sleep_marker]);
        assert!(left.is_empty(), "children left alive: {left:?}");
        // Not cached: the next load retries them (each with its own 2 s).
        let c = Catalog::load_from(&dirs, &f.cache());
        for n in &names[6..] {
            assert!(c.get(&format!("x.{n}")).is_some(), "x.{n} was cached");
        }
    }

    /// [`script`] run by bash. Background jobs (`&`) need it in the
    /// sandbox: dash opens `/dev/null` as their stdin and, as Landlock does
    /// not grant it, never starts them; bash goes on without it.
    fn bash_script(path: &Path, body: &str) {
        script(
            path,
            &format!(
                "exec /bin/bash -c '{}' bash \"$@\"",
                body.replace('\'', "'\\''")
            ),
        );
    }

    /// Pids whose command line is exactly `argv`. Exact, not a substring:
    /// a `bash -c '<script>'` command line carries every token of the
    /// script, so only the process that runs `argv` itself matches.
    fn procs_running(argv: &[&str]) -> Vec<u32> {
        let mut want = Vec::new();
        for a in argv {
            want.extend_from_slice(a.as_bytes());
            want.push(0);
        }
        let mut found = Vec::new();
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == want) {
                found.push(pid);
            }
        }
        found
    }

    /// Whether a process running exactly `argv` was seen, sampled every
    /// `PERIOD` for `span` on another thread while the load runs: the
    /// positive control that a fixture's descendant did start.
    pub(crate) fn seen_running(argv: &[&str], span: Duration) -> std::thread::JoinHandle<bool> {
        /// Often enough to see processes that live for a second.
        const PERIOD: Duration = Duration::from_millis(20);
        let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
        std::thread::spawn(move || {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let end = Instant::now() + span;
            while Instant::now() < end {
                if !procs_running(&argv).is_empty() {
                    return true;
                }
                std::thread::sleep(PERIOD);
            }
            false
        })
    }

    /// Samples every `PERIOD` until a process running exactly each of the
    /// `argvs` has been seen (not necessarily at the same moment), then
    /// creates `marker` for the fixture to proceed on and returns true; false
    /// if `cap` passes first. The wait is on the condition, not on a fixed
    /// window, so a loaded machine only makes it longer, up to `cap`.
    pub(crate) fn seen_all_running(
        argvs: Vec<Vec<String>>,
        marker: std::path::PathBuf,
        cap: Duration,
    ) -> std::thread::JoinHandle<bool> {
        /// As for `seen_running`.
        const PERIOD: Duration = Duration::from_millis(10);
        std::thread::spawn(move || {
            let mut pending = argvs;
            let end = Instant::now() + cap;
            while Instant::now() < end {
                pending.retain(|a| {
                    let a: Vec<&str> = a.iter().map(String::as_str).collect();
                    procs_running(&a).is_empty()
                });
                if pending.is_empty() {
                    return std::fs::write(&marker, b"").is_ok();
                }
                std::thread::sleep(PERIOD);
            }
            false
        })
    }

    /// Shell that waits for `up` to exist, at most 1000 x 10 ms (10 s, past
    /// the manifest deadline: a broken sampler ends the fixture, never
    /// leaves it waiting for good).
    fn hold_until(up: &Path) -> String {
        format!(
            "n=0; while [ ! -e '{}' ] && [ $n -lt 1000 ]; do sleep 0.01; n=$((n+1)); done",
            up.display()
        )
    }

    /// A manifest's descendants: one holds the stdout pipe, writes an
    /// early action line at once and would write a late one after the
    /// leader's exit (a margin after it, by which the engine has seen the
    /// exit); one writes a line, closes the pipe and lingers. Both are
    /// alive at the leader's exit, so the whole manifest is rejected, and
    /// neither survives. Their own lines cannot show through a rejected
    /// manifest, so the proof that they ran is the processes themselves,
    /// seen while the load runs; `manifest_descendant_waited_for_is_kept`
    /// is the same fixture with a `wait`, accepted with both lines. Fixture
    /// processes are found by a token unique to this test in their command
    /// line, which a reused pid would not carry.
    #[test]
    fn manifest_descendant_writer_outlives_leader() {
        let f = Fix::new("mf-desc");
        let d = f.dir("a");
        let token = format!("51.{}{}", std::process::id(), line!());
        // The writer's wait: about 1 s, unique to this test, so the sleep
        // it runs is its own visible proof.
        let wait = format!("1.0{}{}", std::process::id(), line!());
        let line = |id: &str| act(id, "L").replace('\t', "\\t");
        let (early, lingers, late) = (line("x.early"), line("x.lingers"), line("x.late"));
        let up = f.dir("marker").join("up");
        let body = format!(
            "{}\n\
             {{ printf '{lingers}\\n'; exec sleep {token} >&- 2>&-; }} &\n\
             {{ printf '{early}\\n'; sleep {wait}; printf '{late}\\n'; }} &\n\
             {hold}",
            manifest_body("", &[act("x.a", "A")]),
            hold = hold_until(&up)
        );
        bash_script(&d.join("one"), &body);
        // The leader waits for the sampler to have seen both descendants,
        // so the exit meets them however slow the machine is.
        let seen = seen_all_running(
            vec![
                vec!["sleep".to_owned(), token.clone()],
                vec!["sleep".to_owned(), wait.clone()],
            ],
            up,
            Duration::from_secs(20),
        );
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert!(seen.join().unwrap(), "a descendant never ran");
        for id in ["x.a", "x.early", "x.lingers", "x.late"] {
            assert!(c.get(id).is_none(), "{id} was taken");
        }
        // Leaving processes behind is the binary's own fault: it backs off.
        let cached = read_cache(&f.cache()).expect("a cache");
        let e = cached.iter().find(|e| e.bin.path == d.join("one"));
        assert_eq!(e.and_then(|e| e.backoff).map(|b| b.fails), Some(1));
        let left = left_after_kill(&[&token, &wait]);
        assert!(left.is_empty(), "descendants left alive: {left:?}");
    }

    /// The positive control of the test above: a background job the
    /// manifest waits for is finished at its exit, and its line counts.
    #[test]
    fn manifest_descendant_waited_for_is_kept() {
        let f = Fix::new("mf-desc-wait");
        let d = f.dir("a");
        let early = act("x.early", "L").replace('\t', "\\t");
        let body = format!(
            "{}\n{{ printf '{early}\\n'; }} &\nwait",
            manifest_body("", &[act("x.a", "A")])
        );
        bash_script(&d.join("one"), &body);
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(c.get("x.a").is_some());
        assert!(c.get("x.early").is_some(), "the background job never ran");
    }

    /// A manifest that hangs past its deadline (`sleep {token}`), with a
    /// descendant that ignores SIGTERM (`sleep {token}0`, its own argv);
    /// `with` false leaves the descendant out. Returns whether the
    /// descendant was seen running and the catalog.
    fn hang_with_descendant(f: &Fix, token: &str, with: bool) -> (bool, Catalog) {
        let d = f.dir("a");
        let dtoken = format!("{token}0");
        let bg = if with {
            format!("(trap '' TERM; exec sleep {dtoken}) &\n")
        } else {
            String::new()
        };
        let pre = format!("{bg}sleep {token}");
        bash_script(&d.join("hang"), &manifest_body(&pre, &[act("x.a", "A")]));
        // From before the run to a second past the kill (at DEADLINE): the
        // descendant lives from its start to the kill, so however late it
        // starts on a loaded machine, a sample falls in that time.
        let seen = seen_running(&["sleep", &dtoken], DEADLINE + Duration::from_secs(1));
        let t = Instant::now();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(
            t.elapsed() < DEADLINE + Duration::from_secs(1),
            "{:?}",
            t.elapsed()
        );
        assert!(c.get("x.a").is_none());
        let left = left_after_kill(&[token]);
        assert!(left.is_empty(), "left alive: {left:?}");
        (seen.join().unwrap(), c)
    }

    /// A manifest that hangs past its deadline, with a descendant that
    /// ignores SIGTERM: the group is killed and nothing of it survives.
    /// The descendant is seen by its own exact command line.
    #[test]
    fn manifest_descendant_reaped_on_timeout() {
        let f = Fix::new("mf-desc-hang");
        let token = format!("52.{}{}", std::process::id(), line!());
        let (seen, _) = hang_with_descendant(&f, &token, true);
        assert!(seen, "the descendant never ran");
    }

    /// The control's own control: without the descendant, the leader and
    /// its foreground sleep (which carry the token too) are not mistaken
    /// for it.
    #[test]
    fn manifest_timeout_control_needs_the_descendant() {
        let f = Fix::new("mf-desc-hang-none");
        let token = format!("53.{}{}", std::process::id(), line!());
        let (seen, _) = hang_with_descendant(&f, &token, false);
        assert!(!seen, "a descendant was seen where none runs");
    }

    /// A plain child in its own group (as the sandboxed one is by `setsid`)
    /// running `body` under bash, once `ready` exists; for `collect`.
    fn spawn_plain(f: &Fix, body: &str) -> (std::process::Child, PathBuf) {
        use std::os::unix::process::CommandExt;
        let ready = f.dir("plain").join("ready");
        crate::exec::become_subreaper().unwrap();
        let child = Command::new("/bin/bash")
            .process_group(0)
            .arg("-c")
            .arg(body)
            .arg("bash")
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        (child, ready)
    }

    /// Waits (bounded by `cap`) for `ready` to exist: the fixture's
    /// descendant is running, so the deadline given to `collect` after it
    /// cannot expire before the fixture is in place, however loaded the
    /// machine is.
    fn wait_for(ready: &Path, cap: Duration) {
        let end = Instant::now() + cap;
        while !ready.exists() {
            assert!(Instant::now() < end, "the fixture never got ready");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The manifest's output limit is exact: `MAX_OUTPUT` bytes are read
    /// whole, one more is refused (deterministically: the binary is broken,
    /// a retry will not help).
    #[test]
    fn manifest_output_limit_is_exact() {
        use std::os::unix::process::CommandExt;
        let run = |n: usize| {
            let mut c = Command::new("/bin/head")
                .process_group(0)
                .args(["-c", &n.to_string(), "/dev/zero"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            collect(&mut c, Instant::now() + Duration::from_secs(20), true)
        };
        crate::exec::become_subreaper().unwrap();
        assert_eq!(run(MAX_OUTPUT).ok().map(|o| o.len()), Some(MAX_OUTPUT));
        assert!(matches!(
            run(MAX_OUTPUT + 1),
            Err(Fail::Deterministic(m)) if m == "manifest larger than 64 KB"
        ));
    }

    /// A failure's message, for test output.
    fn why(r: Result<Vec<u8>, Fail>) -> Option<String> {
        r.err().map(|e| match e {
            Fail::Transient(m) | Fail::Penalized(m) | Fail::Deterministic(m) => m,
        })
    }

    /// A manifest that outlives its deadline and has a descendant fails as
    /// "timed out", the cause, not as "left processes behind", its
    /// consequence. Past its own deadline that is its fault (penalized);
    /// cut by the refresh's end, it is not (transient).
    #[test]
    fn manifest_timeout_with_a_descendant_says_timed_out() {
        let f = Fix::new("mf-timeout-reason");
        for own in [true, false] {
            let token = format!("55.{}{}{own}", std::process::id(), line!());
            let body = format!("sleep {token} & touch \"$1\"; exec sleep 60");
            let (mut child, ready) = spawn_plain(&f, &body);
            wait_for(&ready, Duration::from_secs(20));
            let _ = std::fs::remove_file(&ready);
            let r = collect(&mut child, Instant::now() + Duration::from_millis(200), own);
            let ok = match &r {
                Err(Fail::Penalized(m)) => own && m == "timed out",
                Err(Fail::Transient(m)) => !own && m == "timed out (refresh budget spent)",
                _ => false,
            };
            assert!(ok, "own={own}: {:?}", why(r));
            assert!(left_after_kill(&[&token]).is_empty());
        }
    }

    /// The early "no stdout" exit also reaps the killed members, so none
    /// stays a zombie of ours until the process ends.
    #[test]
    fn manifest_without_stdout_reaps_its_members() {
        let f = Fix::new("mf-nostdout");
        let token = format!("56.{}{}", std::process::id(), line!());
        let body = format!("sleep {token} & touch \"$1\"; exec sleep 60");
        let (mut child, ready) = spawn_plain(&f, &body);
        wait_for(&ready, Duration::from_secs(20));
        let pgid = child.id() as libc::pid_t;
        drop(child.stdout.take());
        let r = collect(&mut child, Instant::now() + Duration::from_secs(5), true);
        assert!(matches!(&r, Err(Fail::Transient(m)) if m == "no stdout"));
        // A member left unreaped would show up as a child of the group
        // within moments of its reparenting; none may, for a bounded look.
        let end = Instant::now() + Duration::from_millis(300);
        while Instant::now() < end {
            // SAFETY: waitpid with WNOHANG and a null status is plain.
            let w = unsafe { libc::waitpid(-pgid, std::ptr::null_mut(), libc::WNOHANG) };
            assert!(w <= 0, "an unreaped member {w}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A child list that cannot be read (an injected `waitid` error)
    /// rejects the manifest; the same manifest loads once it can be.
    #[test]
    fn manifest_with_an_unreadable_child_list_is_rejected() {
        let f = Fix::new("mf-waitid");
        let d = f.dir("a");
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        crate::exec::tests::WAITID_ERRNO.set(Some(libc::EINVAL));
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        crate::exec::tests::WAITID_ERRNO.set(None);
        assert!(c.get("x.a").is_none(), "taken without the child list");
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(c.get("x.a").is_some(), "control");
    }

    /// The group is killed before its members are counted: even when
    /// they cannot be counted (an injected `waitid` error), a manifest's
    /// lingering descendant does not survive the load. It is seen running
    /// first, by its exact command line.
    #[test]
    fn manifest_with_an_unreadable_child_list_still_kills_the_group() {
        let f = Fix::new("mf-waitid-kill");
        let d = f.dir("a");
        let token = format!("54.{}{}", std::process::id(), line!());
        let up = f.dir("marker").join("up");
        let body = format!(
            "{}\n{{ exec sleep {token} >&- 2>&-; }} &\n{hold}",
            manifest_body("", &[act("x.a", "A")]),
            hold = hold_until(&up)
        );
        bash_script(&d.join("one"), &body);
        let seen = seen_all_running(
            vec![vec!["sleep".to_owned(), token.clone()]],
            up,
            Duration::from_secs(20),
        );
        crate::exec::tests::WAITID_ERRNO.set(Some(libc::EINVAL));
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        crate::exec::tests::WAITID_ERRNO.set(None);
        assert!(seen.join().unwrap(), "the descendant never ran");
        assert!(c.get("x.a").is_none());
        let left = left_after_kill(&[&token]);
        assert!(left.is_empty(), "survived the load: {left:?}");
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

    /// The wheel's load: from the cache alone, never running a manifest.
    /// Stale entries are used as cached while their binary is still there;
    /// a binary the cache does not know is left out; no cache, no catalog.
    #[test]
    fn cached_load_never_runs_a_manifest() {
        let f = Fix::new("cached");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        manifest_script(&d.join("one"), &[act("x.a", "A")]);
        assert_eq!(Catalog::load_cached_from(&dirs, &f.cache()), None);
        manifest_script(&d.join("two"), &[act("y.a", "Y")]);
        Catalog::load_from(&dirs, &f.cache());
        let cache = std::fs::read(f.cache()).unwrap();
        // Changed since it was cached: used as cached.
        manifest_script(&d.join("one"), &[act("x.a", "B")]);
        let t = std::fs::metadata(d.join("one"))
            .unwrap()
            .modified()
            .unwrap();
        set_mtime(&d.join("one"), t + Duration::from_secs(5));
        // Gone: not offered.
        std::fs::remove_file(d.join("two")).unwrap();
        // New, and its manifest would hang: not run.
        let marker = f.root.join("marker");
        script(
            &d.join("three"),
            &format!("echo run >> {}\nexec sleep 10", marker.display()),
        );
        let t = Instant::now();
        let c = Catalog::load_cached_from(&dirs, &f.cache()).expect("a cache");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        let ids: Vec<_> = c
            .actions
            .iter()
            .map(|a| (a.id.as_str(), a.label.as_str()))
            .collect();
        assert_eq!(ids, [("x.a", "A")]);
        assert!(c.actions[0].builtin);
        assert!(!marker.exists(), "a manifest ran");
        assert_eq!(
            std::fs::read(f.cache()).unwrap(),
            cache,
            "the cache is not rewritten"
        );
        // A corrupt cache is no cache.
        std::fs::write(f.cache(), "garbage\n").unwrap();
        assert_eq!(Catalog::load_cached_from(&dirs, &f.cache()), None);
    }

    #[test]
    fn duplicate_id_first_dir_wins() {
        let f = Fix::new("dup");
        let (d1, d2) = (f.dir("a"), f.dir("b"));
        manifest_script(&d1.join("one"), &[act("x.a", "first")]);
        manifest_script(&d2.join("two"), &[act("x.a", "second"), act("x.b", "B")]);
        let c = Catalog::load_from(&[(d1, true), (d2, true)], &f.cache());
        assert_eq!(c.actions.len(), 2);
        assert_eq!(c.get("x.a").unwrap().label, "first");
        assert!(c.get("x.b").is_some());
    }

    /// A failed run is retried once its backoff is due (here, from the
    /// start: a system failure), a broken manifest never until it changes.
    #[test]
    fn failed_run_retried_deterministic_cached() {
        let f = Fix::new("transient");
        let d = f.dir("a");
        // A manifest cannot leave a run marker (it is sandboxed), so reruns
        // are told by a changed outcome: `a-fails` exits 1 now (it backs
        // off) and is fixed after the first load; `b-garbage` has a
        // malformed header and is fixed after it with the same size and
        // mtime.
        let good_a = manifest_body("", &[act("y.a", "Y")]);
        // Same size as the good body: a wrongly cached failure would then
        // stay cached after the fix (size and mtime unchanged).
        let failing = {
            let head = "exit 1\n#";
            format!("{head}{}", "x".repeat(good_a.len() - head.len()))
        };
        script(&d.join("a-fails"), &failing);
        let mtime_a = std::fs::metadata(d.join("a-fails"))
            .unwrap()
            .modified()
            .unwrap();

        let good_b = manifest_body("", &[act("x.b", "B")]);
        let garbage = good_b.replace("inacircle-manifest 0", "inacircle-garbage-00");
        script(&d.join("b-garbage"), &garbage);
        manifest_script(&d.join("c-good"), &[act("x.a", "A")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.actions.len(), 1);
        script(&d.join("a-fails"), &good_a);
        set_mtime(&d.join("a-fails"), mtime_a);
        let mtime = std::fs::metadata(d.join("b-garbage"))
            .unwrap()
            .modified()
            .unwrap();
        script(&d.join("b-garbage"), &good_b);
        set_mtime(&d.join("b-garbage"), mtime);
        let due = wall_secs() + BACKOFF_FIRST;
        let c = Catalog::load_with(&[(d.clone(), true)], Some(&f.cache()), due);
        assert!(
            c.get("y.a").is_some(),
            "the failed run was not retried once due"
        );
        assert!(
            c.get("x.b").is_none(),
            "the deterministic failure was rerun"
        );
        assert!(c.get("x.a").is_some());
    }

    #[test]
    fn wall_secs_is_the_unix_clock() {
        // 2020-01-01 is long past; a constant 0 or 1 is not the clock.
        assert!(wall_secs() > 1_577_836_800);
    }

    #[test]
    fn decimal_enforces_the_digit_limit() {
        assert_eq!(decimal("99", 2), Some(99));
        assert_eq!(decimal("100", 2), None, "three digits past a limit of two");
        assert_eq!(decimal("", 2), None);
        assert_eq!(decimal("01", 2), None);
    }

    #[test]
    fn transient_rerun_failure_keeps_the_old_entry() {
        let f = Fix::new("keep-old");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        manifest_script(&d.join("tool"), &[act("x.a", "old")]);
        manifest_script(&d.join("other"), &[act("y.a", "Y")]);
        let c = Catalog::load_from(&dirs, &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "old");
        // The binary changes and its manifest now fails (exit 1 is a
        // failure of its run, not a broken manifest). A new binary that
        // fails the same way has no old entry and stays out.
        script(&d.join("tool"), "exit 1");
        script(&d.join("fresh"), "exit 1");
        let c = Catalog::load_from(&dirs, &f.cache());
        assert_eq!(c.get("x.a").map(|a| a.label.as_str()), Some("old"));
        assert!(c.get("y.a").is_some());
        // The wheel reads the cache alone and still offers the family.
        let w = Catalog::load_cached_from(&dirs, &f.cache()).expect("a cache");
        assert_eq!(w.get("x.a").map(|a| a.label.as_str()), Some("old"));
        assert_eq!(w.actions.len(), 2);
        // It backs off under its new identity; once changed again, the next
        // start reruns it at once.
        manifest_script(&d.join("tool"), &[act("x.a", "new")]);
        let c = Catalog::load_from(&dirs, &f.cache());
        assert_eq!(c.get("x.a").unwrap().label, "new");
    }

    // ---- negative cache (backoff) ----

    /// The clock of the backoff tests: only the distance from it matters.
    const BASE: u64 = 1_700_000_000;

    fn load_at(dirs: &[(PathBuf, bool)], f: &Fix, now: u64) -> Catalog {
        Catalog::load_with(dirs, Some(&f.cache()), now)
    }

    /// Whether a load at `now` ran a hanging manifest: such a run takes the
    /// whole `DEADLINE`; a load that reruns nothing takes milliseconds.
    fn hung(dirs: &[(PathBuf, bool)], f: &Fix, now: u64) -> bool {
        let t = Instant::now();
        load_at(dirs, f, now);
        t.elapsed() >= DEADLINE
    }

    /// The cached backoff of `bin`: `None` without an entry, `Some(None)`
    /// for an entry without one.
    fn backoff_of(f: &Fix, bin: &Path) -> Option<Option<(u32, u64)>> {
        let entries = read_cache(&f.cache()).expect("a cache");
        entries
            .iter()
            .find(|e| e.bin.path == bin)
            .map(|e| e.backoff.as_ref().map(|b| (b.fails, b.retry)))
    }

    /// `body` padded with a trailing comment to exactly `len` bytes.
    fn padded(body: &str, len: usize) -> String {
        let s = format!("{body}\n#");
        assert!(s.len() <= len, "{body:?} is longer than {len}");
        format!("{s}{}", "x".repeat(len - s.len()))
    }

    /// Replaces the script at `path` by `body` (the same size) and keeps its
    /// mtime: to the catalog it is the same binary. A manifest cannot leave
    /// a run marker (it is sandboxed), so a rerun shows as a new outcome.
    fn swap(path: &Path, body: &str) {
        let m = std::fs::metadata(path).unwrap();
        script(path, body);
        assert_eq!(std::fs::metadata(path).unwrap().len(), m.len());
        set_mtime(path, m.modified().unwrap());
    }

    /// A manifest that hangs is not rerun until its retry time; at it
    /// (`now == retry`), it is, and the wait doubles.
    #[test]
    fn hanging_manifest_is_not_rerun_until_retry() {
        let f = Fix::new("bo-hang");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("hang");
        script(&bin, "exec sleep 10");
        assert!(hung(&dirs, &f, BASE), "the first load did not run it");
        assert_eq!(backoff_of(&f, &bin), Some(Some((1, BASE + 60))));
        assert!(!hung(&dirs, &f, BASE + 30), "rerun during the backoff");
        assert!(!hung(&dirs, &f, BASE + 59), "rerun during the backoff");
        assert!(hung(&dirs, &f, BASE + 60), "not rerun once due");
        assert_eq!(backoff_of(&f, &bin), Some(Some((2, BASE + 60 + 120))));
        let left = left_after_kill(&[&d.to_string_lossy()]);
        assert!(left.is_empty(), "left alive: {left:?}");
    }

    /// Each failure in a row doubles the wait from 60 s up to a day; the
    /// counter stops at 64, the wait stays a day.
    #[test]
    fn backoff_doubles_up_to_a_day() {
        let f = Fix::new("bo-double");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("fails");
        script(&bin, "exit 3");
        let (mut now, mut waits) = (BASE, Vec::new());
        for k in 1..=70u32 {
            load_at(&dirs, &f, now);
            let (fails, retry) = backoff_of(&f, &bin).unwrap().expect("a backoff");
            assert_eq!(fails, k.min(64), "failure {k}");
            waits.push(retry - now);
            now = retry;
        }
        assert_eq!(
            waits[..13],
            [
                60, 120, 240, 480, 960, 1920, 3840, 7680, 15360, 30720, 61440, 86400, 86400
            ]
        );
        assert!(waits[13..].iter().all(|&w| w == 86_400), "{waits:?}");
    }

    /// A binary that recovers (same identity, other behavior) is taken at
    /// its retry time, and its `bin` line loses the backoff.
    #[test]
    fn success_clears_backoff() {
        let f = Fix::new("bo-clear");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("tool");
        let good = manifest_body("", &[act("x.a", "A")]);
        script(&bin, &padded("exec sleep 10", good.len()));
        let c = load_at(&dirs, &f, BASE);
        assert!(c.get("x.a").is_none());
        assert_eq!(backoff_of(&f, &bin), Some(Some((1, BASE + 60))));
        swap(&bin, &good);
        let c = load_at(&dirs, &f, BASE + 30);
        assert!(c.get("x.a").is_none(), "rerun during the backoff");
        let c = load_at(&dirs, &f, BASE + 60);
        assert!(c.get("x.a").is_some(), "not rerun once due");
        assert_eq!(backoff_of(&f, &bin), Some(None));
        let text = std::fs::read_to_string(f.cache()).unwrap();
        assert!(
            !text.contains("fails=") && !text.contains("retry="),
            "{text}"
        );
        let w = Catalog::load_cached_from(&dirs, &f.cache()).expect("a cache");
        assert!(w.get("x.a").is_some());
    }

    /// A binary that changes during its backoff runs at once, and its
    /// failures count from zero again.
    #[test]
    fn changed_binary_retries_at_once() {
        let f = Fix::new("bo-change");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("tool");
        script(&bin, "exit 3");
        load_at(&dirs, &f, BASE);
        load_at(&dirs, &f, BASE + 60);
        assert_eq!(backoff_of(&f, &bin), Some(Some((2, BASE + 180))));
        script(&bin, "exit 4 # changed");
        load_at(&dirs, &f, BASE + 61);
        assert_eq!(
            backoff_of(&f, &bin),
            Some(Some((1, BASE + 121))),
            "not rerun at once, or the old count was kept"
        );
        manifest_script(&bin, &[act("x.a", "A")]);
        let c = load_at(&dirs, &f, BASE + 62);
        assert!(c.get("x.a").is_some(), "not rerun at once");
        assert_eq!(backoff_of(&f, &bin), Some(None));
    }

    /// A binary that worked and hangs after an update keeps its old actions
    /// on the wheel while it backs off, under its new identity.
    #[test]
    fn old_actions_stay_during_backoff() {
        let f = Fix::new("bo-old");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("tool");
        manifest_script(&bin, &[act("x.a", "old")]);
        assert!(load_at(&dirs, &f, BASE).get("x.a").is_some());
        script(&bin, "exec sleep 10");
        let c = load_at(&dirs, &f, BASE + 1);
        assert_eq!(c.get("x.a").map(|a| a.label.as_str()), Some("old"));
        assert_eq!(backoff_of(&f, &bin), Some(Some((1, BASE + 61))));
        let size = std::fs::metadata(&bin).unwrap().len();
        let cached = read_cache(&f.cache()).unwrap();
        assert_eq!(
            cached[0].bin.size, size,
            "not the binary's current identity"
        );
        let w = Catalog::load_cached_from(&dirs, &f.cache()).expect("a cache");
        assert_eq!(w.get("x.a").map(|a| a.label.as_str()), Some("old"));
        let t = Instant::now();
        let c = load_at(&dirs, &f, BASE + 30);
        assert!(t.elapsed() < DEADLINE, "rerun during the backoff");
        assert_eq!(c.get("x.a").map(|a| a.label.as_str()), Some("old"));
    }

    /// The refresh budget is not the binary's fault: five hanging binaries
    /// and a good one after them. The first four fill their own 2 s and back
    /// off; the fifth is cut by the budget's end and the good one finds it
    /// spent: neither backs off, and the next start runs both.
    #[test]
    fn budget_cut_is_not_penalized() {
        let f = Fix::new("bo-budget");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let hangs: Vec<PathBuf> = (1..=5).map(|i| d.join(format!("a{i}"))).collect();
        for h in &hangs {
            script(h, "exec sleep 10");
        }
        let good = d.join("b-good");
        manifest_script(&good, &[act("x.g", "G")]);
        let c = load_at(&dirs, &f, BASE);
        assert!(c.get("x.g").is_none(), "the budget was not spent");
        for h in &hangs[..4] {
            assert_eq!(backoff_of(&f, h), Some(Some((1, BASE + 60))), "{h:?}");
        }
        assert!(
            !matches!(backoff_of(&f, &hangs[4]), Some(Some(_))),
            "the binary cut by the budget backs off"
        );
        assert!(
            !matches!(backoff_of(&f, &good), Some(Some(_))),
            "the binary that found the budget spent backs off"
        );
        let t = Instant::now();
        let c = load_at(&dirs, &f, BASE + 1);
        assert!(
            t.elapsed() < 2 * DEADLINE,
            "backed-off binaries rerun: {:?}",
            t.elapsed()
        );
        assert!(c.get("x.g").is_some(), "the good binary did not run");
        assert_eq!(backoff_of(&f, &hangs[4]), Some(Some((1, BASE + 61))));
        for h in &hangs[..4] {
            assert_eq!(backoff_of(&f, h), Some(Some((1, BASE + 60))), "{h:?}");
        }
        let left = left_after_kill(&[&d.to_string_lossy()]);
        assert!(left.is_empty(), "left alive: {left:?}");
    }

    /// A system failure (the sandbox gate) does not back a binary off: it
    /// keeps its old entry and runs as soon as the gate passes.
    #[test]
    fn system_failures_are_not_penalized() {
        use crate::sandbox::tests::{GATE_FAILURES, abi_or_skip, inject_failure};
        if abi_or_skip("system_failures_are_not_penalized", 0).is_none() {
            return;
        }
        for name in GATE_FAILURES {
            let f = Fix::new("bo-gate");
            let d = f.dir("a");
            let dirs = [(d.clone(), true)];
            let bin = d.join("tool");
            manifest_script(&bin, &[act("x.a", "old")]);
            load_at(&dirs, &f, BASE);
            manifest_script(&bin, &[act("x.a", "new"), act("x.b", "B")]);
            {
                let _g = inject_failure(name);
                let c = load_at(&dirs, &f, BASE + 1);
                assert_eq!(c.get("x.a").map(|a| a.label.as_str()), Some("old"));
            }
            assert_eq!(backoff_of(&f, &bin), Some(None), "{name}: backed off");
            let c = load_at(&dirs, &f, BASE + 2);
            assert_eq!(
                c.get("x.a").map(|a| a.label.as_str()),
                Some("new"),
                "{name}"
            );
        }
    }

    /// A crash (death by a signal) and a non-zero exit are the binary's own
    /// failures: each backs off.
    #[test]
    fn crash_and_nonzero_exit_are_penalized() {
        let f = Fix::new("bo-crash");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        script(&d.join("crash"), "kill -SEGV $$");
        script(&d.join("exit"), "exit 3");
        load_at(&dirs, &f, BASE);
        for b in ["crash", "exit"] {
            assert_eq!(
                backoff_of(&f, &d.join(b)),
                Some(Some((1, BASE + 60))),
                "{b}"
            );
        }
        // The crash is told as one (a signal), not as some other failure.
        let (mut child, _) = spawn_plain(&f, "kill -SEGV $$");
        let r = collect(&mut child, Instant::now() + Duration::from_secs(20), true);
        assert!(
            matches!(&r, Err(Fail::Penalized(m)) if m.contains("SIGSEGV")),
            "{:?}",
            why(r)
        );
    }

    /// A broken manifest stays as today: cached without actions and without
    /// a backoff, not rerun (even days later) until the binary changes.
    #[test]
    fn deterministic_failure_stays_permanent() {
        let f = Fix::new("bo-det");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("tool");
        let good = manifest_body("", &[act("x.a", "A")]);
        script(
            &bin,
            &good.replace("inacircle-manifest 0", "inacircle-garbage-00"),
        );
        let c = load_at(&dirs, &f, BASE);
        assert!(c.actions.is_empty());
        assert_eq!(backoff_of(&f, &bin), Some(None));
        swap(&bin, &good);
        let c = load_at(&dirs, &f, BASE + 10 * 86_400);
        assert!(c.actions.is_empty(), "the broken manifest was rerun");
        // Positive control: once it changes, it runs.
        manifest_script(&bin, &[act("x.a", "A"), act("x.b", "B")]);
        let c = load_at(&dirs, &f, BASE + 10 * 86_400);
        assert!(c.get("x.a").is_some());
    }

    /// A retry further away than the longest wait cannot be ours (the clock
    /// went back): the binary is tried now. At exactly the longest wait it
    /// is not.
    #[test]
    fn retry_far_in_future_retries_now() {
        let f = Fix::new("bo-clock");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("tool");
        let good = manifest_body("", &[act("x.a", "A")]);
        script(&bin, &padded("exit 3", good.len()));
        load_at(&dirs, &f, BASE);
        assert_eq!(backoff_of(&f, &bin), Some(Some((1, BASE + 60))));
        swap(&bin, &good);
        // The clock goes back: `retry` is a day away, then a day and a second.
        let c = load_at(&dirs, &f, BASE + 60 - 86_400);
        assert!(c.get("x.a").is_none(), "tried at exactly a day away");
        let c = load_at(&dirs, &f, BASE + 60 - 86_401);
        assert!(c.get("x.a").is_some(), "not tried with retry > now + a day");
        // Ten days ahead in the cache, written by hand.
        script(&bin, &padded("exit 3", good.len()));
        load_at(&dirs, &f, BASE);
        let (_, retry) = backoff_of(&f, &bin).unwrap().unwrap();
        let text = std::fs::read_to_string(f.cache()).unwrap();
        let far = BASE + 10 * 86_400;
        std::fs::write(
            f.cache(),
            text.replace(&format!("retry={retry}"), &format!("retry={far}")),
        )
        .unwrap();
        assert_eq!(backoff_of(&f, &bin), Some(Some((1, far))));
        swap(&bin, &good);
        let c = load_at(&dirs, &f, BASE + 1);
        assert!(
            c.get("x.a").is_some(),
            "retry ten days ahead was waited for"
        );
    }

    /// A cache of the previous version is rejected loudly and rebuilt,
    /// even when its binaries match the disk.
    #[test]
    fn cache_v0_is_rebuilt() {
        let f = Fix::new("bo-v0");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let bin = d.join("tool");
        manifest_script(&bin, &[act("x.a", "new")]);
        load_at(&dirs, &f, BASE);
        let text = std::fs::read_to_string(f.cache()).unwrap();
        assert!(text.starts_with("inacircle-catalog 1\n"), "{text}");
        let v0 = text
            .replace("inacircle-catalog 1\n", "inacircle-catalog 0\n")
            .replace("label=new", "label=old");
        std::fs::write(f.cache(), &v0).unwrap();
        assert!(read_cache(&f.cache()).is_none(), "a v0 cache was read");
        let c = load_at(&dirs, &f, BASE + 1);
        assert_eq!(c.get("x.a").map(|a| a.label.as_str()), Some("new"));
        let text = std::fs::read_to_string(f.cache()).unwrap();
        assert!(text.starts_with("inacircle-catalog 1\n"), "not rebuilt");
    }

    /// `fails` and `retry` are read strictly: plain decimal, `fails` 1 to
    /// 64, `retry` at most 20 digits and a `u64`, both or neither. Anything
    /// else rejects the cache, which is then rebuilt.
    #[test]
    fn rejects_bad_backoff_fields() {
        let bin = "inacircle-catalog 1\nbin\tpath=/a/one\tmtime=5\tsize=7\tbuiltin=yes";
        let parse = |extra: &str| {
            parse_cache(format!("{bin}{extra}\n").as_bytes())
                .map(|e| e[0].backoff.as_ref().map(|b| (b.fails, b.retry)))
        };
        assert_eq!(parse(""), Some(None));
        assert_eq!(parse("\tfails=1\tretry=0"), Some(Some((1, 0))));
        assert_eq!(parse("\tfails=64\tretry=9"), Some(Some((64, 9))));
        assert_eq!(
            parse("\tfails=12\tretry=18446744073709551615"),
            Some(Some((12, u64::MAX)))
        );
        for bad in [
            "\tfails=0\tretry=9",
            "\tfails=65\tretry=9",
            "\tfails=+1\tretry=9",
            "\tfails=01\tretry=9",
            "\tfails=\tretry=9",
            "\tfails= 1\tretry=9",
            "\tfails=1\tretry=x",
            "\tfails=1\tretry=-1",
            "\tfails=1\tretry=+9",
            "\tfails=1\tretry=09",
            "\tfails=1\tretry=",
            "\tfails=1\tretry=100000000000000000000",
            "\tfails=1\tretry=18446744073709551616",
            "\tfails=1",
            "\tretry=9",
            "\tfails=99999999999\tretry=9",
        ] {
            assert_eq!(parse(bad), None, "{bad:?} accepted");
        }
        // A rejected cache is rebuilt: the bad line is gone afterwards.
        let f = Fix::new("bo-badfield");
        let d = f.dir("a");
        let dirs = [(d.clone(), true)];
        let tool = d.join("tool");
        manifest_script(&tool, &[act("x.a", "A")]);
        load_at(&dirs, &f, BASE);
        let text = std::fs::read_to_string(f.cache()).unwrap();
        let bad = text.replace("builtin=yes", "builtin=yes\tfails=0\tretry=9");
        std::fs::write(f.cache(), &bad).unwrap();
        assert!(read_cache(&f.cache()).is_none());
        let c = load_at(&dirs, &f, BASE + 1);
        assert!(c.get("x.a").is_some());
        assert_eq!(backoff_of(&f, &tool), Some(None));
    }

    /// Benchmark, not a test: the cost of the sandboxed manifest run against
    /// a plain spawn, for docs/benchmarks/manifest-sandbox.md. Needs a
    /// release `inacircle-text` (or `INACIRCLE_BENCH_BIN`). Run:
    /// `INACIRCLE_REQUIRE_LANDLOCK=1 cargo test --release -p engine --lib
    /// manifest_sandbox_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn manifest_sandbox_cost() {
        /// Runs per variant; enough for a stable p50 and a max that shows
        /// the occasional outlier.
        const RUNS: usize = 300;
        /// Runs dropped first: cold page cache and CPU caches.
        const WARMUP: usize = 20;
        let bin = std::env::var_os("INACIRCLE_BENCH_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/inacircle-text")
            });
        assert!(bin.is_file(), "build {} first", bin.display());
        let deadline = || Instant::now() + DEADLINE;
        // (spawn returned, output complete and reaped), in microseconds.
        let mut sandboxed = Vec::new();
        let mut plain = Vec::new();
        for i in 0..WARMUP + RUNS {
            let t = Instant::now();
            let mut c = spawn_manifest(&bin).ok().expect("sandboxed spawn");
            let spawned = t.elapsed();
            let out = collect(&mut c, deadline(), true).ok().expect("output");
            let total = t.elapsed();
            assert!(!out.is_empty());
            if i >= WARMUP {
                sandboxed.push((spawned.as_micros(), total.as_micros()));
            }
            let t = Instant::now();
            use std::os::unix::process::CommandExt;
            // Its own group, as the sandboxed child has (by `setsid`).
            let mut c = Command::new(&bin)
                .process_group(0)
                .arg("--manifest")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("plain spawn");
            let spawned = t.elapsed();
            let out = collect(&mut c, deadline(), true).ok().expect("output");
            let total = t.elapsed();
            assert!(!out.is_empty());
            if i >= WARMUP {
                plain.push((spawned.as_micros(), total.as_micros()));
            }
        }
        let report = |name: &str, v: &[(u128, u128)]| {
            let col = |f: fn(&(u128, u128)) -> u128| {
                let mut x: Vec<u128> = v.iter().map(f).collect();
                x.sort_unstable();
                (x[x.len() / 2], x[x.len() - 1])
            };
            let (sp50, smax) = col(|p| p.0);
            let (tp50, tmax) = col(|p| p.1);
            eprintln!(
                "{name}: spawn p50 {sp50} us max {smax} us; output p50 {tp50} us max {tmax} us"
            );
        };
        report("sandboxed", &sandboxed);
        report("plain    ", &plain);
    }

    #[test]
    fn symlink_dotfile_unknown_kind_and_builtin_on_hit() {
        let f = Fix::new("misc");
        let (d, real) = (f.dir("a"), f.dir("real"));
        let extra = "future\tthing=1".to_owned();
        manifest_script(&real.join("tool"), &[act("x.a", "A"), extra]);
        std::os::unix::fs::symlink(real.join("tool"), d.join("link")).unwrap();
        manifest_script(&d.join(".hidden"), &[act("x.h", "H")]);
        // A plugin dir needs Landlock for its manifest to run; without it
        // (and without INACIRCLE_REQUIRE_LANDLOCK=1) the dir counts as
        // built in here and only the built-in flag check is lost.
        let plugin = crate::sandbox::tests::abi_or_skip("symlink_dotfile", 1).is_some();
        for _ in 0..2 {
            let c = Catalog::load_from(&[(d.clone(), !plugin)], &f.cache());
            assert_eq!(c.actions.len(), 1);
            assert_eq!(c.actions[0].bin, d.join("link"));
            assert_eq!(c.actions[0].builtin, !plugin);
        }
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert!(c.actions[0].builtin);
    }

    #[test]
    fn dirs_from_override_and_defaults() {
        let o = |s: &str| Some(std::ffi::OsString::from(s));
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            dirs_from(o("/a::rel:/b"), Some("/d".into())),
            [
                ("/a".into(), true),
                (cwd.join("rel"), true),
                ("/b".into(), true)
            ]
        );
        assert!(dirs_from(o(""), Some("/d".into())).is_empty());
        let d = dirs_from(None, Some("/d".into()));
        assert_eq!(d.len(), 3);
        assert_eq!(d[2], (PathBuf::from("/d/inacircle/actions"), false));
        assert!(d[0].1 && d[1].1);
        // A refused data home leaves out the user's folder only.
        assert_eq!(dirs_from(None, None), d[..2]);
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
            join: None,
            builtin: true,
        };
        assert_eq!(missing_tools(&act), ["no-such-tool-inacircle"]);
    }

    // ---- join actions ----

    const PDF_LINE: &str = "action\tpages.images-to-pdf\tfamily=pages\tin=image/jpeg,image/png\tout=application/pdf\text=pdf\tlabel=PDF\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\tjoin=yes\tmin=1";

    fn parsed(line: &str) -> Option<Action> {
        parse_action(&manifest::parse_line(line).unwrap())
    }

    #[test]
    fn join_action_parses() {
        let a = parsed(PDF_LINE).unwrap();
        assert_eq!(
            a.join,
            Some(Join {
                types: vec!["image/jpeg".into(), "image/png".into()],
                min: 1
            })
        );
        assert_eq!(a.input, "image/jpeg");
    }

    #[test]
    fn join_defaults_to_no() {
        let l = "action\ttext.txt-to-md\tfamily=text\tin=text/plain\tout=text/markdown\text=md\tlabel=Markdown\tgroup=g\tcost=10\tlossy=no\tbatch=yes";
        let a = parsed(l).unwrap();
        assert_eq!(a.join, None);
        assert_eq!(a.input, "text/plain");
        // An explicit `join=no` is the same.
        assert_eq!(parsed(&format!("{l}\tjoin=no")).unwrap().join, None);
    }

    #[test]
    fn rejects_bad_join_lines() {
        let base = |extra: &str, inp: &str| {
            format!(
                "action\tj.a\tfamily=f\tin={inp}\tout=application/pdf\text=pdf\tlabel=L\tgroup=g\tcost=10\tlossy=no\tbatch=yes{extra}"
            )
        };
        let seventeen = (0..17)
            .map(|i| format!("image/x{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sixteen = (0..16)
            .map(|i| format!("image/x{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let bad = [
            base("\tjoin=maybe", "image/png"),
            base("\tjoin=yes\tmin=0", "image/png"),
            base("\tjoin=yes\tmin=1025", "image/png"),
            base("\tmin=2", "image/png"),
            base("\tjoin=no\tmin=2", "image/png"),
            base("", "a/a,b/b"),
            base("\tjoin=no", "a/a,b/b"),
            base("\tjoin=yes\tmin=1", "image/jpeg,,image/png"),
            base("\tjoin=yes\tmin=1", "image/png,image/png"),
            base("\tjoin=yes\tmin=1", &seventeen),
            base("\tjoin=yes\tmin=x", "image/png"),
            base("\tjoin=yes\tmin=", "image/png"),
            base("\tjoin=yes\tmin=+1", "image/png"),
            base("\tjoin=yes\tmin=01", "image/png"),
            base("\tjoin=yes\tmin=-1", "image/png"),
            base("\tjoin=yes\tmin= 1", "image/png"),
        ];
        for l in &bad {
            assert!(parsed(l).is_none(), "{l}");
        }
        // The limits themselves are taken.
        assert!(parsed(&base("\tjoin=yes\tmin=1024", &sixteen)).is_some());
        assert!(
            parsed(&base("\tjoin=yes", "image/png")).is_some(),
            "min defaults to 1"
        );
        assert_eq!(
            parsed(&base("\tjoin=yes", "image/png"))
                .unwrap()
                .join
                .unwrap()
                .min,
            1
        );
    }

    #[test]
    fn joins_lists_only_join_actions() {
        let one = parsed(PDF_LINE).unwrap();
        let mut plain = one.clone();
        plain.id = "t.plain".into();
        plain.join = None;
        let c = Catalog {
            actions: vec![plain, one],
        };
        let j: Vec<&str> = c.joins().map(|a| a.id.as_str()).collect();
        assert_eq!(j, ["pages.images-to-pdf"]);
    }

    /// The cache stores manifest lines verbatim, so a join line survives a
    /// cache round trip with no format change.
    #[test]
    fn join_line_survives_the_cache() {
        let l = manifest::parse_line(PDF_LINE).unwrap();
        let text = manifest::write_line(&l);
        let back = manifest::parse_line(text.trim_end_matches('\n')).unwrap();
        assert_eq!(parse_action(&back), parse_action(&l));
    }

    // ---- bounded cache and manifest ----

    const HDR: &str = "inacircle-catalog 1\n";

    fn bin_rec(i: usize) -> String {
        format!("bin\tpath=/b/{i}\tmtime=5\tsize=7\tbuiltin=yes\n")
    }

    /// An action line with exactly `fields` fields after the kind (an id
    /// and `fields - 1` pairs).
    fn wide(id: &str, fields: usize) -> String {
        let mut l = format!("action\t{id}");
        for k in 1..fields {
            l.push_str(&format!("\tk{k}=v"));
        }
        l.push('\n');
        l
    }

    fn cache_of(bins: usize, actions: usize) -> String {
        let mut t = String::from(HDR);
        for i in 0..bins.max(1) {
            if i < bins {
                t.push_str(&bin_rec(i));
            }
            if i == 0 {
                for a in 0..actions {
                    t.push_str(&wide(&format!("x.{a}"), 1));
                }
            }
        }
        t
    }

    #[test]
    fn cache_bins_128_and_129() {
        assert_eq!(parse_cache(cache_of(128, 0).as_bytes()).unwrap().len(), 128);
        assert!(parse_cache(cache_of(129, 0).as_bytes()).is_none());
    }

    #[test]
    fn cache_actions_1024_and_1025() {
        assert_eq!(
            parse_cache(cache_of(1, 1024).as_bytes()).unwrap()[0]
                .lines
                .len(),
            1024
        );
        assert!(parse_cache(cache_of(1, 1025).as_bytes()).is_none());
    }

    /// 128 bins + 1024 actions are the 1152 records the cache holds; one
    /// more record is refused by the record count (not by a kind count:
    /// the extra one is of an unknown kind, which `parse_cache` refuses
    /// anyway, so this checks `check_cache` itself).
    #[test]
    fn cache_records_1152_and_1153() {
        let mut t = String::from(HDR);
        for i in 0..128 {
            t.push_str(&bin_rec(i));
        }
        for a in 0..1024 {
            t.push_str(&wide(&format!("x.{a}"), 1));
        }
        assert!(check_cache(t.as_bytes()).is_ok());
        assert!(parse_cache(t.as_bytes()).is_some());
        t.push_str("zzz\n");
        assert_eq!(check_cache(t.as_bytes()), Err("too many records"));
    }

    #[test]
    fn cache_fields_32_and_33() {
        let mk = |n| format!("{HDR}{}{}", bin_rec(0), wide("x.a", n));
        assert!(parse_cache(mk(32).as_bytes()).is_some());
        assert!(parse_cache(mk(33).as_bytes()).is_none());
    }

    #[test]
    fn cache_total_fields_12288_and_12289() {
        // A bin has 4 fields; 383 actions of 32 and one of 28 make 12288.
        let mk = |last| {
            let mut t = format!("{HDR}{}", bin_rec(0));
            for a in 0..383 {
                t.push_str(&wide(&format!("x.{a}"), 32));
            }
            t.push_str(&wide("x.last", last));
            t
        };
        assert!(parse_cache(mk(28).as_bytes()).is_some());
        assert_eq!(
            check_cache(mk(29).as_bytes()),
            Err("too many fields in all")
        );
    }

    #[test]
    fn cache_line_32768_and_32769() {
        let mk = |len: usize| {
            let mut l = String::from("action\tx.a\tpad=");
            while l.len() < len {
                l.push('p');
            }
            format!("{HDR}{}{l}\n", bin_rec(0))
        };
        assert!(parse_cache(mk(32768).as_bytes()).is_some());
        assert_eq!(check_cache(mk(32769).as_bytes()), Err("a line is too long"));
    }

    #[test]
    fn cache_bytes_262144_and_262145() {
        // Eight lines of 32768 B + header + one bin = just under; the
        // final pad line brings the file to exactly the limit.
        let mk = |total: usize| {
            let mut t = format!("{HDR}{}", bin_rec(0));
            let mut n = 0;
            while t.len() + 32769 < total {
                let mut l = format!("action\tx.{n}\tpad=");
                while l.len() < 32768 {
                    l.push('p');
                }
                t.push_str(&l);
                t.push('\n');
                n += 1;
            }
            let mut l = String::from("action\tx.last\tpad=");
            while t.len() + l.len() + 1 < total {
                l.push('p');
            }
            t.push_str(&l);
            t.push('\n');
            assert_eq!(t.len(), total);
            t
        };
        assert!(parse_cache(mk(CACHE_BYTES).as_bytes()).is_some());
        assert_eq!(
            check_cache(mk(CACHE_BYTES + 1).as_bytes()),
            Err("the cache file is too big")
        );
    }

    /// The file is read through the bounded reader: a cache file past the
    /// limit is no cache (rebuilt), one at the limit is read.
    #[test]
    fn read_cache_refuses_a_big_file_and_reads_one_at_the_limit() {
        let f = Fix::new("bigcache");
        let path = f.cache();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut t = format!("{HDR}{}", bin_rec(0));
        while t.len() + 20_000 < CACHE_BYTES {
            t.push_str(&format!(
                "action\tx.{}\tpad={}\n",
                t.len(),
                "p".repeat(10_000)
            ));
        }
        let mut l = String::from("action\tx.last\tpad=");
        while t.len() + l.len() + 1 < CACHE_BYTES {
            l.push('p');
        }
        t.push_str(&l);
        t.push('\n');
        assert_eq!(t.len(), CACHE_BYTES);
        std::fs::write(&path, &t).unwrap();
        assert!(read_cache(&path).is_some());
        std::fs::write(&path, format!("{t}p")).unwrap();
        assert!(read_cache(&path).is_none());
        // Far past the limit: refused without reading it all.
        std::fs::write(&path, vec![b'x'; 8 << 20]).unwrap();
        assert!(read_cache(&path).is_none());
    }

    /// A cache that grows while it is read (past the size the open file
    /// showed, so the size pre-filter cannot see it): the cache reader
    /// stops at the limit plus one byte and rejects it.
    #[test]
    fn a_cache_that_grows_while_read_is_cut_at_the_limit() {
        /// A valid header, then `a` for ever.
        struct Growing(usize);
        impl std::io::Read for Growing {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                assert!(self.0 < 1 << 20, "read far past the bound");
                let hdr = HDR.as_bytes();
                let n = if self.0 < hdr.len() {
                    let n = (hdr.len() - self.0).min(b.len());
                    b[..n].copy_from_slice(&hdr[self.0..self.0 + n]);
                    n
                } else {
                    b.fill(b'a');
                    b.len()
                };
                self.0 += n;
                Ok(n)
            }
        }
        let mut g = Growing(0);
        assert!(read_cache_from(&mut g).is_none());
        assert_eq!(g.0, CACHE_BYTES + 1);
    }

    #[test]
    fn manifest_shape_boundaries() {
        let m = |recs: &[String]| format!("inacircle-manifest 0\n{}", recs.concat());
        let one = |n: usize| wide("x.a", n);
        // Records 256 / 257.
        let r = |n: usize| {
            (0..n)
                .map(|i| wide(&format!("x.{i}"), 1))
                .collect::<Vec<_>>()
        };
        assert!(preflight(&m(&r(256)), &MANIFEST_SHAPE).is_ok());
        assert_eq!(
            preflight(&m(&r(257)), &MANIFEST_SHAPE),
            Err("too many records")
        );
        // Fields per record 32 / 33.
        assert!(preflight(&m(&[one(32)]), &MANIFEST_SHAPE).is_ok());
        assert_eq!(
            preflight(&m(&[one(33)]), &MANIFEST_SHAPE),
            Err("a record has too many fields")
        );
        // Total fields 4096 / 4097: 128 records of 32.
        let t = |extra: usize| {
            let mut v: Vec<String> = (0..128).map(|i| wide(&format!("x.{i}"), 32)).collect();
            if extra > 0 {
                v.push(wide("x.e", extra));
            }
            v
        };
        assert!(preflight(&m(&t(0)), &MANIFEST_SHAPE).is_ok());
        assert_eq!(
            preflight(&m(&t(1)), &MANIFEST_SHAPE),
            Err("too many fields in all")
        );
        // Line 32768 / 32769.
        let line = |len: usize| {
            let mut l = String::from("action\tx.a\tpad=");
            while l.len() < len {
                l.push('p');
            }
            l.push('\n');
            l
        };
        assert!(preflight(&m(&[line(32768)]), &MANIFEST_SHAPE).is_ok());
        assert_eq!(
            preflight(&m(&[line(32769)]), &MANIFEST_SHAPE),
            Err("a line is too long")
        );
        // Unknown and escaped kinds still count and still pass.
        assert!(
            preflight(
                "inacircle-manifest 0\nfuture\\tkind\tx=1\n",
                &MANIFEST_SHAPE
            )
            .is_ok()
        );
    }

    /// A manifest past its limits refuses the binary (deterministic,
    /// cached: it is not rerun until it changes).
    #[test]
    fn a_manifest_over_its_limits_refuses_the_binary() {
        let f = Fix::new("mf-limit");
        let d = f.dir("a");
        let acts = |n: usize| {
            (0..n)
                .map(|i| act(&format!("x.{i}"), "A"))
                .collect::<Vec<_>>()
        };
        manifest_script(&d.join("ok"), &acts(256));
        manifest_script(&d.join("over"), &acts(257));
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 256, "only the binary within the limits");
        assert!(c.actions.iter().all(|a| a.bin.ends_with("ok")));
    }

    /// Each manifest is valid alone; together they cross the cache's
    /// action limit. The binary that crosses it is refused whole (none of
    /// its actions kept), the rest stay, and the cache written is one the
    /// parser accepts.
    #[test]
    fn live_rebuild_obeys_the_aggregate_action_limit() {
        let f = Fix::new("agg-actions");
        let d = f.dir("a");
        for (n, name) in ["a", "b", "c", "d"].iter().enumerate() {
            let acts: Vec<String> = (0..256).map(|i| act(&format!("x{n}.{i}"), "A")).collect();
            manifest_script(&d.join(name), &acts);
        }
        manifest_script(&d.join("e"), &[act("y.only", "A")]);
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.actions.len(), 1024, "1024 fit");
        assert!(c.get("y.only").is_none(), "the 1025th is refused loudly");
        let cached = read_cache(&f.cache()).expect("the written cache parses");
        assert_eq!(cached.len(), 5);
        assert!(cached[4].lines.is_empty());
        // 1024 exactly, without the fifth binary: all loaded.
        std::fs::remove_file(d.join("e")).unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 1024);
    }

    /// 129 binaries: the first 128 are used, the extra is reported.
    /// The scan itself is bounded: it stops at `SCAN_DIR_ENTRIES` + 1
    /// entries of a folder (refusing that folder whole) and holds at most
    /// `room` binaries, the smallest paths, however many there are.
    #[test]
    fn scan_stops_at_its_limits_while_it_runs() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fix::new("scan-bound");
        let fill = |d: &Path, n: usize, exec: usize| {
            for i in 0..n {
                let p = d.join(format!("e{i:05}"));
                std::fs::write(&p, "").unwrap();
                if i < exec {
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
        };
        // 300 binaries, room for CACHE_BINS + 1: the 129 smallest paths.
        let many = f.dir("many");
        fill(&many, 300, 300);
        let mut n = 0;
        let got = scan_dir(&many, &true, CACHE_BINS + 1, &mut n).unwrap();
        assert_eq!(n, 300);
        let want: Vec<PathBuf> = (0..=CACHE_BINS)
            .map(|i| many.join(format!("e{i:05}")))
            .collect();
        assert_eq!(got.iter().map(|b| b.path.clone()).collect::<Vec<_>>(), want);
        // Exactly SCAN_DIR_ENTRIES entries: scanned whole.
        let at = f.dir("at");
        fill(&at, SCAN_DIR_ENTRIES, 1);
        let mut n = 0;
        assert_eq!(
            scan_dir(&at, &true, CACHE_BINS + 1, &mut n).unwrap().len(),
            1
        );
        assert_eq!(n, SCAN_DIR_ENTRIES);
        // Far more entries: refused after examining one past the limit.
        let huge = f.dir("huge");
        fill(&huge, 8 * SCAN_DIR_ENTRIES, 1);
        let mut n = 0;
        assert!(scan_dir(&huge, &true, CACHE_BINS + 1, &mut n).is_err());
        assert_eq!(
            n,
            SCAN_DIR_ENTRIES + 1,
            "the scan did not stop at its limit"
        );
        // The refused folder gives nothing; the others are used.
        let bins = scan(&[(huge, true), (at.clone(), false)]);
        assert_eq!(bins.len(), 1);
        assert_eq!(bins[0].path, at.join("e00000"));
    }

    #[test]
    fn live_rebuild_obeys_the_bin_limit() {
        let f = Fix::new("agg-bins");
        let d = f.dir("a");
        for i in 0..129 {
            manifest_script(
                &d.join(format!("b{i:03}")),
                &[act(&format!("x.{i:03}"), "A")],
            );
        }
        let c = Catalog::load_from(&[(d.clone(), true)], &f.cache());
        assert_eq!(c.actions.len(), 128);
        assert!(c.get("x.128").is_none() && c.get("x.127").is_some());
        assert_eq!(read_cache(&f.cache()).unwrap().len(), 128);
        std::fs::remove_file(d.join("b128")).unwrap();
        let c = Catalog::load_from(&[(d, true)], &f.cache());
        assert_eq!(c.actions.len(), 128);
    }
}
