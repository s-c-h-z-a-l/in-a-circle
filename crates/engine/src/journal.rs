//! The journal: the history log, the running-jobs registry and undo. All
//! formats go through the `manifest` crate.
//!
//! History (`$XDG_STATE_HOME/inacircle/history`), header
//! `inacircle-history 1`. A job's first record is `job-start` (`job=`
//! only), written under the append lock when its recorder opens: every
//! other record of the job follows it in the file, by the lock's order,
//! not by any clock. Every published output group is recorded before
//! its publishing rename (`group-begin`, one `child` per output with its
//! private baseline, `group-end`, `publish-intent`: one block on an
//! `O_APPEND` descriptor, written in whole-line chunks under the history's
//! append lock, then `fsync`) and after it (`publish-commit`, or
//! `publish-uncertain` when its place could not be proved; written and
//! synced). A name taken at the rename adds another `publish-intent` for
//! the same group and operation; a publication that failed after its
//! intent gets `publish-abort`. `skip`, `fail` and `unknown` lines come as
//! they happen, the `job` line when the run ends. Undo (into the Trash,
//! see [`crate::trash`]) adds per group a synced `recovery-intent` before
//! anything is made in the Trash and one synced ending
//! (`recovery-commit`, `-restored`, `-kept`, `-uncertain`, `-abort`), then
//! `undo`. Undo acts only on a job whose every group is complete (strict
//! framing, counts, a synced commit); anything else refuses the whole job
//! before anything is moved. Paths are byte-exact (`%XX` for a byte that is not
//! printable ASCII, and for `%` and `\\`).
//!
//! A version 0 history is archived (`history.v0`, never overwritten) under
//! an exclusive lock the first time a run writes; its jobs cannot be
//! undone. Every state file (history, registry entries, the catalog cache)
//! is created exclusively under a random name, never through a symlink, in
//! a folder opened once and checked (ours, not writable by others), and
//! renamed or linked into place relative to that folder. Running jobs live
//! in `$XDG_RUNTIME_DIR/inacircle/jobs/<job-id>`, one `job` line per file.

use crate::exec::{Durability, FileId, Outcome, Tally};
use crate::output::{
    AnchorHint, CheckPoint, ChildBaseline, DirectoryId, GroupId, LocationEvidence, NodeKind,
    OperationId, OutputGroup,
};
use crate::sys::{c_name, fstat, ident, last_err, open_at, read_dir_upto, stat_at};
use crate::trash::{self, RECOVERY_INTENT_MAX_BYTES};
pub use crate::trash::{GroupOutcome, UndoReason, UndoResult};
use crate::xdg;
use manifest::{Line, parse_line, write_line};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &str = "inacircle-history";
const VERSION: u32 = 1;
/// The format before group records: archived, never undone.
const OLD_VERSION: u32 = 0;
const HISTORY: &str = "history";
/// Taken (`flock`) by whoever creates or archives the history.
const INIT_LOCK: &str = "history.lock";
/// The longest header line read: the proposed `history_header_bytes`
/// (256 B, `parameters.json`, not frozen; T9 fixes the reader's limits).
/// The real header is 20 bytes; a longer first line is not a header.
const HEADER_MAX: usize = 256;
/// Archive names tried (`history.v0`, `history.v0.1`, ...): one per
/// downgrade-and-upgrade cycle, which is rare; past this many the history
/// is left alone and nothing is recorded (so nothing is published).
const ARCHIVE_ATTEMPTS: u32 = 8;
/// Fresh random temp names tried: a 64-bit random name only clashes with a
/// planted or leftover file; past a few the folder is not ours to use.
const TEMP_ATTEMPTS: u32 = 4;
/// `registry_jobfile_bytes` (G0-P): a registry entry is one `job` line of
/// about 200 bytes; 4096 is 20x. A bigger file is not skipped: whether a
/// job runs is then unknown, so the listing is an error (and the writer
/// keeps its own line under this, see [`REGISTRY_ACTION_MAX`]).
const REGISTRY_ENTRY_MAX: usize = 4096;
/// `registry_listing_jobs` (G0-P): at most this many directory entries are
/// examined per listing (hidden temp names and bad ones count). Real use
/// is one to a few concurrent supervisors; 128 is far past that. More is
/// an error, never a shorter list; the dead entries among the first 128
/// are removed before the error, so a pile left by crashes heals itself.
const REGISTRY_ENTRIES_MAX: usize = 128;
/// `registry_fields` (G0-P): a `job` line has 7 pairs; 16 is 2x.
const REGISTRY_FIELDS_MAX: usize = 16;
/// The action name in a registry line is cut to this many bytes: the line
/// has to stay under [`REGISTRY_ENTRY_MAX`] or the job would be unlistable.
/// The fixed fields take about 200 B and the escape at most doubles a
/// name, so 512 B gives under 1.3 KB; it is a display name only.
const REGISTRY_ACTION_MAX: usize = 512;
/// `history_line_bytes` (G0-P): the longest record is the recovery intent
/// ([`RECOVERY_INTENT_MAX_BYTES`], 32 KiB); the real longest line is 219 B.
/// A longer line is never ours; one that names the job refuses its undo.
const HISTORY_LINE_BYTES: usize = 32_768;
/// `history_fields_per_record` (G0-P): the widest real record has about 20
/// fields (columns after the kind); counted on the bytes before parsing.
const HISTORY_FIELDS: usize = 32;
/// `history_outputs_per_job` (G0-P): the engine accepts at most 1024
/// outputs from one action run (`exec::MAX_OUTPUTS`).
const HISTORY_OUTPUTS: usize = 1024;
/// `history_dirs_per_job` (G0-P): at most one wrapper directory per output.
const HISTORY_DIRS: usize = 1024;
/// `history_path_bytes` (G0-P): Linux `PATH_MAX`, the bound the Trash code
/// already works with. Decoded bytes of one path.
const HISTORY_PATH_BYTES: usize = 4096;
/// `history_path_bytes_per_job` (G0-P): the decoded path bytes one job's
/// records keep. 1 MiB measured (T9 preflight) at 4.56 MB PSS for `undo`
/// of a job at the limit; twice that is 6.4 MB, over the 5,000,000 B
/// budget, so the limit is load-bearing.
const HISTORY_PATH_BYTES_PER_JOB: usize = 1_048_576;
/// Intents one job's records keep (no G0-P row; derived): one per group
/// ([`HISTORY_DIRS`]) plus three lost name races per group on average
/// (a race is rare: the name was free when stat'ed). A kept intent costs
/// ~40 B beside its path bytes (a `Vec` and its allocation), so 3072 more
/// than the measured maximal job (1024) add ~120 KB to its 4.56 MB PSS,
/// under the 5 MB budget. An empty target costs no path bytes, so without
/// this count ~80 k of them fit in the window. Past it: refused.
const HISTORY_INTENTS: usize = 4 * HISTORY_DIRS;
/// `history_tail_window_bytes` (G0-P): `undo` looks at the last 8 MiB of
/// the history only (an I/O window, read in [`HISTORY_READ_CHUNK`] pieces,
/// never held). A job not provably inside it is refused before a move.
const HISTORY_TAIL_WINDOW: u64 = 8 * 1024 * 1024;
/// `history_read_chunk_bytes` (G0-P): the history reader's buffer.
const HISTORY_READ_CHUNK: usize = 1024;
/// Running-job files are rewritten at most this often.
const UPDATE_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(pub String);

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<unix-seconds>-<pid>-<8 hex digits>`: seconds and a pid can repeat
/// (pid reuse, a namespace), the random part makes a clash unlikely.
pub fn new_job_id() -> JobId {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Without getrandom (never on a supported kernel), the nanoseconds.
    let r = crate::output::random_u64().unwrap_or(u64::from(now.subsec_nanos()));
    JobId(format!(
        "{}-{}-{:08x}",
        now.as_secs(),
        std::process::id(),
        r as u32
    ))
}

fn history_dir() -> io::Result<PathBuf> {
    Ok(xdg::state_home()?.join("inacircle"))
}

fn jobs_dir() -> Option<PathBuf> {
    xdg::runtime_dir().map(|r| r.join("inacircle").join("jobs"))
}

// ---- private state folders ----

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A private state folder, opened once (never through a symlink at its own
/// name) and checked: a directory owned by this user. One writable by
/// others is tightened first (`fchmod` on the descriptor: it is ours), so
/// no other user can plant or swap names in it. Every file operation is
/// relative to this descriptor, so a later rename or symlink swap of the
/// path cannot redirect it.
pub(crate) struct StateDir {
    fd: OwnedFd,
}

impl StateDir {
    /// Creates what is missing (mode 0700), then opens and checks it.
    pub(crate) fn create(path: &Path) -> io::Result<StateDir> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        StateDir::open(path)
    }

    pub(crate) fn open(path: &Path) -> io::Result<StateDir> {
        let c = c_name(path.as_os_str().as_bytes())?;
        let fd = open_at(libc::AT_FDCWD, &c, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let st = fstat(fd.as_raw_fd())?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::other("state folder is not a directory"));
        }
        if st.st_uid != euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state folder is not owned by this user",
            ));
        }
        let mode = st.st_mode & 0o7777;
        // SAFETY: fd is an open directory we own.
        if mode & 0o022 != 0 && unsafe { libc::fchmod(fd.as_raw_fd(), mode & !0o022) } != 0 {
            return last_err();
        }
        Ok(StateDir { fd })
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Opens `name` in the folder (never through a symlink, never blocking):
    /// a regular file owned by this user.
    pub(crate) fn open_file(
        &self,
        name: &str,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<File> {
        // O_NONBLOCK: a FIFO planted at the name must not block the open
        // (a read-only open of one waits for a writer, for ever); it has no
        // effect on a regular file, and the type is checked right after.
        let flags = flags | libc::O_NONBLOCK;
        let fd = open_at(self.raw(), &c_name(name.as_bytes())?, flags, mode)?;
        let st = fstat(fd.as_raw_fd())?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(io::Error::other(format!("{name} is not a regular file")));
        }
        if st.st_uid != euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{name} is not owned by this user"),
            ));
        }
        Ok(File::from(fd))
    }

    /// `bytes` in a fresh file under a random name (exclusive, never
    /// through a symlink, 0600), synced when asked.
    fn temp(&self, base: &str, bytes: &[u8], sync: bool) -> io::Result<(String, File)> {
        for _ in 0..TEMP_ATTEMPTS {
            let name = format!(".{base}.{:016x}.tmp", crate::output::random_u64()?);
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
            let mut f = match self.open_file(&name, flags, 0o600) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            let written = f
                .write_all(bytes)
                .and_then(|()| if sync { f.sync_all() } else { Ok(()) });
            if let Err(e) = written {
                self.remove_own(&name, &f);
                return Err(e);
            }
            return Ok((name, f));
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free temporary name",
        ))
    }

    /// Unlinks `name` only while it still names `f`'s inode.
    fn remove_own(&self, name: &str, f: &File) {
        if let Ok(st) = fstat(f.as_raw_fd()) {
            let _ = self.unlink_if(name, ident(&st));
        }
    }

    /// Unlinks `name` if it is still the inode `id` (never another file
    /// that took its place). The check and the unlink are two steps: a
    /// same-user swap in between is the remaining window.
    pub(crate) fn unlink_if(&self, name: &str, id: (u64, u64)) -> io::Result<()> {
        let c = c_name(name.as_bytes())?;
        if ident(&stat_at(self.raw(), &c)?) != id {
            return Err(io::Error::other(format!("{name} was replaced")));
        }
        // SAFETY: c is NUL-terminated, the fd an open directory.
        if unsafe { libc::unlinkat(self.raw(), c.as_ptr(), 0) } != 0 {
            return last_err();
        }
        Ok(())
    }

    /// Replaces `name` with a file holding `bytes` (a temp file renamed
    /// over it; a symlink at `name` is replaced, not followed). Returns the
    /// new file's `(dev, ino)` and the open file: whoever keeps it open
    /// keeps the inode from being freed and reused, so a later identity
    /// check against `(dev, ino)` cannot match an unrelated new file.
    pub(crate) fn replace(
        &self,
        name: &str,
        bytes: &[u8],
        sync: bool,
    ) -> io::Result<((u64, u64), File)> {
        let (tmp, f) = self.temp(name, bytes, sync)?;
        let id = ident(&fstat(f.as_raw_fd())?);
        let (from, to) = (c_name(tmp.as_bytes())?, c_name(name.as_bytes())?);
        // SAFETY: both names are NUL-terminated, the fd an open directory.
        if unsafe { libc::renameat(self.raw(), from.as_ptr(), self.raw(), to.as_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            self.remove_own(&tmp, &f);
            return Err(e);
        }
        if sync {
            self.sync()?;
        }
        Ok((id, f))
    }

    /// `renameat2(RENAME_NOREPLACE)` within the folder.
    fn rename_noreplace(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (c_name(from.as_bytes())?, c_name(to.as_bytes())?);
        // SAFETY: both names are NUL-terminated, the fd an open directory.
        let r = unsafe {
            libc::renameat2(
                self.raw(),
                from.as_ptr(),
                self.raw(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r == 0 { Ok(()) } else { last_err() }
    }

    /// `linkat` within the folder: fails if `to` exists.
    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (c_name(from.as_bytes())?, c_name(to.as_bytes())?);
        // SAFETY: both names are NUL-terminated, the fd an open directory.
        let r = unsafe { libc::linkat(self.raw(), from.as_ptr(), self.raw(), to.as_ptr(), 0) };
        if r == 0 { Ok(()) } else { last_err() }
    }

    fn sync(&self) -> io::Result<()> {
        // SAFETY: the fd is an open directory.
        if unsafe { libc::fsync(self.raw()) } == 0 {
            Ok(())
        } else {
            last_err()
        }
    }
}

// ---- the history file ----

/// The header's version; `None` for an empty file (one being created).
/// Anything that is not a header is `InvalidData`.
fn read_header(f: &File) -> io::Result<Option<u32>> {
    Ok(read_header_len(f)?.map(|(v, _)| v))
}

/// The header's version and its length (newline included): at most
/// [`HEADER_MAX`] bytes are read, whatever the file holds.
fn read_header_len(f: &File) -> io::Result<Option<(u32, usize)>> {
    let mut buf = [0u8; HEADER_MAX];
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if n == 0 {
        return Ok(None);
    }
    let v = parse_header(&buf[..n])?;
    let len = buf[..n]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(n, |i| i + 1);
    Ok(Some((v, len)))
}

/// The version of a header line at the start of `bytes` (its newline
/// included).
fn parse_header(bytes: &[u8]) -> io::Result<u32> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bad or unsupported history header",
        )
    };
    let end = bytes.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
    let head = std::str::from_utf8(&bytes[..end]).map_err(|_| bad())?;
    let (m, v) = head.split_once(' ').ok_or_else(bad)?;
    let v: u32 = v.parse().map_err(|_| bad())?;
    if m != MAGIC || v.to_string() != head[m.len() + 1..] {
        return Err(bad());
    }
    Ok(v)
}

fn archive_name(n: u32) -> String {
    if n == 0 {
        "history.v0".to_owned()
    } else {
        format!("history.v0.{n}")
    }
}

/// The version 1 history if it is there, `None` if it must be made (no
/// file, an empty one, or version 0). Another version is an error: never
/// archived, rewritten or appended to.
fn open_v1(dir: &StateDir) -> io::Result<Option<File>> {
    match dir.open_file(HISTORY, libc::O_RDWR | libc::O_APPEND, 0) {
        Ok(f) => match read_header(&f)? {
            Some(VERSION) => Ok(Some(f)),
            None | Some(OLD_VERSION) => Ok(None),
            Some(v) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("history version {v} is not supported; left alone"),
            )),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Opens the version 1 history for appending, making it if needed. Who
/// makes it (or archives a version 0 one) holds the exclusive lock (`lock`,
/// the append lock) and decides again under it, so racing runs make one
/// header and one archive; a history of another version is never touched.
fn open_history(dir: &StateDir, lock: &File) -> io::Result<File> {
    if let Some(f) = open_v1(dir)? {
        return Ok(f);
    }
    let _held = Held::take(lock)?;
    if let Some(f) = open_v1(dir)? {
        return Ok(f);
    }
    match dir.open_file(HISTORY, libc::O_RDWR | libc::O_APPEND, 0) {
        Ok(mut f) => match read_header(&f)? {
            // Made by a run that died before its header (no hard links
            // here, see below): the empty file is ours to finish.
            None => {
                f.write_all(manifest::header(MAGIC, VERSION).as_bytes())?;
                f.sync_all()?;
                return Ok(f);
            }
            Some(OLD_VERSION) => archive(dir)?,
            Some(v) => {
                return Err(io::Error::other(format!(
                    "history version {v} appeared while being made"
                )));
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    create_history(dir)
}

/// Moves a version 0 history to the first free archive name: a no-replace
/// rename, so neither it nor an earlier archive is ever overwritten. A
/// filesystem that cannot rename without replacing refuses.
fn archive(dir: &StateDir) -> io::Result<()> {
    for n in 0..ARCHIVE_ATTEMPTS {
        match dir.rename_noreplace(HISTORY, &archive_name(n)) {
            Ok(()) => return dir.sync(),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name to archive the old history; left alone",
    ))
}

/// A new private history holding only its header: written and synced
/// under a temp name, then hard-linked into place (atomic, never replacing:
/// no line can precede the header). Without hard links, created
/// exclusively and the header written after (the init lock keeps other
/// makers out; an appender that finds it empty waits for the lock).
fn create_history(dir: &StateDir) -> io::Result<File> {
    let header = manifest::header(MAGIC, VERSION);
    let (tmp, f) = dir.temp(HISTORY, header.as_bytes(), true)?;
    let linked = dir.link(&tmp, HISTORY);
    dir.remove_own(&tmp, &f);
    if let Err(e) = linked
        && e.kind() != io::ErrorKind::AlreadyExists
    {
        let flags = libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL;
        let mut f = dir.open_file(HISTORY, flags, 0o600)?;
        f.write_all(header.as_bytes())?;
        f.sync_all()?;
        dir.sync()?;
        return Ok(f);
    }
    dir.sync()?;
    match open_v1(dir)? {
        Some(f) => Ok(f),
        None => Err(io::Error::other("history changed while being made")),
    }
}

/// One `write` of all of `bytes`. A short write is an error (a newline is
/// tried after it so the next writer starts a line of its own); the rest
/// is not written after it.
fn write_whole(f: &mut File, bytes: &[u8]) -> io::Result<()> {
    let n = loop {
        match f.write(bytes) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => break r?,
        }
    };
    if n < bytes.len() {
        if n > 0 {
            let _ = f.write(b"\n");
        }
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short write ({n} of {} bytes)", bytes.len()),
        ));
    }
    Ok(())
}

/// The history's append lock (`flock` on `history.lock`), held while one
/// block of records is written, so a block written in several chunks is
/// never interleaved with another writer's lines. Released on drop.
struct Held<'a>(&'a File);

impl<'a> Held<'a> {
    fn take(lock: &'a File) -> io::Result<Held<'a>> {
        lock_with_deadline(lock, LOCK_WAIT)?;
        Ok(Held(lock))
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        // SAFETY: the fd is open for the duration of the call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Lines gathered up to this many bytes per `write`: a group of 1024
/// outputs makes about 200 kB of records, which held whole would take a
/// visible share of the run's 5 MB memory budget; 16 KiB is a few dozen
/// lines per write.
const CHUNK: usize = 16 * 1024;

/// A block of records written in chunks of whole lines (under [`Held`]).
struct Block<'a> {
    f: &'a mut File,
    buf: Vec<u8>,
}

impl Block<'_> {
    fn line(&mut self, l: &Line) -> io::Result<()> {
        self.text(&write_line(l))
    }

    /// A record whose values need no manifest escape (numbers, ids and
    /// [`enc`] output), formatted straight into one line.
    fn raw(&mut self, args: std::fmt::Arguments) -> io::Result<()> {
        self.text(&std::fmt::format(args))
    }

    fn text(&mut self, t: &str) -> io::Result<()> {
        if self.buf.len() + t.len() > CHUNK {
            self.flush()?;
        }
        if t.len() > CHUNK {
            return write_whole(self.f, t.as_bytes());
        }
        self.buf.extend_from_slice(t.as_bytes());
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            write_whole(self.f, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// Writes one block under the append lock, then syncs it when asked.
fn append(
    f: &mut File,
    lock: &File,
    sync: bool,
    lines: impl FnOnce(&mut Block) -> io::Result<()>,
) -> io::Result<()> {
    {
        let _held = Held::take(lock)?;
        let mut b = Block {
            f,
            buf: Vec::with_capacity(CHUNK),
        };
        lines(&mut b)?;
        b.flush()?;
    }
    if sync {
        f.sync_all()?;
    }
    Ok(())
}

/// A history whose last line was cut off (a run died mid-write) gets a
/// newline, so this run's first record starts a line of its own.
fn end_line(f: &mut File, lock: &File) -> io::Result<()> {
    let _held = Held::take(lock)?;
    let len = f.metadata()?.len();
    let mut last = [0u8];
    if len > 0 && f.read_at(&mut last, len - 1)? == 1 && last[0] != b'\n' {
        write_whole(f, b"\n")?;
    }
    Ok(())
}

// ---- record encoding ----

/// Path bytes as printable ASCII, losslessly: a byte that is not printable
/// ASCII (space and controls included), `%` and `\\` become `%XX` (upper
/// case). The result never needs a manifest escape in a pair value.
pub(crate) fn enc(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c > b' ' && c < 0x7f && c != b'%' && c != b'\\' {
            s.push(char::from(c));
        } else {
            let _ = write!(s, "%{c:02X}");
        }
    }
    s
}

/// The inverse of [`enc`], for its exact output only (anything else, a
/// lower-case escape included, is `None`).
fn dec(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    (enc(&out) == s).then_some(out)
}

fn hex(v: u64) -> String {
    format!("{v:016x}")
}

fn kind_str(k: NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Directory => "dir",
    }
}

/// `synced`, or `unconfirmed` (the cause goes to the outcome's own report).
pub fn durability_str(d: &Durability) -> &'static str {
    match d {
        Durability::Synced => "synced",
        Durability::Unconfirmed(_) => "unconfirmed",
    }
}

/// A group's manifest: `group-begin`, a `child` per output, `group-end`
/// with the count and the total bytes of the names. `job` is escaped.
fn manifest_lines(b: &mut Block, job: &str, g: &OutputGroup) -> io::Result<()> {
    let (group, w, n) = (g.group.0, &g.id, g.children.len());
    b.raw(format_args!(
        "group-begin\tjob={job}\tgroup={group:016x}\top={:016x}\twrapper_dev={}\twrapper_ino={}\twrapper_type={}\twrapper_uid={}\twrapper_mode={:o}\tchildren={n}\n",
        g.op.0,
        w.dev,
        w.ino,
        kind_str(w.kind),
        w.uid,
        w.mode,
    ))?;
    let mut bytes = 0usize;
    for c in &g.children {
        let rel = c.rel.as_os_str().as_bytes();
        bytes += rel.len();
        let id = &c.file_id;
        b.raw(format_args!(
            "child\tjob={job}\tgroup={group:016x}\trel={}\ttype={}\tdev={}\tino={}\tuid={}\tmode={:o}\tnlink={}\tsize={}\tmtime={}\tctime={}\n",
            enc(rel),
            kind_str(c.kind),
            id.dev,
            id.ino,
            c.uid,
            c.mode,
            c.nlink,
            id.size,
            id.mtime_ns,
            id.ctime_ns,
        ))?;
    }
    b.raw(format_args!(
        "group-end\tjob={job}\tgroup={group:016x}\tchildren={n}\tbytes={bytes}\n"
    ))
}

/// `done` when every input that ended is done, `partial` when some are,
/// else `failed`.
fn status(t: &Tally) -> &'static str {
    let (done, skipped, failed) = t.counts();
    if done == 0 {
        "failed"
    } else if skipped + failed == 0 {
        "done"
    } else {
        "partial"
    }
}

fn path_enc(p: &Path) -> String {
    enc(p.as_os_str().as_bytes())
}

/// The history line of an outcome: `skip`, `fail` and `unknown`. A `Done`
/// has none (its group's records hold its file); a warning neither.
fn outcome_line(job: &str, o: &Outcome) -> Option<Line> {
    Some(match o {
        Outcome::Skipped { input, reason } => Line::new("skip")
            .pair("job", job)
            .pair("in", &path_enc(input))
            .pair("reason", reason),
        Outcome::Failed { input, reason } => Line::new("fail")
            .pair("job", job)
            .pair("in", &path_enc(input))
            .pair("reason", reason),
        Outcome::PublishedUnknown {
            input,
            group,
            location,
            reason,
            durability,
        } => {
            let a = &location.anchor;
            let mut l = Line::new("unknown")
                .pair("job", job)
                .pair("in", &path_enc(input))
                .pair("group", &hex(group.0))
                .pair("op", &hex(a.op.0))
                .pair("reason", reason.as_str())
                .pair("durability", durability_str(durability))
                .pair("parent", &format!("{}:{}", a.parent_dev, a.parent_ino))
                .pair("object", &format!("{}:{}", a.object_dev, a.object_ino))
                .pair("leaf", &enc(a.leaf.as_bytes()));
            if let Some(p) = &location.path_at_check {
                l = l.pair("path", &path_enc(p));
            }
            l
        }
        Outcome::Done { .. } | Outcome::Warned { .. } => return None,
    })
}

/// Why undo refuses a job with a publication of unknown place or baseline.
pub const UNKNOWN_REFUSAL: &str =
    "an output was published where it could not be confirmed; nothing undone";
/// Why undo refuses a job of the previous history format.
pub const OLD_REFUSAL: &str =
    "recorded by an older version of inacircle; undo unavailable, its outputs are kept";
/// Why undo refuses a job with a publication not confirmed on disk.
pub const UNCONFIRMED_REFUSAL: &str =
    "an output was not confirmed on disk when it was published; nothing undone";
/// Why undo refuses a job whose run could not record itself completely.
pub const WITHDRAWN_REFUSAL: &str =
    "the run could not record its outputs completely; nothing undone";

// ---- recording a run ----

/// Writes a job into the history as it runs. Before each group's
/// publishing rename, its manifest and intent ([`Recorder::prepare_group`],
/// written and synced: an error refuses that publication); after it, the
/// commit ([`Recorder::group`]); `skip`, `fail` and `unknown` lines as they
/// arrive, and the `job` line at [`Recorder::finish`]. A run that dies
/// midway leaves its complete groups, which undo still finds. The first
/// write or sync error is sticky: nothing more is recorded (so nothing more
/// is published), and the job is not undoable. A recording error never
/// takes back a published output. Holds one tally entry per input and at
/// most one group (the one being published).
pub struct Recorder {
    /// The history and its append lock.
    file: Option<(File, File)>,
    /// The first recording error: sticky.
    failed: Option<String>,
    job: String,
    /// `job` as a record value (escaped once, for the raw records).
    job_field: String,
    action: String,
    /// Also holds the inputs published at a place (or with a baseline)
    /// that could not be proved (once per input).
    tally: Tally,
    /// The group whose intent is written and whose commit is not (yet):
    /// its identity, not a copy of its children (up to 1024 of them).
    pending: Option<Pending>,
    /// Committed groups not synced to disk.
    unconfirmed: usize,
    /// Committed groups whose place could not be proved.
    uncertain: usize,
    /// Committed groups that had no intent of this recorder.
    unprepared: usize,
}

/// What a re-run of [`Recorder::prepare_group`] for the same group must
/// match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    group: crate::output::GroupId,
    op: crate::output::OperationId,
    id: DirectoryId,
    children: usize,
}

impl Pending {
    fn of(g: &OutputGroup) -> Pending {
        Pending {
            group: g.group,
            op: g.op,
            id: g.id,
            children: g.children.len(),
        }
    }
}

/// A closed job: the per-input tally, whether undo may be offered (every
/// group complete, durable and synced in the history), and the warning
/// that goes with a job that may not.
#[derive(Debug)]
pub struct Recorded {
    pub tally: Tally,
    pub undoable: bool,
    pub warning: Option<String>,
}

impl Recorder {
    /// Opens the history (making or archiving it as needed). An error is
    /// not reported here: it is sticky, refuses every publication and comes
    /// back as [`Recorded::warning`].
    pub fn open(job: &JobId, action: &str) -> Recorder {
        let file = history_dir()
            .and_then(|d| StateDir::create(&d))
            .and_then(|d| {
                let lock = d.open_file(INIT_LOCK, libc::O_RDWR | libc::O_CREAT, 0o600)?;
                let mut f = open_history(&d, &lock)?;
                end_line(&mut f, &lock)?;
                Ok((f, lock))
            });
        let (file, failed) = match file {
            Ok(f) => (Some(f), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut r = Recorder {
            file,
            failed,
            job: job.0.clone(),
            job_field: manifest::escape(&job.0),
            action: action.to_owned(),
            tally: Tally::default(),
            pending: None,
            unconfirmed: 0,
            uncertain: 0,
            unprepared: 0,
        };
        // The job's first record: what proves, in a history window, that
        // none of its records lie before it. Not synced by itself: the
        // first group's block is, and its `fsync` covers this line too. An
        // error is sticky, so nothing of the job is published without it.
        let job_field = r.job_field.clone();
        let _ = r.write(false, |b| {
            b.raw(format_args!("job-start\tjob={job_field}\n"))
        });
        r
    }

    /// One block of records (see [`append`]); the first error is sticky.
    fn write(
        &mut self,
        sync: bool,
        lines: impl FnOnce(&mut Block) -> io::Result<()>,
    ) -> io::Result<()> {
        let not_written = |why: &str| io::Error::other(format!("history not written: {why}"));
        if let Some(why) = &self.failed {
            return Err(not_written(why));
        }
        let Some((f, lock)) = &mut self.file else {
            return Err(not_written("not open"));
        };
        append(f, lock, sync, lines).map_err(|e| {
            let why = e.to_string();
            let err = not_written(&why);
            self.failed = Some(why);
            err
        })
    }

    /// A group's outputs, complete and fixed in private at `private_path`:
    /// its whole manifest (wrapper identity, every child's baseline, count
    /// and name bytes) and the publish intent for its planned name, as one
    /// block, synced, before the publishing rename. Called again for the
    /// same group (the name was taken), only a new intent is written. An
    /// error refuses the publication: nothing becomes visible.
    pub fn prepare_group(&mut self, group: &OutputGroup, private_path: &Path) -> io::Result<()> {
        let again = match self.pending {
            Some(p) if p.group == group.group => {
                if p != Pending::of(group) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a group was prepared again with another baseline",
                    ));
                }
                true
            }
            _ => {
                self.abort_pending();
                self.pending = Some(Pending::of(group));
                false
            }
        };
        let (job, private) = (self.job_field.clone(), path_enc(private_path));
        let target = path_enc(&group.directory);
        let (op, gid) = (group.op.0, group.group.0);
        self.write(true, |b| {
            if !again {
                manifest_lines(b, &job, group)?;
            }
            b.raw(format_args!(
                "publish-intent\top={op:016x}\tjob={job}\tgroup={gid:016x}\tprivate={private}\ttarget={target}\n"
            ))
        })
    }

    /// The prepared group was not committed (its publication failed after
    /// the intent: nothing of it is visible, see `publish_beneath`).
    fn abort_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            let job = self.job_field.clone();
            let _ = self.write(false, |b| {
                b.raw(format_args!(
                    "publish-abort\top={:016x}\tjob={job}\tgroup={:016x}\n",
                    p.op.0, p.group.0
                ))
            });
        }
    }

    /// A group after its commit: `publish-commit` with the proved place, or
    /// `publish-uncertain` with the anchor when it could not be proved.
    /// Written and synced; an error leaves the output and its success and
    /// makes the job not undoable.
    pub fn group(&mut self, group: &OutputGroup, location: &LocationEvidence, d: &Durability) {
        match self.pending {
            Some(p) if p.group == group.group && p.op == group.op => self.pending = None,
            _ => self.unprepared += 1,
        }
        if let Durability::Unconfirmed(_) = d {
            self.unconfirmed += 1;
        }
        let (op, gid, job) = (group.op.0, group.group.0, self.job_field.clone());
        let d = durability_str(d);
        let line = match &location.path_at_check {
            Some(p) => format!(
                "publish-commit\top={op:016x}\tjob={job}\tgroup={gid:016x}\tactual={}\tdurability={d}\n",
                path_enc(p)
            ),
            None => {
                self.uncertain += 1;
                let a = &location.anchor;
                format!(
                    "publish-uncertain\top={op:016x}\tjob={job}\tgroup={gid:016x}\tlocation_known=no\tparent_dev={}\tparent_ino={}\tleaf={}\tobject_dev={}\tobject_ino={}\treason=AncestorChanged\tdurability={d}\n",
                    a.parent_dev,
                    a.parent_ino,
                    enc(a.leaf.as_bytes()),
                    a.object_dev,
                    a.object_ino,
                )
            }
        };
        let _ = self.write(true, |b| b.text(&line));
    }

    /// Counts the outcome; `skip`, `fail` and `unknown` get their line (an
    /// `unknown` one synced: it refuses undo). A `Done` stats nothing: the
    /// group's baseline is its record.
    pub fn outcome(&mut self, o: &Outcome) {
        self.tally.add(o);
        if let Some(l) = outcome_line(&self.job, o) {
            let sync = matches!(o, Outcome::PublishedUnknown { .. });
            let _ = self.write(sync, |b| b.line(&l));
        }
    }

    /// Closes the job: a group still prepared did not commit (aborted),
    /// then the `job` line (with `undo=no` when not undoable; tried even
    /// after an earlier error, which may have been a sync alone).
    pub fn finish(mut self, sandboxed: bool) -> Recorded {
        self.abort_pending();
        let unknown = self.tally.unknown();
        let refused = unknown + self.unconfirmed + self.uncertain + self.unprepared;
        let mut undoable = self.failed.is_none() && refused == 0;
        let mut l = Line::new("job")
            .pair("id", &self.job)
            .pair("time", &unix_secs().to_string())
            .pair("action", &self.action)
            .pair("status", status(&self.tally))
            .pair("sandbox", if sandboxed { "yes" } else { "no" });
        if !undoable {
            l = l.pair("undo", "no");
        }
        let mut marker_unproven = false;
        if self.failed.is_some() {
            // The record of an earlier error may be in the page cache only
            // (a failed sync, a short write). The `undo=no` that withdraws
            // it is synced too, and a cut-off earlier line gets its newline
            // first so this one stands alone. If even that fails, the
            // history cannot prove the withdrawal: say so. The reader still
            // fails closed on a cut line (`a record cut off`).
            if let Some((f, lock)) = &mut self.file {
                let written =
                    end_line(f, lock).and_then(|()| append(f, lock, true, |b| b.line(&l)));
                marker_unproven = written.is_err();
            }
        } else if self.write(false, |b| b.line(&l)).is_err() {
            undoable = false;
        }
        let mut warning = Vec::new();
        if unknown > 0 {
            warning.push(format!(
                "{unknown} input(s) published where it could not be confirmed"
            ));
        }
        if let Some(why) = &self.failed {
            warning.push(format!("history not written: {why}; outputs kept"));
        }
        if marker_unproven {
            warning.push("the history could not be made durable".to_owned());
        }
        if self.unconfirmed > 0 {
            warning.push(format!(
                "{} output folder(s) not confirmed on disk",
                self.unconfirmed
            ));
        }
        if self.unprepared > 0 {
            warning.push(format!(
                "{} output folder(s) committed without a record",
                self.unprepared
            ));
        }
        Recorded {
            tally: self.tally,
            undoable,
            warning: (!undoable).then(|| {
                if warning.is_empty() {
                    "undo unavailable".to_owned()
                } else {
                    format!("{}; undo unavailable", warning.join("; "))
                }
            }),
        }
    }
}

// ---- the running-jobs registry ----

/// A registered running job. Its file is written by rename in the
/// registry folder opened at the start, and removed on drop if it is
/// still the file written last. That file is held open until then, so its
/// inode cannot be freed and reused by another file at the name (on ext4
/// a removed file's inode number comes back at once): the check before
/// the unlink compares against a live inode. What remains is the
/// check-to-unlink window of `unlink_if` against a same-user swap.
pub struct Running {
    dir: Option<StateDir>,
    written: Option<((u64, u64), File)>,
    job: String,
    action: String,
    inputs: usize,
    /// Own identity, taken once; `None` when `/proc` cannot tell.
    id: Option<ProcId>,
    last: Instant,
}

impl Running {
    /// Registers the job; without `$XDG_RUNTIME_DIR` (or on any I/O error)
    /// the registry is skipped silently.
    pub fn start(job: &JobId, action: &str, inputs: usize) -> Running {
        let mut r = Running {
            dir: jobs_dir().and_then(|d| StateDir::create(&d).ok()),
            written: None,
            job: job.0.clone(),
            action: action.to_owned(),
            inputs,
            id: ProcId::of(std::process::id()),
            last: Instant::now(),
        };
        r.write(0, 0.0);
        r
    }

    /// Rewrites the file, at most four times a second. A call inside the
    /// interval is dropped; every call carries the whole run's current
    /// state, so the next call after the interval writes a value at least
    /// as new as any dropped one, with no pending copy and no timer.
    pub fn update(&mut self, done: usize, progress: f32) {
        if self.last.elapsed() < UPDATE_EVERY {
            return;
        }
        self.last = Instant::now();
        self.write(done, progress);
    }

    fn write(&mut self, done: usize, progress: f32) {
        let Some(dir) = &self.dir else { return };
        let mut line = Line::new("job")
            .pair("id", &self.job)
            .pair("pid", &std::process::id().to_string())
            .pair("action", cut_at_boundary(&self.action, REGISTRY_ACTION_MAX))
            .pair("inputs", &self.inputs.to_string())
            .pair("done", &done.to_string())
            .pair("progress", &format!("{progress:.3}"));
        if let Some(id) = &self.id {
            line = line
                .pair("start", &id.ticks.to_string())
                .pair("boot", &id.boot);
        }
        if let Ok(w) = dir.replace(&self.job, write_line(&line).as_bytes(), false) {
            self.written = Some(w);
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let (Some(dir), Some((id, _))) = (&self.dir, &self.written) {
            let _ = dir.unlink_if(&self.job, *id);
        }
    }
}

/// `s` cut to at most `max` bytes at a character boundary.
fn cut_at_boundary(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Most bytes read from a `/proc` file for process identity: a `stat` line
/// is `comm` (at most 16 bytes) plus 50-odd numbers, about 300 bytes; 4 KiB
/// is far past that, and a longer file is not a `stat` line (read fails).
const PROC_READ_MAX: usize = 4096;

/// The first `PROC_READ_MAX` bytes of a `/proc` file, opened `O_NONBLOCK`
/// (nothing under `/proc` should block; a hostile mount must not hang us).
fn read_proc(path: &str) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    String::from_utf8(action::read_bounded(&f, PROC_READ_MAX).ok()?).ok()
}

/// Start time of `pid` in clock ticks since boot: `/proc/<pid>/stat`
/// field 22. It is a monotonic count, so a step of the wall clock (NTP,
/// by hand) cannot change it, unlike a time derived from `btime`.
fn proc_start_ticks(pid: u32) -> Option<u64> {
    parse_start_ticks(&read_proc(&format!("/proc/{pid}/stat"))?)
}

fn parse_start_ticks(stat: &str) -> Option<u64> {
    // `comm` may contain spaces and parentheses: fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest starts at field 3 (state), so field 22 is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// This boot's id (`/proc/sys/kernel/random/boot_id`, a UUID of 36
/// characters): ticks since boot are comparable only within one boot.
fn boot_id() -> Option<String> {
    let id = read_proc("/proc/sys/kernel/random/boot_id")?;
    let id = id.trim();
    (!id.is_empty() && id.len() <= 64).then(|| id.to_owned())
}

/// What a registry entry records of its process, besides the pid: the
/// start time in ticks since boot and the boot id. Neither depends on the
/// wall clock.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcId {
    ticks: u64,
    boot: String,
}

impl ProcId {
    fn of(pid: u32) -> Option<ProcId> {
        Some(ProcId {
            ticks: proc_start_ticks(pid)?,
            boot: boot_id()?,
        })
    }
}

/// Whether the process now at the pid is the one that was recorded. An
/// unreadable current identity (`None`) cannot prove a difference: same.
fn same_process(recorded: &ProcId, now: Option<&ProcId>) -> bool {
    now.is_none_or(|n| n == recorded)
}

/// Whether the process that registered the job still runs: the pid exists
/// and, when the entry records an identity, it is the same start time in
/// the same boot. A reused pid has another start time; an entry from a
/// previous boot is dead whatever its pid now is.
fn job_alive(pid: u32, recorded: Option<&ProcId>) -> bool {
    job_alive_with(pid, recorded, ProcId::of(pid).as_ref())
}

fn job_alive_with(pid: u32, recorded: Option<&ProcId>, now: Option<&ProcId>) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks existence; no preconditions.
    let r = unsafe { libc::kill(p, 0) };
    if r != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return false;
    }
    // EPERM (another user's process) counts as alive, unless /proc shows
    // another process there. An entry without an identity (written by an
    // older version) is judged by the pid alone.
    recorded.is_none_or(|r| same_process(r, now))
}

/// Why a registry listing is refused (rejection copy of the registry rows).
const REGISTRY_REFUSAL: &str = "The running-jobs registry exceeds its safety limits.";

/// How long a temporary registry file of ours must be untouched before a
/// listing removes it: one lives from its exclusive create to its rename,
/// one write of at most 4 KiB, no sync (microseconds). A minute is far past
/// that, so one in use is never taken under a working clock; if one is
/// anyway (the writer stopped, the clock stepped), only that update's
/// rename fails (the write is dropped silently). The job's next `update`
/// writes anew; if none comes (its first write was the one lost), the job
/// stays out of the registry until it ends, the same as a registry that
/// could not be written at all (see [`Running::start`]).
const STALE_TEMP: Duration = Duration::from_secs(60);

/// Removes `name` if it is one of our temporary files
/// (`.<base>.<16 hex>.tmp`, see `StateDir::temp`) left by a crash: a
/// regular file of ours (checked on its open fd), untouched for
/// [`STALE_TEMP`], unlinked only while the name is still that file.
fn remove_stale_temp(dir: &StateDir, name: &str) {
    let ours = name
        .strip_prefix('.')
        .and_then(|n| n.strip_suffix(".tmp"))
        .and_then(|n| n.rsplit_once('.'))
        .is_some_and(|(base, hex)| {
            !base.is_empty()
                && hex.len() == 16
                && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
    if !ours {
        return;
    }
    let Ok(f) = dir.open_file(name, libc::O_RDONLY, 0) else {
        return;
    };
    let stale = f
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age > STALE_TEMP);
    if stale && let Ok(st) = fstat(f.as_raw_fd()) {
        let _ = dir.unlink_if(name, ident(&st));
    }
}

fn registry_refusal(what: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{REGISTRY_REFUSAL} ({what})"),
    )
}

/// Running jobs: `(id, action, pid, progress)`. An entry whose process is
/// gone is deleted and left out. Entries are read through the folder's
/// descriptor, never through a symlink. Not knowing is an error, never an
/// empty list: a folder that cannot be opened (other than missing: then no
/// job was ever registered), more than [`REGISTRY_ENTRIES_MAX`] entries, an
/// entry over [`REGISTRY_ENTRY_MAX`] bytes or [`REGISTRY_FIELDS_MAX`]
/// fields, or a read that fails. An entry that is not a regular file of
/// ours, or does not parse as a `job` line, cannot be a job of ours and is
/// skipped.
pub fn list_running() -> io::Result<Vec<(JobId, String, u32, f32)>> {
    let Some(path) = jobs_dir() else {
        return Ok(Vec::new());
    };
    // Listed through the folder's own descriptor (D-28), never by path.
    let dir = match StateDir::open(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let (names, more) = read_dir_upto(dir.raw(), REGISTRY_ENTRIES_MAX)?;
    let mut out = Vec::new();
    for name in &names {
        let Ok(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            remove_stale_temp(&dir, name);
            continue;
        }
        let f = match dir.open_file(name, libc::O_RDONLY, 0) {
            Ok(f) => f,
            // Gone since the listing (the job ended), or not a regular file
            // of ours (a symlink: ELOOP; a socket: ENXIO; the type and
            // owner checks: no errno): not an entry. Anything else (EACCES,
            // EMFILE, EIO) leaves an entry unread: fail closed.
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    None | Some(libc::ENOENT | libc::ELOOP | libc::ENXIO)
                ) =>
            {
                continue;
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("registry entry {name} cannot be opened: {e}"),
                ));
            }
        };
        let bytes = match action::read_bounded(&f, REGISTRY_ENTRY_MAX) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(registry_refusal(format!(
                    "entry {name} is over {REGISTRY_ENTRY_MAX} bytes"
                )));
            }
            Err(e) => return Err(e),
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let Ok(line) = parse_line(&text) else {
            continue;
        };
        if line.kind != "job" {
            continue;
        }
        if line.items.len() > REGISTRY_FIELDS_MAX {
            return Err(registry_refusal(format!(
                "entry {name} has more than {REGISTRY_FIELDS_MAX} fields"
            )));
        }
        let (Some(id), Some(Ok(pid))) = (line.get("id"), line.get("pid").map(str::parse::<u32>))
        else {
            continue;
        };
        let recorded = line
            .get("start")
            .and_then(|s| s.parse().ok())
            .zip(line.get("boot"))
            .map(|(ticks, boot)| ProcId {
                ticks,
                boot: boot.to_owned(),
            });
        if !job_alive(pid, recorded.as_ref()) {
            // Removed only while the name is still the file just read: the
            // identity comes from its open fd, and holding the fd keeps the
            // inode from being freed and reused. What remains is the
            // check-to-unlink window of `unlink_if` against a rename onto
            // this name in between; the only writer of a name is the job
            // with that id (`Running`), and this one is dead, so only
            // another process of this user acting on purpose could, and it
            // could unlink the entry itself anyway.
            if let Ok(st) = fstat(f.as_raw_fd()) {
                let _ = dir.unlink_if(name, ident(&st));
            }
            continue;
        }
        let progress = line
            .get("progress")
            .and_then(|p| p.parse().ok())
            .unwrap_or(0.0);
        out.push((
            JobId(id.to_owned()),
            line.get("action").unwrap_or("").to_owned(),
            pid,
            progress,
        ));
    }
    if more {
        return Err(registry_refusal(format!(
            "more than {REGISTRY_ENTRIES_MAX} entries"
        )));
    }
    Ok(out)
}

// ---- reading a job back ----

/// One group as its records say.
#[derive(Debug)]
struct GroupRec {
    group: u64,
    op: u64,
    wrapper: DirectoryId,
    declared: usize,
    children: Vec<ChildBaseline>,
    /// The names' total bytes so far.
    bytes: usize,
    ended: bool,
    /// Every intent's target (several after a taken name).
    intents: Vec<Vec<u8>>,
    /// The committed place.
    commit: Option<Vec<u8>>,
    synced: bool,
    /// Committed at a place that could not be proved.
    uncertain: bool,
    /// Its publication failed after the intent: nothing was published.
    aborted: bool,
    /// Its undo into the Trash, as recorded.
    recovery: Recovery,
}

/// A group's undo records: none, an intent without its ending (a crash),
/// or the ending. A `Kept` ending (nothing moved) lets a later intent in.
#[derive(Debug)]
enum Recovery {
    None,
    Open(OpenRec),
    Closed(GroupOutcome),
}

#[derive(Debug)]
struct OpenRec {
    op: u64,
    source: Vec<u8>,
    target: Vec<u8>,
    info_rel: usize,
    source_parent: (u64, u64),
    target_parent: (u64, u64),
    date: String,
}

impl GroupRec {
    /// Whether a commit, an uncertain commit or an abort closed it.
    fn closed(&self) -> bool {
        self.commit.is_some() || self.uncertain || self.aborted
    }
}

/// What the history says about one job.
#[derive(Debug, Default)]
struct JobRecords {
    known: bool,
    undone: bool,
    /// A `PublishedUnknown`: undo refuses.
    unknown: bool,
    /// The `job` line says `undo=no`: undo refuses.
    withdrawn: bool,
    /// Found in a version 0 history: undo refuses.
    old: bool,
    groups: Vec<GroupRec>,
    /// The first framing fault: undo refuses the whole job.
    broken: Option<&'static str>,
    /// Outputs (children) of all groups so far.
    outputs: usize,
    /// Decoded path bytes kept so far.
    path_bytes: usize,
    /// Intents (of all groups) kept so far.
    intents: usize,
    /// In a window that does not start at the history's start, a line of
    /// the job came before its `job-start`.
    incomplete: bool,
    /// The job's `job-start` record was read.
    started: bool,
    /// A record of the job other than `job-start` was read.
    records: bool,
}

type Fault = &'static str;

/// The values of exactly `keys` (each once, in any order, nothing else),
/// in the order of `keys`.
fn fields<'a>(l: &'a Line, keys: &[&str]) -> Result<Vec<&'a str>, Fault> {
    let mut out = vec![None; keys.len()];
    if l.items.len() != keys.len() {
        return Err("a record with missing or extra fields");
    }
    for item in &l.items {
        let manifest::Item::Pair(k, v) = item else {
            return Err("a record with a bare field");
        };
        let i = keys
            .iter()
            .position(|x| x == k)
            .ok_or("a record with an unknown field")?;
        if out[i].replace(v.as_str()).is_some() {
            return Err("a record with a field twice");
        }
    }
    Ok(out.into_iter().flatten().collect())
}

/// A number in its one canonical decimal form.
fn num(s: &str) -> Result<u64, Fault> {
    s.parse::<u64>()
        .ok()
        .filter(|v| v.to_string() == s)
        .ok_or("a bad number")
}

fn small(s: &str) -> Result<u32, Fault> {
    u32::try_from(num(s)?).map_err(|_| "a bad number")
}

fn count(s: &str) -> Result<usize, Fault> {
    usize::try_from(num(s)?).map_err(|_| "a bad number")
}

/// Nanoseconds since the epoch (may be negative).
fn time(s: &str) -> Result<i128, Fault> {
    s.parse::<i128>()
        .ok()
        .filter(|v| v.to_string() == s)
        .ok_or("a bad time")
}

fn octal(s: &str) -> Result<u32, Fault> {
    u32::from_str_radix(s, 8)
        .ok()
        .filter(|v| format!("{v:o}") == s)
        .ok_or("a bad mode")
}

fn hex_id(s: &str) -> Result<u64, Fault> {
    u64::from_str_radix(s, 16)
        .ok()
        .filter(|v| hex(*v) == s)
        .ok_or("a bad id")
}

/// A path's bytes; one over [`HISTORY_PATH_BYTES`] decoded is a limit fault.
fn path_dec(s: &str) -> Result<Vec<u8>, Fault> {
    let d = dec(s).ok_or("a bad path encoding")?;
    if d.len() > HISTORY_PATH_BYTES {
        return Err("over the limit: a path longer than the history's path limit");
    }
    Ok(d)
}

fn node_kind(s: &str) -> Result<NodeKind, Fault> {
    match s {
        "file" => Ok(NodeKind::File),
        "dir" => Ok(NodeKind::Directory),
        _ => Err("a bad type"),
    }
}

const BEGIN: &[&str] = &[
    "job",
    "group",
    "op",
    "wrapper_dev",
    "wrapper_ino",
    "wrapper_type",
    "wrapper_uid",
    "wrapper_mode",
    "children",
];
const CHILD: &[&str] = &[
    "job", "group", "rel", "type", "dev", "ino", "uid", "mode", "nlink", "size", "mtime", "ctime",
];
const START: &[&str] = &["job"];
const JOB: &[&str] = &["id", "time", "action", "status", "sandbox"];
const JOB_WITHDRAWN: &[&str] = &["id", "time", "action", "status", "sandbox", "undo"];
const END: &[&str] = &["job", "group", "children", "bytes"];
const INTENT: &[&str] = &["op", "job", "group", "private", "target"];
const COMMIT: &[&str] = &["op", "job", "group", "actual", "durability"];
const UNCERTAIN: &[&str] = &[
    "op",
    "job",
    "group",
    "location_known",
    "parent_dev",
    "parent_ino",
    "leaf",
    "object_dev",
    "object_ino",
    "reason",
    "durability",
];
const ABORT: &[&str] = &["op", "job", "group"];
const RINTENT: &[&str] = &[
    "op",
    "job",
    "group",
    "source",
    "target",
    "info_rel",
    "source_parent_dev",
    "source_parent_ino",
    "target_parent_dev",
    "target_parent_ino",
    "wrapper_dev",
    "wrapper_ino",
    "mount",
    "deletion_date",
];
const RCLOSE: &[&str] = &[
    "op",
    "job",
    "group",
    "actual",
    "parent",
    "object",
    "leaf",
    "checked",
    "reason",
    "errno",
    "info",
    "durability",
];
/// The closing record of each undo outcome.
const R_KEPT: &str = "recovery-abort";
const R_MOVED: &str = "recovery-commit";
const R_RESTORED: &str = "recovery-restored";
const R_RECOVERY_KEPT: &str = "recovery-kept";
const R_UNCERTAIN: &str = "recovery-uncertain";

/// `dev:ino`.
fn pair_id(s: &str) -> Result<(u64, u64), Fault> {
    let (d, i) = s.split_once(':').ok_or("a bad identity")?;
    Ok((num(d)?, num(i)?))
}

/// `-` or a value.
fn opt(s: &str) -> Option<&str> {
    (s != "-").then_some(s)
}

/// An undo outcome back from its closing record.
fn closed_outcome(kind: &str, group: u64, op: u64, v: &[&str]) -> Result<GroupOutcome, Fault> {
    let (parent, object) = (pair_id(v[4])?, pair_id(v[5])?);
    let location = LocationEvidence {
        path_at_check: opt(v[3])
            .map(path_dec)
            .transpose()?
            .map(|p| PathBuf::from(OsStr::from_bytes(&p))),
        last_unverified_path: None,
        anchor: AnchorHint {
            parent_dev: parent.0,
            parent_ino: parent.1,
            object_dev: object.0,
            object_ino: object.1,
            leaf: OsStr::from_bytes(&path_dec(v[6])?).to_owned(),
            op: OperationId(op),
        },
        checked_at: CheckPoint::parse(v[7]).ok_or("a bad check point")?,
    };
    let errno = opt(v[9])
        .map(|e| e.parse::<i32>().map_err(|_| "a bad errno"))
        .transpose()?;
    // The errno belongs to the reason when its kind carries one, else it
    // is the outcome's own (a failed way back).
    let reason = opt(v[8])
        .map(|r| {
            UndoReason::parse(r, None)
                .or_else(|| UndoReason::parse(r, errno))
                .ok_or("a bad reason")
        })
        .transpose()?;
    let durability = match v[11] {
        "synced" => Durability::Synced,
        // `-`: an ending without one (kept, uncertain).
        "unconfirmed" | "-" => Durability::Unconfirmed("recorded unconfirmed".to_owned()),
        _ => return Err("a bad durability"),
    };
    let group = GroupId(group);
    let reason = || reason.ok_or("a missing reason");
    Ok(match kind {
        R_KEPT => GroupOutcome::Kept {
            group,
            location,
            reason: reason()?,
        },
        R_MOVED => GroupOutcome::MovedToTrash {
            group,
            location,
            info_leaf: OsStr::from_bytes(&path_dec(opt(v[10]).ok_or("a missing info")?)?)
                .to_owned(),
            durability,
        },
        R_RESTORED => GroupOutcome::Restored {
            group,
            location,
            reason: reason()?,
            durability,
        },
        R_RECOVERY_KEPT => GroupOutcome::RecoveryKept {
            group,
            location,
            reason: reason()?,
            errno,
            durability,
        },
        _ => GroupOutcome::Uncertain {
            group,
            op: OperationId(op),
            locations: vec![location],
            reason: reason()?,
        },
    })
}

/// The closing record of `op`, ending as `o`.
fn close_line(job: &str, op: OperationId, o: &GroupOutcome) -> String {
    let (kind, group, loc, reason, errno, info, durability) = match o {
        GroupOutcome::Kept {
            group,
            location,
            reason,
        } => (
            R_KEPT,
            group,
            location,
            Some(*reason),
            reason.errno(),
            None,
            None,
        ),
        GroupOutcome::MovedToTrash {
            group,
            location,
            info_leaf,
            durability,
        } => (
            R_MOVED,
            group,
            location,
            None,
            None,
            Some(info_leaf),
            Some(durability),
        ),
        GroupOutcome::Restored {
            group,
            location,
            reason,
            durability,
        } => (
            R_RESTORED,
            group,
            location,
            Some(*reason),
            reason.errno(),
            None,
            Some(durability),
        ),
        GroupOutcome::RecoveryKept {
            group,
            location,
            reason,
            errno,
            durability,
        } => (
            R_RECOVERY_KEPT,
            group,
            location,
            Some(*reason),
            *errno,
            None,
            Some(durability),
        ),
        GroupOutcome::Uncertain {
            group,
            locations,
            reason,
            ..
        } => (
            R_UNCERTAIN,
            group,
            &locations[0],
            Some(*reason),
            reason.errno(),
            None,
            None,
        ),
    };
    let a = &loc.anchor;
    let dash = || "-".to_owned();
    write_line(
        &Line::new(kind)
            .pair("op", &hex(op.0))
            .pair("job", job)
            .pair("group", &hex(group.0))
            .pair(
                "actual",
                &loc.path_at_check.as_deref().map_or_else(dash, path_enc),
            )
            .pair("parent", &format!("{}:{}", a.parent_dev, a.parent_ino))
            .pair("object", &format!("{}:{}", a.object_dev, a.object_ino))
            .pair("leaf", &enc(a.leaf.as_bytes()))
            .pair("checked", loc.checked_at.as_str())
            .pair("reason", reason.map_or("-", UndoReason::name))
            .pair("errno", &errno.map_or_else(dash, |e| e.to_string()))
            .pair("info", &info.map_or_else(dash, |i| enc(i.as_bytes())))
            .pair("durability", durability.map_or("-", durability_str)),
    )
}

impl JobRecords {
    /// Counts `n` decoded path bytes kept for the job (checked).
    fn keep(&mut self, n: usize) -> Result<(), Fault> {
        self.path_bytes = self
            .path_bytes
            .checked_add(n)
            .filter(|&t| t <= HISTORY_PATH_BYTES_PER_JOB)
            .ok_or("over the limit: more path bytes in the job than the history allows")?;
        Ok(())
    }

    /// The begun group `id` (with operation `op`, when given).
    fn group(&mut self, id: &str, op: Option<&str>) -> Result<&mut GroupRec, Fault> {
        let id = hex_id(id)?;
        let g = self
            .groups
            .iter_mut()
            .find(|g| g.group == id)
            .ok_or("a record of a group without its begin")?;
        if let Some(op) = op
            && hex_id(op)? != g.op
        {
            return Err("an operation that is not the group's");
        }
        Ok(g)
    }

    /// Takes one parsed line; an error is a framing fault of this job.
    fn take(&mut self, l: &Line, job: &str) -> Result<(), Fault> {
        let mine = |k: &str| l.get(k) == Some(job);
        let ours = if l.kind == "job" {
            mine("id")
        } else {
            mine("job")
        };
        self.take_kind(l, &mine)?;
        if ours && l.kind != "job-start" {
            self.records = true;
        }
        Ok(())
    }

    fn take_kind(&mut self, l: &Line, mine: &dyn Fn(&str) -> bool) -> Result<(), Fault> {
        match l.kind.as_str() {
            "job" if mine("id") => {
                // Exactly the fields `finish` writes; `undo` only as `no`.
                // A cut or altered line is a framing fault, not an
                // accepted job line that lost its `undo=no`.
                let withdrawn = l.items.len() == JOB.len() + 1;
                let v = fields(l, if withdrawn { JOB_WITHDRAWN } else { JOB })?;
                if withdrawn && v[5] != "no" {
                    return Err("a job record with an unknown undo value");
                }
                self.known = true;
                self.withdrawn |= withdrawn;
            }
            "job" => {}
            _ if !mine("job") => {}
            "job-start" => {
                fields(l, START)?;
                if self.started {
                    return Err("a duplicate job start");
                }
                if self.records {
                    return Err("a job start after the job's records");
                }
                self.started = true;
            }
            "undo" => self.undone = true,
            "skip" | "fail" => self.known = true,
            "unknown" => {
                self.known = true;
                self.unknown = true;
            }
            "group-begin" => self.begin(&fields(l, BEGIN)?)?,
            "child" => self.child(&fields(l, CHILD)?)?,
            "group-end" => {
                let v = fields(l, END)?;
                let (n, bytes) = (count(v[2])?, count(v[3])?);
                let g = self.group(v[1], None)?;
                if g.ended {
                    return Err("a duplicate group end");
                }
                if n != g.declared || n != g.children.len() {
                    return Err("a child count that does not match");
                }
                if bytes != g.bytes {
                    return Err("name bytes that do not match");
                }
                g.ended = true;
            }
            "publish-intent" => {
                let v = fields(l, INTENT)?;
                path_dec(v[3])?;
                let target = path_dec(v[4])?;
                self.keep(target.len())?;
                if self.intents >= HISTORY_INTENTS {
                    return Err("over the limit: more intents in the job than the history allows");
                }
                self.intents += 1;
                let g = self.group(v[2], Some(v[0]))?;
                if !g.ended || g.closed() {
                    return Err("an intent out of order");
                }
                g.intents.push(target);
            }
            "publish-commit" => {
                let v = fields(l, COMMIT)?;
                let actual = path_dec(v[3])?;
                self.keep(actual.len())?;
                let synced = match v[4] {
                    "synced" => true,
                    "unconfirmed" => false,
                    _ => return Err("a bad durability"),
                };
                let g = self.group(v[2], Some(v[0]))?;
                if g.closed() {
                    return Err("a duplicate commit");
                }
                // D-24(b): the place is the committed name, which must be
                // one of the intents.
                if !g.intents.contains(&actual) {
                    return Err("a commit at a place no intent named");
                }
                g.commit = Some(actual);
                g.synced = synced;
            }
            "publish-uncertain" => {
                let v = fields(l, UNCERTAIN)?;
                if v[3] != "no" {
                    return Err("an uncertain place said known");
                }
                for n in [v[4], v[5], v[7], v[8]] {
                    num(n)?;
                }
                path_dec(v[6])?;
                let g = self.group(v[2], Some(v[0]))?;
                if g.closed() || g.intents.is_empty() {
                    return Err("an uncertain commit out of order");
                }
                g.uncertain = true;
            }
            "publish-abort" => {
                let v = fields(l, ABORT)?;
                let g = self.group(v[2], Some(v[0]))?;
                if g.closed() {
                    return Err("an abort after the outcome");
                }
                g.aborted = true;
            }
            "recovery-intent" => {
                let v = fields(l, RINTENT)?;
                let (source, target) = (path_dec(v[3])?, path_dec(v[4])?);
                self.keep(source.len())?;
                self.keep(target.len())?;
                let info_rel = count(v[5])?;
                if info_rel > source.len() {
                    return Err("a Trash path outside its source");
                }
                for n in &v[6..13] {
                    num(n)?;
                }
                let date = v[13];
                if date.len() != 19
                    || !date
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"-T:".contains(&b))
                {
                    return Err("a bad deletion date");
                }
                let op = hex_id(v[0])?;
                let source_parent = (num(v[6])?, num(v[7])?);
                let target_parent = (num(v[8])?, num(v[9])?);
                let g = self.group(v[2], None)?;
                if g.commit.is_none() || !g.synced {
                    return Err("an undo of a group not committed");
                }
                if !matches!(
                    g.recovery,
                    Recovery::None | Recovery::Closed(GroupOutcome::Kept { .. })
                ) {
                    return Err("an undo intent out of order");
                }
                g.recovery = Recovery::Open(OpenRec {
                    op,
                    source,
                    target,
                    info_rel,
                    source_parent,
                    target_parent,
                    date: date.to_owned(),
                });
            }
            kind @ (R_KEPT | R_MOVED | R_RESTORED | R_RECOVERY_KEPT | R_UNCERTAIN) => {
                let v = fields(l, RCLOSE)?;
                if let Some(p) = opt(v[3]) {
                    self.keep(path_dec(p)?.len())?;
                }
                self.keep(path_dec(v[6])?.len())?;
                let op = hex_id(v[0])?;
                let g = self.group(v[2], None)?;
                let Recovery::Open(o) = &g.recovery else {
                    return Err("an undo ending without its intent");
                };
                if o.op != op {
                    return Err("an undo ending of another operation");
                }
                g.recovery = Recovery::Closed(closed_outcome(kind, g.group, op, &v)?);
            }
            _ => return Err("an unknown record"),
        }
        Ok(())
    }

    fn begin(&mut self, v: &[&str]) -> Result<(), Fault> {
        self.known = true;
        let (group, op) = (hex_id(v[1])?, hex_id(v[2])?);
        if self.groups.len() >= HISTORY_DIRS {
            return Err("over the limit: more groups in the job than the history allows");
        }
        if self.groups.iter().any(|g| g.group == group) {
            return Err("a duplicate group");
        }
        if self.groups.iter().any(|g| g.op == op) {
            return Err("a duplicate operation");
        }
        if node_kind(v[5])? != NodeKind::Directory {
            return Err("a wrapper that is not a directory");
        }
        self.groups.push(GroupRec {
            group,
            op,
            wrapper: DirectoryId {
                dev: num(v[3])?,
                ino: num(v[4])?,
                kind: NodeKind::Directory,
                uid: small(v[6])?,
                mode: octal(v[7])?,
            },
            declared: count(v[8])?,
            children: Vec::new(),
            bytes: 0,
            ended: false,
            intents: Vec::new(),
            commit: None,
            synced: false,
            uncertain: false,
            aborted: false,
            recovery: Recovery::None,
        });
        Ok(())
    }

    fn child(&mut self, v: &[&str]) -> Result<(), Fault> {
        let rel = path_dec(v[2])?;
        self.keep(rel.len())?;
        if self.outputs >= HISTORY_OUTPUTS {
            return Err("over the limit: more outputs in the job than the history allows");
        }
        self.outputs += 1;
        if rel.is_empty() || rel == b"." || rel == b".." || rel.contains(&b'/') {
            return Err("a child name that is not one plain component");
        }
        let child = ChildBaseline {
            rel: PathBuf::from(OsStr::from_bytes(&rel)),
            file_id: FileId {
                dev: num(v[4])?,
                ino: num(v[5])?,
                size: num(v[9])?,
                mtime_ns: time(v[10])?,
                ctime_ns: time(v[11])?,
            },
            kind: node_kind(v[3])?,
            uid: small(v[6])?,
            mode: octal(v[7])?,
            nlink: num(v[8])?,
        };
        let g = self.group(v[1], None)?;
        if g.ended {
            return Err("a child after its group's end");
        }
        if g.children.len() == g.declared {
            return Err("more children than declared");
        }
        if g.children.iter().any(|c| c.rel == child.rel) {
            return Err("a duplicate child");
        }
        g.bytes += rel.len();
        g.children.push(child);
        Ok(())
    }
}

/// Whether `line` (with or without its newline) is `job\tid=` and then a
/// proper start of `job`: a `job` line cut inside the id.
fn is_job_id_prefix(line: &[u8], job: &str) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_prefix(b"job\tid=")
        .is_some_and(|rest| rest.len() < job.len() && job.as_bytes().starts_with(rest))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// One line as [`read_line_bounded`] found it.
enum Got {
    Eof,
    /// `content` bytes before the newline (all of them, kept or not);
    /// `ended`: the line had its newline (false: cut off by the end).
    Line {
        content: usize,
        ended: bool,
    },
}

/// The next line into `buf`, never more than [`HISTORY_LINE_BYTES`] + 1
/// bytes of it kept (a line over the limit keeps its first bytes, so a
/// mention of a job in it can still be seen, and the rest is dropped
/// without being stored). `buf` ends with the newline exactly when the
/// line has one and is within the limit. Interrupted reads are retried;
/// a failed reservation is `OutOfMemory`.
fn read_line_bounded(rd: &mut dyn BufRead, buf: &mut Vec<u8>) -> io::Result<Got> {
    buf.clear();
    let (mut content, mut any, mut ended) = (0usize, false, false);
    loop {
        let chunk = match rd.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            break;
        }
        any = true;
        let (used, nl) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        content = content.saturating_add(if nl { used - 1 } else { used });
        let keep = (HISTORY_LINE_BYTES + 1).saturating_sub(buf.len()).min(used);
        buf.try_reserve(keep)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        buf.extend_from_slice(&chunk[..keep]);
        rd.consume(used);
        if nl {
            ended = true;
            break;
        }
    }
    Ok(if any {
        Got::Line { content, ended }
    } else {
        Got::Eof
    })
}

/// Reads a whole history stream (header, then lines) and collects the
/// records of one job. Pure: no file, no lock. See [`scan_records`].
#[cfg(any(test, fuzzing))]
fn job_records(rd: &mut dyn BufRead, job: &JobId) -> io::Result<JobRecords> {
    let mut buf = Vec::new();
    rd.take(HEADER_MAX as u64).read_until(b'\n', &mut buf)?;
    let version = parse_header(&buf)?;
    scan_records(rd, version, job, false)
}

/// The lines after the header, for one job. It streams (only lines that
/// mention the job are parsed), so `undo` hands it the locked file and the
/// fuzz target a byte slice. A version 0 history only says whether it names
/// the job (`old`). In version 1 a line that may be the job's (it holds
/// `job=<id>` or `id=<id>`) but is cut off, longer than
/// [`HISTORY_LINE_BYTES`], has more than [`HISTORY_FIELDS`] fields, is not
/// UTF-8 or does not parse, and any framing fault or limit of the job's
/// records, is `broken`: undo then refuses. Other jobs' lines are skipped,
/// however damaged. In a `windowed` read (a window not from the start) a
/// line of the job before its `job-start` makes it `incomplete`.
fn scan_records(
    rd: &mut dyn BufRead,
    version: u32,
    job: &JobId,
    windowed: bool,
) -> io::Result<JobRecords> {
    let mut r = JobRecords::default();
    let mut buf = Vec::new();
    let needle = format!("job={}", job.0);
    let id_needle = format!("id={}", job.0);
    // Only the kept prefix of a long line (`read_line_bounded`: 32 KiB + 1)
    // is searched. Every record names its job in its first three fields,
    // well inside it; a record that ever names the job later must keep it
    // inside the prefix, or its over-long line would be skipped.
    let mentions = |b: &[u8]| contains(b, needle.as_bytes()) || contains(b, id_needle.as_bytes());
    if version != VERSION && version != OLD_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad or unsupported history header",
        ));
    }
    let mut seen_ours = false;
    loop {
        let (content, ended) = match read_line_bounded(rd, &mut buf)? {
            Got::Eof => break,
            Got::Line { content, ended } => (content, ended),
        };
        let over = content > HISTORY_LINE_BYTES;
        if !mentions(&buf) {
            // A `job` line cut inside its id names no job in full, but if
            // what is there could be the start of ours, it may be our
            // withdrawal (`undo=no`) that never landed: refuse. Only after
            // a record of ours: the withdrawal follows the job's own
            // records, and a cut line before them (ids start with the
            // clock, so it is a prefix of nearly every id) must not
            // refuse other, intact jobs. (A cut before the id's first
            // byte stays invisible.)
            if version == VERSION && seen_ours && !over && is_job_id_prefix(&buf, &job.0) {
                r.broken.get_or_insert("a record cut off");
            }
            continue;
        }
        if version == OLD_VERSION {
            r.old = true;
            return Ok(r);
        }
        seen_ours = true;
        let fault = if over {
            Err("over the limit: a record longer than the history's line limit")
        } else if !ended {
            Err("a record cut off")
        } else if buf.iter().filter(|&&b| b == b'\t').count() > HISTORY_FIELDS {
            Err("over the limit: a record with more fields than the history allows")
        } else {
            buf.pop();
            std::str::from_utf8(&buf)
                .map_err(|_| "a record that is not UTF-8")
                .and_then(|t| parse_line(t).map_err(|_| "a garbled record"))
                .and_then(|l| r.take(&l, &job.0))
        };
        if let Err(why) = fault {
            r.broken.get_or_insert(why);
        }
        // Any line naming the job (even a damaged one, or one of another
        // job that holds the needle) before its start: the start may be
        // the one outside the window, so the job may not be whole.
        if windowed && !r.started {
            r.incomplete = true;
        }
    }
    if r.groups.iter().any(|g| !g.ended) {
        r.broken.get_or_insert("a group without its end");
    }
    Ok(r)
}

/// A read of `f` at its own offsets, up to `end`: no shared file offset,
/// nothing past the size the scan began with.
struct At<'a> {
    f: &'a File,
    pos: u64,
    end: u64,
}

impl Read for At<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let left = self.end.saturating_sub(self.pos);
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(b.len());
        if want == 0 {
            return Ok(0);
        }
        let n = self.f.read_at(&mut b[..want], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Why a job that is not provably in the window is refused (rejection copy
/// of `history_tail_window_bytes`).
pub const WINDOW_REFUSAL: &str = "This action's record is older than the history window. Its outputs were kept; nothing was moved.";

/// The records of `job` in the history file `f` (the caller holds the
/// history lock). The header is read by itself, bounded ([`HEADER_MAX`]);
/// then at most [`HISTORY_TAIL_WINDOW`] bytes before the size the file has
/// now, from the first whole line, through a [`HISTORY_READ_CHUNK`] buffer
/// (nothing of the window is held). Lines appended meanwhile are not read.
/// If the window does not start at the history's start, the job must be
/// provably all inside it: its `job-start` has to be in the window, before
/// any other line of it. The append lock orders the records (a job's
/// recorder writes its start before anything else), so no clock is
/// trusted. Otherwise the job is refused as older than the window: a job
/// with records before the window must not be undone as if it were whole.
/// (A version 1 job written before `job-start` existed is refused there
/// too; inside an untruncated history its start is optional.)
fn scan_file(f: &File, job: &JobId) -> io::Result<JobRecords> {
    let size = f.metadata()?.len();
    let Some((version, header_len)) = read_header_len(f)? else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad or unsupported history header",
        ));
    };
    let header_len = header_len as u64;
    let truncated = size.saturating_sub(header_len) > HISTORY_TAIL_WINDOW;
    let start = if truncated {
        size - HISTORY_TAIL_WINDOW
    } else {
        header_len
    };
    let mut rd = BufReader::with_capacity(
        HISTORY_READ_CHUNK,
        At {
            f,
            pos: start,
            end: size,
        },
    );
    if truncated {
        // Start at a line: when the byte before is not a newline, the
        // first line is partly outside the window and is dropped.
        let mut before = [0u8; 1];
        let n = f.read_at(&mut before, start - 1)?;
        if n == 1 && before[0] != b'\n' {
            read_line_bounded(&mut rd, &mut Vec::new())?;
        }
    }
    let windowed = truncated && version == VERSION;
    let r = scan_records(&mut rd, version, job, windowed)?;
    if windowed && (r.incomplete || !r.started) {
        return Err(io::Error::other(WINDOW_REFUSAL));
    }
    Ok(r)
}

/// Fuzz entry point: the history reader on arbitrary bytes. The job id is
/// the first one the input names (`job=<id>` up to the next tab or newline),
/// so the matching branches run; the empty id and a foreign one run too.
#[cfg(fuzzing)]
pub fn fuzz_history(bytes: &[u8]) {
    let named = bytes
        .windows(4)
        .position(|w| w == b"job=")
        .map(|i| &bytes[i + 4..])
        .map(|t| {
            &t[..t
                .iter()
                .position(|&b| b == b'\t' || b == b'\n')
                .unwrap_or(t.len())]
        });
    let ids = [
        String::from_utf8_lossy(named.unwrap_or_default()).into_owned(),
        String::new(),
        "0-0".to_owned(),
    ];
    for id in ids {
        let _ = job_records(&mut &bytes[..], &JobId(id));
    }
}

/// How long `undo` and the history's makers wait for a lock before giving
/// up: a held lock is another run making the history or another undo, each
/// a matter of milliseconds; past 2 s something hangs, and waiting longer
/// would only hold up the user.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_STEP: Duration = Duration::from_millis(50);

/// Takes the exclusive `flock` without ever blocking for good: retries
/// every 50 ms until `wait` has passed, then `WouldBlock` "history is busy".
fn lock_with_deadline(file: &File, wait: Duration) -> io::Result<()> {
    let end = Instant::now() + wait;
    loop {
        // SAFETY: the fd is open for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) && e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
        if Instant::now() >= end {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "history is busy"));
        }
        std::thread::sleep(LOCK_STEP);
    }
}

/// `OLD_REFUSAL` if an archived version 0 history names the job, else
/// "unknown job".
fn old_or_unknown(dir: &StateDir, job: &JobId) -> io::Error {
    for n in 0..ARCHIVE_ATTEMPTS {
        let Ok(f) = dir.open_file(&archive_name(n), libc::O_RDONLY, 0) else {
            continue;
        };
        if scan_file(&f, job).is_ok_and(|r| r.old) {
            return io::Error::other(OLD_REFUSAL);
        }
    }
    io::Error::new(io::ErrorKind::NotFound, "unknown job")
}

/// D-25: where a group without its commit record is, if anywhere: an
/// intent's target that holds the wrapper itself (same device and inode,
/// never through a symlink). A lookalike at a target is not it.
fn recovered_place(g: &GroupRec) -> Option<PathBuf> {
    g.intents.iter().find_map(|t| {
        let st = stat_at(libc::AT_FDCWD, &c_name(t).ok()?).ok()?;
        (st.st_mode & libc::S_IFMT == libc::S_IFDIR && ident(&st) == (g.wrapper.dev, g.wrapper.ino))
            .then(|| PathBuf::from(OsStr::from_bytes(t)))
    })
}

/// Why undo refuses the job, if it does, before anything is touched.
fn refusal(r: &JobRecords) -> Option<io::Error> {
    if r.old {
        return Some(io::Error::other(OLD_REFUSAL));
    }
    if r.undone {
        return Some(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "already undone",
        ));
    }
    // A group whose place or baseline was not proved could be anything by
    // now.
    if r.unknown || r.groups.iter().any(|g| g.uncertain) {
        return Some(io::Error::other(UNKNOWN_REFUSAL));
    }
    if let Some(why) = &r.broken {
        if why.starts_with("over the limit") {
            return Some(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("History exceeds the safety limit ({why}). No recovery changes were made."),
            ));
        }
        return Some(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("this job's history record is damaged ({why}); nothing undone"),
        ));
    }
    if let Some(g) = r.groups.iter().find(|g| g.commit.is_none() && !g.aborted) {
        let place = match recovered_place(g) {
            Some(p) => format!("its folder is at {}, ", p.display()),
            None => String::new(),
        };
        return Some(io::Error::other(format!(
            "an output's record is incomplete ({place}the run ended before its commit was recorded); nothing undone"
        )));
    }
    if r.groups.iter().any(|g| g.commit.is_some() && !g.synced) {
        return Some(io::Error::other(UNCONFIRMED_REFUSAL));
    }
    if r.withdrawn {
        return Some(io::Error::other(WITHDRAWN_REFUSAL));
    }
    None
}

/// The history as the Trash undo writes it: each record appended under
/// the append lock and synced.
struct HistLog<'a> {
    file: &'a mut File,
    lock: &'a File,
    job: String,
}

impl trash::Log for HistLog<'_> {
    fn intent(&mut self, i: &trash::Intent) -> io::Result<()> {
        let line = format!(
            "recovery-intent\top={}\tjob={}\tgroup={}\tsource={}\ttarget={}\tinfo_rel={}\tsource_parent_dev={}\tsource_parent_ino={}\ttarget_parent_dev={}\ttarget_parent_ino={}\twrapper_dev={}\twrapper_ino={}\tmount={}\tdeletion_date={}\n",
            hex(i.op.0),
            self.job,
            hex(i.group.0),
            enc(i.source),
            enc(i.target),
            i.info_rel,
            i.source_parent.0,
            i.source_parent.1,
            i.target_parent.0,
            i.target_parent.1,
            i.wrapper.0,
            i.wrapper.1,
            i.mount,
            i.date,
        );
        if line.len() > RECOVERY_INTENT_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery intent exceeds the safety limit",
            ));
        }
        append(self.file, self.lock, true, |b| b.text(&line))
    }

    fn close(&mut self, op: OperationId, o: &GroupOutcome) -> io::Result<()> {
        let line = close_line(&self.job, op, o);
        append(self.file, self.lock, true, |b| b.text(&line))
    }
}

/// Undoes a job by moving each committed group, whole, into the Trash
/// (see [`trash`]): never deleting, never overwriting. A group that
/// changed since publication (any child, the wrapper, an extra or missing
/// child) is kept whole; one that differs after the move goes back; each
/// group gets its own typed outcome, and other groups are not undone
/// because of it. The whole job is refused (`Err`), before anything moves,
/// when any record is incomplete, damaged or unknown, when a group's place
/// or durability was not confirmed, and for a job of the previous history
/// format; after the first move, problems are outcomes and warnings, never
/// an `Err`. The history is held under an exclusive `flock` for the whole
/// undo (waiting at most 2 s), so two undos of one job cannot both run. A
/// job that is still running is refused (`ResourceBusy`). An undo a crash
/// interrupted is resumed from its records: endings are reported as
/// recorded, an open intent is recovered (see [`trash::recover`]).
///
/// The history is streamed: only lines that mention the job are parsed.
pub fn undo(job: &JobId) -> io::Result<UndoResult> {
    // Not knowing which jobs run is an error: nothing is moved on a guess.
    if list_running()?.iter().any(|(id, ..)| id == job) {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "job is running; cancel it first",
        ));
    }
    let dir = match StateDir::open(&history_dir()?) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(io::ErrorKind::NotFound, "unknown job"));
        }
        Err(e) => return Err(e),
    };
    let mut file = match dir.open_file(HISTORY, libc::O_RDWR | libc::O_APPEND, 0) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(old_or_unknown(&dir, job)),
        Err(e) => return Err(e),
    };
    lock_with_deadline(&file, LOCK_WAIT)?;
    let r = scan_file(&file, job)?;
    // Taken before anything moves: the records need it.
    let lock = dir.open_file(INIT_LOCK, libc::O_RDWR | libc::O_CREAT, 0o600)?;
    if !r.known && !r.old && !r.undone {
        return Err(old_or_unknown(&dir, job));
    }
    if let Some(e) = refusal(&r) {
        return Err(e);
    }
    if !r.known {
        return Err(io::Error::new(io::ErrorKind::NotFound, "unknown job"));
    }
    // A line another run left cut off would swallow our first record.
    end_line(&mut file, &lock)?;
    let mut res = UndoResult::default();
    let mut log = HistLog {
        file: &mut file,
        lock: &lock,
        job: manifest::escape(&job.0),
    };
    for g in &r.groups {
        let Some(place) = &g.commit else { continue };
        let grp = trash::Group {
            group: GroupId(g.group),
            place,
            wrapper: g.wrapper,
            children: &g.children,
        };
        let warn = &mut res.warnings;
        let o = match &g.recovery {
            Recovery::Closed(o) if !matches!(o, GroupOutcome::Kept { .. }) => o.clone(),
            Recovery::Open(o) => {
                let open = trash::Open {
                    op: OperationId(o.op),
                    source: &o.source,
                    target: &o.target,
                    info_rel: o.info_rel,
                    source_parent: o.source_parent,
                    target_parent: o.target_parent,
                    date: &o.date,
                };
                match trash::recover(&grp, &open, &mut log, warn) {
                    Some(o) => o,
                    None => trash::undo_group(&grp, &mut log, warn),
                }
            }
            _ => trash::undo_group(&grp, &mut log, warn),
        };
        res.groups.push(o);
    }
    let moved = res
        .groups
        .iter()
        .filter(|o| matches!(o, GroupOutcome::MovedToTrash { .. }))
        .count();
    let l = Line::new("undo")
        .pair("job", &job.0)
        .pair("time", &unix_secs().to_string())
        .pair("moved", &moved.to_string())
        .pair("other", &(res.groups.len() - moved).to_string());
    if let Err(e) = append(log.file, &lock, true, |b| b.line(&l)) {
        res.warnings
            .push(format!("history: the undo was not recorded: {e}"));
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{AnchorHint, CheckPoint, GroupId, LocationEvidence, OperationId};
    use crate::output::{
        self, ChildBaseline, DirectoryId, NodeKind, OutputGroup, Publication, PublicationReason,
    };
    use crate::xdg::tests::with_env;
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A private root per test. Unique per call (tag, pid and a counter):
    /// two tests sharing a tag once shared the directory, and one's cleanup
    /// removed the other's files while it waited for the environment lock.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "engine-journal-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            for d in ["state", "run", "out", "data"] {
                fs::create_dir_all(p.join(d)).unwrap();
            }
            Tmp(p)
        }
        fn env<R>(&self, f: impl FnOnce() -> R) -> R {
            let s = self.0.join("state");
            let r = self.0.join("run");
            let d = self.0.join("data");
            with_env(
                &[
                    ("XDG_STATE_HOME", Some(s.to_str().unwrap())),
                    ("XDG_RUNTIME_DIR", Some(r.to_str().unwrap())),
                    // Each test's own Trash, never the user's.
                    ("XDG_DATA_HOME", Some(d.to_str().unwrap())),
                ],
                f,
            )
        }
        fn out(&self) -> PathBuf {
            self.0.join("out")
        }
        fn trash(&self) -> PathBuf {
            self.0.join("data/Trash")
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn history_path() -> PathBuf {
        history_dir().unwrap().join(HISTORY)
    }

    fn history() -> String {
        fs::read_to_string(history_path()).unwrap()
    }

    type Before<'a> = &'a mut dyn FnMut(&OutputGroup, &Path) -> io::Result<()>;

    /// One input's outputs (`(name, content)`) published into `out/` by the
    /// real publication, with `rec` writing its intent as the executor's
    /// `before_commit` does, then `extra`.
    fn try_publish(
        t: &Tmp,
        rec: &mut Recorder,
        stem: &str,
        files: &[(&str, &str)],
        extra: Before,
    ) -> io::Result<Publication> {
        let files: Vec<(&std::ffi::OsStr, &str)> = files
            .iter()
            .map(|(n, b)| (std::ffi::OsStr::new(*n), *b))
            .collect();
        try_publish_raw(t, rec, stem, &files, extra)
    }

    fn try_publish_raw(
        t: &Tmp,
        rec: &mut Recorder,
        stem: &str,
        files: &[(&std::ffi::OsStr, &str)],
        extra: Before,
    ) -> io::Result<Publication> {
        static N: AtomicU32 = AtomicU32::new(0);
        let root =
            t.0.join(format!("producer-{}", N.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(root.join("0")).unwrap();
        let mut rels = Vec::new();
        for (name, body) in files {
            let rel = Path::new("0").join(name);
            fs::write(root.join(&rel), body).unwrap();
            rels.push(rel);
        }
        output::publish_beneath(&root, &rels, &t.out(), stem, "md", &mut |g, p| {
            rec.prepare_group(g, p)?;
            extra(g, p)
        })
    }

    /// What the executor does after a commit: the group record, then one
    /// `Done` per file with its private baseline. Returns the files.
    fn commit(rec: &mut Recorder, input: &str, p: &Publication) -> Vec<PathBuf> {
        rec.group(&p.group, &p.location, &p.durability);
        let files = output::postcheck(p).unwrap();
        for (path, id) in &files {
            rec.outcome(&Outcome::Done {
                input: input.into(),
                output: path.clone(),
                id: *id,
            });
        }
        files.into_iter().map(|(p, _)| p).collect()
    }

    /// A published, recorded group: its files.
    fn publish(t: &Tmp, rec: &mut Recorder, stem: &str, files: &[(&str, &str)]) -> Vec<PathBuf> {
        let p = try_publish(t, rec, stem, files, &mut |_, _| Ok(())).unwrap();
        commit(rec, &format!("/in/{stem}"), &p)
    }

    /// A whole job of single-file groups `(stem, content)`.
    fn job_of(t: &Tmp, job: &JobId, groups: &[(&str, &str)]) -> Vec<PathBuf> {
        let mut rec = Recorder::open(job, "convert");
        let mut files = Vec::new();
        for (stem, body) in groups {
            files.extend(publish(t, &mut rec, stem, &[("x.md", body)]));
        }
        let r = rec.finish(true);
        assert!(r.undoable, "{:?}", r.warning);
        files
    }

    /// Whether every group went to the Trash and none of `files` is left.
    fn all_moved(r: &UndoResult, files: &[PathBuf]) -> bool {
        !r.groups.is_empty()
            && r.groups
                .iter()
                .all(|o| matches!(o, GroupOutcome::MovedToTrash { .. }))
            && files.iter().all(|f| fs::symlink_metadata(f).is_err())
    }

    /// The reasons of the `Kept` groups.
    fn kept(r: &UndoResult) -> Vec<UndoReason> {
        r.groups
            .iter()
            .filter_map(|o| match o {
                GroupOutcome::Kept { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    }

    fn unknown(input: &str) -> Outcome {
        Outcome::PublishedUnknown {
            input: PathBuf::from(input),
            group: GroupId(0xab),
            location: LocationEvidence {
                path_at_check: None,
                last_unverified_path: None,
                anchor: AnchorHint {
                    parent_dev: 1,
                    parent_ino: 2,
                    object_dev: 1,
                    object_ino: 3,
                    leaf: std::ffi::OsStr::from_bytes(b"b\xff%").into(),
                    op: OperationId(0xcd),
                },
                checked_at: CheckPoint::PostPublish,
            },
            reason: PublicationReason::AncestorChanged,
            durability: Durability::Unconfirmed("folder sync failed".into()),
        }
    }

    // ---- recording ----

    /// The whole manifest and the intent are on disk (synced) before the
    /// rename, the commit after it; the history has no digest of any kind.
    #[test]
    fn intent_is_written_before_the_rename_and_the_commit_after() {
        let t = Tmp::new("order");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let target = t.out().join("doc");
            let mut seen = None;
            let p = try_publish(
                &t,
                &mut rec,
                "doc",
                &[("1.md", "one"), ("2.md", "two")],
                &mut |_, _| {
                    seen = Some((history(), target.exists()));
                    Ok(())
                },
            )
            .unwrap();
            let (before, visible) = seen.unwrap();
            assert!(!visible, "intent written after the rename");
            let kinds: Vec<String> = before
                .lines()
                .skip(1)
                .map(|l| parse_line(l).unwrap().kind)
                .collect();
            assert_eq!(
                kinds,
                [
                    "job-start",
                    "group-begin",
                    "child",
                    "child",
                    "group-end",
                    "publish-intent"
                ]
            );
            assert!(before.starts_with("inacircle-history 1\n"));
            commit(&mut rec, "/in/doc", &p);
            let after = history();
            let last = parse_line(after.lines().last().unwrap()).unwrap();
            assert_eq!(last.kind, "publish-commit");
            assert_eq!(last.get("durability"), Some("synced"));
            assert_eq!(
                last.get("actual"),
                Some(&*enc(target.as_os_str().as_bytes()))
            );
            for word in ["digest", "hash", "sha"] {
                assert!(!after.contains(word), "{after}");
            }
            let r = rec.finish(true);
            assert!(r.undoable && r.warning.is_none(), "{:?}", r.warning);
            assert_eq!(r.tally.counts(), (1, 0, 0));
        });
    }

    /// The baseline T6 fixed in private is what the history holds, field
    /// for field: read back, it equals the group (no restat, no digest).
    #[test]
    fn recorded_baseline_equals_the_published_group() {
        let t = Tmp::new("baseline");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let p = try_publish(
                &t,
                &mut rec,
                "doc",
                &[("a.md", "a"), ("b\tc%\u{e9}.md", "b")],
                &mut |_, _| Ok(()),
            )
            .unwrap();
            commit(&mut rec, "/in/doc", &p);
            rec.finish(true);
            let r = job_records(&mut history().as_bytes(), &job).unwrap();
            assert!(r.broken.is_none(), "{:?}", r.broken);
            let [g] = &r.groups[..] else {
                panic!("{:?}", r.groups)
            };
            assert_eq!(
                (g.group, g.op, g.wrapper),
                (p.group.group.0, p.group.op.0, p.group.id)
            );
            assert_eq!(g.children, p.group.children);
            assert_eq!(
                g.commit.as_deref(),
                Some(p.location.path_at_check.unwrap().as_os_str().as_bytes())
            );
        });
    }

    /// D-24(b): a name taken at the rename re-runs the intent for the same
    /// group and operation; only the committed name is the group's place,
    /// and the other name's folder is not touched by undo.
    #[test]
    fn a_retried_name_adds_an_intent_and_only_the_committed_name_counts() {
        let t = Tmp::new("retry");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let theirs = t.out().join("a");
            let mut first = true;
            let p = try_publish(&t, &mut rec, "a", &[("x.md", "x")], &mut |_, _| {
                if std::mem::take(&mut first) {
                    fs::create_dir(&theirs).unwrap();
                    fs::write(theirs.join("a.md"), "theirs").unwrap();
                }
                Ok(())
            })
            .unwrap();
            let files = commit(&mut rec, "/in/a", &p);
            assert_eq!(files, [t.out().join("a (1)/a.md")]);
            let r = rec.finish(true);
            assert!(r.undoable, "{:?}", r.warning);
            let h = history();
            let count = |k: &str| h.lines().filter(|l| l.starts_with(k)).count();
            assert_eq!((count("group-begin\t"), count("publish-intent\t")), (1, 2));
            let u = undo(&job).unwrap();
            assert!(all_moved(&u, &files), "{u:?}");
            assert!(!t.out().join("a (1)").exists());
            assert_eq!(fs::read_to_string(theirs.join("a.md")).unwrap(), "theirs");
        });
    }

    /// A publication that failed after its intent (a cancel, no free name)
    /// is closed with `publish-abort`; the job stays undoable.
    #[test]
    fn a_publication_that_fails_after_its_intent_is_aborted() {
        let t = Tmp::new("abort");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let a = publish(&t, &mut rec, "a", &[("x.md", "a")]);
            let e = try_publish(&t, &mut rec, "b", &[("x.md", "b")], &mut |_, _| {
                Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
            })
            .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Interrupted);
            assert!(!t.out().join("b").exists());
            let r = rec.finish(true);
            assert!(r.undoable, "{:?}", r.warning);
            assert!(history().contains("publish-abort\t"), "{}", history());
            assert!(all_moved(&undo(&job).unwrap(), &a));
        });
    }

    /// D-25: after a crash between the rename and the commit record, an
    /// intent's target is the group's only if it holds the wrapper (same
    /// device and inode); a lookalike there is not. Either way the job is
    /// refused and nothing is removed.
    #[test]
    fn an_intent_target_is_accepted_only_with_the_wrapper_identity() {
        let t = Tmp::new("crash");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let p = try_publish(&t, &mut rec, "a", &[("x.md", "a")], &mut |_, _| Ok(())).unwrap();
            // The process dies here: no commit record, no finish.
            drop(rec);
            let place = p.location.path_at_check.unwrap();
            let e = undo(&job).unwrap_err();
            let msg = e.to_string();
            assert!(msg.contains("incomplete"), "{msg}");
            assert!(msg.contains(&*place.to_string_lossy()), "{msg}");
            assert!(place.join("a.md").exists());
            // The real wrapper moves away, a lookalike takes the name.
            fs::rename(&place, t.out().join("moved")).unwrap();
            fs::create_dir(&place).unwrap();
            fs::write(place.join("a.md"), "a").unwrap();
            let msg = undo(&job).unwrap_err().to_string();
            assert!(msg.contains("incomplete"), "{msg}");
            assert!(!msg.contains(&*place.to_string_lossy()), "{msg}");
            assert!(place.join("a.md").exists() && t.out().join("moved/a.md").exists());
        });
    }

    /// An unconfirmed publication (a sync after the commit failed) is
    /// recorded as such: no Undo, a warning, and undo refuses.
    #[test]
    fn an_unconfirmed_publication_is_not_undoable() {
        let t = Tmp::new("unconfirmed");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let mut p =
                try_publish(&t, &mut rec, "a", &[("x.md", "a")], &mut |_, _| Ok(())).unwrap();
            p.durability = Durability::Unconfirmed("folder sync failed".into());
            let files = commit(&mut rec, "/in/a", &p);
            let r = rec.finish(true);
            assert_eq!(r.tally.counts(), (1, 0, 0));
            assert!(!r.undoable);
            let w = r.warning.unwrap();
            assert!(w.contains("not confirmed on disk"), "{w}");
            assert!(history().contains("durability=unconfirmed"));
            let e = undo(&job).unwrap_err();
            assert_eq!(e.to_string(), UNCONFIRMED_REFUSAL);
            assert!(files[0].exists());
        });
    }

    /// A publication of unknown place counts as a conversion once, however
    /// often it is seen, is recorded with its operation and a lossless
    /// anchor leaf, makes the job not undoable (sticky, with a warning),
    /// and undo refuses the whole job before removing anything.
    #[test]
    fn published_unknown_counts_once_warns_and_refuses_undo() {
        let t = Tmp::new("unknown");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let a = publish(&t, &mut rec, "a", &[("x.md", "a")]);
            rec.outcome(&unknown("/in/b"));
            rec.outcome(&unknown("/in/b"));
            let closed = rec.finish(true);
            assert_eq!(closed.tally.counts(), (2, 0, 0));
            assert!(!closed.undoable);
            let warning = closed.warning.unwrap();
            assert!(warning.starts_with("1 input(s) published"), "{warning}");
            assert!(warning.contains("undo unavailable"));
            let hist = history();
            let line = hist.lines().find(|l| l.starts_with("unknown\t")).unwrap();
            for part in [
                "\tin=/in/b\t",
                "\tgroup=00000000000000ab\t",
                "\top=00000000000000cd\t",
                "\treason=AncestorChanged\t",
                "\tdurability=unconfirmed\t",
                "\tparent=1:2\t",
                "\tobject=1:3\t",
                "\tleaf=b%FF%25",
            ] {
                assert!(line.contains(part), "{part}: {line}");
            }
            assert!(!line.contains("\tpath="), "{line}");
            assert!(
                hist.lines().last().unwrap().ends_with("\tundo=no"),
                "{hist}"
            );
            let e = undo(&job).unwrap_err();
            assert_eq!(e.to_string(), UNKNOWN_REFUSAL);
            assert!(a[0].exists(), "undo removed a file of a refused job");
            // Refused, not done: still refused, not "already undone".
            assert_eq!(undo(&job).unwrap_err().to_string(), UNKNOWN_REFUSAL);
            // A run that dies before its job line is refused all the same.
            let job2 = new_job_id();
            let mut rec = Recorder::open(&job2, "convert");
            rec.outcome(&unknown("/in/c"));
            drop(rec);
            assert_eq!(undo(&job2).unwrap_err().to_string(), UNKNOWN_REFUSAL);
        });
    }

    /// A committed group whose place could not be proved is recorded as
    /// `publish-uncertain` (anchor, no invented path): not undoable.
    #[test]
    fn a_commit_of_unknown_place_is_recorded_uncertain() {
        let t = Tmp::new("uncertain");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let p = try_publish(&t, &mut rec, "a", &[("x.md", "a")], &mut |_, _| Ok(())).unwrap();
            let mut location = p.location.clone();
            location.path_at_check = None;
            rec.group(&p.group, &location, &p.durability);
            let o = unknown("/in/a");
            rec.outcome(&o);
            let r = rec.finish(true);
            assert!(!r.undoable);
            let h = history();
            let line = h
                .lines()
                .find(|l| l.starts_with("publish-uncertain\t"))
                .unwrap();
            assert!(line.contains("\tlocation_known=no\t"), "{line}");
            assert!(!h.contains("publish-commit\t"), "{h}");
            assert_eq!(undo(&job).unwrap_err().to_string(), UNKNOWN_REFUSAL);
            assert!(t.out().join("a/a.md").exists());
        });
    }

    #[test]
    fn finish_writes_status_and_returns_tally() {
        let t = Tmp::new("finish");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            publish(&t, &mut rec, "a", &[("x.md", "a")]);
            rec.outcome(&Outcome::Failed {
                input: "/in/b".into(),
                reason: "broken".into(),
            });
            let closed = rec.finish(false);
            assert_eq!(closed.tally.counts(), (1, 0, 1));
            assert!(closed.undoable && closed.warning.is_none());
            let hist = history();
            let last = hist.lines().last().unwrap();
            assert!(last.starts_with(&format!("job\tid={}\t", job.0)), "{last}");
            assert!(
                last.ends_with("\taction=convert\tstatus=partial\tsandbox=no"),
                "{last}"
            );
        });
    }

    // ---- recording failures (child processes with injected faults) ----

    /// Runs the child side `case` of `helper_case` in a fresh process of
    /// this binary (its own rlimits and seccomp filters).
    fn assert_case_ok(case: &str) {
        let t = Tmp::new(case);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "journal::tests::helper_case",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("INACIRCLE_HELPER", &t.0)
            .env("INACIRCLE_CASE", case)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "case {case} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("ran case"));
    }

    /// Lets the history grow by `n` more bytes only: the next write that
    /// crosses it is cut short there (`SIGXFSZ` ignored, so it returns the
    /// bytes it wrote, then `EFBIG`). Child side only.
    fn limit_history_growth(n: u64) {
        let len = fs::metadata(history_path()).unwrap().len();
        let lim = libc::rlimit {
            rlim_cur: len + n,
            rlim_max: libc::RLIM_INFINITY,
        };
        // SAFETY: plain calls with valid arguments; this is a helper
        // process of its own.
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &lim), 0);
        }
    }

    #[test]
    fn a_history_sync_failure_after_the_commit_keeps_the_output() {
        assert_case_ok("commit_fsync_fails");
    }

    #[test]
    fn a_cut_job_line_after_a_failed_sync_refuses_undo() {
        assert_case_ok("commit_fsync_fails_job_line_cut");
    }

    #[test]
    fn a_short_history_write_after_the_commit_keeps_the_output() {
        assert_case_ok("commit_write_short");
    }

    #[test]
    fn a_short_history_write_before_the_rename_refuses_the_publication() {
        assert_case_ok("prepare_write_short");
    }

    /// Child side of the fault cases; a no-op without the env.
    #[test]
    fn helper_case() {
        let (Some(dir), Some(case)) = (
            std::env::var_os("INACIRCLE_HELPER"),
            std::env::var_os("INACIRCLE_CASE"),
        ) else {
            return;
        };
        // SAFETY: alarm has no preconditions; a hang dies by SIGALRM.
        unsafe { libc::alarm(10) };
        let t = Tmp(PathBuf::from(dir));
        // The parent's Tmp owns the directory.
        let t = std::mem::ManuallyDrop::new(t);
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            match case.to_str().unwrap() {
                "commit_fsync_fails" | "commit_write_short" => {
                    let p = try_publish(&t, &mut rec, "a", &[("x.md", "mine")], &mut |_, _| Ok(()))
                        .unwrap();
                    if case == "commit_fsync_fails" {
                        crate::output::tests::deny_syscalls(
                            &[libc::SYS_fsync, libc::SYS_fdatasync],
                            libc::EIO as u32,
                        );
                    } else {
                        limit_history_growth(10);
                    }
                    let files = commit(&mut rec, "/in/a", &p);
                    let r = rec.finish(true);
                    // The output and its success stay; only Undo goes.
                    assert_eq!(r.tally.counts(), (1, 0, 0));
                    assert!(!r.undoable);
                    let w = r.warning.unwrap();
                    assert!(w.contains("history not written"), "{w}");
                    assert!(w.contains("undo unavailable"), "{w}");
                    if case == "commit_fsync_fails" {
                        // The withdrawal was tried synced, and failed too.
                        assert!(w.contains("could not be made durable"), "{w}");
                    }
                    assert_eq!(fs::read_to_string(&files[0]).unwrap(), "mine");
                    let e = undo(&job).unwrap_err();
                    assert_ne!(e.kind(), io::ErrorKind::NotFound, "{e}");
                    assert!(files[0].exists(), "undo removed after a refusal");
                }
                "commit_fsync_fails_job_line_cut" => {
                    let p = try_publish(&t, &mut rec, "a", &[("x.md", "mine")], &mut |_, _| Ok(()))
                        .unwrap();
                    crate::output::tests::deny_syscalls(
                        &[libc::SYS_fsync, libc::SYS_fdatasync],
                        libc::EIO as u32,
                    );
                    let files = commit(&mut rec, "/in/a", &p);
                    // The commit line is in (its sync failed); the `job`
                    // line is cut inside, past its id so the reader sees it.
                    limit_history_growth(30);
                    let r = rec.finish(true);
                    assert!(!r.undoable);
                    // What a later `undo` would read (its own newline write
                    // cannot run here, fsync stays denied): the job is
                    // refused by the reader, not left looking complete.
                    let bytes = fs::read(history_path()).unwrap();
                    let rec = job_records(&mut &bytes[..], &job).unwrap();
                    assert_eq!(
                        rec.broken,
                        Some("a record cut off"),
                        "{:?}",
                        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(120)..])
                    );
                    assert!(files[0].exists());
                }
                "prepare_write_short" => {
                    limit_history_growth(10);
                    let e = try_publish(&t, &mut rec, "a", &[("x.md", "mine")], &mut |_, _| Ok(()))
                        .unwrap_err();
                    assert!(e.to_string().contains("history not written"), "{e}");
                    // Nothing visible, the private copy gone.
                    let names: Vec<_> = fs::read_dir(t.out()).unwrap().collect();
                    assert!(names.is_empty(), "{names:?}");
                    let r = rec.finish(true);
                    assert!(!r.undoable);
                    assert!(r.warning.unwrap().contains("history not written"));
                }
                c => panic!("unknown case {c}"),
            }
        });
        println!("ran case");
    }

    // ---- read-only opens never block ----

    /// A FIFO planted at the name of a registry entry or of an archive is
    /// not opened for reading (it would block for ever): the entry is
    /// skipped, the archive is "not there".
    #[test]
    fn a_fifo_in_place_of_a_registry_entry_or_archive_does_not_block() {
        use crate::sys::tests::{mkfifo, within};
        let t = Tmp::new("fifo");
        t.env(|| {
            let job = JobId("10-3".into());
            let running = Running::start(&job, "convert", 1);
            mkfifo(&jobs_dir().unwrap().join("planted"));
            let jobs = within(5, || list_running().unwrap());
            assert_eq!(jobs.len(), 1, "{jobs:?}");
            drop(running);

            let rec = Recorder::open(&new_job_id(), "a");
            rec.finish(true);
            mkfifo(&history_dir().unwrap().join("history.v0"));
            let e = within(5, || undo(&JobId("1-1".into())).unwrap_err());
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        });
    }

    // ---- private state files ----

    /// A same-user process plants symlinks at every temp name the old code
    /// would use: their targets' bytes stay as they were.
    #[test]
    fn temp_symlinks_of_history_and_registry_are_not_followed() {
        let t = Tmp::new("tempsym");
        t.env(|| {
            let sentinel = t.0.join("sentinel");
            fs::write(&sentinel, "SENTINEL").unwrap();
            let state = history_dir().unwrap();
            fs::create_dir_all(&state).unwrap();
            for seq in 0..8 {
                let name = format!(".history.{}.{seq}.tmp", std::process::id());
                symlink(&sentinel, state.join(name)).unwrap();
            }
            let jobs = jobs_dir().unwrap();
            fs::create_dir_all(&jobs).unwrap();
            let job = JobId("10-1".into());
            symlink(&sentinel, jobs.join(".10-1.tmp")).unwrap();
            let running = Running::start(&job, "convert", 1);
            let rec = Recorder::open(&job, "convert");
            assert!(rec.finish(true).undoable);
            assert!(history().starts_with("inacircle-history 1\n"));
            assert_eq!(list_running().unwrap().len(), 1);
            drop(running);
            assert_eq!(fs::read_to_string(&sentinel).unwrap(), "SENTINEL");
        });
    }

    /// The history name itself a symlink: never followed, the run records
    /// nothing and publishes nothing.
    #[test]
    fn a_symlinked_history_is_refused() {
        let t = Tmp::new("histsym");
        t.env(|| {
            let sentinel = t.0.join("sentinel");
            fs::write(&sentinel, "inacircle-history 1\n").unwrap();
            fs::create_dir_all(history_dir().unwrap()).unwrap();
            symlink(&sentinel, history_path()).unwrap();
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let e =
                try_publish(&t, &mut rec, "a", &[("x.md", "a")], &mut |_, _| Ok(())).unwrap_err();
            assert!(e.to_string().contains("history not written"), "{e}");
            assert!(!t.out().join("a").exists());
            assert!(!rec.finish(true).undoable);
            assert_eq!(
                fs::read_to_string(&sentinel).unwrap(),
                "inacircle-history 1\n"
            );
        });
    }

    /// A state folder writable by others is tightened before use; one that
    /// is a symlink is refused.
    #[test]
    fn state_folder_properties_are_checked() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("stateperm");
        t.env(|| {
            let dir = history_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
            let rec = Recorder::open(&new_job_id(), "a");
            assert!(rec.finish(true).undoable);
            assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o755);
            let elsewhere = t.0.join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            fs::rename(&dir, t.0.join("old-state")).unwrap();
            symlink(&elsewhere, &dir).unwrap();
            let rec = Recorder::open(&new_job_id(), "a");
            assert!(!rec.finish(true).undoable);
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        });
    }

    /// The registry works through the folder it opened: a folder swapped
    /// for a symlink later is never written through, and a job file
    /// replaced by another file is not removed at the end.
    #[test]
    fn registry_folder_and_file_swaps_are_not_followed() {
        let t = Tmp::new("regswap");
        t.env(|| {
            let jobs = jobs_dir().unwrap();
            let job = JobId("10-2".into());
            let mut r = Running::start(&job, "convert", 4);
            let moved = t.0.join("moved-jobs");
            fs::rename(&jobs, &moved).unwrap();
            let elsewhere = t.0.join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            symlink(&elsewhere, &jobs).unwrap();
            std::thread::sleep(UPDATE_EVERY + Duration::from_millis(20));
            r.update(3, 0.75);
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
            let f = fs::read_to_string(moved.join("10-2")).unwrap();
            assert!(f.contains("\tdone=3\t"), "{f}");
            // The user's own file under the job's name stays.
            fs::remove_file(moved.join("10-2")).unwrap();
            fs::write(moved.join("10-2"), "mine").unwrap();
            drop(r);
            assert_eq!(fs::read_to_string(moved.join("10-2")).unwrap(), "mine");
        });
    }

    // ---- history versions ----

    const V0: &str = "inacircle-history 0\nout\tjob=1-1\tin=/i\tpath=/o/a\tsize=1\tmtime=2\tctime=3\tdev=4\tino=5\njob\tid=1-1\ttime=1\taction=a\tstatus=done\tsandbox=yes\n";

    /// A version 0 history is archived (no overwrite, no delete) under the
    /// init lock and a private version 1 history published; an old job is
    /// refused with its reason, its outputs kept.
    #[test]
    fn a_v0_history_is_archived_and_its_jobs_refused() {
        let t = Tmp::new("v0");
        t.env(|| {
            let dir = history_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let old_out = t.out().join("old.md");
            fs::write(&old_out, "o").unwrap();
            fs::write(history_path(), V0).unwrap();
            let job = new_job_id();
            job_of(&t, &job, &[("a", "a")]);
            assert_eq!(fs::read_to_string(dir.join("history.v0")).unwrap(), V0);
            assert!(history().starts_with("inacircle-history 1\n"));
            assert_eq!(fs::metadata(history_path()).unwrap().mode() & 0o777, 0o600);
            let e = undo(&JobId("1-1".into())).unwrap_err();
            assert_eq!(e.to_string(), OLD_REFUSAL);
            assert!(old_out.exists());
            // A later v0 history (a downgrade, then this version again) gets
            // the next free archive name.
            fs::rename(history_path(), dir.join("v1-saved")).unwrap();
            fs::write(history_path(), V0.replace("1-1", "2-2")).unwrap();
            drop(Recorder::open(&new_job_id(), "a"));
            assert_eq!(fs::read_to_string(dir.join("history.v0")).unwrap(), V0);
            assert_eq!(
                fs::read_to_string(dir.join("history.v0.1")).unwrap(),
                V0.replace("1-1", "2-2")
            );
            assert_eq!(
                undo(&JobId("2-2".into())).unwrap_err().to_string(),
                OLD_REFUSAL
            );
            assert_eq!(
                undo(&JobId("9-9".into())).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        });
    }

    /// While another process holds the init lock, a v0 history is neither
    /// archived nor written: the publication is refused instead.
    #[test]
    fn a_held_init_lock_refuses_and_leaves_v0_alone() {
        let t = Tmp::new("v0lock");
        t.env(|| {
            let dir = history_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            fs::write(history_path(), V0).unwrap();
            let holder = File::create(dir.join("history.lock")).unwrap();
            // SAFETY: the fd is open for the duration of the call.
            assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
            let mut rec = Recorder::open(&new_job_id(), "a");
            let e =
                try_publish(&t, &mut rec, "a", &[("x.md", "a")], &mut |_, _| Ok(())).unwrap_err();
            assert!(e.to_string().contains("busy"), "{e}");
            assert!(!rec.finish(true).undoable);
            assert_eq!(history(), V0);
            assert!(!dir.join("history.v0").exists());
        });
    }

    /// A history of an unknown version or with a garbled header is never
    /// archived, rewritten or appended to.
    #[test]
    fn an_unknown_history_header_is_left_alone() {
        let t = Tmp::new("v2");
        t.env(|| {
            fs::create_dir_all(history_dir().unwrap()).unwrap();
            for text in ["inacircle-history 2\n", "garbage\nmore\n"] {
                fs::write(history_path(), text).unwrap();
                let rec = Recorder::open(&new_job_id(), "a");
                assert!(!rec.finish(true).undoable);
                assert_eq!(history(), text);
                assert_eq!(
                    undo(&JobId("1-1".into())).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
            }
        });
    }

    /// Header collision: runs racing to create (or to archive) the history
    /// make exactly one header and one archive.
    #[test]
    fn racing_first_runs_make_one_header_and_one_archive() {
        let t = Tmp::new("hdrrace");
        t.env(|| {
            let dir = history_dir().unwrap();
            for v0 in [false, true] {
                let _ = fs::remove_dir_all(&dir);
                fs::create_dir_all(&dir).unwrap();
                if v0 {
                    fs::write(history_path(), V0).unwrap();
                }
                let hs: Vec<_> = (0..8)
                    .map(|i| {
                        std::thread::spawn(move || {
                            let mut rec = Recorder::open(&JobId(format!("5-{i}")), "a");
                            rec.outcome(&Outcome::Skipped {
                                input: "/i".into(),
                                reason: "r".into(),
                            });
                            rec.finish(true).undoable
                        })
                    })
                    .collect();
                for h in hs {
                    assert!(h.join().unwrap());
                }
                let text = history();
                let (v, lines) = manifest::parse(&text, MAGIC).unwrap();
                // Per run: `job-start`, `skip`, `job`.
                assert_eq!((v, lines.len()), (1, 24), "{text}");
                let names: Vec<_> = fs::read_dir(&dir)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .filter(|n| n.starts_with("history.v0"))
                    .collect();
                assert_eq!(names.len(), usize::from(v0), "{names:?}");
            }
        });
    }

    /// A history whose last line was cut off (a crash) gets a newline before
    /// the next run's first record, so that record stays whole.
    #[test]
    fn a_cut_off_last_line_does_not_spoil_the_next_run() {
        let t = Tmp::new("cutoff");
        t.env(|| {
            let job = new_job_id();
            job_of(&t, &job, &[("a", "a")]);
            let mut h = OpenOptions::new()
                .append(true)
                .open(history_path())
                .unwrap();
            h.write_all(b"publish-commit\top=1\tjob=").unwrap();
            drop(h);
            let next = new_job_id();
            let files = job_of(&t, &next, &[("b", "b")]);
            assert!(all_moved(&undo(&next).unwrap(), &files));
        });
    }

    // ---- the strict reader ----

    const J: &str = "10-1";

    /// A complete record of one job: one group of two files, committed.
    fn good() -> Vec<String> {
        let child = |rel: &str, ino: u64| {
            write_line(
                &Line::new("child")
                    .pair("job", J)
                    .pair("group", "00000000000000aa")
                    .pair("rel", rel)
                    .pair("type", "file")
                    .pair("dev", "1")
                    .pair("ino", &ino.to_string())
                    .pair("uid", "1000")
                    .pair("mode", "644")
                    .pair("nlink", "1")
                    .pair("size", "3")
                    .pair("mtime", "5")
                    .pair("ctime", "6"),
            )
        };
        vec![
            write_line(
                &Line::new("group-begin")
                    .pair("job", J)
                    .pair("group", "00000000000000aa")
                    .pair("op", "00000000000000bb")
                    .pair("wrapper_dev", "1")
                    .pair("wrapper_ino", "2")
                    .pair("wrapper_type", "dir")
                    .pair("wrapper_uid", "1000")
                    .pair("wrapper_mode", "755")
                    .pair("children", "2"),
            ),
            child("a.md", 3),
            child("b%25.md", 4),
            write_line(
                &Line::new("group-end")
                    .pair("job", J)
                    .pair("group", "00000000000000aa")
                    .pair("children", "2")
                    .pair("bytes", "9"),
            ),
            write_line(
                &Line::new("publish-intent")
                    .pair("op", "00000000000000bb")
                    .pair("job", J)
                    .pair("group", "00000000000000aa")
                    .pair("private", "/o/.inacircle-x/w")
                    .pair("target", "/o/a"),
            ),
            write_line(
                &Line::new("publish-commit")
                    .pair("op", "00000000000000bb")
                    .pair("job", J)
                    .pair("group", "00000000000000aa")
                    .pair("actual", "/o/a")
                    .pair("durability", "synced"),
            ),
        ]
    }

    fn read(lines: &[String]) -> io::Result<JobRecords> {
        let text = format!("inacircle-history 1\n{}", lines.concat());
        job_records(&mut text.as_bytes(), &JobId(J.into()))
    }

    #[test]
    fn a_complete_record_reads_back_whole() {
        let r = read(&good()).unwrap();
        assert!(r.known && r.broken.is_none(), "{:?}", r.broken);
        // A `job-start` first is part of the record (v1 lines without it
        // stay readable, above).
        let mut l = good();
        l.insert(0, format!("job-start\tjob={J}\n"));
        let r = read(&l).unwrap();
        assert!(r.known && r.started && r.broken.is_none(), "{:?}", r.broken);
        let [g] = &r.groups[..] else { panic!() };
        assert_eq!(g.children.len(), 2);
        assert_eq!(g.children[1].rel, Path::new("b%.md"));
        assert_eq!(g.commit.as_deref(), Some(&b"/o/a"[..]));
        // Another job's identical record is not this job's.
        let other: Vec<String> = good()
            .iter()
            .map(|l| l.replace("job=10-1", "job=10-10"))
            .collect();
        let r = read(&other).unwrap();
        assert!(!r.known && r.groups.is_empty());
    }

    /// Every framing fault refuses the job: nothing incomplete, duplicated
    /// or unknown grants undo.
    #[test]
    fn framing_faults_are_refused() {
        type Edit = fn(&mut Vec<String>);
        let cases: &[(&str, Edit)] = &[
            ("duplicate group", |l| {
                let b = l[0].clone();
                l.insert(4, b)
            }),
            ("duplicate child", |l| {
                let c = l[1].clone();
                l[2] = c
            }),
            ("duplicate commit", |l| {
                let c = l[5].clone();
                l.push(c)
            }),
            ("duplicate end", |l| {
                let e = l[3].clone();
                l.insert(4, e)
            }),
            ("missing begin", |l| {
                l.remove(0);
            }),
            ("missing end", |l| {
                l.remove(3);
            }),
            ("count", |l| l[3] = l[3].replace("children=2", "children=3")),
            ("declared", |l| {
                l[0] = l[0].replace("children=2", "children=1")
            }),
            ("bytes", |l| l[3] = l[3].replace("bytes=9", "bytes=10")),
            ("op", |l| {
                l[4] = l[4].replace("op=00000000000000bb", "op=00000000000000cc")
            }),
            ("target", |l| {
                l[5] = l[5].replace("actual=/o/a", "actual=/o/b")
            }),
            ("unknown record", |l| {
                l.push(format!("publish-later\tjob={J}\n"))
            }),
            ("unknown field", |l| {
                l[1] = l[1].replace("\tctime=6", "\tctime=6\textra=1")
            }),
            ("missing field", |l| l[1] = l[1].replace("\tnlink=1", "")),
            ("number", |l| l[1] = l[1].replace("ino=3", "ino=+3")),
            ("encoding", |l| l[2] = l[2].replace("b%25.md", "b%2.md")),
            ("encoding", |l| l[2] = l[2].replace("b%25.md", "b%2f.md")),
            ("bad name", |l| {
                l[2] = l[2].replace("b%25.md", "..");
                l[3] = l[3].replace("bytes=9", "bytes=6")
            }),
            ("bad name", |l| {
                l[2] = l[2].replace("b%25.md", "x/y");
                l[3] = l[3].replace("bytes=9", "bytes=7")
            }),
            ("cut", |l| l[1] = l[1].replace("\tctime=6\n", "\tct\n")),
            ("cut", |l| {
                let last = l.pop().unwrap();
                l.push(last.trim_end_matches('\n').to_owned())
            }),
            ("garbled", |l| l.push(format!("child\tjob={J}\\q\n"))),
            ("job-start after records", |l| {
                l.push(format!("job-start\tjob={J}\n"))
            }),
            ("duplicate job-start", |l| {
                l.insert(0, format!("job-start\tjob={J}\n"));
                l.insert(0, format!("job-start\tjob={J}\n"))
            }),
            ("job-start field", |l| {
                l.insert(0, format!("job-start\tjob={J}\tx=1\n"))
            }),
        ];
        for (what, edit) in cases {
            let mut l = good();
            edit(&mut l);
            let r = read(&l).unwrap();
            assert!(r.broken.is_some(), "{what}: accepted {l:?}");
        }
        // A wrong version is refused before any record is read.
        let text = format!("inacircle-history 2\n{}", good().concat());
        let e = job_records(&mut text.as_bytes(), &JobId(J.into())).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// The framing branches the table above does not name, each with the
    /// fault it must give (a wrong reason would mean another branch fired).
    #[test]
    fn more_framing_faults_give_their_own_reason() {
        type Edit = fn(&mut Vec<String>);
        let cases: &[(&str, &str, Edit)] = &[
            (
                "a duplicate operation",
                "op repeated across two groups",
                |l| {
                    let b = l[0].replace("group=00000000000000aa", "group=00000000000000ab");
                    l.push(b)
                },
            ),
            ("an intent out of order", "intent before group-end", |l| {
                l.swap(3, 4)
            }),
            (
                "a commit at a place no intent named",
                "commit without intent",
                |l| {
                    l.remove(4);
                },
            ),
            ("an abort after the outcome", "abort after commit", |l| {
                l.push(format!(
                    "publish-abort\top=00000000000000bb\tjob={J}\tgroup=00000000000000aa\n"
                ))
            }),
            ("an uncertain place said known", "location_known=yes", |l| {
                l[5] = format!(
                    "publish-uncertain\top=00000000000000bb\tjob={J}\tgroup=00000000000000aa\tlocation_known=yes\tparent_dev=1\tparent_ino=2\tleaf=a\tobject_dev=1\tobject_ino=2\treason=AncestorChanged\tdurability=synced\n"
                )
            }),
            ("a bad durability", "durability=maybe", |l| {
                l[5] = l[5].replace("durability=synced", "durability=maybe")
            }),
            (
                "a wrapper that is not a directory",
                "wrapper_type=file",
                |l| l[0] = l[0].replace("wrapper_type=dir", "wrapper_type=file"),
            ),
            (
                "a child after its group's end",
                "child after group-end",
                |l| {
                    let c = l[1].replace("rel=a.md", "rel=c.md");
                    l.insert(4, c)
                },
            ),
        ];
        for (why, what, edit) in cases {
            let mut l = good();
            edit(&mut l);
            let r = read(&l).unwrap();
            assert_eq!(r.broken, Some(*why), "{what}: {l:?}");
        }
    }

    /// The `job` line is read as strictly as the others: a cut `undo=n`, a
    /// value other than `no`, a missing field and a field twice are all a
    /// refusal; the two exact forms are not.
    #[test]
    fn the_job_line_is_read_strictly() {
        let line = |tail: &str| {
            format!("job\tid={J}\ttime=1\taction=convert\tstatus=ok\tsandbox=no{tail}\n")
        };
        for tail in ["", "\tundo=no"] {
            let mut l = good();
            l.push(line(tail));
            assert!(read(&l).unwrap().broken.is_none(), "{tail:?}");
        }
        for tail in [
            "\tundo=n",
            "\tundo=",
            "\tundo=yes",
            "\tundo=no\tundo=no",
            "\tx=1",
        ] {
            let mut l = good();
            l.push(line(tail));
            let r = read(&l).unwrap();
            assert!(r.broken.is_some() && !r.withdrawn, "{tail:?} accepted");
        }
        let mut l = good();
        l.push(line("").replace("\tsandbox=no", ""));
        assert!(read(&l).unwrap().broken.is_some(), "missing field accepted");
        // A cut inside the id: it could be ours, so the job is refused; a
        // line that cannot be ours is not.
        for (cut, broken) in [
            ("job\tid=1", true),
            ("job\tid=", true),
            ("job\tid=10-2", false),
        ] {
            let mut l = good();
            l.push(format!("{cut}\n"));
            assert_eq!(read(&l).unwrap().broken.is_some(), broken, "{cut:?}");
            // Before the job's own records the same line is nobody's
            // business: it does not refuse an intact job.
            let mut l = good();
            l.insert(0, format!("{cut}\n"));
            assert!(read(&l).unwrap().broken.is_none(), "{cut:?} before");
        }
        // A cut that left only the start of the line.
        let mut l = good();
        l.push(format!("job\tid={J}\ttime=1\tact\n"));
        assert!(read(&l).unwrap().broken.is_some());
    }

    /// The reader on a whole history file: a broken record is a refusal by
    /// `undo`, with nothing removed.
    #[test]
    fn undo_refuses_a_broken_record() {
        let t = Tmp::new("broken");
        t.env(|| {
            let job = new_job_id();
            let files = job_of(&t, &job, &[("a", "a")]);
            let text = history();
            let dup = text
                .lines()
                .find(|l| l.starts_with("child\t"))
                .unwrap()
                .to_owned();
            let mut h = OpenOptions::new()
                .append(true)
                .open(history_path())
                .unwrap();
            writeln!(h, "{dup}").unwrap();
            drop(h);
            let e = undo(&job).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
            assert!(files[0].exists());
        });
    }

    #[test]
    fn path_encoding_is_lossless_and_canonical() {
        let all: Vec<u8> = (0..=255).collect();
        let e = enc(&all);
        assert!(e.bytes().all(|b| (0x21..0x7f).contains(&b)), "{e}");
        assert_eq!(dec(&e).unwrap(), all);
        for bad in ["%", "%2", "%2f", "%zz", "a b", "%41", "%+1"] {
            assert_eq!(dec(bad), None, "{bad}");
        }
    }

    // ---- undo ----

    // ---- undo into the Trash (T8) ----

    /// A normal group goes to the test's own Trash whole: its bytes are
    /// readable there, and `info/<name>.trashinfo` names where it was.
    #[test]
    fn undo_moves_whole_groups_to_the_trash_with_their_info() {
        let t = Tmp::new("t8-move");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let a = publish(&t, &mut rec, "a", &[("x.md", "aaa")]);
            let doc = publish(&t, &mut rec, "doc", &[("1.md", "1"), ("2.md", "22")]);
            rec.finish(true);
            let _ = undo(&job);
            assert!(!t.out().join("a").exists() && !t.out().join("doc").exists());
            let files = t.trash().join("files");
            let in_trash = |f: &Path| {
                let w = f.parent().unwrap().file_name().unwrap();
                files.join(w).join(f.file_name().unwrap())
            };
            assert_eq!(fs::read_to_string(in_trash(&a[0])).unwrap(), "aaa");
            assert_eq!(fs::read_to_string(in_trash(&doc[1])).unwrap(), "22");
            let info = fs::read_to_string(t.trash().join("info/a.trashinfo")).unwrap();
            let src = t.out().join("a");
            assert!(
                info.starts_with(&format!("[Trash Info]\nPath={}\nDeletionDate=", url(&src))),
                "{info}"
            );
        });
    }

    /// One changed child keeps its whole group, the unchanged sibling
    /// included; another input's group still goes.
    #[test]
    fn undo_keeps_a_whole_group_when_one_child_changed() {
        let t = Tmp::new("t8-keep");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let doc = publish(&t, &mut rec, "doc", &[("1.md", "one"), ("2.md", "two")]);
            publish(&t, &mut rec, "b", &[("x.md", "b")]);
            rec.finish(true);
            fs::write(&doc[1], "edited").unwrap();
            let _ = undo(&job);
            assert_eq!(fs::read_to_string(&doc[0]).unwrap(), "one");
            assert_eq!(fs::read_to_string(&doc[1]).unwrap(), "edited");
            assert!(t.trash().join("files/b").is_dir());
            assert!(!t.out().join("b").exists());
        });
    }

    /// A writer that keeps its descriptor across the undo writes into the
    /// Trash copy: nothing it writes is lost to an unlink.
    #[test]
    fn undo_keeps_a_retained_writers_bytes() {
        let t = Tmp::new("t8-writer");
        t.env(|| {
            let job = new_job_id();
            let f = job_of(&t, &job, &[("a", "data")]);
            let mut w = OpenOptions::new().append(true).open(&f[0]).unwrap();
            let _ = undo(&job);
            w.write_all(b"+late").unwrap();
            drop(w);
            let moved = t.trash().join("files/a").join(f[0].file_name().unwrap());
            assert_eq!(fs::read_to_string(moved).unwrap(), "data+late");
        });
    }

    use crate::trash::Point;
    use crate::trash::tests::fault;

    /// The Trash as its spec pairs it (§6): each `info/X.trashinfo` that
    /// is a real trashinfo (its `[Trash Info]` header) with its `files/X`.
    /// Asserts every such info has its data; returns `(X, Path=)`.
    fn pairs(t: &Tmp) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(t.trash().join("info")) else {
            return out;
        };
        for e in rd {
            let name = e.unwrap().file_name().into_string().unwrap();
            let Some(x) = name.strip_suffix(".trashinfo") else {
                continue;
            };
            let text = fs::read_to_string(t.trash().join("info").join(&name)).unwrap();
            let Some(rest) = text.strip_prefix("[Trash Info]\nPath=") else {
                continue;
            };
            let path = rest.split('\n').next().unwrap().to_owned();
            assert!(
                fs::symlink_metadata(t.trash().join("files").join(x)).is_ok(),
                "an authentic info without its data: {name}"
            );
            out.push((x.to_owned(), path));
        }
        out.sort();
        out
    }

    /// The names in `files/` of the test's Trash.
    fn trashed(t: &Tmp) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(t.trash().join("files"))
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Our detached infos (`.<op>.inacircle-detached`) and their bytes.
    fn detached(t: &Tmp) -> Vec<Vec<u8>> {
        fs::read_dir(t.trash().join("info"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_str().unwrap().ends_with(".inacircle-detached"))
            .map(|p| fs::read(p).unwrap())
            .collect()
    }

    fn one(r: &UndoResult) -> &GroupOutcome {
        assert_eq!(r.groups.len(), 1, "{r:?}");
        &r.groups[0]
    }

    /// A group of one file `<stem>.md` published at `out/<stem>`: the job,
    /// the wrapper and the file.
    fn single(t: &Tmp, stem: &str) -> (JobId, PathBuf, PathBuf) {
        let job = new_job_id();
        let f = job_of(t, &job, &[(stem, "data")]);
        let w = f[0].parent().unwrap().to_owned();
        (job, w, f[0].clone())
    }

    /// Runs `undo` with a crash (an unwind: no cleanup step runs) at `at`.
    fn crash_at(job: &JobId, at: Point) {
        let _f = fault(move |p| {
            if p == at {
                panic!("simulated crash at {p:?}");
            }
            None
        });
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| undo(job)));
        assert!(r.is_err(), "no crash at {at:?}");
    }

    /// A wrapper swapped for a lookalike between the check and the move
    /// (barrier at `BeforeMove`): the lookalike is what moved, the
    /// postcheck catches it, and it goes back. The user's original folder
    /// is untouched, our info is closed, the Trash is left empty.
    #[test]
    fn undo_restores_a_wrapper_swapped_before_the_move() {
        let t = Tmp::new("t8-swap");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let (w2, kept) = (w.clone(), t.0.join("user-a"));
            let k2 = kept.clone();
            let name = f.file_name().unwrap().to_owned();
            let _f = fault(move |p| {
                if p == Point::BeforeMove {
                    fs::rename(&w2, &k2).unwrap();
                    fs::create_dir(&w2).unwrap();
                    fs::write(w2.join(&name), "data").unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::Restored {
                location, reason, ..
            } = one(&r)
            else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::PostMoveMismatch);
            assert_eq!(location.path_at_check.as_deref(), Some(w.as_path()));
            assert_eq!(location.checked_at, CheckPoint::PostRestore);
            assert_eq!(fs::read_to_string(&f).unwrap(), "data");
            assert_eq!(
                fs::read_to_string(kept.join(f.file_name().unwrap())).unwrap(),
                "data"
            );
            assert!(trashed(&t).is_empty() && pairs(&t).is_empty());
            assert_eq!(detached(&t).len(), 1);
        });
    }

    /// A child swapped between the check and the move: back it goes.
    #[test]
    fn undo_restores_a_child_swapped_before_the_move() {
        let t = Tmp::new("t8-childswap");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            let f2 = f.clone();
            let _f = fault(move |p| {
                if p == Point::BeforeMove {
                    let o = f2.with_extension("other");
                    fs::write(&o, "data").unwrap();
                    fs::rename(&o, &f2).unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            assert!(matches!(one(&r), GroupOutcome::Restored { .. }), "{r:?}");
            assert_eq!(fs::read_to_string(&f).unwrap(), "data");
            assert!(trashed(&t).is_empty() && pairs(&t).is_empty());
        });
    }

    /// A folder fd kept open across the move adds a file inside the moved
    /// group (barrier at `AfterMove`): the group no longer matches and goes
    /// back whole, the new file with it.
    #[test]
    fn undo_restores_after_a_change_through_a_retained_dirfd() {
        let t = Tmp::new("t8-dirfd");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let dir = File::open(&w).unwrap();
            let raw = dir.as_raw_fd();
            let _f = fault(move |p| {
                if p == Point::AfterMove {
                    // SAFETY: raw is the open folder; the name is NUL-terminated.
                    let fd = unsafe {
                        libc::openat(
                            raw,
                            c"extra".as_ptr(),
                            libc::O_CREAT | libc::O_WRONLY,
                            0o600,
                        )
                    };
                    assert!(fd >= 0);
                    // SAFETY: fd was just opened.
                    unsafe { libc::close(fd) };
                }
                None
            });
            let r = undo(&job).unwrap();
            assert!(matches!(one(&r), GroupOutcome::Restored { .. }), "{r:?}");
            assert!(w.join("extra").exists() && f.exists());
            drop(dir);
        });
    }

    /// A writer through a shared mapping kept across the undo: its bytes
    /// land in the Trash copy, nothing is lost.
    #[test]
    fn undo_keeps_a_retained_mapping_writable() {
        let t = Tmp::new("t8-mmap");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            let file = OpenOptions::new().read(true).write(true).open(&f).unwrap();
            // SAFETY: a shared 4-byte mapping of an open 4-byte file.
            let m = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(m, libc::MAP_FAILED);
            let r = undo(&job).unwrap();
            assert!(matches!(one(&r), GroupOutcome::MovedToTrash { .. }));
            // SAFETY: m maps 4 writable bytes; msync/munmap take the same range.
            unsafe {
                std::ptr::copy_nonoverlapping(b"DATA".as_ptr(), m.cast::<u8>(), 4);
                libc::msync(m, 4, libc::MS_SYNC);
                libc::munmap(m, 4);
            }
            let moved = t.trash().join("files/a").join(f.file_name().unwrap());
            assert_eq!(fs::read_to_string(moved).unwrap(), "DATA");
        });
    }

    /// Missing, extra and changed children and a wrapper mode change each
    /// keep their group whole before anything moves; nothing is written to
    /// the Trash.
    #[test]
    fn undo_keeps_missing_extra_and_changed_groups() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("t8-precheck");
        t.env(|| {
            type Change = fn(&Path, &Path);
            let changes: [(&str, Change); 4] = [
                ("missing", |_, f| fs::remove_file(f).unwrap()),
                ("extra", |w, _| fs::write(w.join("extra"), "x").unwrap()),
                ("changed", |_, f| fs::write(f, "edited").unwrap()),
                ("wrapper mode", |w, _| {
                    let m = fs::metadata(w).unwrap().permissions().mode() & 0o777;
                    let new = if m == 0o700 { 0o750 } else { 0o700 };
                    fs::set_permissions(w, fs::Permissions::from_mode(new)).unwrap()
                }),
            ];
            for (what, change) in changes {
                let (job, w, f) = single(&t, what.split(' ').next().unwrap());
                change(&w, &f);
                let r = undo(&job).unwrap();
                assert_eq!(kept(&r), [UndoReason::Changed], "{what}");
                assert!(w.is_dir(), "{what}");
            }
            assert!(trashed(&t).is_empty());
            assert!(!t.trash().exists(), "nothing made before the check passed");
        });
    }

    /// The way back taken by a new folder (barrier at `AfterMove`): the
    /// moved group stays in the Trash, its info still pairs with it, the
    /// user's new folder is untouched, and the place is given.
    #[test]
    fn undo_keeps_in_the_trash_when_the_way_back_is_taken() {
        let t = Tmp::new("t8-collide");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let (w2, moved) = (
                w.clone(),
                t.trash().join("files/a").join(f.file_name().unwrap()),
            );
            let _f = fault(move |p| {
                if p == Point::AfterMove {
                    fs::write(&moved, "changed in Trash").unwrap();
                    fs::create_dir(&w2).unwrap();
                    fs::write(w2.join("new"), "user").unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::RecoveryKept {
                location,
                reason,
                errno,
                ..
            } = one(&r)
            else {
                panic!("{r:?}")
            };
            assert_eq!(
                (*reason, *errno),
                (UndoReason::RestoreCollision, Some(libc::EEXIST))
            );
            assert_eq!(location.path_at_check, Some(t.trash().join("files/a")));
            assert_eq!(fs::read_to_string(w.join("new")).unwrap(), "user");
            assert_eq!(pairs(&t), [("a".to_owned(), url(&w))]);
        });
    }

    /// I1: the Trash renamed before the place is proved: the place is the
    /// path resolved from the held fd (`PostResolve`).
    #[test]
    fn undo_proves_the_place_after_the_trash_was_renamed() {
        let t = Tmp::new("t8-resolve");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let (w2, moved) = (
                w.clone(),
                t.trash().join("files/a").join(f.file_name().unwrap()),
            );
            let (trash, renamed) = (t.trash(), t.0.join("data/Trash-moved"));
            let r2 = renamed.clone();
            let _f = fault(move |p| {
                match p {
                    Point::AfterMove => {
                        fs::write(&moved, "changed").unwrap();
                        fs::create_dir(&w2).unwrap();
                    }
                    Point::AfterRestoreFail => fs::rename(&trash, &r2).unwrap(),
                    _ => {}
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::RecoveryKept { location, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert_eq!(location.checked_at, CheckPoint::PostResolve);
            let want = fs::canonicalize(renamed.join("files/a")).unwrap();
            assert_eq!(location.path_at_check.as_deref(), Some(want.as_path()));
        });
    }

    /// I1: a renamer that wins every resolution attempt: the place is
    /// `None` (never made up), the last path seen is only a hint, the
    /// anchor names the moved wrapper, and its bytes are all there.
    #[test]
    fn undo_is_uncertain_when_the_trash_keeps_moving() {
        let t = Tmp::new("t8-uncertain");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let (w2, moved) = (
                w.clone(),
                t.trash().join("files/a").join(f.file_name().unwrap()),
            );
            let data = t.0.join("data");
            let n = std::rc::Rc::new(std::cell::Cell::new(0u32));
            let k = n.clone();
            let _f = fault(move |p| {
                let name = |i: u32| {
                    data.join(if i == 0 {
                        "Trash".to_owned()
                    } else {
                        format!("T{i}")
                    })
                };
                match p {
                    Point::AfterMove => {
                        fs::write(&moved, "changed").unwrap();
                        fs::create_dir(&w2).unwrap();
                    }
                    // From the failed way back on (a resolve before the
                    // move, of the source, is left alone).
                    Point::AfterRestoreFail => {
                        fs::rename(name(0), name(1)).unwrap();
                        k.set(1);
                    }
                    Point::Resolve if k.get() > 0 => {
                        fs::rename(name(k.get()), name(k.get() + 1)).unwrap();
                        k.set(k.get() + 1);
                    }
                    _ => {}
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::Uncertain {
                locations, reason, ..
            } = one(&r)
            else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::AncestorChanged);
            let l = &locations[0];
            assert_eq!(l.path_at_check, None);
            assert!(l.last_unverified_path.is_some());
            let wid = fs::metadata(t.0.join(format!("data/T{}/files/a", n.get()))).unwrap();
            assert_eq!(
                (l.anchor.object_dev, l.anchor.object_ino),
                (wid.dev(), wid.ino())
            );
            let at = t.0.join(format!("data/T{}/files/a", n.get()));
            assert_eq!(
                fs::read_to_string(at.join(f.file_name().unwrap())).unwrap(),
                "changed"
            );
        });
    }

    /// The parent folder renamed away (and a lookalike made) after the
    /// check: the move goes through the held parent fd, so the published
    /// group itself is trashed; the lookalike is untouched.
    #[test]
    fn undo_moves_from_a_parent_renamed_after_the_check() {
        let t = Tmp::new("t8-parent");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let (out, away) = (t.out(), t.0.join("out-away"));
            let name = f.file_name().unwrap().to_owned();
            let _f = fault(move |p| {
                if p == Point::BeforeMove {
                    fs::rename(&out, &away).unwrap();
                    fs::create_dir_all(out.join("a")).unwrap();
                    fs::write(out.join("a").join(&name), "look").unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            assert!(
                matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                "{r:?}"
            );
            assert_eq!(fs::read_to_string(&f).unwrap(), "look");
            assert_eq!(trashed(&t), ["a"]);
            assert_eq!(pairs(&t), [("a".to_owned(), url(&w))]);
        });
    }

    /// EXDEV at the move (labelled injection): kept in place, nothing
    /// copied, our info closed: no authentic info is left in the Trash.
    #[test]
    fn undo_refuses_exdev_and_closes_its_info() {
        let t = Tmp::new("t8-exdev");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let _f = fault(|p| (p == Point::Rename).then_some(libc::EXDEV));
            let r = undo(&job).unwrap();
            let GroupOutcome::Kept {
                location, reason, ..
            } = one(&r)
            else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::MoveRefused(libc::EXDEV));
            assert_eq!(location.path_at_check.as_deref(), Some(w.as_path()));
            assert!(f.exists());
            assert!(trashed(&t).is_empty() && pairs(&t).is_empty());
            let d = detached(&t);
            assert_eq!(d.len(), 1);
            assert!(d[0].starts_with(b"inacircle-detached-trashinfo op="));
        });
    }

    /// The data name taken in the Trash between the choice and the move:
    /// EEXIST, kept, and the foreign data never gets our info. N1: with
    /// the neutralisation failing (at the write, at the sync) the info is
    /// still detached, and the group is `Uncertain` with where it is.
    #[test]
    fn undo_closes_its_info_when_the_data_name_is_taken() {
        for inject in [
            None,
            Some((Point::NeutralWrite, libc::ENOSPC)),
            Some((Point::NeutralSync, libc::EIO)),
        ] {
            let t = Tmp::new("t8-taken");
            t.env(|| {
                let (job, _, f) = single(&t, "a");
                let foreign = t.trash().join("files/a");
                let _f = fault(move |p| {
                    if p == Point::BeforeMove {
                        fs::create_dir(&foreign).unwrap();
                        fs::write(foreign.join("theirs"), "x").unwrap();
                    }
                    inject.and_then(|(at, e)| (p == at).then_some(e))
                });
                let r = undo(&job).unwrap();
                assert!(f.exists());
                assert!(pairs(&t).is_empty(), "foreign data paired with our info");
                assert!(!t.trash().join("info/a.trashinfo").exists());
                match (inject, one(&r)) {
                    (None, GroupOutcome::Kept { reason, .. }) => {
                        assert_eq!(*reason, UndoReason::MoveRefused(libc::EEXIST));
                    }
                    (
                        Some((_, e)),
                        GroupOutcome::Uncertain {
                            reason, locations, ..
                        },
                    ) => {
                        assert_eq!(*reason, UndoReason::ContentNotNeutralized(e));
                        let info = locations[1].path_at_check.as_ref().unwrap();
                        assert!(info.to_str().unwrap().ends_with(".inacircle-detached"));
                        assert_eq!(locations[1].checked_at, CheckPoint::PostMove);
                    }
                    other => panic!("{inject:?}: {other:?}"),
                }
            });
        }
    }

    /// ENOSPC and EDQUOT at the info write (labelled injection: a short
    /// write) and EIO at its sync: kept, no move, no authentic info left.
    #[test]
    fn undo_info_write_faults_keep_the_group() {
        for (at, e) in [
            (Point::InfoWrite, libc::ENOSPC),
            (Point::InfoWrite, libc::EDQUOT),
            (Point::InfoSync, libc::EIO),
        ] {
            let t = Tmp::new("t8-info");
            t.env(|| {
                let (job, _, f) = single(&t, "a");
                let _f = fault(move |p| (p == at).then_some(e));
                let r = undo(&job).unwrap();
                assert_eq!(kept(&r), [UndoReason::InfoNotWritten(e)], "{at:?}");
                assert!(f.exists());
                assert!(trashed(&t).is_empty() && pairs(&t).is_empty(), "{at:?}");
            });
        }
    }

    /// The intent not written (labelled injection): nothing moves, no info
    /// is made.
    #[test]
    fn undo_history_fault_before_the_move_keeps_the_group() {
        let t = Tmp::new("t8-intent");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            let _f = fault(|p| (p == Point::Intent).then_some(libc::ENOSPC));
            let r = undo(&job).unwrap();
            assert_eq!(kept(&r), [UndoReason::HistoryNotWritten(libc::ENOSPC)]);
            assert!(f.exists());
            assert!(
                fs::read_dir(t.trash().join("info"))
                    .unwrap()
                    .next()
                    .is_none()
            );
        });
    }

    /// The commit not recorded after the move: never "nothing changed"; the
    /// group is in the Trash with its info, reported with its place.
    #[test]
    fn undo_commit_fault_reports_the_trash_place() {
        let t = Tmp::new("t8-commit");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            let _f = fault(|p| (p == Point::Commit).then_some(libc::EIO));
            let r = undo(&job).unwrap();
            let GroupOutcome::RecoveryKept {
                location, reason, ..
            } = one(&r)
            else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::CommitNotRecorded);
            assert_eq!(location.path_at_check, Some(t.trash().join("files/a")));
            assert_eq!(pairs(&t), [("a".to_owned(), url(&w))]);
            assert!(!r.warnings.is_empty());
        });
    }

    /// Our info swapped for a foreign one after the move: the group goes
    /// back, the foreign info is left byte-identical.
    #[test]
    fn undo_restores_when_its_info_was_swapped() {
        let t = Tmp::new("t8-infoswap");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            let info = t.trash().join("info");
            let _f = fault(move |p| {
                if p == Point::AfterMove {
                    fs::rename(info.join("a.trashinfo"), info.join("mine-moved")).unwrap();
                    fs::write(info.join("a.trashinfo"), "foreign").unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::Restored { reason, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::InfoChanged);
            assert!(w.is_dir());
            let info = t.trash().join("info");
            assert_eq!(
                fs::read_to_string(info.join("a.trashinfo")).unwrap(),
                "foreign"
            );
            assert!(
                fs::read(info.join("mine-moved"))
                    .unwrap()
                    .starts_with(b"inacircle-detached")
            );
        });
    }

    /// The group taken out of the Trash right after the move (another
    /// program, or a file manager's Restore): the outcome is uncertain and
    /// our info is closed, so data that later appears at `files/a` never
    /// pairs with an authentic `a.trashinfo`.
    #[test]
    fn undo_closes_its_info_when_the_group_leaves_the_trash() {
        let t = Tmp::new("t8-gone");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            let (files, away) = (t.trash().join("files"), t.0.join("away"));
            let a2 = away.clone();
            let _f = fault(move |p| {
                if p == Point::AfterMove {
                    fs::rename(files.join("a"), &a2).unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::Uncertain { reason, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::AncestorChanged);
            assert_eq!(
                fs::read_to_string(away.join(f.file_name().unwrap())).unwrap(),
                "data"
            );
            fs::create_dir(t.trash().join("files/a")).unwrap();
            assert_eq!(pairs(&t), Vec::<(String, String)>::new());
            assert_eq!(detached(&t).len(), 1);
            assert!(detached(&t)[0].starts_with(b"inacircle-detached"));
        });
    }

    /// The group changed in the Trash, our info renamed away by another
    /// program and a foreign info at our name, the way back taken: the
    /// pair is not ours any more, so our info is closed wherever it went
    /// (neutralised through its fd) and the foreign one is untouched.
    #[test]
    fn undo_closes_its_renamed_info_when_the_restore_collides() {
        let t = Tmp::new("t8-mismatch-renamed");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            let (files, info) = (t.trash().join("files"), t.trash().join("info"));
            let w2 = w.clone();
            let _f = fault(move |p| {
                if p == Point::AfterMove {
                    fs::write(files.join("a/extra"), "x").unwrap();
                    fs::rename(info.join("a.trashinfo"), info.join("mine-moved")).unwrap();
                    fs::write(info.join("a.trashinfo"), "foreign").unwrap();
                    fs::create_dir(&w2).unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::RecoveryKept { reason, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::RestoreCollision);
            let info = t.trash().join("info");
            assert_eq!(
                fs::read_to_string(info.join("a.trashinfo")).unwrap(),
                "foreign"
            );
            assert!(
                fs::read(info.join("mine-moved"))
                    .unwrap()
                    .starts_with(b"inacircle-detached")
            );
        });
    }

    /// A foreign file caught at our detached name cannot go back (its
    /// name was taken again): it stays there, and the undo says where.
    #[test]
    fn undo_reports_a_foreign_info_kept_at_the_detached_name() {
        let t = Tmp::new("t8-foreign-kept");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            let (files, info) = (t.trash().join("files"), t.trash().join("info"));
            let _f = fault(move |p| {
                match p {
                    // The data name taken: the move fails, our info closes.
                    Point::BeforeMove => fs::create_dir(files.join("a")).unwrap(),
                    Point::Detach => {
                        fs::rename(info.join("a.trashinfo"), info.join("mine-moved")).unwrap();
                        fs::write(info.join("a.trashinfo"), "foreign 1").unwrap();
                    }
                    Point::PutBack => fs::write(info.join("a.trashinfo"), "foreign 2").unwrap(),
                    _ => {}
                }
                None
            });
            let r = undo(&job).unwrap();
            assert!(f.exists());
            let d = detached(&t);
            assert_eq!(d, [b"foreign 1".to_vec()]);
            assert!(
                r.warnings
                    .iter()
                    .any(|w| w.contains("another program's Trash metadata kept at")
                        && w.ends_with(".inacircle-detached")),
                "{r:?}"
            );
        });
    }

    /// After a crash past the move, a foreign folder stands at our Trash
    /// name (ours taken elsewhere): recovery never takes it for ours, so
    /// it is never moved out of the Trash into the user's place.
    #[test]
    fn undo_recovery_leaves_a_foreign_folder_at_its_trash_name() {
        let t = Tmp::new("t8-rec-foreign");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            crash_at(&job, Point::AfterMove);
            let files = t.trash().join("files");
            fs::rename(files.join("a"), t.0.join("ours-away")).unwrap();
            fs::create_dir(files.join("a")).unwrap();
            fs::write(files.join("a/theirs"), "x").unwrap();
            let _ = undo(&job).unwrap();
            assert_eq!(fs::read_to_string(files.join("a/theirs")).unwrap(), "x");
            assert!(!w.exists());
            assert_eq!(pairs(&t), Vec::<(String, String)>::new());
        });
    }

    /// The op id of the (only) recovery intent in the history.
    fn intent_op() -> String {
        history()
            .lines()
            .find_map(|l| l.strip_prefix("recovery-intent\top="))
            .map(|r| r[..16].to_owned())
            .unwrap()
    }

    /// After a crash before the move, our info was being neutralised and
    /// is cut short (the neutral text over its first bytes) under the
    /// detached name: it is still ours, so it is finished there, never put
    /// back at the `.trashinfo` name as if foreign.
    #[test]
    fn undo_recovery_finishes_a_half_neutralised_info() {
        let t = Tmp::new("t8-rec-half");
        t.env(|| {
            let (job, _, _) = single(&t, "a");
            crash_at(&job, Point::BeforeMove);
            let info = t.trash().join("info");
            let op = intent_op();
            let detached_at = info.join(format!(".{op}.inacircle-detached"));
            fs::rename(info.join("a.trashinfo"), &detached_at).unwrap();
            let n = format!("inacircle-detached-trashinfo op={op}\n");
            let mut f = OpenOptions::new().write(true).open(&detached_at).unwrap();
            std::io::Write::write_all(&mut f, &n.as_bytes()[..12]).unwrap();
            drop(f);
            let r = undo(&job).unwrap();
            assert!(
                matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                "{r:?}"
            );
            assert_eq!(fs::read(&detached_at).unwrap(), n.as_bytes());
            // The move took the free name: the half-neutral file was not
            // put back at it.
            assert!(
                pairs(&t).iter().any(|(b, _)| b == "a"),
                "{:?}",
                pairs(&t)
            );
        });
    }

    /// An empty file at our info name, after our info was closed under the
    /// detached name, is someone else's: never neutralised.
    #[test]
    fn undo_recovery_leaves_an_empty_foreign_info_alone() {
        let t = Tmp::new("t8-rec-empty");
        t.env(|| {
            let (job, _, _) = single(&t, "a");
            crash_at(&job, Point::BeforeMove);
            let info = t.trash().join("info");
            let op = intent_op();
            let detached_at = info.join(format!(".{op}.inacircle-detached"));
            fs::rename(info.join("a.trashinfo"), &detached_at).unwrap();
            fs::write(&detached_at, format!("inacircle-detached-trashinfo op={op}\n")).unwrap();
            fs::write(info.join("a.trashinfo"), "").unwrap();
            let r = undo(&job).unwrap();
            assert!(
                matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                "{r:?}"
            );
            assert_eq!(fs::read(info.join("a.trashinfo")).unwrap(), b"");
        });
    }

    /// After a crash past the move, the Trash's `info` folder is open to
    /// others: recovery does not trust it (it checks it as the move did),
    /// and reports the group's place as uncertain instead of resuming.
    #[test]
    fn undo_recovery_refuses_an_info_folder_that_is_not_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("t8-rec-info-mode");
        t.env(|| {
            let (job, _, _) = single(&t, "a");
            crash_at(&job, Point::AfterMove);
            let info = t.trash().join("info");
            fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).unwrap();
            let r = undo(&job).unwrap();
            assert!(
                matches!(one(&r), GroupOutcome::Uncertain { .. }),
                "{r:?}"
            );
            assert_eq!(
                fs::metadata(&info).unwrap().permissions().mode() & 0o777,
                0o755,
                "never chmod-ed"
            );
        });
    }

    /// After a crash past the move, the group in the Trash no longer
    /// matches (so it is to go back), and the source folder is a different
    /// one at the same path: it is not restored into the look-alike.
    #[test]
    fn undo_recovery_does_not_restore_into_a_replaced_source_folder() {
        let t = Tmp::new("t8-rec-src");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            crash_at(&job, Point::AfterMove);
            fs::write(t.trash().join("files/a/extra"), "x").unwrap();
            let parent = w.parent().unwrap().to_owned();
            fs::rename(&parent, t.0.join("out-away")).unwrap();
            fs::create_dir(&parent).unwrap();
            let _ = undo(&job).unwrap();
            assert!(!w.exists(), "restored into a look-alike folder");
            assert_eq!(trashed(&t), ["a"]);
        });
    }

    /// A restore that succeeded but whose place cannot be shown (the
    /// folder moved away and the path cannot be resolved) is `Uncertain`,
    /// never a `Restored` without a path.
    #[test]
    fn undo_restored_without_a_provable_place_is_uncertain() {
        use std::cell::Cell;
        use std::rc::Rc;
        let t = Tmp::new("t8-rest-none");
        t.env(|| {
            let (job, w, f) = single(&t, "a");
            let (w2, kept) = (w.clone(), t.0.join("user-a"));
            let (parent, away) = (w.parent().unwrap().to_owned(), t.0.join("out-away"));
            let name = f.file_name().unwrap().to_owned();
            let armed = Rc::new(Cell::new(false));
            let a2 = armed.clone();
            let _f = fault(move |p| {
                match p {
                    Point::BeforeMove => {
                        fs::rename(&w2, &kept).unwrap();
                        fs::create_dir(&w2).unwrap();
                        fs::write(w2.join(&name), "data").unwrap();
                    }
                    Point::Restore => {
                        fs::rename(&parent, &away).unwrap();
                        a2.set(true);
                    }
                    Point::Resolve if a2.get() => return Some(libc::EIO),
                    _ => {}
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::Uncertain { locations, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert!(locations[0].path_at_check.is_none(), "{r:?}");
        });
    }

    /// After a crash before the move, the Trash's `files` is not where the
    /// intent says, and the group is still at its source: a warning, and a
    /// fresh undo moves it (never `Uncertain`).
    #[test]
    fn undo_recovery_with_the_trash_moved_runs_a_fresh_undo() {
        let t = Tmp::new("t8-rec-moved");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            crash_at(&job, Point::BeforeMove);
            fs::rename(t.trash().join("files"), t.0.join("files-old")).unwrap();
            let r = undo(&job).unwrap();
            assert!(
                matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                "{r:?}"
            );
            assert!(!w.exists());
            assert!(
                r.warnings
                    .iter()
                    .any(|w| w.contains("Trash metadata could not be checked")),
                "{r:?}"
            );
        });
    }

    /// A real EACCES at the move (the parent made read-only): kept.
    #[test]
    fn undo_permission_denied_keeps_the_group() {
        use std::os::unix::fs::PermissionsExt;
        if euid() == 0 {
            return; // root ignores the folder's mode
        }
        let t = Tmp::new("t8-eacces");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            fs::set_permissions(t.out(), fs::Permissions::from_mode(0o555)).unwrap();
            let r = undo(&job).unwrap();
            fs::set_permissions(t.out(), fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(kept(&r), [UndoReason::MoveRefused(libc::EACCES)]);
            assert!(f.exists() && pairs(&t).is_empty());
        });
    }

    /// No safe Trash: a symlinked `Trash`, or a `files` folder open to
    /// others; refused before anything is written, nothing chmod-ed.
    #[test]
    fn undo_refuses_an_unsafe_trash() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("t8-unsafe");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            let real = t.0.join("real-trash");
            fs::create_dir(&real).unwrap();
            symlink(&real, t.trash()).unwrap();
            // A rejected home Trash refuses: no fallback to the volume's
            // (the fixture topdir stays without a `.Trash-$uid`).
            crate::trash::tests::TOPDIR.with_borrow_mut(|p| *p = Some(t.0.clone()));
            assert_eq!(kept(&undo(&job).unwrap()), [UndoReason::NoTrash]);
            crate::trash::tests::TOPDIR.with_borrow_mut(|p| *p = None);
            assert!(!t.0.join(format!(".Trash-{}", euid())).exists());
            assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
            fs::remove_file(t.trash()).unwrap();
            let (job, _, g) = single(&t, "b");
            fs::create_dir_all(t.trash().join("files")).unwrap();
            fs::set_permissions(t.trash(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(t.trash().join("files"), fs::Permissions::from_mode(0o755))
                .unwrap();
            assert_eq!(kept(&undo(&job).unwrap()), [UndoReason::NoTrash]);
            let m = fs::metadata(t.trash().join("files"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o755);
            assert!(f.exists() && g.exists());
        });
    }

    /// A crash at each step of the move: the next undo of the job finds
    /// the operation from its intent (only the target and our two info
    /// names are looked at), closes what is stale, and ends with the group
    /// in the Trash under exactly one authentic info.
    #[test]
    fn undo_recovers_from_a_crash_at_each_step() {
        for at in [
            Point::InfoWrite,
            Point::InfoSync,
            Point::BeforeMove,
            Point::AfterMove,
            Point::Commit,
        ] {
            let t = Tmp::new("t8-crash");
            t.env(|| {
                let (job, w, f) = single(&t, "a");
                crash_at(&job, at);
                let r = undo(&job).unwrap();
                assert!(
                    matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                    "{at:?}: {r:?}"
                );
                assert!(!f.exists());
                let p = pairs(&t);
                assert_eq!(p.len(), 1, "{at:?}: {p:?}");
                assert_eq!(p[0].1, url(&w), "{at:?}");
                let moved = t.trash().join("files").join(&p[0].0);
                assert_eq!(
                    fs::read_to_string(moved.join(f.file_name().unwrap())).unwrap(),
                    "data"
                );
            });
        }
    }

    /// A crash while our info was being closed (after the neutralisation,
    /// before the detach): the next undo detaches it and moves the group.
    #[test]
    fn undo_recovers_from_a_crash_while_closing_its_info() {
        let t = Tmp::new("t8-crash-close");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            {
                let _f = fault(|p| match p {
                    Point::Rename => Some(libc::EXDEV),
                    Point::Detach => panic!("simulated crash at Detach"),
                    _ => None,
                });
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| undo(&job)));
                assert!(r.is_err());
            }
            let info = t.trash().join("info/a.trashinfo");
            assert!(fs::read(&info).unwrap().starts_with(b"inacircle-detached"));
            let r = undo(&job).unwrap();
            assert!(
                matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                "{r:?}"
            );
            assert_eq!(pairs(&t), [("a".to_owned(), url(&w))]);
        });
    }

    /// N1 in the crash recovery: our authentic info left by a crash cannot
    /// be neutralised; it is still detached (no `.trashinfo` name), and the
    /// group is reported `Uncertain`, not moved.
    #[test]
    fn undo_recovery_detaches_even_when_neutralising_fails() {
        let t = Tmp::new("t8-crash-n1");
        t.env(|| {
            let (job, _, f) = single(&t, "a");
            crash_at(&job, Point::BeforeMove);
            let _f = fault(|p| (p == Point::NeutralWrite).then_some(libc::ENOSPC));
            let r = undo(&job).unwrap();
            let GroupOutcome::Uncertain { reason, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::ContentNotNeutralized(libc::ENOSPC));
            assert!(f.exists());
            assert!(pairs(&t).is_empty());
            assert!(!t.trash().join("info/a.trashinfo").exists());
            assert!(
                detached(&t)[0].starts_with(b"[Trash Info]"),
                "authentic, but unpaired"
            );
        });
    }

    /// An undo that a crash cut after one group's commit: the next undo
    /// reports that group from its records (no second move) and finishes
    /// the other.
    #[test]
    fn undo_resumes_an_interrupted_job_from_its_records() {
        let t = Tmp::new("t8-resume");
        t.env(|| {
            let job = new_job_id();
            let f = job_of(&t, &job, &[("a", "a"), ("b", "b")]);
            let n = std::rc::Rc::new(std::cell::Cell::new(0));
            let k = n.clone();
            {
                let _f = fault(move |p| {
                    if p == Point::Intent {
                        k.set(k.get() + 1);
                        if k.get() == 2 {
                            panic!("simulated crash at the second group");
                        }
                    }
                    None
                });
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| undo(&job)));
                assert!(r.is_err());
            }
            let r = undo(&job).unwrap();
            assert!(all_moved(&r, &f), "{r:?}");
            let GroupOutcome::MovedToTrash { location, .. } = &r.groups[0] else {
                unreachable!()
            };
            assert_eq!(location.checked_at, CheckPoint::Commit);
            assert_eq!(trashed(&t), ["a", "b"]);
            assert_eq!(pairs(&t).len(), 2);
            assert_eq!(undo(&job).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        });
    }

    /// The engine never unlinks: across every scenario above, each inode
    /// published is still reachable. Here: the whole undo of a job holds
    /// every published byte somewhere (source or Trash), counted by inode.
    #[test]
    fn undo_never_unlinks_a_published_inode() {
        let t = Tmp::new("t8-nounlink");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let a = publish(&t, &mut rec, "a", &[("1.md", "1"), ("2.md", "2")]);
            let b = publish(&t, &mut rec, "b", &[("x.md", "x")]);
            rec.finish(true);
            fs::write(&b[0], "edited").unwrap();
            let inos: Vec<u64> = a
                .iter()
                .chain(&b)
                .map(|p| fs::metadata(p).unwrap().ino())
                .collect();
            undo(&job).unwrap();
            let mut found = Vec::new();
            for root in [t.out(), t.trash().join("files")] {
                for w in fs::read_dir(root).unwrap() {
                    let w = w.unwrap().path();
                    if w.is_dir() {
                        for c in fs::read_dir(w).unwrap() {
                            found.push(c.unwrap().metadata().unwrap().ino());
                        }
                    }
                }
            }
            for i in inos {
                assert!(found.contains(&i), "inode {i} gone");
            }
        });
    }

    /// The home Trash on another filesystem (labelled: `EXDEV` injected
    /// at the home lookup): the volume Trash is used, `.Trash-$uid` under
    /// the topdir (the fixture, standing in for the mount point), with
    /// `Path=` relative to the topdir; a crash before the move recovers
    /// there too.
    #[test]
    fn undo_uses_the_volume_trash_when_home_is_elsewhere() {
        for crash in [false, true] {
            let t = Tmp::new("t8-volume");
            t.env(|| {
                crate::trash::tests::TOPDIR.with_borrow_mut(|p| *p = Some(t.0.clone()));
                let (job, _, f) = single(&t, "a");
                if crash {
                    let _f = fault(|p| match p {
                        Point::Home => Some(libc::EXDEV),
                        Point::BeforeMove => panic!("simulated crash at BeforeMove"),
                        _ => None,
                    });
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| undo(&job)));
                    assert!(r.is_err());
                }
                let r = {
                    let _f = fault(|p| (p == Point::Home).then_some(libc::EXDEV));
                    undo(&job).unwrap()
                };
                crate::trash::tests::TOPDIR.with_borrow_mut(|p| *p = None);
                let vol = t.0.join(format!(".Trash-{}", euid()));
                let GroupOutcome::MovedToTrash { location, .. } = one(&r) else {
                    panic!("{r:?}")
                };
                assert_eq!(location.path_at_check, Some(vol.join("files/a")));
                assert!(vol.join("files/a").join(f.file_name().unwrap()).exists());
                let infos: Vec<_> = fs::read_dir(vol.join("info"))
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .filter(|p| p.extension().is_some_and(|e| e == "trashinfo"))
                    .collect();
                assert_eq!(infos.len(), 1, "crash={crash}: {infos:?}");
                let text = fs::read_to_string(&infos[0]).unwrap();
                assert!(text.starts_with("[Trash Info]\nPath=out/a\n"), "{text}");
                assert!(!t.trash().exists(), "the home Trash is not used");
            });
        }
    }

    /// The info swapped and the way back taken: the group stays in the
    /// Trash, and our info (renamed away by someone) is closed: it must not
    /// stay authentic anywhere; the foreign info is left alone.
    #[test]
    fn undo_closes_a_swapped_info_when_the_way_back_is_taken() {
        let t = Tmp::new("t8-swap-collide");
        t.env(|| {
            let (job, w, _) = single(&t, "a");
            let (info, w2) = (t.trash().join("info"), w.clone());
            let _f = fault(move |p| {
                if p == Point::AfterMove {
                    fs::rename(info.join("a.trashinfo"), info.join("mine-moved")).unwrap();
                    fs::write(info.join("a.trashinfo"), "foreign").unwrap();
                    fs::create_dir(&w2).unwrap();
                }
                None
            });
            let r = undo(&job).unwrap();
            let GroupOutcome::RecoveryKept { reason, .. } = one(&r) else {
                panic!("{r:?}")
            };
            assert_eq!(*reason, UndoReason::RestoreCollision);
            let info = t.trash().join("info");
            assert_eq!(
                fs::read_to_string(info.join("a.trashinfo")).unwrap(),
                "foreign"
            );
            assert!(
                fs::read(info.join("mine-moved"))
                    .unwrap()
                    .starts_with(b"inacircle-detached")
            );
        });
    }

    /// The detaching rename fails (labelled EACCES after an EXDEV move):
    /// the neutral inode stays under its name and a warning says where.
    #[test]
    fn undo_warns_when_its_info_cannot_be_detached() {
        let t = Tmp::new("t8-nodetach");
        t.env(|| {
            let (job, _, _) = single(&t, "a");
            let _f = fault(|p| match p {
                Point::Rename => Some(libc::EXDEV),
                Point::Detach => Some(libc::EACCES),
                _ => None,
            });
            let r = undo(&job).unwrap();
            assert_eq!(kept(&r), [UndoReason::MoveRefused(libc::EXDEV)]);
            let info = t.trash().join("info/a.trashinfo");
            assert!(fs::read(&info).unwrap().starts_with(b"inacircle-detached"));
            assert!(
                r.warnings
                    .iter()
                    .any(|w| w.contains("left neutralised") && w.contains(info.to_str().unwrap())),
                "{:?}",
                r.warnings
            );
        });
    }

    /// Crash recovery at the detached name: our authentic bytes there (a
    /// detach that ran after a failed neutralisation) are neutralised; a
    /// foreign file there (a lost race) is put back under its name. Then
    /// the undo goes on.
    #[test]
    fn undo_recovery_settles_the_detached_name() {
        for foreign in [false, true] {
            let t = Tmp::new("t8-detached");
            t.env(|| {
                let (job, _, _) = single(&t, "a");
                crash_at(&job, Point::BeforeMove);
                let info = t.trash().join("info");
                let det = fs::read_dir(&info)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| p.to_str().unwrap().ends_with(".trashinfo"))
                    .unwrap();
                let op = history()
                    .lines()
                    .find_map(|l| l.strip_prefix("recovery-intent\top="))
                    .map(|r| r[..16].to_owned())
                    .unwrap();
                let detached_at = info.join(format!(".{op}.inacircle-detached"));
                fs::rename(&det, &detached_at).unwrap();
                if foreign {
                    fs::write(&detached_at, "foreign").unwrap();
                }
                let r = undo(&job).unwrap();
                assert!(
                    matches!(one(&r), GroupOutcome::MovedToTrash { .. }),
                    "{r:?}"
                );
                if foreign {
                    // Put back under the name; the move took the next name.
                    assert_eq!(
                        fs::read_to_string(info.join("a.trashinfo")).unwrap(),
                        "foreign"
                    );
                    assert_eq!(pairs(&t).len(), 1);
                } else {
                    assert!(
                        fs::read(&detached_at)
                            .unwrap()
                            .starts_with(b"inacircle-detached")
                    );
                }
            });
        }
    }

    /// A FIFO planted at our info name during a crash: never opened for
    /// reading in a blocking way, never taken for ours; the undo goes on
    /// under the next name.
    #[test]
    fn undo_recovery_leaves_a_fifo_at_its_info_name() {
        let t = Tmp::new("t8-fifo");
        t.env(|| {
            let (job, _, _) = single(&t, "a");
            crash_at(&job, Point::BeforeMove);
            let info = t.trash().join("info/a.trashinfo");
            fs::rename(&info, t.0.join("ours")).unwrap();
            crate::sys::tests::mkfifo(&info);
            let r = crate::sys::tests::within(10, move || undo(&job).map(|r| r.groups.len()));
            assert_eq!(r.unwrap(), 1);
            let ft = fs::symlink_metadata(&info).unwrap().file_type();
            assert!(std::os::unix::fs::FileTypeExt::is_fifo(&ft));
        });
    }

    /// The recovery-intent record is bounded: 32768 bytes are written,
    /// 32769 refused before anything is written.
    #[test]
    fn undo_intent_record_is_bounded() {
        let t = Tmp::new("t8-bound");
        t.env(|| {
            let job = new_job_id();
            job_of(&t, &job, &[("a", "a")]);
            let dir = StateDir::open(&history_dir().unwrap()).unwrap();
            let mut file = dir
                .open_file(HISTORY, libc::O_RDWR | libc::O_APPEND, 0)
                .unwrap();
            let lock = dir
                .open_file(INIT_LOCK, libc::O_RDWR | libc::O_CREAT, 0o600)
                .unwrap();
            let mut log = HistLog {
                file: &mut file,
                lock: &lock,
                job: job.0.clone(),
            };
            let mut intent = |source: &[u8]| -> io::Result<()> {
                let i = trash::Intent {
                    op: OperationId(1),
                    group: GroupId(2),
                    source,
                    target: b"/t",
                    info_rel: 0,
                    source_parent: (1, 2),
                    target_parent: (3, 4),
                    wrapper: (5, 6),
                    mount: 7,
                    date: "2026-10-27T10:00:00",
                };
                trash::Log::intent(&mut log, &i)
            };
            let before = fs::metadata(history_path()).unwrap().len();
            intent(b"/").unwrap();
            let frame = fs::metadata(history_path()).unwrap().len() - before - 1;
            let fill = vec![b'a'; RECOVERY_INTENT_MAX_BYTES - frame as usize];
            intent(&fill).unwrap();
            let len = fs::metadata(history_path()).unwrap().len();
            let mut over = fill.clone();
            over.push(b'a');
            let e = intent(&over).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(fs::metadata(history_path()).unwrap().len(), len);
        });
    }

    /// Every undo ending survives its record: what the next undo reports
    /// is what this one did (the unproved hint excepted: not recorded).
    #[test]
    fn undo_endings_roundtrip_through_their_records() {
        let loc = |p: Option<&str>| LocationEvidence {
            path_at_check: p.map(PathBuf::from),
            last_unverified_path: None,
            anchor: AnchorHint {
                parent_dev: 1,
                parent_ino: 2,
                object_dev: 3,
                object_ino: 4,
                leaf: OsStr::from_bytes(b"a\tb\xff").to_owned(),
                op: OperationId(9),
            },
            checked_at: CheckPoint::PostMove,
        };
        let g = GroupId(5);
        for o in [
            GroupOutcome::Kept {
                group: g,
                location: loc(Some("/o/a")),
                reason: UndoReason::MoveRefused(18),
            },
            GroupOutcome::MovedToTrash {
                group: g,
                location: loc(Some("/t/files/a b")),
                info_leaf: "a b.trashinfo".into(),
                durability: Durability::Synced,
            },
            GroupOutcome::Restored {
                group: g,
                location: loc(Some("/o/a")),
                reason: UndoReason::InfoChanged,
                durability: Durability::Synced,
            },
            GroupOutcome::RecoveryKept {
                group: g,
                location: loc(Some("/t/files/a")),
                reason: UndoReason::RestoreCollision,
                errno: Some(17),
                durability: Durability::Synced,
            },
            GroupOutcome::Uncertain {
                group: g,
                op: OperationId(9),
                locations: vec![loc(None)],
                reason: UndoReason::AncestorChanged,
            },
        ] {
            let line = close_line("1-2", OperationId(9), &o);
            let l = parse_line(&line).unwrap();
            let v = fields(&l, RCLOSE).unwrap();
            assert_eq!(closed_outcome(&l.kind, 5, 9, &v).unwrap(), o, "{line}");
        }
    }

    /// The Trash spec's `Path=` form of `p`: bytes outside the unreserved
    /// set (and `/`) as `%XX`.
    fn url(p: &Path) -> String {
        let mut s = String::new();
        for &b in p.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                s.push(char::from(b));
            } else {
                s.push_str(&format!("%{b:02X}"));
            }
        }
        s
    }

    #[test]
    fn undo_removes_unchanged_outputs_and_their_folders() {
        let t = Tmp::new("undo");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let a = publish(&t, &mut rec, "a", &[("x.md", "aaa")]);
            let many = publish(&t, &mut rec, "doc", &[("1.md", "1"), ("2.md", "22")]);
            rec.finish(true);
            let r = undo(&job).unwrap();
            let mut want = a.clone();
            want.extend(many.clone());
            assert!(all_moved(&r, &want), "{r:?}");
            assert_eq!(r.groups.len(), 2);
            assert!(!t.out().join("a").exists() && !t.out().join("doc").exists());
            assert!(t.out().exists(), "a user folder is never moved");
        });
    }

    /// Regression: one `job` line cut inside its id (a crash, a full disk)
    /// refuses only its own job; a later, intact job is still undoable.
    #[test]
    fn a_cut_job_line_does_not_refuse_later_jobs() {
        let t = Tmp::new("cutlater");
        t.env(|| {
            let a = new_job_id();
            let a_files = job_of(&t, &a, &[("a", "a")]);
            // Cut the last line (A's `job` line) just after "job\tid=1".
            let text = history();
            let last = text.trim_end().rfind('\n').unwrap() + 1;
            let f = OpenOptions::new().write(true).open(history_path()).unwrap();
            f.set_len((last + "job\tid=1".len()) as u64).unwrap();
            drop(f);
            let b = new_job_id();
            let b_files = job_of(&t, &b, &[("b", "b")]);
            assert!(all_moved(&undo(&b).unwrap(), &b_files));
            let e = undo(&a).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
            assert!(a_files[0].exists());
        });
    }

    /// A run that dies midway leaves complete groups: undo still finds them.
    #[test]
    fn a_dead_run_is_undoable() {
        let t = Tmp::new("dead-run");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let a = publish(&t, &mut rec, "a", &[("x.md", "a")]);
            drop(rec);
            assert!(all_moved(&undo(&job).unwrap(), &a));
        });
    }

    #[test]
    fn undo_keeps_what_changed() {
        let t = Tmp::new("changed");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let f = publish(&t, &mut rec, "doc", &[("1.md", "one"), ("2.md", "two")]);
            rec.finish(true);
            fs::write(&f[1], "longer content").unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(kept(&r), [UndoReason::Changed]);
            assert_eq!(fs::read_to_string(&f[1]).unwrap(), "longer content");
            assert_eq!(
                fs::read_to_string(&f[0]).unwrap(),
                "one",
                "its sibling stays too"
            );
        });
    }

    /// Same size, other inode; same inode with mtime put back (ctime
    /// catches it); a symlink in its place; a hard link added (nlink); a
    /// mode change: each is kept.
    #[test]
    fn every_identity_change_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("ident");
        t.env(|| {
            type Change = fn(&Path, &Path);
            let changes: [(&str, Change); 5] = [
                ("replaced", |f, t| {
                    let o = t.join("other");
                    fs::write(&o, "one").unwrap();
                    fs::rename(&o, f).unwrap();
                }),
                ("mtime restored", |f, _| {
                    let before = fs::metadata(f).unwrap().modified().unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                    let h = File::options().write(true).open(f).unwrap();
                    (&h).write_all(b"two").unwrap();
                    h.set_modified(before).unwrap();
                }),
                ("symlink", |f, t| {
                    let v = t.join("victim");
                    fs::write(&v, "one").unwrap();
                    fs::remove_file(f).unwrap();
                    symlink(&v, f).unwrap();
                }),
                ("hard link", |f, t| {
                    fs::hard_link(f, t.join("link")).unwrap()
                }),
                ("mode", |f, _| {
                    fs::set_permissions(f, fs::Permissions::from_mode(0o600)).unwrap()
                }),
            ];
            for (n, (what, change)) in changes.iter().enumerate() {
                let job = JobId(format!("30-{n}"));
                let stem = format!("s{n}");
                let f = job_of(&t, &job, &[(&stem, "one")]);
                change(&f[0], &t.0);
                let r = undo(&job).unwrap();
                assert_eq!(kept(&r), [UndoReason::Changed], "{what}");
                assert!(fs::symlink_metadata(&f[0]).is_ok(), "{what}");
            }
            assert_eq!(fs::read_to_string(t.0.join("victim")).unwrap(), "one");
        });
    }

    /// The wrapper moved away and a lookalike folder (same names, same
    /// bytes) put in its place, or a symlink to one: nothing is moved.
    #[test]
    fn a_swapped_wrapper_keeps_the_lookalike() {
        let t = Tmp::new("wrapswap");
        t.env(|| {
            for (n, link) in [false, true].into_iter().enumerate() {
                let job = JobId(format!("31-{n}"));
                let stem = format!("w{n}");
                let f = job_of(&t, &job, &[(&stem, "data")]);
                let wrapper = f[0].parent().unwrap().to_owned();
                let moved = t.0.join(format!("moved{n}"));
                fs::rename(&wrapper, &moved).unwrap();
                let look = t.0.join(format!("look{n}"));
                fs::create_dir(&look).unwrap();
                fs::write(look.join(f[0].file_name().unwrap()), "data").unwrap();
                if link {
                    symlink(&look, &wrapper).unwrap();
                } else {
                    fs::rename(&look, &wrapper).unwrap();
                }
                let r = undo(&job).unwrap();
                assert_eq!(kept(&r), [UndoReason::Changed], "link={link}");
                assert!(f[0].exists() && moved.join(f[0].file_name().unwrap()).exists());
            }
        });
    }

    #[test]
    fn undo_reports_missing() {
        let t = Tmp::new("missing");
        t.env(|| {
            let job = new_job_id();
            let f = job_of(&t, &job, &[("a", "d")]);
            // A missing child keeps the group; a missing folder is missing.
            fs::remove_file(&f[0]).unwrap();
            assert_eq!(kept(&undo(&job).unwrap()), [UndoReason::Changed]);
            let job = new_job_id();
            let f = job_of(&t, &job, &[("b", "d")]);
            let moved = t.0.join("b-moved");
            fs::rename(f[0].parent().unwrap(), &moved).unwrap();
            let r = undo(&job).unwrap();
            assert_eq!(kept(&r), [UndoReason::Missing]);
            let GroupOutcome::Kept { location, .. } = &r.groups[0] else {
                unreachable!()
            };
            assert_eq!(location.path_at_check, None, "no place is made up");
            assert!(moved.join(f[0].file_name().unwrap()).exists());
        });
    }

    #[test]
    fn undo_twice_is_already_exists() {
        let t = Tmp::new("twice");
        t.env(|| {
            let job = new_job_id();
            job_of(&t, &job, &[("a", "d")]);
            undo(&job).unwrap();
            assert_eq!(undo(&job).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        });
    }

    #[test]
    fn unknown_job_not_found() {
        let t = Tmp::new("nojob");
        t.env(|| {
            // No history at all, then a history without that job.
            assert_eq!(
                undo(&JobId("9-9".into())).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            Recorder::open(&JobId("1-1".into()), "a").finish(true);
            assert_eq!(
                undo(&JobId("9-9".into())).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        });
    }

    /// Tabs, newlines, `=`, backslashes and bytes that are not UTF-8 in a
    /// name are recorded losslessly: undo finds the exact file.
    #[test]
    fn any_name_bytes_roundtrip() {
        let t = Tmp::new("weird");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "a=b");
            let name = std::ffi::OsStr::from_bytes(b"a\tb\nc=d\\e #x\xff%.md");
            let p = try_publish_raw(
                &t,
                &mut rec,
                "w",
                &[(std::ffi::OsStr::new("1.md"), "1"), (name, "2")],
                &mut |_, _| Ok(()),
            )
            .unwrap();
            let files = commit(&mut rec, "/in/p\tq\n=", &p);
            assert_eq!(files[1].file_name(), Some(name));
            assert!(rec.finish(true).undoable);
            let r = undo(&job).unwrap();
            assert!(all_moved(&r, &files), "{r:?}");
        });
    }

    /// Undo is scoped to its job: interleaved records of three jobs whose
    /// ids prefix each other (`10-1`, `10-10`, `10-100`).
    #[test]
    fn undo_touches_only_its_own_job_among_interleaved_prefix_ids() {
        let t = Tmp::new("interleaved");
        t.env(|| {
            let ids = ["10-1", "10-10", "10-100"];
            let jobs: Vec<JobId> = ids.iter().map(|i| JobId((*i).into())).collect();
            let mut recs: Vec<Recorder> =
                jobs.iter().map(|j| Recorder::open(j, "convert")).collect();
            let mut files: Vec<Vec<PathBuf>> = vec![Vec::new(); 3];
            for n in 0..2 {
                for (k, r) in recs.iter_mut().enumerate() {
                    let stem = format!("{}-{n}", ids[k]);
                    files[k].extend(publish(&t, r, &stem, &[("x.md", "x")]));
                }
            }
            for r in &mut recs {
                r.outcome(&Outcome::Skipped {
                    input: PathBuf::from("/in/s"),
                    reason: "same".into(),
                });
                r.outcome(&Outcome::Failed {
                    input: PathBuf::from("/in/f"),
                    reason: "bad".into(),
                });
            }
            for r in recs {
                assert!(r.finish(true).undoable);
            }
            assert!(all_moved(&undo(&jobs[1]).unwrap(), &files[1]));
            for k in [0, 2] {
                assert!(files[k].iter().all(|f| f.exists()), "{} untouched", ids[k]);
            }
            assert!(all_moved(&undo(&jobs[0]).unwrap(), &files[0]));
            assert!(all_moved(&undo(&jobs[2]).unwrap(), &files[2]));
            for j in &jobs {
                assert_eq!(undo(j).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            }
        });
    }

    #[test]
    fn status_values() {
        let d = Outcome::Done {
            input: "/a".into(),
            output: "/o".into(),
            id: FileId {
                dev: 0,
                ino: 0,
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
            },
        };
        let s = Outcome::Skipped {
            input: "/b".into(),
            reason: "x".into(),
        };
        let f = Outcome::Failed {
            input: "/b".into(),
            reason: "x".into(),
        };
        let st = |os: &[&Outcome]| {
            let mut t = Tally::default();
            for o in os {
                t.add(o);
            }
            status(&t)
        };
        assert_eq!(st(&[&d]), "done");
        assert_eq!(st(&[&d, &s]), "partial");
        assert_eq!(st(&[&f]), "failed");
        assert_eq!(st(&[]), "failed");
    }

    // ---- registry ----

    #[test]
    fn running_file_lifecycle() {
        let t = Tmp::new("running");
        t.env(|| {
            let job = JobId("10-1".into());
            let path = jobs_dir().unwrap().join("10-1");
            {
                let mut r = Running::start(&job, "convert", 3);
                assert!(path.exists());
                assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
                let l = list_running().unwrap();
                assert_eq!(l.len(), 1);
                assert_eq!(l[0].0, job);
                assert_eq!(l[0].1, "convert");
                assert_eq!(l[0].2, std::process::id());
                r.update(1, 0.5); // throttled: too soon after start
                assert_eq!(list_running().unwrap()[0].3, 0.0);
                std::thread::sleep(UPDATE_EVERY + Duration::from_millis(20));
                r.update(1, 0.5);
                assert_eq!(list_running().unwrap()[0].3, 0.5);
            }
            assert!(!path.exists());
            assert!(list_running().unwrap().is_empty());
            let names: Vec<_> = fs::read_dir(jobs_dir().unwrap()).unwrap().collect();
            assert!(names.is_empty(), "temp files left: {names:?}");
        });
    }

    #[test]
    fn job_ids_differ_within_one_second_and_process() {
        let (a, b) = (new_job_id(), new_job_id());
        assert_ne!(a, b);
        let parts: Vec<&str> = a.0.split('-').collect();
        assert_eq!(parts.len(), 3, "{}", a.0);
        assert!(parts[0].parse::<u64>().is_ok(), "{}", a.0);
        assert_eq!(parts[1], std::process::id().to_string());
        assert!(
            parts[2].len() == 8 && parts[2].bytes().all(|c| c.is_ascii_hexdigit()),
            "{}",
            a.0
        );
    }

    #[test]
    fn undo_of_a_running_job_is_refused() {
        let t = Tmp::new("undorun");
        t.env(|| {
            let job = JobId("10-1".into());
            let running = Running::start(&job, "a", 1);
            let mut rec = Recorder::open(&job, "a");
            let f = publish(&t, &mut rec, "a", &[("x.md", "x")]);
            let e = undo(&job).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::ResourceBusy);
            assert!(f[0].exists());
            drop(running);
            drop(rec.finish(true));
            assert!(all_moved(&undo(&job).unwrap(), &f));
        });
    }

    #[test]
    fn undo_gives_up_on_a_held_history_lock() {
        let t = Tmp::new("undolock");
        t.env(|| {
            let job = JobId("10-3".into());
            let f = job_of(&t, &job, &[("a", "x")]);
            // A second open file description conflicts even in-process.
            let holder = File::open(history_path()).unwrap();
            // SAFETY: the fd is open for the duration of the call.
            assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
            let start = Instant::now();
            let e = undo(&job).unwrap_err();
            let took = start.elapsed();
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
            assert_eq!(e.to_string(), "history is busy");
            assert!(took <= Duration::from_millis(2100), "took {took:?}");
            assert!(f[0].exists(), "nothing is deleted without the lock");
            drop(holder);
            assert!(all_moved(&undo(&job).unwrap(), &f));
        });
    }

    #[test]
    fn throttled_update_is_written_by_the_next_call_after_the_interval() {
        let t = Tmp::new("throttle");
        t.env(|| {
            let job = JobId("10-2".into());
            let mut r = Running::start(&job, "convert", 4);
            r.update(1, 0.25); // dropped: too soon after start
            r.update(2, 0.5); // dropped
            assert_eq!(list_running().unwrap()[0].3, 0.0);
            std::thread::sleep(UPDATE_EVERY + Duration::from_millis(20));
            r.update(3, 0.75);
            let f = fs::read_to_string(jobs_dir().unwrap().join("10-2")).unwrap();
            assert!(f.contains("\tdone=3\t"), "{f}");
            assert_eq!(list_running().unwrap()[0].3, 0.75);
        });
    }

    #[test]
    fn no_runtime_dir_skips_registry_but_history_works() {
        let t = Tmp::new("norun");
        let s = t.0.join("state");
        with_env(
            &[
                ("XDG_STATE_HOME", Some(s.to_str().unwrap())),
                ("XDG_RUNTIME_DIR", None),
            ],
            || {
                let job = JobId("10-1".into());
                drop(Running::start(&job, "a", 1));
                assert!(list_running().unwrap().is_empty());
                Recorder::open(&job, "a").finish(true);
                assert!(history_path().exists());
            },
        );
    }

    #[test]
    fn dead_pid_entries_pruned() {
        let t = Tmp::new("dead");
        t.env(|| {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let line = Line::new("job")
                .pair("id", "5-5")
                .pair("pid", &pid.to_string())
                .pair("action", "x")
                .pair("progress", "0.1");
            let p = dir.join("5-5");
            fs::write(&p, write_line(&line)).unwrap();
            assert!(list_running().unwrap().is_empty());
            assert!(!p.exists());
        });
    }

    #[test]
    fn reused_pid_entry_is_pruned() {
        let t = Tmp::new("reuse");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            // A live pid (ours) recorded with another start tick.
            let own = ProcId::of(std::process::id()).unwrap();
            let old = Line::new("job")
                .pair("id", "5-5")
                .pair("pid", &std::process::id().to_string())
                .pair("action", "x")
                .pair("start", &(own.ticks + 1000).to_string())
                .pair("boot", &own.boot);
            fs::write(dir.join("5-5"), write_line(&old)).unwrap();
            assert!(list_running().unwrap().is_empty());
            assert!(!dir.join("5-5").exists());
        });
    }

    /// A fake group: `n` children with long names, nothing on disk.
    fn fake_group(seed: u64, n: usize, long: &str) -> OutputGroup {
        let id = FileId {
            dev: 1,
            ino: 0,
            size: 1,
            mtime_ns: 1,
            ctime_ns: 1,
        };
        OutputGroup {
            group: GroupId(seed),
            op: OperationId(!seed),
            directory: PathBuf::from("/nonexistent/g"),
            id: DirectoryId {
                dev: 1,
                ino: seed,
                kind: NodeKind::Directory,
                uid: 1000,
                mode: 0o755,
            },
            children: (0..n)
                .map(|i| ChildBaseline {
                    rel: PathBuf::from(format!("{i}{long}")),
                    file_id: id,
                    kind: NodeKind::File,
                    uid: 1000,
                    mode: 0o644,
                    nlink: 1,
                })
                .collect(),
        }
    }

    /// Each record block is one `write` on an `O_APPEND` descriptor:
    /// concurrent runs never interleave inside one.
    #[test]
    fn concurrent_appends_do_not_interleave() {
        let t = Tmp::new("conc");
        t.env(|| {
            let long = "x".repeat(3000);
            let work = |tag: u64| {
                let long = long.clone();
                std::thread::spawn(move || {
                    for i in 0..200u64 {
                        let mut rec = Recorder::open(&JobId(format!("{tag}-{i}")), "a");
                        let g = fake_group(tag << 32 | i, 2, &long);
                        rec.prepare_group(&g, Path::new("/nonexistent/p")).unwrap();
                        rec.finish(true);
                    }
                })
            };
            let (a, b) = (work(1), work(2));
            a.join().unwrap();
            b.join().unwrap();
            let text = history();
            let (v, lines) = manifest::parse(&text, MAGIC).unwrap();
            assert_eq!(v, 1);
            let count = |k: &str| lines.iter().filter(|l| l.kind == k).count();
            assert_eq!(count("group-begin"), 400);
            assert_eq!(count("child"), 800);
            assert_eq!(count("publish-abort"), 400);
            assert_eq!(count("job"), 400);
        });
    }

    #[test]
    fn concurrent_undos_one_wins() {
        let t = Tmp::new("race");
        t.env(|| {
            let job = JobId("10-1".into());
            job_of(&t, &job, &[("a", "d")]);
            let results: Vec<_> = (0..4)
                .map(|_| {
                    let j = job.clone();
                    std::thread::spawn(move || undo(&j))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect();
            let ok = results.iter().filter(|r| r.is_ok()).count();
            let dup = results
                .iter()
                .filter(|r| {
                    r.as_ref()
                        .is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
                })
                .count();
            assert_eq!((ok, dup), (1, 3));
            assert_eq!(
                history()
                    .lines()
                    .filter(|l| l.starts_with("undo\t"))
                    .count(),
                1
            );
        });
    }

    fn vm_hwm_kb() -> u64 {
        let s = fs::read_to_string("/proc/self/status").unwrap();
        let l = s.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
        l.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    /// Runs `helper_undo_memory` alone in a child process, so other tests'
    /// allocations cannot disturb its peak-RSS reading.
    #[test]
    fn undo_streams_a_large_history() {
        let t = Tmp::new("mem");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "journal::tests::helper_undo_memory",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("INACIRCLE_HELPER", &t.0)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "helper failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("ran helper"));
    }

    /// Child side of `undo_streams_a_large_history`; a no-op otherwise.
    #[test]
    fn helper_undo_memory() {
        let Some(dir) = std::env::var_os("INACIRCLE_HELPER").map(PathBuf::from) else {
            return;
        };
        if std::env::var_os("INACIRCLE_CASE").is_some() {
            return;
        }
        let t = std::mem::ManuallyDrop::new(Tmp(dir));
        let state = t.0.join("state");
        let state_s = state.to_str().unwrap().to_owned();
        // Its own Trash: the env inherited from the parent names whatever
        // test held it when this child was spawned.
        let data = t.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let data_s = data.to_str().unwrap().to_owned();
        // No registry: the env inherited from the parent may name another
        // test's runtime dir, whose "running" job 10-1 would block the undo.
        let v = [
            ("XDG_STATE_HOME", Some(&*state_s)),
            ("XDG_DATA_HOME", Some(&*data_s)),
            ("XDG_RUNTIME_DIR", None),
        ];
        with_env(&v, || {
            let job = JobId("10-1".into());
            // The history exists (header) before the filler.
            drop(Recorder::open(&JobId("9-9".into()), "a").finish(true));
            // ~50 MB of other jobs' lines in front of ours, ending with a
            // finished job's line written long before ours started (the
            // proof that all of our records are inside the I/O window).
            let p = history_path();
            let mut w = io::BufWriter::new(OpenOptions::new().append(true).open(&p).unwrap());
            let filler = Line::new("child")
                .pair("job", "99-9")
                .pair("group", "00000000000000aa")
                .pair("rel", &"p".repeat(200));
            let text = write_line(&filler);
            let n = 50_000_000 / text.len() + 1;
            for _ in 0..n {
                w.write_all(text.as_bytes()).unwrap();
            }
            w.write_all(b"job\tid=99-9\ttime=1\taction=a\tstatus=done\tsandbox=yes\n")
                .unwrap();
            w.flush().unwrap();
            drop(w);
            let f = job_of(&t, &job, &[("a", "d")]);
            assert!(fs::metadata(&p).unwrap().len() >= 50_000_000);
            let before = vm_hwm_kb();
            let r = undo(&job).unwrap();
            let after = vm_hwm_kb();
            assert!(all_moved(&r, &f));
            assert!(
                after.saturating_sub(before) < 8 * 1024,
                "peak RSS grew by {} kB",
                after.saturating_sub(before)
            );
        });
        println!("ran helper");
    }

    // ---- bounded reads (T9) ----

    const SJ: &str = "1790000000-1-deadbeef";

    /// A hand-made version 1 history of job `SJ`: `groups` groups, each
    /// with `children` children named by `rel(group, child)` and a committed
    /// place of `place_len` decoded bytes (also its one intent).
    fn synth(
        groups: usize,
        children: usize,
        rel: impl Fn(usize, usize) -> String,
        place_len: usize,
    ) -> String {
        let mut t = String::from("inacircle-history 1\n");
        let uid = unsafe { libc::geteuid() };
        for g in 0..groups {
            let gid = format!("{:016x}", g + 1);
            let op = format!("{:016x}", g + 0x1000);
            let mut place = format!("/t/{g}/");
            while place.len() < place_len {
                place.push('a');
            }
            place.truncate(place_len);
            t.push_str(&format!(
                "group-begin\tjob={SJ}\tgroup={gid}\top={op}\twrapper_dev=1\twrapper_ino={}\twrapper_type=dir\twrapper_uid={uid}\twrapper_mode=700\tchildren={children}\n",
                g + 10
            ));
            let mut bytes = 0;
            for c in 0..children {
                let r = rel(g, c);
                bytes += r.len();
                t.push_str(&format!(
                    "child\tjob={SJ}\tgroup={gid}\trel={r}\ttype=file\tdev=1\tino={}\tuid={uid}\tmode=600\tnlink=1\tsize=1\tmtime=1\tctime=1\n",
                    g * 100_000 + c + 5000
                ));
            }
            t.push_str(&format!(
                "group-end\tjob={SJ}\tgroup={gid}\tchildren={children}\tbytes={bytes}\n"
            ));
            t.push_str(&format!(
                "publish-intent\top={op}\tjob={SJ}\tgroup={gid}\tprivate=/p\ttarget={place}\n"
            ));
            t.push_str(&format!(
                "publish-commit\top={op}\tjob={SJ}\tgroup={gid}\tactual={place}\tdurability=synced\n"
            ));
        }
        t.push_str(&format!(
            "job\tid={SJ}\ttime=1790000001\taction=x\tstatus=done\tsandbox=yes\n"
        ));
        t
    }

    fn scan_text(t: &str) -> io::Result<JobRecords> {
        job_records(&mut t.as_bytes(), &JobId(SJ.into()))
    }

    fn broken_of(t: &str) -> Option<&'static str> {
        scan_text(t).unwrap().broken
    }

    fn is_limit(b: Option<&'static str>) -> bool {
        b.is_some_and(|w| w.starts_with("over the limit"))
    }

    fn rel_n(_: usize, c: usize) -> String {
        format!("c{c}")
    }

    #[test]
    fn history_outputs_per_job_1024_and_1025() {
        assert_eq!(broken_of(&synth(1, 1024, rel_n, 20)), None);
        assert!(is_limit(broken_of(&synth(1, 1025, rel_n, 20))));
        // Across groups the count is the job's.
        assert_eq!(broken_of(&synth(2, 512, rel_n, 20)), None);
        assert!(is_limit(broken_of(&synth(2, 513, rel_n, 20))));
    }

    #[test]
    fn history_dirs_per_job_1024_and_1025() {
        assert_eq!(broken_of(&synth(1024, 1, |_, _| "c".into(), 20)), None);
        assert!(is_limit(broken_of(&synth(1025, 1, |_, _| "c".into(), 20))));
    }

    /// Intents per job: 4096 are read, 4097 are a limit fault, even when
    /// each costs no path bytes (an empty target), so the count alone is
    /// bounded.
    #[test]
    fn history_intents_per_job_4096_and_4097() {
        let with = |n: usize| {
            let mut l = good();
            let intent = l[4].clone();
            for _ in 1..n {
                l.insert(4, intent.replace("target=/o/a", "target="));
            }
            let r = read(&l).unwrap();
            assert!(r.known, "the job's records were read");
            r.broken
        };
        assert_eq!(with(HISTORY_INTENTS), None);
        let b = with(HISTORY_INTENTS + 1);
        assert!(is_limit(b), "{b:?}");
    }

    #[test]
    fn history_path_4096_and_4097() {
        assert_eq!(broken_of(&synth(1, 1, rel_n, 4096)), None);
        assert!(is_limit(broken_of(&synth(1, 1, rel_n, 4097))));
    }

    /// 512 groups x (2 B name + 1023 B intent + 1023 B commit) is exactly
    /// 1 MiB of kept path bytes; one more byte is over the job's limit
    /// although no single limit is hit.
    #[test]
    fn history_path_bytes_per_job_max_and_max_plus_one() {
        let at = synth(512, 1, |_, _| "cc".into(), 1023);
        assert_eq!(broken_of(&at), None);
        assert!(is_limit(broken_of(&synth(
            512,
            1,
            |g, _| if g == 0 { "ccc".into() } else { "cc".into() },
            1023
        ))));
    }

    /// Fields per record: a `skip` of the job with 32 fields is read, 33
    /// is a limit fault (counted before the line is parsed).
    #[test]
    fn history_fields_32_and_33() {
        let mk = |n: usize| {
            let mut t = synth(1, 1, rel_n, 20);
            let mut l = format!("skip\tjob={SJ}");
            for k in 1..n {
                l.push_str(&format!("\tk{k}=v"));
            }
            t.push_str(&l);
            t.push('\n');
            t
        };
        assert_eq!(broken_of(&mk(32)), None);
        assert!(is_limit(broken_of(&mk(33))));
    }

    /// Lines of exactly 32768 B are read, 32769 B are a limit fault when
    /// they name the job; one that does not name it is skipped (a damaged
    /// line invalidates only its own job).
    #[test]
    fn history_line_32768_and_32769() {
        let line = |len: usize, job: &str| {
            let mut l = format!("skip\tjob={job}\treason=");
            while l.len() < len {
                l.push('p');
            }
            l.push('\n');
            l
        };
        let mut t = synth(1, 1, rel_n, 20);
        t.push_str(&line(32768, SJ));
        assert_eq!(broken_of(&t), None);
        let mut t = synth(1, 1, rel_n, 20);
        t.push_str(&line(32769, SJ));
        assert!(is_limit(broken_of(&t)));
        let mut t = synth(1, 1, rel_n, 20);
        t.push_str(&line(5_000_000, "99-9"));
        assert_eq!(broken_of(&t), None, "another job's long line");
        // After the long line the reader is on the next line.
        t.push_str(&format!("skip\tjob={SJ}\treason=after\n"));
        assert_eq!(broken_of(&t), None);
    }

    /// The reader's lines across a small buffer's chunk edges.
    #[test]
    fn bounded_lines_across_chunk_edges() {
        for cap in [1usize, 7, HISTORY_READ_CHUNK] {
            for len in [
                0,
                1,
                cap.saturating_sub(1),
                cap,
                cap + 1,
                3 * cap,
                32768,
                32769,
                70_000,
            ] {
                let text = format!("{}\nnext\n", "a".repeat(len));
                let mut rd = BufReader::with_capacity(cap, text.as_bytes());
                let mut buf = Vec::new();
                let Got::Line { content, ended } = read_line_bounded(&mut rd, &mut buf).unwrap()
                else {
                    panic!("eof");
                };
                assert_eq!((content, ended), (len, true), "cap {cap} len {len}");
                assert!(buf.len() <= HISTORY_LINE_BYTES + 1);
                if len <= HISTORY_LINE_BYTES {
                    assert_eq!(buf.len(), len + 1, "kept whole with its newline");
                }
                let Got::Line { content, .. } = read_line_bounded(&mut rd, &mut buf).unwrap()
                else {
                    panic!("eof");
                };
                assert_eq!((content, &buf[..]), (4, &b"next\n"[..]));
            }
        }
        // A line cut by the end is not `ended`.
        let mut rd = BufReader::with_capacity(4, &b"abc"[..]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_bounded(&mut rd, &mut buf).unwrap(),
            Got::Line {
                content: 3,
                ended: false
            }
        ));
        assert!(matches!(
            read_line_bounded(&mut rd, &mut buf).unwrap(),
            Got::Eof
        ));
    }

    /// The header is read by itself with its own bound: a first line of
    /// megabytes without a newline is a bad header after at most 256 B.
    #[test]
    fn history_header_is_read_bounded() {
        struct Endless(usize);
        impl Read for Endless {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                assert!(self.0 < 1 << 20, "read far past the header bound");
                b.fill(b'h');
                self.0 += b.len();
                Ok(b.len())
            }
        }
        let mut e = Endless(0);
        let err = job_records(
            &mut BufReader::with_capacity(64, &mut e),
            &JobId("1-1".into()),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(e.0 <= HEADER_MAX + 64, "read {}", e.0);
        // The file side: 256 B and 257 B first lines, and a huge one.
        let t = Tmp::new("hdr");
        for (len, nl) in [(255, true), (256, true), (257, true), (3_000_000, false)] {
            let p = t.0.join("h");
            let mut body = "x".repeat(len);
            if nl {
                body.push('\n');
            }
            fs::write(&p, &body).unwrap();
            let f = File::open(&p).unwrap();
            let e = read_header_len(&f).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "len {len}");
        }
        // Positive control: the real header.
        let p = t.0.join("h");
        fs::write(&p, "inacircle-history 1\nrest").unwrap();
        assert_eq!(
            read_header_len(&File::open(&p).unwrap()).unwrap(),
            Some((1, 20))
        );
    }

    /// The window's reader never goes past the size the scan began with,
    /// whatever the file grows to meanwhile.
    #[test]
    fn the_scan_stops_at_the_size_it_began_with() {
        let t = Tmp::new("grow");
        let p = t.0.join("g");
        fs::write(&p, "0123456789").unwrap();
        let f = File::open(&p).unwrap();
        let mut at = At {
            f: &f,
            pos: 2,
            end: 10,
        };
        let mut w = OpenOptions::new().append(true).open(&p).unwrap();
        w.write_all(&[b'z'; 100_000]).unwrap();
        let mut out = Vec::new();
        at.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"23456789");
    }

    /// A fresh job in the 8 MiB window behind a long old prefix is handled
    /// (the prefix is never read); a job partly or fully outside it is
    /// refused before anything moves.
    #[test]
    fn a_fresh_job_behind_a_big_prefix_is_handled_and_one_outside_is_not() {
        let filler = |w: &mut dyn Write, bytes: usize, last_time: u64| {
            let l = write_line(
                &Line::new("child")
                    .pair("job", "99-9")
                    .pair("group", "00000000000000aa")
                    .pair("rel", &"p".repeat(200)),
            );
            for _ in 0..bytes / l.len() + 1 {
                w.write_all(l.as_bytes()).unwrap();
            }
            // An odd tail so the window start lands inside a line.
            w.write_all(b"child\tjob=99-9\tpartial").unwrap();
            writeln!(
                w,
                "\njob\tid=99-9\ttime={last_time}\taction=a\tstatus=done\tsandbox=yes"
            )
            .unwrap();
        };
        let append = |w: &mut dyn FnMut(&mut dyn Write)| {
            let mut f = OpenOptions::new()
                .append(true)
                .open(history_path())
                .unwrap();
            w(&mut f);
        };
        // (1) A big old prefix, then the whole fresh job: undone.
        let t = Tmp::new("window-ok");
        t.env(|| {
            let job = JobId("1790000000-1-aaaaaaaa".into());
            drop(Recorder::open(&JobId("9-9".into()), "a").finish(true));
            append(&mut |w| filler(w, 9 << 20, 1));
            let f = job_of(&t, &job, &[("a", "d"), ("b", "e")]);
            assert!(fs::metadata(history_path()).unwrap().len() > 9 << 20);
            assert!(all_moved(&undo(&job).unwrap(), &f));
        });
        // (2) The job entirely before a big suffix: outside the window.
        let t = Tmp::new("window-out");
        t.env(|| {
            let job = JobId("10-1".into());
            let f = job_of(&t, &job, &[("a", "d")]);
            append(&mut |w| filler(w, 9 << 20, 1));
            let e = undo(&job).unwrap_err();
            assert_eq!(e.to_string(), WINDOW_REFUSAL);
            assert!(f[0].exists(), "nothing moved");
        });
        // (3) A job whose start and first group lie before the window and
        // the second group inside: refused, nothing moved.
        let t = Tmp::new("window-part");
        t.env(|| {
            let job = new_job_id();
            let mut rec = Recorder::open(&job, "convert");
            let mut f = publish(&t, &mut rec, "a", &[("x.md", "1")]);
            append(&mut |w| filler(w, 9 << 20, unix_secs()));
            f.extend(publish(&t, &mut rec, "b", &[("x.md", "2")]));
            assert!(rec.finish(true).undoable);
            let e = undo(&job).unwrap_err();
            assert_eq!(e.to_string(), WINDOW_REFUSAL);
            assert!(f.iter().all(|p| p.exists()), "nothing moved");
        });
    }

    /// In a window that does not start at the history's start, only the
    /// job's own `job-start` proves that none of its records lie before
    /// the window, not any clock: each case below has a finished-job line
    /// that the old rule (a `job` line whose `time` is 5 s before the job
    /// started) took as proof, with the job's first group outside.
    #[test]
    fn only_the_jobs_own_start_proves_it_whole_in_the_window() {
        // Another job's `job` line, written after `time` was taken.
        let late = |w: &mut dyn Write, time: u64| {
            writeln!(
                w,
                "job\tid=77-7\ttime={time}\taction=a\tstatus=done\tsandbox=yes"
            )
            .unwrap();
        };
        let filler = |w: &mut dyn Write| {
            let l = write_line(
                &Line::new("child")
                    .pair("job", "99-9")
                    .pair("group", "00000000000000aa")
                    .pair("rel", &"p".repeat(200)),
            );
            for _ in 0..(9 << 20) / l.len() + 1 {
                w.write_all(l.as_bytes()).unwrap();
            }
        };
        let append = |w: &mut dyn FnMut(&mut dyn Write)| {
            let mut f = OpenOptions::new()
                .append(true)
                .open(history_path())
                .unwrap();
            w(&mut f);
        };
        let started = |job: &JobId| -> u64 { job.0.split('-').next().unwrap().parse().unwrap() };
        // (a) `finish()`'s error branch: `time` is taken, then two append
        // lock waits (2 s each) and scheduling delay pass before the line
        // lands; here 6 s, after this job's first group.
        // (b) A wall clock stepped forward (NTP, by hand) by an hour
        // between the other job's `time` and this job's start (the job's
        // start is injected through its id).
        let jump = JobId(format!("{}-1-bbbbbbbb", unix_secs() + 3600));
        for (name, job, back) in [("delay", new_job_id(), 6), ("jump", jump.clone(), 3600)] {
            let t = Tmp::new(&format!("window-{name}"));
            t.env(|| {
                let mut rec = Recorder::open(&job, "convert");
                let mut f = publish(&t, &mut rec, "a", &[("x.md", "1")]);
                append(&mut |w| filler(w));
                append(&mut |w| late(w, started(&job) - back));
                f.extend(publish(&t, &mut rec, "b", &[("x.md", "2")]));
                assert!(rec.finish(true).undoable);
                let e = undo(&job).unwrap_err();
                assert_eq!(e.to_string(), WINDOW_REFUSAL, "{name}");
                assert!(f.iter().all(|p| p.exists()), "{name}: nothing moved");
            });
        }
        // (c) The job's start inside the window, but a record of it before
        // the start: refused (a record cannot precede its job's start).
        let t = Tmp::new("window-before-start");
        t.env(|| {
            let job = new_job_id();
            drop(Recorder::open(&JobId("9-9".into()), "a").finish(true));
            append(&mut |w| filler(w));
            append(&mut |w| late(w, 1));
            append(&mut |w| writeln!(w, "skip\tjob={}\tin=x\treason=y", job.0).unwrap());
            let f = job_of(&t, &job, &[("a", "d")]);
            let e = undo(&job).unwrap_err();
            assert_eq!(e.to_string(), WINDOW_REFUSAL);
            assert!(f[0].exists(), "nothing moved");
        });
        // (d) Positive control: the same history with the job's start in
        // the window and nothing of it before: undone.
        let t = Tmp::new("window-start-ok");
        t.env(|| {
            let job = new_job_id();
            drop(Recorder::open(&JobId("9-9".into()), "a").finish(true));
            append(&mut |w| filler(w));
            append(&mut |w| late(w, 1));
            let f = job_of(&t, &job, &[("a", "d"), ("b", "e")]);
            assert!(fs::metadata(history_path()).unwrap().len() > 9 << 20);
            assert!(all_moved(&undo(&job).unwrap(), &f));
        });
    }

    /// A job file over the limit, or a listing past it, is an error: undo
    /// then moves nothing (it cannot tell whether the job runs).
    #[test]
    fn undo_refuses_unknown_running_state() {
        let t = Tmp::new("unkrun");
        t.env(|| {
            let job = JobId("10-1".into());
            let f = job_of(&t, &job, &[("a", "d")]);
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            // The job's own file is the one that is too big.
            let mut line = write_line(
                &Line::new("job")
                    .pair("id", "10-1")
                    .pair("pid", &std::process::id().to_string())
                    .pair("action", &"a".repeat(REGISTRY_ENTRY_MAX)),
            );
            line.truncate(REGISTRY_ENTRY_MAX + 1);
            fs::write(dir.join("10-1"), &line).unwrap();
            let e = undo(&job).unwrap_err();
            assert!(e.to_string().contains("safety limits"), "{e}");
            assert!(f[0].exists(), "nothing moved on a guess");
            // Control: with the file gone the same undo works.
            fs::remove_file(dir.join("10-1")).unwrap();
            assert!(all_moved(&undo(&job).unwrap(), &f));
        });
        // More entries than the listing takes: also an error.
        let t = Tmp::new("unkrun2");
        t.env(|| {
            let job = JobId("10-2".into());
            let f = job_of(&t, &job, &[("a", "d")]);
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            for i in 0..=REGISTRY_ENTRIES_MAX {
                fs::write(dir.join(format!("live{i}")), "junk").unwrap();
            }
            let e = undo(&job).unwrap_err();
            assert!(e.to_string().contains("safety limits"), "{e}");
            assert!(f[0].exists());
        });
    }

    fn put_job(dir: &Path, id: &str, pid: u32, extra: &[(String, String)], action: &str) {
        let mut l = Line::new("job")
            .pair("id", id)
            .pair("pid", &pid.to_string())
            .pair("action", action);
        for (k, v) in extra {
            l = l.pair(k, v);
        }
        fs::write(dir.join(id), write_line(&l)).unwrap();
    }

    fn put_ident(dir: &Path, id: &str, pid: u32, start: u64, boot: &str, started: &str) {
        let extra = [
            ("start".to_owned(), start.to_string()),
            ("boot".to_owned(), boot.to_owned()),
            ("started".to_owned(), started.to_owned()),
        ];
        put_job(dir, id, pid, &extra, "x");
    }

    /// Liveness is the pid plus the start time in ticks since boot plus
    /// the boot id; the wall clock is no input. A stale or far-future
    /// `started` field (what a stepped clock would have made of a
    /// btime-derived time) changes nothing.
    #[test]
    fn job_identity_is_pid_ticks_and_boot_not_wall_time() {
        let t = Tmp::new("regident");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let me = std::process::id();
            let own = ProcId::of(me).expect("/proc readable");
            // Live job, whatever the wall clock said when it started.
            for started in ["1", "99999999999", "0"] {
                put_ident(&dir, "7-1", me, own.ticks, &own.boot, started);
                assert_eq!(list_running().unwrap().len(), 1, "started={started}");
            }
            // The pid reused by a process that started at another tick.
            put_ident(&dir, "7-1", me, own.ticks + 1, &own.boot, "1");
            assert!(list_running().unwrap().is_empty());
            assert!(!dir.join("7-1").exists(), "dead entry removed");
            put_ident(&dir, "7-1", me, own.ticks.wrapping_sub(1), &own.boot, "1");
            assert!(list_running().unwrap().is_empty());
            // After a reboot: same pid, same ticks, another boot id.
            put_ident(&dir, "7-1", me, own.ticks, "00000000-other-boot", "1");
            assert!(list_running().unwrap().is_empty());
            // No identity recorded (older version): the pid decides.
            put_job(&dir, "7-1", me, &[], "x");
            assert_eq!(list_running().unwrap().len(), 1);
        });
    }

    #[test]
    fn job_alive_compares_identity_only() {
        let me = std::process::id();
        let rec = ProcId {
            ticks: 100,
            boot: "b".into(),
        };
        let other = |ticks, boot: &str| ProcId {
            ticks,
            boot: boot.into(),
        };
        assert!(job_alive_with(me, Some(&rec), Some(&rec)));
        assert!(job_alive_with(me, Some(&rec), None), "unreadable /proc");
        assert!(job_alive_with(me, None, Some(&rec)));
        assert!(!job_alive_with(me, Some(&rec), Some(&other(101, "b"))));
        assert!(!job_alive_with(me, Some(&rec), Some(&other(100, "c"))));
        // A pid that does not exist is dead whatever is recorded.
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let gone = c.id();
        c.wait().unwrap();
        assert!(!job_alive_with(gone, None, None));
    }

    #[test]
    fn start_ticks_parse_after_the_last_paren_of_comm() {
        // Fields 3..: state ppid ... starttime is field 22.
        let tail: Vec<String> = (3..=30).map(|n| n.to_string()).collect();
        let stat = format!("42 (a) b (c)) {}", tail.join(" "));
        assert_eq!(parse_start_ticks(&stat), Some(22));
        assert_eq!(parse_start_ticks("42 (x) S 1 2"), None);
        assert_eq!(parse_start_ticks("no paren"), None);
    }

    /// A truncated history window that starts exactly at the job's
    /// `job-start` line reads it whole (job undone); one that cuts inside
    /// that line drops it, so the rest of the job is refused.
    #[test]
    fn a_window_at_or_inside_the_job_start_line() {
        for (cut, ok) in [(0u64, true), (1, false), (6, false)] {
            let t = Tmp::new(&format!("window-edge-{cut}"));
            t.env(|| {
                // A prior job, so the job's start is not at the header.
                drop(Recorder::open(&JobId("9-9".into()), "a").finish(true));
                let job = new_job_id();
                let len0 = fs::metadata(history_path()).unwrap().len();
                let f = job_of(&t, &job, &[("a", "d")]);
                let mine = fs::metadata(history_path()).unwrap().len() - len0;
                // Filler so the file ends HISTORY_TAIL_WINDOW - cut bytes
                // ... after the start line begins: window start = len0 + cut.
                let want = HISTORY_TAIL_WINDOW + cut - mine;
                let line = |rel: usize| {
                    write_line(
                        &Line::new("child")
                            .pair("job", "99-9")
                            .pair("group", "00000000000000aa")
                            .pair("rel", &"p".repeat(rel)),
                    )
                };
                let one = line(200).len() as u64;
                let n = want / one;
                let rest = want - n * one; // folded into the last line
                let mut h = OpenOptions::new()
                    .append(true)
                    .open(history_path())
                    .unwrap();
                for _ in 0..n - 1 {
                    h.write_all(line(200).as_bytes()).unwrap();
                }
                h.write_all(line(200 + rest as usize).as_bytes()).unwrap();
                drop(h);
                let size = fs::metadata(history_path()).unwrap().len();
                assert_eq!(size - HISTORY_TAIL_WINDOW, len0 + cut, "window start");
                let r = undo(&job);
                if ok {
                    assert!(all_moved(&r.unwrap(), &f), "cut={cut}");
                } else {
                    assert_eq!(r.unwrap_err().to_string(), WINDOW_REFUSAL, "cut={cut}");
                    assert!(f[0].exists(), "cut={cut}: nothing moved");
                }
            });
        }
    }

    #[test]
    fn registry_file_4096_and_4097() {
        let t = Tmp::new("regbytes");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let mut l = write_line(
                &Line::new("job")
                    .pair("id", "5-1")
                    .pair("pid", &std::process::id().to_string())
                    .pair("action", ""),
            );
            let pad = REGISTRY_ENTRY_MAX - l.len() + 1; // newline replaced
            l = write_line(
                &Line::new("job")
                    .pair("id", "5-1")
                    .pair("pid", &std::process::id().to_string())
                    .pair("action", &"a".repeat(pad - 1)),
            );
            assert_eq!(l.len(), REGISTRY_ENTRY_MAX);
            fs::write(dir.join("5-1"), &l).unwrap();
            assert_eq!(list_running().unwrap().len(), 1);
            l.insert(l.len() - 1, 'a');
            fs::write(dir.join("5-1"), &l).unwrap();
            assert!(
                list_running()
                    .unwrap_err()
                    .to_string()
                    .contains("safety limits")
            );
        });
    }

    #[test]
    fn registry_fields_16_and_17() {
        let t = Tmp::new("regfields");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let me = std::process::id();
            let extra = |n: usize| -> Vec<(String, String)> {
                (0..n).map(|i| (format!("k{i}"), "v".to_owned())).collect()
            };
            // id, pid, action are 3 fields.
            put_job(&dir, "5-1", me, &extra(13), "x");
            assert_eq!(list_running().unwrap().len(), 1);
            put_job(&dir, "5-1", me, &extra(14), "x");
            assert!(list_running().is_err());
        });
    }

    #[test]
    fn registry_entries_128_and_129_and_dead_ones_heal() {
        let t = Tmp::new("regcount");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let dead = child.id();
            child.wait().unwrap();
            // 128 dead entries: listed (examined) and removed.
            for i in 0..REGISTRY_ENTRIES_MAX {
                put_job(&dir, &format!("5-{i}"), dead, &[], "x");
            }
            assert!(list_running().unwrap().is_empty());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
            // 129 live entries: an error (never a shorter list).
            for i in 0..=REGISTRY_ENTRIES_MAX {
                put_job(&dir, &format!("6-{i}"), std::process::id(), &[], "x");
            }
            assert!(
                list_running()
                    .unwrap_err()
                    .to_string()
                    .contains("safety limits")
            );
            for i in 0..=REGISTRY_ENTRIES_MAX {
                fs::remove_file(dir.join(format!("6-{i}"))).unwrap();
            }
            // 129 dead entries: the first call errs after removing the
            // 128 it examined, the second works.
            for i in 0..=REGISTRY_ENTRIES_MAX {
                put_job(&dir, &format!("7-{i}"), dead, &[], "x");
            }
            assert!(list_running().is_err());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
            assert!(list_running().unwrap().is_empty());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        });
    }

    /// An entry that cannot be opened for a reason other than "gone, or
    /// not a regular file of ours" is an error, never "no job runs"; the
    /// skipped kinds (a FIFO, a symlink) still let the rest be listed.
    #[test]
    fn registry_open_errors_fail_closed() {
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root opens a mode-000 file");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("regopen");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let me = std::process::id();
            put_job(&dir, "5-1", me, &[], "x");
            crate::sys::tests::mkfifo(&dir.join("5-2"));
            std::os::unix::fs::symlink(dir.join("5-1"), dir.join("5-3")).unwrap();
            let l = list_running().unwrap();
            assert_eq!(l.len(), 1, "{l:?}");
            put_job(&dir, "5-4", me, &[], "x");
            fs::set_permissions(dir.join("5-4"), fs::Permissions::from_mode(0o000)).unwrap();
            let e = list_running().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        });
    }

    /// Our own temporary files left by a crash (`.<id>.<16 hex>.tmp`, a
    /// regular file of ours, untouched for a minute) are removed as the
    /// dead entries are, so they cannot fill the listing for good. A fresh
    /// one and any other dot file stay.
    #[test]
    fn registry_own_stale_temps_heal() {
        let t = Tmp::new("regtmp");
        t.env(|| {
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(&dir).unwrap();
            let old = SystemTime::now() - Duration::from_secs(3600);
            let temp = |i: usize, stale: bool| {
                let p = dir.join(format!(".5-{i}.{i:016x}.tmp"));
                let f = File::create(&p).unwrap();
                if stale {
                    f.set_modified(old).unwrap();
                }
                p
            };
            for i in 0..=REGISTRY_ENTRIES_MAX {
                temp(i, true);
            }
            assert!(list_running().is_err(), "129 entries: over the limit");
            assert!(list_running().unwrap().is_empty());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
            let fresh = temp(1, false);
            let other = dir.join(".5-2.notours.tmp");
            File::create(&other).unwrap().set_modified(old).unwrap();
            assert!(list_running().unwrap().is_empty());
            assert!(fresh.exists() && other.exists());
        });
    }

    /// A registry folder that cannot be opened is an error, not "no job
    /// runs"; a missing one is no job.
    #[test]
    fn registry_folder_unopenable_is_an_error_missing_is_empty() {
        let t = Tmp::new("regdir");
        t.env(|| {
            assert!(list_running().unwrap().is_empty(), "missing: no job");
            let dir = jobs_dir().unwrap();
            fs::create_dir_all(dir.parent().unwrap()).unwrap();
            fs::write(&dir, "a file where the folder should be").unwrap();
            assert!(list_running().is_err());
        });
    }

    /// The writer keeps its line listable: a huge action name is cut.
    #[test]
    fn a_huge_action_name_keeps_the_registry_entry_listable() {
        let t = Tmp::new("regaction");
        t.env(|| {
            let job = JobId("10-9".into());
            let _r = Running::start(&job, &"é".repeat(100_000), 1);
            let l = list_running().unwrap();
            assert_eq!(l.len(), 1);
            assert!(l[0].1.len() <= REGISTRY_ACTION_MAX && !l[0].1.is_empty());
        });
    }
}
