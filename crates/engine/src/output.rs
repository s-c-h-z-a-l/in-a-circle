//! Output policy: where results go and how each input's results are
//! published (fresh copies, one no-replace rename) without ever
//! overwriting or deleting anything visible.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::sys::{self, fstat, ident, last_err};
use crate::xdg;

const WORK_PREFIX: &str = ".inacircle-";
const PROBE_PREFIX: &str = "probe-";
const MAX_CONFLICT: u32 = 999;
const NAME_MAX: usize = 255;
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// A hidden 0700 scratch directory inside the target folder (the
/// producers' staging). Removed recursively on drop, through the target
/// folder's fd and only while its name is still the directory made here:
/// a renamed folder still gets its own work dir removed, and a look-alike
/// at the old path is left alone.
pub struct WorkDir {
    pub path: PathBuf,
    parent: Fd,
    name: CString,
    id: (u64, u64),
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let Ok(dir) = open_dir_for_removal(Some(self.parent.0), &self.name) else {
            return;
        };
        if fstat(dir.0).map(|st| ident(&st)).ok() != Some(self.id) {
            return;
        }
        empty_dir(&dir);
        unlink_own(self.parent.0, &self.name, self.id, libc::AT_REMOVEDIR);
    }
}

/// Closes the fd on drop.
struct Fd(RawFd);
impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: self.0 is an open fd owned exclusively by this guard.
        unsafe { libc::close(self.0) };
    }
}

/// Opens a directory relative to `at` (or the cwd for `None`) for removal,
/// refusing to follow a symlink in the last component, and gives it owner
/// rwx first: a 0o000 or 0o300 directory can be neither opened for reading
/// nor emptied. The chmod goes through an `O_PATH` fd (`/proc/self/fd/<n>`),
/// so it hits exactly the inode that was opened. Without `/proc` (or when
/// chmod is refused) it falls back to opening the name again, which fails
/// for a directory without `r`; that open is `O_NONBLOCK` (a FIFO swapped in
/// cannot block it) and its fd must be the very inode of the `O_PATH` fd, or
/// the directory is refused and left alone: both routes end at the verified
/// inode.
fn open_dir_for_removal(at: Option<RawFd>, name: &CString) -> io::Result<Fd> {
    let at = at.unwrap_or(libc::AT_FDCWD);
    // mutants: equivalent — the flags are distinct single bits, so `|` and
    // `^` build the same value (the same holds for every flag set below).
    let path_flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: name is a valid NUL-terminated string that outlives the call;
    // `at` is an open directory fd or AT_FDCWD.
    let p = unsafe { libc::openat(at, name.as_ptr(), path_flags) };
    if p < 0 {
        return last_err();
    }
    let p = Fd(p);
    #[cfg(test)]
    tests::AFTER_OPATH.with_borrow_mut(|h| h.as_mut().map(|f| f()));
    let magic = CString::new(format!("/proc/self/fd/{}", p.0)).expect("no NUL in a number");
    // mutants: equivalent — `^` as above; and `O_RDONLY | O_DIRECTORY` as
    // `&` only drops O_DIRECTORY: on the magic link the inode is already
    // proved a directory, and on the fallback the device/inode comparison
    // below refuses anything that is not that directory (a regular file or
    // FIFO swapped in is opened O_NONBLOCK and then refused there).
    let read_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // mutants: `== 0` as `!= 0` is NOT equivalent and is killed by the
    // `swap_magic` case: a successful chmod would send the mutant to the
    // fallback, which opens the name again (the swapped-in impostor is only
    // refused afterwards, by the inode check) instead of using the magic
    // link to the opened inode; and with chmod refused on a readable,
    // non-writable directory the mutant would skip the fchmod's route
    // entirely. The real route is pinned by the original-inode assertion.
    // SAFETY: magic is a valid NUL-terminated path naming our O_PATH fd.
    if unsafe { libc::chmod(magic.as_ptr(), 0o700) } == 0 {
        // SAFETY: as above; the magic link resolves to the opened inode.
        let fd = unsafe { libc::openat(libc::AT_FDCWD, magic.as_ptr(), read_flags) };
        if fd >= 0 {
            return Ok(Fd(fd));
        }
    }
    // mutants: equivalent — `^` as above; and `fd < 0` as `fd <= 0`: `p`
    // is still open and was the lowest free fd, so this open cannot get 0.
    // mutants: equivalent — `^` as above; `|` as `&` on the last flag
    // (`NOFOLLOW & NONBLOCK`, by precedence) drops both: O_DIRECTORY already
    // makes a FIFO or file fail at once without blocking, and any symlink or
    // directory swapped in is refused by the device/inode comparison below
    // (a symlink to the very same inode is the directory we opened).
    // SAFETY: as for the O_PATH open.
    let fd = unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            read_flags | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return last_err();
    }
    let fd = Fd(fd);
    if !same_inode(p.0, fd.0) {
        return Err(io::Error::other("directory changed while being opened"));
    }
    // SAFETY: fd.0 is an open fd.
    unsafe { libc::fchmod(fd.0, 0o700) };
    Ok(fd)
}

/// Whether the two open fds name the same file (device and inode).
fn same_inode(a: RawFd, b: RawFd) -> bool {
    let id = |fd: RawFd| {
        // SAFETY: zeroed stat is valid storage; fd is an open fd.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev, st.st_ino))
    };
    matches!((id(a), id(b)), (Some(x), Some(y)) if x == y)
}

/// Entry names of the directory open as `dir` (without `.` and `..`).
fn list_dir(dir: &Fd) -> Vec<CString> {
    // SAFETY: dir.0 is an open fd; dup gives fdopendir its own fd to own.
    let dup = unsafe { libc::fcntl(dir.0, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Vec::new();
    }
    // SAFETY: dup is an open directory fd; on success the DIR owns it.
    let d = unsafe { libc::fdopendir(dup) };
    if d.is_null() {
        // SAFETY: fdopendir failed, so dup is still ours to close.
        unsafe { libc::close(dup) };
        return Vec::new();
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: d is a valid DIR until closedir below.
        let ent = unsafe { libc::readdir(d) };
        if ent.is_null() {
            break;
        }
        // SAFETY: readdir returned a valid dirent whose d_name is
        // NUL-terminated; we copy it before the next readdir call.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    // SAFETY: d is a valid DIR, closed exactly once (this also closes dup).
    unsafe { libc::closedir(d) };
    names
}

/// Removes everything under the open directory `dir`. Symlinks are unlinked,
/// never followed; every step is relative to an fd, so a concurrent swap of
/// a path component cannot redirect the deletion. Errors are ignored.
fn empty_dir(dir: &Fd) {
    for name in list_dir(dir) {
        // SAFETY: st is a valid out-pointer; name is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(dir.0, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if r != 0 {
            continue;
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if is_dir && let Ok(child) = open_dir_for_removal(Some(dir.0), &name) {
            empty_dir(&child);
        }
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: dir.0 is an open dir fd, name is NUL-terminated.
        unsafe { libc::unlinkat(dir.0, name.as_ptr(), flags) };
    }
}

/// Removes the directory `path` and everything under it, including
/// read-only files and directories. `path` itself must be a real directory
/// (a symlink is left alone). Never panics; errors are ignored.
fn remove_tree(path: &Path) {
    let Ok(c) = cstr(path) else { return };
    let Ok(dir) = open_dir_for_removal(None, &c) else {
        return;
    };
    empty_dir(&dir);
    // SAFETY: c is a valid NUL-terminated path; rmdir never follows symlinks.
    unsafe { libc::rmdir(c.as_ptr()) };
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

pub(crate) fn random_u64() -> io::Result<u64> {
    let mut buf = [0u8; 8];
    loop {
        // SAFETY: buf is a valid, writable 8-byte buffer and we pass its
        // exact length; flags 0 is valid.
        let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n == buf.len() as isize {
            return Ok(u64::from_ne_bytes(buf));
        }
        let err = io::Error::last_os_error();
        // mutants: equivalent — `n < 0` as `n <= 0` differs only for a
        // return of 0 with a stale errno of EINTR, which the kernel does not
        // produce (the 0 case is tested: it is a short read).
        if n < 0 && err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if n < 0 {
            err
        } else {
            io::Error::new(io::ErrorKind::UnexpectedEof, "short getrandom")
        });
    }
}

fn mkdir_mode(path: &Path, mode: libc::mode_t) -> io::Result<()> {
    let c = cstr(path)?;
    // SAFETY: c is a valid NUL-terminated path that outlives the call.
    if unsafe { libc::mkdir(c.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        last_err()
    }
}

/// Creates `<target>/.inacircle-<16 hex>` with mode 0700.
pub fn make_work_dir(target_dir: &Path) -> io::Result<WorkDir> {
    let parent = open_path(libc::AT_FDCWD, &cstr(target_dir)?, 0)?;
    for _ in 0..16 {
        let name = format!("{WORK_PREFIX}{:016x}", random_u64()?);
        let path = target_dir.join(&name);
        let name = CString::new(name).expect("no NUL in a hex name");
        // SAFETY: name is NUL-terminated; parent is an open dir fd.
        if unsafe { libc::mkdirat(parent.0, name.as_ptr(), 0o700) } != 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(e);
        }
        let id = ident(&stat_at(At {
            dir: parent.0,
            name: &name,
        })?);
        // Independent of the umask. The WorkDir guard removes the
        // directory again if this fails.
        let dir = WorkDir {
            path,
            parent,
            name,
            id,
        };
        // SAFETY: as above; the name is the directory just made.
        if unsafe { libc::fchmodat(dir.parent.0, dir.name.as_ptr(), 0o700, 0) } != 0 {
            return last_err();
        }
        return Ok(dir);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free work directory name",
    ))
}

fn is_lower_hex16(b: &[u8]) -> bool {
    b.len() == 16 && b.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Exactly the names we create: `.inacircle-<16 hex>` and
/// `.inacircle-probe-<16 hex>`.
fn is_our_name(name: &[u8]) -> bool {
    let Some(rest) = name.strip_prefix(WORK_PREFIX.as_bytes()) else {
        return false;
    };
    is_lower_hex16(rest)
        || rest
            .strip_prefix(PROBE_PREFIX.as_bytes())
            .is_some_and(is_lower_hex16)
}

/// Removes our own work and probe directories older than a day (left by a
/// crash). Anything else in the folder is untouched, symlinks included.
pub fn clean_stale(target_dir: &Path) {
    let Ok(entries) = fs::read_dir(target_dir) else {
        return;
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let now = SystemTime::now();
    for e in entries.flatten() {
        if !is_our_name(e.file_name().as_bytes()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(e.path()) else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            // mutants: equivalent — `>` as `>=` differs only for an age of
            // exactly 24 h to the nanosecond, read from the running clock.
            .is_some_and(|age| age > STALE_AFTER);
        // symlink_metadata: a symlink is not a dir here, and remove_tree
        // refuses to open one anyway.
        if meta.is_dir() && meta.uid() == uid && old {
            remove_tree(&e.path());
        }
    }
}

/// The source's folder if a test directory can really be created there
/// (access(2) alone lies on read-only mounts and ACLs), else the download
/// folder.
pub fn writable_target(source_dir: &Path) -> io::Result<PathBuf> {
    let ok = random_u64().is_ok_and(|r| {
        let probe = source_dir.join(format!("{WORK_PREFIX}{PROBE_PREFIX}{r:016x}"));
        match mkdir_mode(&probe, 0o700) {
            Ok(()) => {
                let _ = fs::remove_dir(&probe);
                true
            }
            // The name is taken, so the directory accepts new entries.
            Err(e) => e.kind() == io::ErrorKind::AlreadyExists,
        }
    });
    if ok {
        Ok(source_dir.to_path_buf())
    } else {
        xdg::download_dir()
    }
}

/// A name relative to a directory fd (`AT_FDCWD` for a plain path).
#[derive(Clone, Copy)]
struct At<'a> {
    dir: RawFd,
    name: &'a CStr,
}

/// The identity of a file at the moment it was placed. Undo deletes a file
/// only if all of it still matches; `ctime` cannot be set from user space,
/// so it also catches an edit followed by an mtime restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

impl FileId {
    // st_dev/st_ino/st_size widths differ between targets.
    #[allow(clippy::unnecessary_cast)]
    pub fn from_stat(st: &libc::stat) -> FileId {
        FileId {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            size: st.st_size as u64,
            mtime_ns: i128::from(st.st_mtime) * 1_000_000_000 + i128::from(st.st_mtime_nsec),
            ctime_ns: i128::from(st.st_ctime) * 1_000_000_000 + i128::from(st.st_ctime_nsec),
        }
    }
}

/// The inode type of a published object, kept apart from its mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Directory,
}

/// The identity of an input's wrapper directory, taken while it was still
/// private. `mode` holds only the permission and special bits; the type is
/// `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryId {
    pub dev: u64,
    pub ino: u64,
    pub kind: NodeKind,
    pub uid: u32,
    pub mode: u32,
}

/// The baseline of one published child, taken from the final private copy
/// before publication; never retaken afterwards, so a later edit cannot
/// become the baseline. `rel` is relative to the wrapper directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildBaseline {
    pub rel: PathBuf,
    pub file_id: FileId,
    pub kind: NodeKind,
    pub uid: u32,
    pub mode: u32,
    pub nlink: u64,
}

/// One input's output group, unique per publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u64);

/// The publication operation of a group (the journal intent's id): the
/// same before the commit and after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u64);

/// An input's outputs as fixed in private: the wrapper's identity and every
/// child's baseline. `directory` is the planned target name, history only:
/// on its own it is no evidence of where the group is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputGroup {
    pub group: GroupId,
    pub op: OperationId,
    pub directory: PathBuf,
    pub id: DirectoryId,
    pub children: Vec<ChildBaseline>,
}

/// Whether the publication is known to be on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Durability {
    /// Every file, the wrapper and both parents were synced.
    Synced,
    /// Visible, but a sync after the commit failed or was impossible.
    Unconfirmed(String),
}

/// Where a published object was anchored: its parent's and its own
/// device/inode, its leaf name and the operation. A hint, not a handle
/// that can be reopened later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorHint {
    pub parent_dev: u64,
    pub parent_ino: u64,
    pub object_dev: u64,
    pub object_ino: u64,
    pub leaf: OsString,
    pub op: OperationId,
}

/// The moment a location was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckPoint {
    /// Undo: before anything moved.
    Precheck,
    /// Right after the publishing rename and its syncs.
    PostPublish,
    /// Undo: after the move into the Trash (or its failed way back).
    PostMove,
    /// Undo: after the move back.
    PostRestore,
    /// When an object's path was resolved from its fd (undo).
    PostResolve,
    /// Undo: when the move into the Trash was committed.
    Commit,
}

impl CheckPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckPoint::Precheck => "Precheck",
            CheckPoint::PostPublish => "PostPublish",
            CheckPoint::PostMove => "PostMove",
            CheckPoint::PostRestore => "PostRestore",
            CheckPoint::PostResolve => "PostResolve",
            CheckPoint::Commit => "Commit",
        }
    }

    pub fn parse(s: &str) -> Option<CheckPoint> {
        [
            CheckPoint::Precheck,
            CheckPoint::PostPublish,
            CheckPoint::PostMove,
            CheckPoint::PostRestore,
            CheckPoint::PostResolve,
            CheckPoint::Commit,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }
}

/// What is known about where an object is: a path proved to name it at
/// `checked_at`, or `None` when that could not be proved. Nothing is
/// promised about later renames by other programs. `last_unverified_path`
/// is a diagnostic only (the last path seen, not proved), never evidence
/// of a place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationEvidence {
    pub path_at_check: Option<PathBuf>,
    pub last_unverified_path: Option<PathBuf>,
    pub anchor: AnchorHint,
    pub checked_at: CheckPoint,
}

/// A committed publication: visible under its name. Durability and the
/// location are separate facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub group: OutputGroup,
    pub location: LocationEvidence,
    pub durability: Durability,
}

/// Why a committed group cannot be reported as placed files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationReason {
    /// The checked path no longer reaches the published wrapper (an
    /// ancestor or the wrapper itself was renamed or replaced).
    AncestorChanged,
    /// The wrapper's children no longer match their private baselines.
    BaselineChanged,
}

impl PublicationReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PublicationReason::AncestorChanged => "AncestorChanged",
            PublicationReason::BaselineChanged => "BaselineChanged",
        }
    }
}

fn kind_of(st: &libc::stat) -> Option<NodeKind> {
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => Some(NodeKind::File),
        libc::S_IFDIR => Some(NodeKind::Directory),
        _ => None,
    }
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// A name part: no `/`, no NUL, not `.` or `..`.
fn check_part(part: &[u8]) -> io::Result<()> {
    if part.iter().any(|&b| b == b'/' || b == 0) || part == b"." || part == b".." {
        return Err(invalid("invalid output name"));
    }
    Ok(())
}

/// `<stem> (n).<ext>` as bytes. The stem is cut (on a UTF-8 boundary) so
/// the whole name fits in `NAME_MAX`.
fn candidate(stem: &[u8], ext: &[u8], n: u32) -> io::Result<CString> {
    let mut tail = Vec::new();
    if n > 0 {
        tail.extend_from_slice(format!(" ({n})").as_bytes());
    }
    if !ext.is_empty() {
        tail.push(b'.');
        tail.extend_from_slice(ext);
    }
    let budget = NAME_MAX.saturating_sub(tail.len());
    let mut cut = stem.len().min(budget);
    // Never end inside a multi-byte character.
    while cut > 0 && cut < stem.len() && stem[cut] & 0xC0 == 0x80 {
        cut -= 1;
    }
    if cut == 0 {
        return Err(invalid("output name too long"));
    }
    let mut name = stem[..cut].to_vec();
    name.extend_from_slice(&tail);
    CString::new(name).map_err(|_| invalid("invalid output name"))
}

/// Splits at the last dot (a leading dot is part of the stem).
fn split_name(name: &[u8]) -> (&[u8], &[u8]) {
    match name.iter().rposition(|&c| c == b'.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, &[]),
    }
}

/// `openat(at, name, O_PATH | O_DIRECTORY | extra)`.
fn open_path(at: RawFd, name: &CStr, extra: libc::c_int) -> io::Result<Fd> {
    // mutants: equivalent — distinct flag bits, so `|` and `^` agree.
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | extra;
    // SAFETY: name is NUL-terminated; at is an open dir fd or AT_FDCWD.
    let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
    if fd < 0 { last_err() } else { Ok(Fd(fd)) }
}

/// Resolves `rel` beneath `root` without following any symlink: each
/// directory is opened (`O_PATH | O_NOFOLLOW`) from the previous one and
/// the leaf must be a regular file. Returns the parent fd and leaf name.
fn resolve_beneath(root: &Path, rel: &Path) -> io::Result<(Fd, CString)> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(p) => parts.push(p),
            _ => return Err(invalid("output path is not a plain relative path")),
        }
    }
    let Some((leaf, dirs)) = parts.split_last() else {
        return Err(invalid("empty output path"));
    };
    let mut dir = open_path(libc::AT_FDCWD, &cstr(root)?, libc::O_NOFOLLOW)?;
    for d in dirs {
        let name = cstr(Path::new(d))?;
        dir = open_path(dir.0, &name, libc::O_NOFOLLOW)?;
    }
    let leaf = cstr(Path::new(leaf))?;
    let st = stat_at(At {
        dir: dir.0,
        name: &leaf,
    })?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(invalid("output is not a regular file"));
    }
    Ok((dir, leaf))
}

/// Opens `rel` beneath `root` (`O_PATH`), resolved as in
/// [`resolve_beneath`]: a regular file reached without any symlink on the
/// way. The type is checked on the fd, so it is the file that was opened.
pub(crate) fn open_regular_beneath(root: &Path, rel: &Path) -> io::Result<OwnedFd> {
    let (dir, leaf) = resolve_beneath(root, rel)?;
    // mutants: equivalent — distinct flag bits, so `|` and `^` agree.
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // mutants: equivalent — `fd < 0` as `fd <= 0`: `dir` is open and was
    // allocated first, so this open cannot get fd 0.
    // SAFETY: leaf is NUL-terminated; dir.0 is an open directory fd.
    let fd = unsafe { libc::openat(dir.0, leaf.as_ptr(), flags) };
    if fd < 0 {
        return last_err();
    }
    // SAFETY: fd was just returned to us and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: zeroed stat is valid storage; fd is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fd is the open descriptor owned above; st is valid storage.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return last_err();
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(invalid("output is not a regular file"));
    }
    Ok(fd)
}

/// Bytes per read and write of an output's copy: few syscalls for outputs
/// of several MB, and one small heap buffer (well inside the 5 MB PSS
/// budget). Any value works; this one only sets the syscall count.
const COPY_CHUNK: usize = 64 * 1024;
/// The largest TXT/Markdown output published (which must also be valid
/// UTF-8): `document_output_bytes` (32 MiB) of the hardening parameters,
/// proposed, finalised at G0-P/T10 (D-23/D-24 accepted it provisionally). A text
/// output past it is refused before it is copied, so a runaway converter
/// cannot fill the user's folder with a text nobody can open.
const TEXT_OUTPUT_MAX: u64 = 32 << 20;
/// The wrapper's name inside its private staging folder.
const WRAPPER: &CStr = c"w";

/// The process's umask, read once. Published outputs follow it like `cp`
/// and file managers do (D-23). It is read from `/proc/self/status`
/// (`Umask:`, Linux 4.7+), which changes nothing. Only without that line
/// is it read by `umask(0)` and an immediate restore: that briefly zeroes
/// the mask for the whole process, which is safe here because the engine
/// publishes on the supervisor's thread, and `inacircle` starts no other
/// thread that creates files (the cancel token's contract keeps it
/// single-threaded).
fn process_umask() -> libc::mode_t {
    static UMASK: std::sync::OnceLock<libc::mode_t> = std::sync::OnceLock::new();
    *UMASK.get_or_init(|| {
        let from_proc = fs::read_to_string("/proc/self/status").ok().and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Umask:"))
                .and_then(|v| libc::mode_t::from_str_radix(v.trim(), 8).ok())
        });
        from_proc.unwrap_or_else(|| {
            // SAFETY: umask has no preconditions; the old mask is restored
            // at once (see above for why the window is safe).
            let old = unsafe { libc::umask(0) };
            // SAFETY: as above.
            unsafe { libc::umask(old) };
            old
        }) & 0o777
    })
}

/// A published file's final mode: `cp`'s `0666 & !umask`, but the owner
/// keeps read and write whatever the umask, because the baseline reads the
/// file back through the same user (a mask like `0o200` or `0o400` would
/// otherwise make our own check fail and report a false `BaselineChanged`).
fn published_file_mode() -> libc::mode_t {
    0o600 | (0o666 & !process_umask())
}

/// A published wrapper's final mode: `0777 & !umask`, but the owner keeps
/// `rwx` whatever the umask. Without owner `w` the kernel refuses to move
/// the directory out of its staging parent; without owner `x` our own
/// baseline cannot look inside it. `S_ISGID` of the created directory is
/// carried over (`mkdirat` inherits it from a setgid target, a plain
/// `chmod` mode would drop it, and the children would then get the user's
/// primary group instead of the folder's).
fn published_dir_mode(created: libc::mode_t) -> libc::mode_t {
    0o700 | (0o777 & !process_umask()) | (created & libc::S_ISGID)
}

/// Points of a publication where the tests inject a fault or a race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    /// After an output's parent is resolved, before the output is opened.
    Open,
    /// Before each read of an output during its copy.
    Copy,
    /// Before each write of the private copy.
    Write,
    SyncChild,
    SyncWrapper,
    SyncStaging,
    /// After `before_commit`, right before the publishing rename.
    BeforeRename,
    /// The publishing rename itself (an injected errno replaces it).
    Rename,
    /// The staging folder's sync after the commit.
    SyncStagingAfter,
    /// The target folder's sync after the commit.
    SyncParentAfter,
}

/// A test's fault at `p` (an errno), or its race action; nothing outside
/// tests.
#[cfg(test)]
fn at(p: Point) -> io::Result<()> {
    match tests::FAULT.with_borrow_mut(|f| f.as_mut().and_then(|f| f(p))) {
        Some(e) => Err(io::Error::from_raw_os_error(e)),
        None => Ok(()),
    }
}

#[cfg(not(test))]
#[inline(always)]
fn at(_: Point) -> io::Result<()> {
    Ok(())
}

/// `sys::open_at` with the descriptor under this module's guard.
fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<Fd> {
    sys::open_at(dir, name, flags, mode).map(|fd| Fd(fd.into_raw_fd()))
}

fn stat_at(at: At) -> io::Result<libc::stat> {
    sys::stat_at(at.dir, at.name)
}

fn sync(fd: RawFd, p: Point) -> io::Result<()> {
    at(p)?;
    // SAFETY: fd is an open fd.
    if unsafe { libc::fsync(fd) } == 0 {
        Ok(())
    } else {
        last_err()
    }
}

/// `unlinkat(dir, name, flags)` only while `name` is still `id`: what we
/// made, never a look-alike put in its place. Errors are ignored.
///
/// Limit (D-28): the identity is `(dev, ino)`, checked by name. Where the
/// object's own fd is held until this call (the staging folder, the private
/// wrapper, the work dir, which `WorkDir::drop` also reopens and checks),
/// its inode cannot be freed and its number reused, so only a same-user
/// swap between the check and the unlink remains. A staging child's fd is
/// closed after its copy (holding one per output would cost up to 1024 fds
/// per group): if a same-user process removed it from the private 0700
/// staging folder and a new file got the freed inode number under the same
/// name, that file would match and be removed. This is never published
/// content: staging is unlinked only when the group was not published.
fn unlink_own(dir: RawFd, name: &CStr, id: (u64, u64), flags: libc::c_int) {
    let at = At { dir, name };
    if stat_at(at).is_ok_and(|st| ident(&st) == id) {
        // SAFETY: dir is an open dir fd, name is NUL-terminated.
        unsafe { libc::unlinkat(dir, name.as_ptr(), flags) };
    }
}

/// A made directory: `mkdirat(dir, name, 0700)`, the mode set regardless of
/// the umask, opened for reading (the syncs need it) without following a
/// link, and checked to be ours.
fn make_private_dir(dir: RawFd, name: &CStr) -> io::Result<(Fd, (u64, u64))> {
    // SAFETY: name is NUL-terminated; dir is an open dir fd.
    if unsafe { libc::mkdirat(dir, name.as_ptr(), 0o700) } != 0 {
        return last_err();
    }
    let made_st = stat_at(At { dir, name });
    let made = made_st.as_ref().map(ident).map_err(|_| ());
    let opened = (|| {
        // Setgid is inherited from a setgid target and kept (the group of
        // everything made below follows it); the permission bits are
        // private regardless of the umask.
        let mode = 0o700 | (made_st.as_ref().map_or(0, |st| st.st_mode) & libc::S_ISGID);
        // SAFETY: as above; the name is the directory just made.
        if unsafe { libc::fchmodat(dir, name.as_ptr(), mode, 0) } != 0 {
            return last_err();
        }
        let fd = open_at(dir, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let st = fstat(fd.0)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if made.as_ref().ok() != Some(&ident(&st)) || st.st_uid != uid {
            return Err(io::Error::other("private folder changed while being made"));
        }
        Ok((fd, ident(&st)))
    })();
    if opened.is_err()
        && let Ok(id) = made
    {
        unlink_own(dir, name, id, libc::AT_REMOVEDIR);
    }
    opened
}

/// The private staging of one publication: `<target>/.inacircle-<16 hex>/w`,
/// both 0700 and ours, on the target's filesystem. Until the commit,
/// dropping it removes exactly what it made, through its own fds and only
/// while each name is still the object it made; a crash leaves a name the
/// stale cleanup knows. Nothing else is ever removed.
struct Staging<'a> {
    parent: &'a Fd,
    name: CString,
    dir: Fd,
    id: (u64, u64),
    wrapper: Option<(Fd, (u64, u64))>,
    children: Vec<(CString, (u64, u64))>,
    published: bool,
}

impl<'a> Staging<'a> {
    fn new(parent: &'a Fd) -> io::Result<Staging<'a>> {
        for _ in 0..16 {
            let name = CString::new(format!("{WORK_PREFIX}{:016x}", random_u64()?))
                .expect("no NUL in a hex name");
            match make_private_dir(parent.0, &name) {
                Ok((dir, id)) => {
                    let mut s = Staging {
                        parent,
                        name,
                        dir,
                        id,
                        wrapper: None,
                        children: Vec::new(),
                        published: false,
                    };
                    s.wrapper = Some(make_private_dir(s.dir.0, WRAPPER)?);
                    return Ok(s);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free staging name",
        ))
    }

    fn wrapper(&self) -> &(Fd, (u64, u64)) {
        self.wrapper.as_ref().expect("made in new")
    }

    /// Removes the (empty) staging folder after the commit.
    fn remove_empty(&mut self) {
        unlink_own(self.parent.0, &self.name, self.id, libc::AT_REMOVEDIR);
    }
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if !self.published
            && let Some((w, wid)) = &self.wrapper
        {
            for (name, id) in &self.children {
                unlink_own(w.0, name, *id, 0);
            }
            unlink_own(self.dir.0, WRAPPER, *wid, libc::AT_REMOVEDIR);
        }
        self.remove_empty();
    }
}

/// A streaming UTF-8 check: at most 3 bytes of a character cut by a chunk
/// boundary wait for the next chunk.
#[derive(Default)]
struct Utf8 {
    pending: Vec<u8>,
}

impl Utf8 {
    fn feed(&mut self, mut data: &[u8]) -> io::Result<()> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "text output is not UTF-8");
        if let Some(&lead) = self.pending.first() {
            let width = match lead {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            let take = (width - self.pending.len()).min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() < width {
                return Ok(());
            }
            std::str::from_utf8(&self.pending).map_err(|_| bad())?;
            self.pending.clear();
        }
        match std::str::from_utf8(data) {
            Ok(_) => Ok(()),
            Err(e) if e.error_len().is_none() => {
                self.pending.extend_from_slice(&data[e.valid_up_to()..]);
                Ok(())
            }
            Err(_) => Err(bad()),
        }
    }

    fn finish(&self) -> io::Result<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "text output is not UTF-8",
            ))
        }
    }
}

fn is_text(ext: &[u8]) -> bool {
    ext.eq_ignore_ascii_case(b"txt") || ext.eq_ignore_ascii_case(b"md")
}

fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "output changed while being copied",
    )
}

/// Copies the open regular file `src` into the fresh `dst`, exactly the
/// size it had when it was opened (a change during the copy refuses it),
/// checking text as UTF-8. Returns the bytes copied.
fn copy_checked(src: RawFd, dst: RawFd, text: bool, buf: &mut [u8]) -> io::Result<u64> {
    let before = fstat(src)?;
    let size = before.st_size as u64;
    if text && size > TEXT_OUTPUT_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "text output larger than 32 MiB",
        ));
    }
    let mut utf8 = Utf8::default();
    let mut copied: u64 = 0;
    loop {
        at(Point::Copy)?;
        // One byte past the size, to see a file that grew.
        let want = (size - copied + 1).min(buf.len() as u64) as usize;
        // SAFETY: buf is valid for `want` <= buf.len() bytes; src is open.
        let n = unsafe { libc::read(src, buf.as_mut_ptr().cast(), want) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            break;
        }
        let chunk = &buf[..n as usize];
        copied += chunk.len() as u64;
        if copied > size {
            return Err(changed());
        }
        if text {
            utf8.feed(chunk)?;
        }
        let mut rest = chunk;
        while !rest.is_empty() {
            at(Point::Write)?;
            // SAFETY: rest is valid for its length; dst is open for writing.
            let w = unsafe { libc::write(dst, rest.as_ptr().cast(), rest.len()) };
            if w < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if w == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            rest = &rest[w as usize..];
        }
    }
    if text {
        utf8.finish()?;
    }
    let after = fstat(src)?;
    if copied != size || FileId::from_stat(&after) != FileId::from_stat(&before) {
        return Err(changed());
    }
    Ok(copied)
}

/// Copies the producer's output `rel` beneath `root` into a fresh file of
/// the private wrapper (numbered on a clash among this input's outputs),
/// synced, and takes its baseline from the private copy.
fn copy_one(
    staging: &mut Staging,
    root: &Path,
    rel: &Path,
    name: (&[u8], &[u8]),
    buf: &mut [u8],
) -> io::Result<ChildBaseline> {
    let (parent, leaf) = resolve_beneath(root, rel)?;
    at(Point::Open)?;
    // O_NONBLOCK: a FIFO swapped in opens at once (and is refused below).
    let src = open_at(parent.0, &leaf, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    if fstat(src.0)?.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(invalid("output is not a regular file"));
    }
    let (stem, ext) = name;
    check_part(stem)?;
    check_part(ext)?;
    let w = staging.wrapper().0.0;
    let mut made = None;
    for n in 0..=MAX_CONFLICT {
        let child = candidate(stem, ext, n)?;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
        match open_at(w, &child, flags, 0o600) {
            Ok(fd) => {
                made = Some((child, fd));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let (child, dst) =
        made.ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free output name"))?;
    staging
        .children
        .push((child.clone(), ident(&fstat(dst.0)?)));
    // Private (0600) while written, independent of the umask.
    // SAFETY: dst is an open fd.
    if unsafe { libc::fchmod(dst.0, 0o600) } != 0 {
        return last_err();
    }
    let copied = copy_checked(src.0, dst.0, is_text(ext), buf)?;
    // The final mode, as `cp` would make it, before the sync (so it is
    // durable) and before the baseline (so it records it). Still private:
    // the 0700 staging folder above keeps everyone else out.
    // SAFETY: dst is an open fd.
    if unsafe { libc::fchmod(dst.0, published_file_mode()) } != 0 {
        return last_err();
    }
    sync(dst.0, Point::SyncChild)?;
    let st = fstat(dst.0)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    // st_nlink's width differs between targets.
    #[allow(clippy::unnecessary_cast)]
    let nlink = st.st_nlink as u64;
    if kind_of(&st) != Some(NodeKind::File)
        || st.st_uid != uid
        || nlink != 1
        || st.st_size as u64 != copied
    {
        return Err(io::Error::other("private copy changed while being made"));
    }
    Ok(ChildBaseline {
        rel: PathBuf::from(OsStr::from_bytes(child.as_bytes())),
        file_id: FileId::from_stat(&st),
        kind: NodeKind::File,
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
        nlink,
    })
}

fn rename_noreplace(from: At, to: At) -> io::Result<()> {
    at(Point::Rename)?;
    // SAFETY: both names are NUL-terminated and outlive the call; both dirs
    // are open directory fds.
    let r = unsafe {
        libc::renameat2(
            from.dir,
            from.name.as_ptr(),
            to.dir,
            to.name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 { Ok(()) } else { last_err() }
}

/// The path that names the published wrapper right now, if it can be
/// proved: `target` still reaches the folder the commit went into, and
/// `leaf` in it is the wrapper (never followed if it is a link).
fn locate(target: &Path, parent: &Fd, leaf: &CStr, object: (u64, u64)) -> Option<PathBuf> {
    let now = open_path(libc::AT_FDCWD, &cstr(target).ok()?, 0).ok()?;
    if !same_inode(now.0, parent.0) {
        return None;
    }
    let st = stat_at(At {
        dir: now.0,
        name: leaf,
    })
    .ok()?;
    (kind_of(&st) == Some(NodeKind::Directory) && ident(&st) == object)
        .then(|| target.join(OsStr::from_bytes(leaf.to_bytes())))
}

/// Publishes one input's outputs, `rels` beneath `root` (the producer's
/// staging; its whole process group is dead), as one new folder in
/// `target`: `<stem>/` (numbered on a clash, `<stem> (1)/` ...), holding
/// `<stem>.<ext>` for a single output, else the outputs under their own
/// names (numbered on a clash).
///
/// The outputs are copied into fresh files of a private 0700 staging
/// folder on the target's filesystem (a producer's old fd cannot reach
/// them), validated on the stream that is written (the source is regular
/// and unchanged during the copy, the bytes written to the private file are
/// UTF-8 for TXT/MD, up to 32 MiB; the finished copy is checked only for
/// type, owner, link count and size, not read again, which is equivalent
/// for a dead producer, a fresh inode and a 0700 staging folder), given their final umask modes (still private: the
/// staging folder is 0700), synced, and their baseline taken; then the
/// wrapper (its final mode likewise), the
/// staging folder, `before_commit(group, private path)`, and one
/// `renameat2(RENAME_NOREPLACE)` of the wrapper: the commit. Until it,
/// any error removes only this publication's own staging and nothing is
/// visible; a filesystem that cannot rename without replacing is refused
/// (`Unsupported`), never worked around. After it nothing is removed: a
/// failed sync is `Durability::Unconfirmed`, and a path that no longer
/// reaches the wrapper is `path_at_check: None`.
///
/// On a name taken between the check and the rename, the next name is
/// tried and `before_commit` runs again for it (same group and operation).
pub fn publish_beneath(
    root: &Path,
    rels: &[PathBuf],
    target: &Path,
    stem: &str,
    ext: &str,
    before_commit: &mut dyn FnMut(&OutputGroup, &Path) -> io::Result<()>,
) -> io::Result<Publication> {
    if rels.is_empty() {
        return Err(invalid("no outputs"));
    }
    if stem.is_empty() {
        return Err(invalid("empty output stem"));
    }
    check_part(stem.as_bytes())?;
    check_part(ext.as_bytes())?;
    let group = GroupId(random_u64()?);
    let op = OperationId(random_u64()?);
    let parent = open_path(libc::AT_FDCWD, &cstr(target)?, 0)?;
    // Taken now: after the commit nothing may fail any more.
    let pst = fstat(parent.0)?;
    let mut staging = Staging::new(&parent)?;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut children = Vec::with_capacity(rels.len());
    for rel in rels {
        let leaf = rel
            .file_name()
            .ok_or_else(|| invalid("output has no name"))?;
        let name = if rels.len() == 1 {
            (stem.as_bytes(), ext.as_bytes())
        } else {
            split_name(leaf.as_bytes())
        };
        children.push(copy_one(&mut staging, root, rel, name, &mut buf)?);
    }
    drop(buf);
    let (wrapper, wid) = staging.wrapper();
    let (wrapper, wid) = (wrapper.0, *wid);
    // The wrapper's final mode, likewise before its sync and baseline; the
    // staging folder (0700) stays private until it is removed.
    let created = fstat(wrapper)?.st_mode;
    // SAFETY: wrapper is an open fd.
    if unsafe { libc::fchmod(wrapper, published_dir_mode(created)) } != 0 {
        return last_err();
    }
    sync(wrapper, Point::SyncWrapper)?;
    let st = fstat(wrapper)?;
    let id = DirectoryId {
        dev: ident(&st).0,
        ino: ident(&st).1,
        kind: NodeKind::Directory,
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
    };
    sync(staging.dir.0, Point::SyncStaging)?;
    let private = target
        .join(OsStr::from_bytes(staging.name.as_bytes()))
        .join(OsStr::from_bytes(WRAPPER.to_bytes()));
    let mut out = OutputGroup {
        group,
        op,
        directory: PathBuf::new(),
        id,
        children,
    };
    let mut committed = None;
    for n in 0..=MAX_CONFLICT {
        let leaf = candidate(stem.as_bytes(), b"", n)?;
        // Never a name the stale cleanup would take for its own.
        if is_our_name(leaf.as_bytes()) {
            continue;
        }
        match stat_at(At {
            dir: parent.0,
            name: &leaf,
        }) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        out.directory = target.join(OsStr::from_bytes(leaf.as_bytes()));
        before_commit(&out, &private)?;
        at(Point::BeforeRename)?;
        if !stat_at(At {
            dir: staging.dir.0,
            name: WRAPPER,
        })
        .is_ok_and(|st| ident(&st) == wid)
        {
            return Err(io::Error::other("private staging changed"));
        }
        let from = At {
            dir: staging.dir.0,
            name: WRAPPER,
        };
        let to = At {
            dir: parent.0,
            name: &leaf,
        };
        match rename_noreplace(from, to) {
            Ok(()) => {
                committed = Some(leaf);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("the folder's filesystem cannot publish without replacing ({e})"),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    let leaf = committed
        .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free output name"))?;
    // Committed: visible from here on, and never removed again.
    staging.published = true;
    let mut unsynced = Vec::new();
    if let Err(e) = sync(staging.dir.0, Point::SyncStagingAfter) {
        unsynced.push(format!("staging folder sync failed: {e}"));
    }
    staging.remove_empty();
    // The target fd is O_PATH (the folder may not be readable); its own
    // inode is reopened for reading through /proc to be synced.
    let magic = CString::new(format!("/proc/self/fd/{}", parent.0)).expect("no NUL in a number");
    // mutants: equivalent — distinct flag bits, so `|` and `^` agree.
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: magic is NUL-terminated; it names our own open fd (a magic
    // link, so it is followed: O_NOFOLLOW would refuse it).
    let fd = unsafe { libc::open(magic.as_ptr(), flags) };
    let reopened = if fd < 0 { last_err() } else { Ok(Fd(fd)) };
    match reopened {
        Ok(fd) if same_inode(fd.0, parent.0) => {
            if let Err(e) = sync(fd.0, Point::SyncParentAfter) {
                unsynced.push(format!("folder sync failed: {e}"));
            }
        }
        Ok(_) => unsynced.push("folder changed while being synced".to_owned()),
        Err(e) => unsynced.push(format!("cannot open the folder to sync it: {e}")),
    }
    let durability = if unsynced.is_empty() {
        Durability::Synced
    } else {
        Durability::Unconfirmed(unsynced.join("; "))
    };
    let anchor = AnchorHint {
        parent_dev: ident(&pst).0,
        parent_ino: ident(&pst).1,
        object_dev: id.dev,
        object_ino: id.ino,
        leaf: OsStr::from_bytes(leaf.as_bytes()).to_owned(),
        op,
    };
    Ok(Publication {
        location: LocationEvidence {
            path_at_check: locate(target, &parent, &leaf, (id.dev, id.ino)),
            last_unverified_path: None,
            anchor,
            checked_at: CheckPoint::PostPublish,
        },
        group: out,
        durability,
    })
}

/// The check before any `Done`: the publication's proved path still
/// reaches its wrapper (the same folder, the same wrapper inode), and the
/// wrapper holds exactly its children, each still matching its private
/// baseline. Returns each child's path and baseline identity.
pub fn postcheck(p: &Publication) -> Result<Vec<(PathBuf, FileId)>, PublicationReason> {
    use PublicationReason::{AncestorChanged, BaselineChanged};
    let path = p.location.path_at_check.as_ref().ok_or(AncestorChanged)?;
    let (Some(dir), Some(leaf)) = (path.parent(), path.file_name()) else {
        return Err(AncestorChanged);
    };
    let a = &p.location.anchor;
    let parent = cstr(dir)
        .and_then(|c| open_path(libc::AT_FDCWD, &c, 0))
        .map_err(|_| AncestorChanged)?;
    let leaf = cstr(Path::new(leaf)).map_err(|_| AncestorChanged)?;
    let st = stat_at(At {
        dir: parent.0,
        name: &leaf,
    })
    .map_err(|_| AncestorChanged)?;
    let pst = fstat(parent.0).map_err(|_| AncestorChanged)?;
    if ident(&pst) != (a.parent_dev, a.parent_ino) || ident(&st) != (a.object_dev, a.object_ino) {
        return Err(AncestorChanged);
    }
    let g = &p.group;
    let w = open_at(parent.0, &leaf, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|_| BaselineChanged)?;
    let wst = fstat(w.0).map_err(|_| BaselineChanged)?;
    let now = DirectoryId {
        dev: ident(&wst).0,
        ino: ident(&wst).1,
        kind: kind_of(&wst).ok_or(BaselineChanged)?,
        uid: wst.st_uid,
        mode: wst.st_mode & 0o7777,
    };
    if now != g.id || list_dir(&w).len() != g.children.len() {
        return Err(BaselineChanged);
    }
    let mut files = Vec::with_capacity(g.children.len());
    for c in &g.children {
        let name = cstr(&c.rel).map_err(|_| BaselineChanged)?;
        let st = stat_at(At {
            dir: w.0,
            name: &name,
        })
        .map_err(|_| BaselineChanged)?;
        // st_nlink's width differs between targets.
        #[allow(clippy::unnecessary_cast)]
        let same = kind_of(&st) == Some(c.kind)
            && st.st_uid == c.uid
            && st.st_mode & 0o7777 == c.mode
            && st.st_nlink as u64 == c.nlink
            && FileId::from_stat(&st) == c.file_id;
        if !same {
            return Err(BaselineChanged);
        }
        files.push((path.join(&c.rel), c.file_id));
    }
    Ok(files)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::xdg::tests::with_env;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    type Hook = Option<Box<dyn FnMut()>>;

    type Fault = Option<Box<dyn FnMut(Point) -> Option<i32>>>;

    thread_local! {
        /// Runs in `open_dir_for_removal` right after the `O_PATH` open:
        /// the window in which a swap of the name must not matter.
        pub(super) static AFTER_OPATH: std::cell::RefCell<Hook> =
            const { std::cell::RefCell::new(None) };
        /// Called at each publication [`Point`]: an errno to inject there,
        /// or `None` (after any race the test stages).
        pub(super) static FAULT: std::cell::RefCell<Fault> =
            const { std::cell::RefCell::new(None) };
    }

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "inacircle-out-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            remove_tree(&self.0);
        }
    }

    fn file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn readonly_dir_falls_back_to_downloads() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let t = Tmp::new("ro");
        let ro = t.0.join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let home = t.0.join("home");
        with_env(
            &[
                ("HOME", Some(home.to_str().unwrap())),
                ("XDG_CONFIG_HOME", None),
            ],
            || {
                assert_eq!(writable_target(&ro).unwrap(), home.join("Downloads"));
                assert_eq!(writable_target(&t.0).unwrap(), t.0);
            },
        );
        assert_eq!(fs::read_dir(&t.0).unwrap().count(), 1, "probe left behind");
        assert_eq!(fs::read_dir(&ro).unwrap().count(), 0);
    }

    #[test]
    fn work_dir_hidden_0700_and_removed_on_drop() {
        let t = Tmp::new("work");
        let w = make_work_dir(&t.0).unwrap();
        let path = w.path.clone();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let hex = name.strip_prefix(".inacircle-").unwrap();
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        let w2 = make_work_dir(&t.0).unwrap();
        assert_ne!(w2.path, path);
        // Read-only file inside a read-only subdirectory must not block removal.
        let sub = path.join("sub");
        fs::create_dir(&sub).unwrap();
        let f = file(&sub, "ro.txt", "x");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o400)).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
        drop(w);
        assert!(!path.exists());
        assert!(w2.path.exists());
    }

    /// Runs one test of this binary in a child with RLIMIT_NOFILE `n`,
    /// with `INACIRCLE_HELPER` set to `dir`.
    fn run_helper_with_nofile(name: &str, dir: &Path, n: u64) {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("INACIRCLE_HELPER", dir)
            .stdin(std::process::Stdio::null());
        // SAFETY: setrlimit is async-signal-safe; `lim` is on the stack.
        unsafe {
            cmd.pre_exec(move || {
                let lim = libc::rlimit {
                    rlim_cur: n,
                    rlim_max: n,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "helper failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn drop_removes_unreadable_and_unlistable_subdirs() {
        let t = Tmp::new("work-modes");
        let w = make_work_dir(&t.0).unwrap();
        let path = w.path.clone();
        for (name, mode) in [("none", 0o000), ("wx", 0o300), ("x", 0o100)] {
            let sub = path.join(name);
            fs::create_dir(&sub).unwrap();
            let deeper = sub.join("deeper");
            fs::create_dir(&deeper).unwrap();
            file(&deeper, "f", "x");
            file(&sub, "g", "y");
            fs::set_permissions(&deeper, fs::Permissions::from_mode(mode)).unwrap();
            fs::set_permissions(&sub, fs::Permissions::from_mode(mode)).unwrap();
        }
        drop(w);
        assert!(!path.exists(), "work dir left behind");
    }

    /// Runs one test of this binary in a child under the seccomp filter
    /// (which denies chmod) and an open ruleset, with `INACIRCLE_HELPER` set.
    fn run_helper_under_seccomp(name: &str, dir: &Path) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let limits = crate::sandbox::limits_for(0);
        let open = crate::sandbox::tests::open_ruleset();
        let fd = open.as_raw_fd();
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("INACIRCLE_HELPER", dir)
            .stdin(std::process::Stdio::null());
        // SAFETY: apply_in_child only makes async-signal-safe calls; `open`
        // outlives the spawn.
        unsafe {
            cmd.pre_exec(move || crate::sandbox::apply_in_child(fd, &limits));
        }
        let out = cmd.output().unwrap();
        drop(open);
        assert!(
            out.status.success(),
            "helper failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn work_dir_removed_when_chmod_fails() {
        if !crate::sandbox::tests::seccomp_or_skip("work_dir_removed_when_chmod_fails") {
            return;
        }
        let t = Tmp::new("chmod-fail");
        run_helper_under_seccomp("output::tests::helper_make_work_dir_chmod_denied", &t.0);
        let left: Vec<_> = fs::read_dir(&t.0).unwrap().flatten().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    /// Child side of `work_dir_removed_when_chmod_fails`; a no-op otherwise.
    #[test]
    fn helper_make_work_dir_chmod_denied() {
        let Some(dir) = std::env::var_os("INACIRCLE_HELPER") else {
            return;
        };
        assert!(make_work_dir(Path::new(&dir)).is_err(), "chmod not denied");
    }

    #[test]
    fn stale_cleanup_only_ours_and_old() {
        let t = Tmp::new("stale-clean");
        let old = t.0.join(".inacircle-00000000000000a1");
        let old_probe = t.0.join(".inacircle-probe-00000000000000b2");
        let fresh = t.0.join(".inacircle-0000000000000002");
        let other = t.0.join(".other-dir");
        let mine = t.0.join(".inacircle-mine");
        let upper = t.0.join(".inacircle-00000000000000A1");
        let short = t.0.join(".inacircle-abc");
        let plain_file = t.0.join(".inacircle-0000000000000003");
        for d in [&old, &old_probe, &fresh, &other, &mine, &upper, &short] {
            fs::create_dir(d).unwrap();
        }
        fs::write(&plain_file, "x").unwrap();
        fs::write(old.join("junk"), "x").unwrap();
        fs::write(mine.join("precious"), "x").unwrap();
        for p in [&old, &old_probe, &other, &plain_file, &mine, &upper, &short] {
            set_age(p, 2 * 24 * 3600);
        }
        clean_stale(&t.0);
        assert!(!old.exists());
        assert!(!old_probe.exists());
        assert!(fresh.exists());
        assert!(other.exists());
        assert!(plain_file.exists());
        assert!(mine.join("precious").exists(), "user folder survives");
        assert!(upper.exists() && short.exists());
    }

    #[test]
    fn stale_cleanup_ignores_symlink() {
        let t = Tmp::new("stale-link");
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        file(&outside, "keep", "k");
        let link = t.0.join(".inacircle-00000000000000c3");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        set_age(&outside, 2 * 24 * 3600);
        set_age(&link, 2 * 24 * 3600);
        clean_stale(&t.0);
        assert!(fs::symlink_metadata(&link).is_ok(), "link left alone");
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn drop_does_not_follow_symlinks() {
        let t = Tmp::new("drop-link");
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        file(&outside, "keep", "k");
        let w = make_work_dir(&t.0).unwrap();
        std::os::unix::fs::symlink(&outside, w.path.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(outside.join("keep"), w.path.join("filelink")).unwrap();
        let path = w.path.clone();
        drop(w);
        assert!(!path.exists());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn drop_leaves_a_swapped_in_symlink_alone() {
        let t = Tmp::new("drop-swap");
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        file(&outside, "keep", "k");
        let w = make_work_dir(&t.0).unwrap();
        fs::remove_dir(&w.path).unwrap();
        std::os::unix::fs::symlink(&outside, &w.path).unwrap();
        let link = w.path.clone();
        drop(w);
        assert!(outside.join("keep").exists());
        fs::remove_file(link).unwrap();
    }

    fn set_age(p: &Path, secs: i64) {
        let c = cstr(p).unwrap();
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: now is a valid out-pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
        let t = libc::timespec {
            tv_sec: now.tv_sec - secs,
            tv_nsec: 0,
        };
        let times = [t, t];
        // SAFETY: c is a valid path, times points to two timespecs.
        let r = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(r, 0);
    }

    // ---- Mutation-hardening tests: placement, removal, descriptor use ----

    /// True (with a note) when the tests run as root, which ignores the
    /// permission bits the test relies on.
    fn root_or_skip(test: &str) -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        if root {
            eprintln!("SKIPPED {test}: running as root ignores permission bits");
        }
        root
    }

    /// Makes the calling thread's `nrs` syscalls fail with `errno` (a
    /// seccomp filter; `errno` 0 makes them "succeed" with 0). Child-side
    /// only: the filter cannot be removed again. Native architecture, so
    /// no arch check.
    pub(crate) fn deny_syscalls(nrs: &[libc::c_long], errno: u32) {
        use libc::{
            BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
            SECCOMP_RET_ERRNO, sock_filter, sock_fprog,
        };
        // SECCOMP_SET_MODE_FILTER (linux/seccomp.h).
        const SET_MODE_FILTER: libc::c_long = 1;
        let ins = |code: u32, jt: u8, k: u32| sock_filter {
            code: code as u16,
            jt,
            jf: 0,
            k,
        };
        let n = nrs.len();
        // Load the syscall number (offset 0 of seccomp_data); one equality
        // test per number jumping to the errno return, which sits `n - i`
        // instructions past the test's successor; else allow.
        let mut prog = vec![ins(BPF_LD | BPF_W | BPF_ABS, 0, 0)];
        for (i, nr) in nrs.iter().enumerate() {
            prog.push(ins(BPF_JMP | BPF_JEQ | BPF_K, (n - i) as u8, *nr as u32));
        }
        prog.push(ins(BPF_RET | BPF_K, 0, SECCOMP_RET_ALLOW));
        prog.push(ins(BPF_RET | BPF_K, 0, SECCOMP_RET_ERRNO | errno));
        let fprog = sock_fprog {
            len: prog.len() as u16,
            filter: prog.as_mut_ptr(),
        };
        // SAFETY: prctl and seccomp take plain integers and a pointer to
        // `fprog`, which points at `prog`; both outlive the calls.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(
                libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, 0, &fprog),
                0
            );
        }
    }

    fn mkdir_syscalls() -> Vec<libc::c_long> {
        // x86_64 also has the legacy mkdir.
        vec![
            libc::SYS_mkdirat,
            #[cfg(target_arch = "x86_64")]
            libc::SYS_mkdir,
        ]
    }

    fn open_fds() -> usize {
        fs::read_dir("/proc/self/fd").unwrap().count()
    }

    fn cloexec(fd: RawFd) -> bool {
        // SAFETY: F_GETFD on an fd number reads a flag and nothing else.
        let f = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        f >= 0 && f & libc::FD_CLOEXEC != 0
    }

    /// Runs the child side `case` of `helper_case` in a fresh process of
    /// this binary (single-threaded, its own fd table and filters).
    fn run_case(case: &str, dir: &Path) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "output::tests::helper_case",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("INACIRCLE_HELPER", dir)
            .env("INACIRCLE_CASE", case)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn assert_case_ok(case: &str, dir: &Path) {
        let out = run_case(case, dir);
        assert!(
            out.status.success(),
            "case {case} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Child side of the `run_case` tests; a no-op without the env.
    #[test]
    fn helper_case() {
        let (Some(dir), Some(case)) = (
            std::env::var_os("INACIRCLE_HELPER"),
            std::env::var_os("INACIRCLE_CASE"),
        ) else {
            return;
        };
        let dir = PathBuf::from(dir);
        // A 2 s watchdog (these cases take milliseconds): a loop that
        // should end but does not dies by SIGALRM instead of hanging the
        // suite.
        // SAFETY: alarm has no preconditions.
        unsafe { libc::alarm(2) };
        match case.to_str().unwrap() {
            "fd0_remove_tree" => {
                // A daemon without stdin: the first descriptors we open are
                // number 0, which is a valid fd, not a failure.
                // SAFETY: closes the unused stdin of this test process.
                unsafe { libc::close(0) };
                let tree = dir.join("tree");
                fs::create_dir_all(tree.join("ro/inner")).unwrap();
                fs::create_dir(tree.join("locked")).unwrap();
                fs::write(tree.join("ro/f"), "x").unwrap();
                fs::write(tree.join("locked/g"), "x").unwrap();
                fs::set_permissions(tree.join("ro"), fs::Permissions::from_mode(0o500)).unwrap();
                fs::set_permissions(tree.join("locked"), fs::Permissions::from_mode(0o000))
                    .unwrap();
                remove_tree(&tree);
                assert!(!tree.exists(), "tree left behind with fd 0 closed");
            }
            "fd0_publish" => {
                // SAFETY: as above.
                unsafe { libc::close(0) };
                let (root, target) = (dir.join("root"), dir.join("target"));
                fs::create_dir(&target).unwrap();
                produced(&root, "sub/leaf", b"x");
                let p = publish(&root, &["sub/leaf"], &target, "out", "md")
                    .expect("publish with fd 0 closed");
                assert_eq!(fs::read(target.join("out/out.md")).unwrap(), b"x");
                assert_eq!(p.durability, Durability::Synced);
            }
            "open_dir_flags_and_no_leak" => {
                let d = dir.join("d");
                fs::create_dir(&d).unwrap();
                let c = cstr(&d).unwrap();
                let before = open_fds();
                let fd = open_dir_for_removal(None, &c).unwrap();
                assert!(cloexec(fd.0), "fd leaks into children");
                drop(fd);
                assert_eq!(open_fds(), before, "descriptor leaked");
            }
            "emfile_on_root" | "emfile_on_leaf" => {
                let root = dir.join("root");
                fs::create_dir(&root).unwrap();
                fs::write(root.join("leaf"), "x").unwrap();
                // SAFETY: dup returns the lowest free fd, which we close.
                let lowest = unsafe { libc::dup(0) };
                assert!(lowest >= 0);
                // SAFETY: closes the fd just made.
                unsafe { libc::close(lowest) };
                // Descriptor numbers below the limit are usable: `lowest`
                // free fds exist for `limit = lowest + k`, k = 0 or 1.
                let k = u64::from(case == "emfile_on_leaf");
                let lim = libc::rlimit {
                    rlim_cur: lowest as u64 + k,
                    rlim_max: lowest as u64 + k,
                };
                // SAFETY: lim is a valid rlimit.
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
                let e = open_regular_beneath(&root, Path::new("leaf")).unwrap_err();
                assert_eq!(e.raw_os_error(), Some(libc::EMFILE), "{e:?}");
            }
            "swap_magic" | "swap_fallback_dir" | "swap_fallback_fifo" | "swap_fallback_file" => {
                let fallback = case != "swap_magic";
                if fallback {
                    // chmod (the magic-link route) refused, fchmod allowed:
                    // a mode change of the impostor stays visible.
                    let nrs = [
                        libc::SYS_fchmodat,
                        #[cfg(target_arch = "x86_64")]
                        libc::SYS_chmod,
                    ];
                    deny_syscalls(&nrs, libc::EPERM as u32);
                }
                let d = dir.join("d");
                fs::create_dir(&d).unwrap();
                let original = fs::metadata(&d).unwrap().ino();
                let (d2, moved) = (d.clone(), dir.join("moved"));
                let kind = case.to_str().unwrap().to_owned();
                let impostor = std::rc::Rc::new(std::cell::Cell::new(0u32));
                let seen = impostor.clone();
                AFTER_OPATH.set(Some(Box::new(move || {
                    fs::rename(&d2, &moved).unwrap();
                    let c = cstr(&d2).unwrap();
                    match kind.as_str() {
                        "swap_fallback_fifo" => {
                            // SAFETY: c is a valid path.
                            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
                        }
                        "swap_fallback_file" => fs::write(&d2, "x").unwrap(),
                        _ => {
                            fs::create_dir(&d2).unwrap();
                            fs::write(d2.join("keep"), "k").unwrap();
                        }
                    }
                    seen.set(fs::symlink_metadata(&d2).unwrap().mode());
                })));
                let c = cstr(&d).unwrap();
                let r = open_dir_for_removal(None, &c);
                AFTER_OPATH.set(None);
                if fallback {
                    let e = r.err().expect("opened a swapped-in name");
                    assert!(
                        e.kind() == io::ErrorKind::Other || e.raw_os_error() == Some(libc::ENOTDIR),
                        "{e:?}"
                    );
                    // Left alone: still the mode it was made with.
                    assert_eq!(fs::symlink_metadata(&d).unwrap().mode(), impostor.get());
                } else {
                    let fd = r.expect("the magic link names the opened inode");
                    // SAFETY: zeroed stat is valid storage; fd is open.
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    assert_eq!(unsafe { libc::fstat(fd.0, &mut st) }, 0);
                    assert_eq!(st.st_ino as u64, original, "not the original directory");
                    assert_eq!(fs::symlink_metadata(&d).unwrap().mode(), impostor.get());
                }
            }
            "getrandom_eperm" => {
                deny_syscalls(&[libc::SYS_getrandom], libc::EPERM as u32);
                let e = random_u64().unwrap_err();
                assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e:?}");
            }
            "getrandom_short" => {
                deny_syscalls(&[libc::SYS_getrandom], 0);
                let e = random_u64().unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{e:?}");
            }
            "getrandom_eintr" => {
                // An interrupted call is retried, so this never returns:
                // the watchdog ends it.
                deny_syscalls(&[libc::SYS_getrandom], libc::EINTR as u32);
                let r = random_u64();
                panic!("gave up on EINTR: {r:?}");
            }
            "mkdir_always_exists" => {
                deny_syscalls(&mkdir_syscalls(), libc::EEXIST as u32);
                let e = make_work_dir(&dir).err().expect("no work dir");
                assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
                assert!(e.to_string().contains("no free work directory name"), "{e}");
            }
            "publish_renameat2_einval" | "publish_renameat2_enosys" => {
                let errno = if case == "publish_renameat2_einval" {
                    libc::EINVAL
                } else {
                    libc::ENOSYS
                };
                deny_syscalls(&[libc::SYS_renameat2], errno as u32);
                let (root, target) = (dir.join("root"), dir.join("target"));
                fs::create_dir(&target).unwrap();
                produced(&root, "0/a.md", b"mine");
                file(&target, "a.md", "theirs");
                let e = publish(&root, &["0/a.md"], &target, "a", "md").unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e:?}");
                assert!(e.to_string().contains("without replacing"), "{e}");
                assert_eq!(names(&target), ["a.md"]);
                assert_eq!(fs::read_to_string(target.join("a.md")).unwrap(), "theirs");
                assert!(root.join("0/a.md").exists(), "source left alone");
            }
            "umask_022" | "umask_077" | "umask_222" | "umask_100" => {
                // (mask, file mode, wrapper mode). The owner keeps what
                // our own baseline needs: file rw, wrapper rwx.
                let (mask, file, dir_mode) = match case.to_str().unwrap() {
                    "umask_022" => (0o022, 0o644, 0o755),
                    "umask_077" => (0o077, 0o600, 0o700),
                    "umask_222" => (0o222, 0o644, 0o755),
                    _ => (0o100, 0o666, 0o777),
                };
                let (root, target) = (dir.join("root"), dir.join("target"));
                fs::create_dir(&target).unwrap();
                produced(&root, "0/a.png", b"1");
                produced(&root, "0/b.png", b"2");
                // After the fixture (a mask like 0o222 would make the
                // fixture itself unwritable). SAFETY: umask only sets this
                // process's mask; the child is single-threaded here.
                unsafe { libc::umask(mask) };
                let mut private = None;
                let p = publish_beneath(
                    &root,
                    &rels(&["0/a.png", "0/b.png"]),
                    &target,
                    "doc",
                    "png",
                    &mut |g, path| {
                        // Final modes already, before the commit, while the
                        // staging folder above still keeps others out.
                        let staging = path.parent().unwrap();
                        let sm = fs::symlink_metadata(staging).unwrap().mode() & 0o7777;
                        private = Some((g.clone(), sm));
                        Ok(())
                    },
                )
                .unwrap();
                let (prepared, staging_mode) = private.unwrap();
                assert_eq!(staging_mode, 0o700);
                assert_eq!(prepared, p.group);
                let w = target.join("doc");
                assert_eq!(fs::symlink_metadata(&w).unwrap().mode() & 0o7777, dir_mode);
                assert_eq!(p.group.id.mode, dir_mode);
                for c in &p.group.children {
                    let m = fs::symlink_metadata(w.join(&c.rel)).unwrap().mode() & 0o7777;
                    assert_eq!((m, c.mode), (file, file), "{:?}", c.rel);
                }
                assert_eq!(postcheck(&p).unwrap().len(), 2);
            }
            "private_modes_before_chmod" => {
                // umask 022, the process dies while the first output is
                // written: everything left is private.
                // SAFETY: as above.
                unsafe { libc::umask(0o022) };
                let (root, target) = (dir.join("root"), dir.join("target"));
                fs::create_dir(&target).unwrap();
                produced(&root, "0/a.md", b"a");
                FAULT.set(Some(Box::new(|p| {
                    if p == Point::Write {
                        // SAFETY: ends this helper process at once.
                        unsafe { libc::_exit(7) };
                    }
                    None
                })));
                let _ = publish(&root, &["0/a.md"], &target, "a", "md");
                panic!("did not reach the write");
            }
            "crash_before_commit" | "crash_after_commit" => {
                // The process dies at the point: a crash, not an error.
                let point = if case == "crash_before_commit" {
                    Point::BeforeRename
                } else {
                    Point::SyncStagingAfter
                };
                let (root, target) = (dir.join("root"), dir.join("target"));
                fs::create_dir(&target).unwrap();
                produced(&root, "0/a.md", b"a");
                FAULT.set(Some(Box::new(move |p| {
                    if p == point {
                        // SAFETY: ends this helper process at once.
                        unsafe { libc::_exit(7) };
                    }
                    None
                })));
                let _ = publish(&root, &["0/a.md"], &target, "a", "md");
                panic!("did not reach {point:?}");
            }
            "publish_renameat2_eperm" => {
                // Any other refusal is the commit's failure, not a reason
                // to call the filesystem unsupported.
                deny_syscalls(&[libc::SYS_renameat2], libc::EPERM as u32);
                let (root, target) = (dir.join("root"), dir.join("target"));
                fs::create_dir(&target).unwrap();
                produced(&root, "0/a.md", b"mine");
                let e = publish(&root, &["0/a.md"], &target, "a", "md").unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e:?}");
                assert_eq!(names(&target), Vec::<String>::new());
            }
            other => panic!("unknown case {other}"),
        }
        // SAFETY: skips the harness' teardown, which a filter may break.
        unsafe { libc::_exit(0) };
    }

    #[test]
    fn work_dir_cleanup_works_with_fd_0_closed() {
        let t = Tmp::new("fd0-remove");
        assert_case_ok("fd0_remove_tree", &t.0);
    }

    #[test]
    fn removal_open_survives_a_swapped_name_on_both_routes() {
        // Magic-link route: the opened inode is used, the impostor untouched.
        let t = Tmp::new("swap-magic");
        assert_case_ok("swap_magic", &t.0);
        // Fallback route (chmod refused): a directory, a FIFO or a file
        // swapped in after the O_PATH open is refused, neither blocked on
        // (the child's watchdog would fire) nor chmod-ed.
        if !crate::sandbox::tests::seccomp_or_skip("removal_open_swapped_name_fallback") {
            return;
        }
        for case in [
            "swap_fallback_dir",
            "swap_fallback_fifo",
            "swap_fallback_file",
        ] {
            let t = Tmp::new("swap-fallback");
            assert_case_ok(case, &t.0);
        }
    }

    #[test]
    fn removal_fd_is_close_on_exec_and_not_leaked() {
        let t = Tmp::new("rm-fd");
        assert_case_ok("open_dir_flags_and_no_leak", &t.0);
    }

    #[test]
    fn descriptor_exhaustion_is_an_error_not_a_bogus_fd() {
        for case in ["emfile_on_root", "emfile_on_leaf"] {
            let t = Tmp::new("emfile");
            assert_case_ok(case, &t.0);
        }
    }

    #[test]
    fn random_failures_are_reported_and_interrupts_retried() {
        if !crate::sandbox::tests::seccomp_or_skip("random_failures") {
            return;
        }
        let t = Tmp::new("getrandom");
        assert_case_ok("getrandom_eperm", &t.0);
        assert_case_ok("getrandom_short", &t.0);
        // Retried forever: only the watchdog ends the child.
        let out = run_case("getrandom_eintr", &t.0);
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&out.status),
            Some(libc::SIGALRM),
            "{out:?}"
        );
    }

    #[test]
    fn work_dir_names_exhausted_is_reported_as_such() {
        if !crate::sandbox::tests::seccomp_or_skip("work_dir_names_exhausted") {
            return;
        }
        let t = Tmp::new("mkdir-exists");
        assert_case_ok("mkdir_always_exists", &t.0);
    }

    #[test]
    fn work_dir_in_an_unwritable_folder_reports_permission_denied() {
        if root_or_skip("work_dir_in_an_unwritable_folder") {
            return;
        }
        let t = Tmp::new("ro-workdir");
        fs::set_permissions(&t.0, fs::Permissions::from_mode(0o500)).unwrap();
        let e = make_work_dir(&t.0).err().expect("no work dir");
        fs::set_permissions(&t.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e:?}");
    }

    /// Child side of `open_dir_fallback_without_chmod`; a no-op otherwise.
    #[test]
    fn helper_open_dir_fallback() {
        let Some(dir) = std::env::var_os("INACIRCLE_HELPER") else {
            return;
        };
        if std::env::var_os("INACIRCLE_CASE").is_some() {
            return;
        }
        let dir = PathBuf::from(dir);
        // The parent made `open` (0o700) and `locked` (0o000) before the
        // filter: chmod is denied here.
        let c = cstr(&dir.join("open")).unwrap();
        let fd = open_dir_for_removal(None, &c).expect("a readable directory opens");
        assert!(cloexec(fd.0), "fallback fd leaks into children");
        drop(fd);
        // chmod is denied, so a directory without `r` cannot be opened: an
        // error, never a pretend descriptor.
        let c = cstr(&dir.join("locked")).unwrap();
        let e = open_dir_for_removal(None, &c)
            .err()
            .expect("opened a 0o000 dir");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e:?}");
        // Not even a name that does not exist.
        let c = cstr(&dir.join("missing")).unwrap();
        let e = open_dir_for_removal(None, &c)
            .err()
            .expect("opened a missing dir");
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e:?}");
    }

    #[test]
    fn open_dir_fallback_without_chmod() {
        if root_or_skip("open_dir_fallback_without_chmod")
            || !crate::sandbox::tests::seccomp_or_skip("open_dir_fallback_without_chmod")
        {
            return;
        }
        let t = Tmp::new("open-fallback");
        fs::create_dir(t.0.join("open")).unwrap();
        fs::create_dir(t.0.join("locked")).unwrap();
        fs::set_permissions(t.0.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        run_helper_under_seccomp("output::tests::helper_open_dir_fallback", &t.0);
        fs::set_permissions(t.0.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn opening_a_missing_directory_is_not_found() {
        let t = Tmp::new("missing-dir");
        let c = cstr(&t.0.join("nope")).unwrap();
        let e = open_dir_for_removal(None, &c).err().expect("opened");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&t.0, &link).unwrap();
        let c = cstr(&link).unwrap();
        assert!(
            open_dir_for_removal(None, &c).is_err(),
            "followed a symlink"
        );
    }

    #[test]
    fn stale_cleanup_waits_a_full_day() {
        let t = Tmp::new("stale-age");
        let hour = 3600;
        let ages = [
            ("0000000000000001", 12 * hour),
            ("0000000000000002", 23 * hour),
        ];
        for (name, age) in ages {
            let d = t.0.join(format!(".inacircle-{name}"));
            fs::create_dir(&d).unwrap();
            set_age(&d, age);
        }
        let gone = t.0.join(".inacircle-0000000000000003");
        fs::create_dir(&gone).unwrap();
        set_age(&gone, 25 * hour);
        clean_stale(&t.0);
        assert!(
            t.0.join(".inacircle-0000000000000001").exists(),
            "12 h old removed"
        );
        assert!(
            t.0.join(".inacircle-0000000000000002").exists(),
            "23 h old removed"
        );
        assert!(!gone.exists(), "25 h old kept");
    }

    #[test]
    fn candidate_cuts_multibyte_stems_on_a_character_boundary() {
        let name = |stem: &str, ext: &str, n: u32| {
            let p = candidate(stem.as_bytes(), ext.as_bytes(), n).unwrap();
            p.into_string().unwrap()
        };
        assert_eq!(name("a", "md", 0), "a.md");
        assert_eq!(name("a", "md", 2), "a (2).md");
        // 2-byte characters: the budget (251) ends inside one.
        let two = "é".repeat(200);
        let n = name(&two, "txt", 0);
        assert_eq!(n, format!("{}.txt", "é".repeat(125)));
        assert_eq!(n.len(), 254);
        // 3-byte characters: two bytes back to the boundary.
        let three = "€".repeat(100);
        let n = name(&three, "txt", 0);
        assert_eq!(n, format!("{}.txt", "€".repeat(83)));
        // 4-byte characters, and the number counts toward the limit.
        let four = "😀".repeat(100);
        let n = name(&four, "md", 12);
        assert!(n.len() <= NAME_MAX && n.ends_with(" (12).md"), "{n}");
        assert!(n.starts_with('😀'));
        // A name that fits exactly is not cut.
        let exact = "a".repeat(251);
        assert_eq!(name(&exact, "txt", 0).len(), 255);
        let over = "a".repeat(300);
        assert_eq!(name(&over, "txt", 0).len(), 255);
    }

    #[test]
    fn candidate_refuses_when_no_stem_fits() {
        let long_ext = vec![b'a'; NAME_MAX - 1];
        // Not even a byte of stem fits; a stem that starts with a
        // continuation byte (a non-UTF-8 name) must not underflow.
        for stem in [&b"abc"[..], &b"\x80abc"[..]] {
            let e = candidate(stem, &long_ext, 0).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn split_name_splits_at_the_last_dot_but_not_a_leading_one() {
        let s = |n: &str| {
            let (a, b) = split_name(n.as_bytes());
            (a.to_vec(), b.to_vec())
        };
        assert_eq!(s("a.txt"), (b"a".to_vec(), b"txt".to_vec()));
        assert_eq!(s("a.tar.gz"), (b"a.tar".to_vec(), b"gz".to_vec()));
        assert_eq!(s(".bashrc"), (b".bashrc".to_vec(), Vec::new()));
        assert_eq!(s(".a.b"), (b".a".to_vec(), b"b".to_vec()));
        assert_eq!(s("noext"), (b"noext".to_vec(), Vec::new()));
    }

    #[test]
    fn open_regular_beneath_opens_unreadable_files_close_on_exec() {
        if root_or_skip("open_regular_beneath_opens_unreadable") {
            return;
        }
        let t = Tmp::new("open-regular");
        let f = file(&t.0, "secret", "x");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
        let fd = open_regular_beneath(&t.0, Path::new("secret")).expect("O_PATH needs no read");
        assert!(cloexec(fd.as_raw_fd()));
        let err = open_regular_beneath(&t.0, Path::new("nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let d = t.0.join("dir");
        fs::create_dir(&d).unwrap();
        assert!(open_regular_beneath(&t.0, Path::new("dir")).is_err());
    }

    #[test]
    fn open_path_is_close_on_exec_and_directory_only() {
        let t = Tmp::new("open-path");
        let f = file(&t.0, "plain", "x");
        let c = cstr(&t.0).unwrap();
        let d = open_path(libc::AT_FDCWD, &c, 0).unwrap();
        assert!(cloexec(d.0));
        let cf = cstr(&f).unwrap();
        let e = open_path(libc::AT_FDCWD, &cf, 0)
            .err()
            .expect("opened a file");
        assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR));
    }

    // ---- Per-input publication ----

    /// An output of the (dead) producer at `root/<rel>`.
    fn produced(root: &Path, rel: &str, body: &[u8]) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    fn rels(r: &[&str]) -> Vec<PathBuf> {
        r.iter().map(PathBuf::from).collect()
    }

    /// `publish_beneath` with a `before_commit` that accepts.
    fn publish(
        root: &Path,
        r: &[&str],
        target: &Path,
        stem: &str,
        ext: &str,
    ) -> io::Result<Publication> {
        publish_beneath(root, &rels(r), target, stem, ext, &mut |_, _| Ok(()))
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Runs `f` with `fault` installed at every publication point.
    fn with_fault<R>(
        fault: impl FnMut(Point) -> Option<i32> + 'static,
        f: impl FnOnce() -> R,
    ) -> R {
        FAULT.set(Some(Box::new(fault)));
        let r = f();
        FAULT.set(None);
        r
    }

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// A target with a root (producer) dir and a target dir in it.
    fn pub_fixture(tag: &str) -> (Tmp, PathBuf, PathBuf) {
        let t = Tmp::new(tag);
        let (root, target) = (t.0.join("root"), t.0.join("target"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(&target).unwrap();
        (t, root, target)
    }

    #[test]
    fn publication_wraps_a_single_output_in_its_own_folder() {
        let (_t, root, target) = pub_fixture("pub-single");
        let src = produced(&root, "0/out.md", b"# a\n");
        let p = publish(&root, &["0/out.md"], &target, "a", "md").unwrap();
        let wrapper = target.join("a");
        assert_eq!(p.location.path_at_check.as_deref(), Some(wrapper.as_path()));
        assert_eq!(p.location.checked_at, CheckPoint::PostPublish);
        assert_eq!(p.durability, Durability::Synced);
        assert_eq!(names(&target), ["a"]);
        assert_eq!(names(&wrapper), ["a.md"]);
        assert_eq!(fs::read(wrapper.join("a.md")).unwrap(), b"# a\n");
        let wm = fs::symlink_metadata(&wrapper).unwrap();
        assert!(wm.is_dir());
        let (dmode, fmode) = (published_dir_mode(0), published_file_mode());
        assert_eq!(wm.mode() & 0o7777, dmode);
        // The wrapper's private identity is the published one.
        assert_eq!(
            p.group.id,
            DirectoryId {
                dev: wm.dev(),
                ino: wm.ino(),
                kind: NodeKind::Directory,
                uid: uid(),
                mode: dmode
            }
        );
        assert_eq!(
            (p.location.anchor.object_dev, p.location.anchor.object_ino),
            (wm.dev(), wm.ino())
        );
        let pm = fs::metadata(&target).unwrap();
        assert_eq!(
            (p.location.anchor.parent_dev, p.location.anchor.parent_ino),
            (pm.dev(), pm.ino())
        );
        assert_eq!(p.location.anchor.leaf, "a");
        assert_eq!(p.location.anchor.op, p.group.op);
        // One child, its baseline from the private copy, still matching.
        let cm = fs::symlink_metadata(wrapper.join("a.md")).unwrap();
        let [c] = &p.group.children[..] else {
            panic!("{:?}", p.group.children)
        };
        assert_eq!(c.rel, Path::new("a.md"));
        assert_eq!(
            (c.kind, c.uid, c.mode, c.nlink),
            (NodeKind::File, uid(), fmode, 1)
        );
        assert_eq!(c.file_id.ino, cm.ino());
        assert_eq!(c.file_id.size, 4);
        // A fresh inode: the producer's file is copied, not moved.
        assert!(src.exists());
        assert_ne!(fs::metadata(&src).unwrap().ino(), cm.ino());
        let files = postcheck(&p).unwrap();
        assert_eq!(files, [(wrapper.join("a.md"), c.file_id)]);
    }

    #[test]
    fn publication_of_several_outputs_keeps_their_names() {
        let (_t, root, target) = pub_fixture("pub-many");
        produced(&root, "0/p-1.png", b"1");
        produced(&root, "0/sub/p-1.png", b"2");
        produced(&root, "0/.hidden", b"3");
        file(&target, "doc", "taken");
        let p = publish(
            &root,
            &["0/p-1.png", "0/sub/p-1.png", "0/.hidden"],
            &target,
            "doc",
            "png",
        )
        .unwrap();
        let w = target.join("doc (1)");
        assert_eq!(p.location.path_at_check.as_deref(), Some(w.as_path()));
        assert_eq!(names(&w), [".hidden", "p-1 (1).png", "p-1.png"]);
        let rels: Vec<_> = p.group.children.iter().map(|c| c.rel.clone()).collect();
        assert_eq!(
            rels,
            [
                PathBuf::from("p-1.png"),
                "p-1 (1).png".into(),
                ".hidden".into()
            ]
        );
        assert_eq!(fs::read_to_string(target.join("doc")).unwrap(), "taken");
        assert_eq!(postcheck(&p).unwrap().len(), 3);
    }

    #[test]
    fn publication_never_takes_an_existing_name() {
        let (_t, root, target) = pub_fixture("pub-noreplace");
        produced(&root, "0/x.md", b"mine");
        file(&target, "a", "theirs");
        fs::create_dir(target.join("a (1)")).unwrap();
        file(&target.join("a (1)"), "a.md", "theirs too");
        // A name taken right before the rename, by a racing program.
        let racer = target.join("a (2)");
        let r = racer.clone();
        let p = with_fault(
            move |pt| {
                if pt == Point::BeforeRename && !r.exists() {
                    fs::create_dir(&r).unwrap();
                    fs::write(r.join("a.md"), "raced").unwrap();
                }
                None
            },
            || publish(&root, &["0/x.md"], &target, "a", "md"),
        )
        .unwrap();
        assert_eq!(p.location.path_at_check, Some(target.join("a (3)")));
        assert_eq!(fs::read_to_string(target.join("a")).unwrap(), "theirs");
        assert_eq!(
            fs::read_to_string(target.join("a (1)/a.md")).unwrap(),
            "theirs too"
        );
        assert_eq!(fs::read_to_string(racer.join("a.md")).unwrap(), "raced");
        assert_eq!(
            fs::read_to_string(target.join("a (3)/a.md")).unwrap(),
            "mine"
        );
        assert_eq!(names(&target), ["a", "a (1)", "a (2)", "a (3)"]);
    }

    #[test]
    fn publication_is_all_or_nothing_for_one_input() {
        let (t, root, target) = pub_fixture("pub-atomic");
        produced(&root, "0/a.png", b"1");
        file(&target, "keep", "k");
        // The second output is missing: the first is never visible.
        let e = publish(&root, &["0/a.png", "0/b.png"], &target, "doc", "png").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e:?}");
        assert_eq!(
            names(&target),
            ["keep"],
            "something of the input became visible"
        );
        assert!(root.join("0/a.png").exists());
        // Every failure before the commit, injected one at a time: nothing
        // visible, the private staging gone, the source untouched.
        for (point, errno) in [
            (Point::Write, libc::ENOSPC),
            (Point::SyncChild, libc::EIO),
            (Point::SyncWrapper, libc::EIO),
            (Point::SyncStaging, libc::EIO),
            (Point::Rename, libc::EIO),
        ] {
            produced(&root, "0/b.png", b"2");
            let e = with_fault(
                move |p| (p == point).then_some(errno),
                || publish(&root, &["0/a.png", "0/b.png"], &target, "doc", "png"),
            )
            .unwrap_err();
            assert_eq!(e.raw_os_error(), Some(errno), "{point:?}: {e:?}");
            assert_eq!(names(&target), ["keep"], "{point:?}");
            assert_eq!(fs::read(root.join("0/b.png")).unwrap(), b"2");
        }
        assert_eq!(names(&t.0), ["root", "target"]);
    }

    #[test]
    fn publication_refused_by_before_commit_publishes_nothing() {
        let (_t, root, target) = pub_fixture("pub-prepare");
        produced(&root, "0/a.md", b"a");
        let mut seen = None;
        let e = publish_beneath(
            &root,
            &rels(&["0/a.md"]),
            &target,
            "a",
            "md",
            &mut |g, private| {
                // The private copy is complete and fixed when this runs.
                let m = fs::symlink_metadata(private).unwrap();
                assert_eq!(
                    (m.dev(), m.ino(), m.mode() & 0o7777),
                    (g.id.dev, g.id.ino, published_dir_mode(0))
                );
                let c = &g.children[0];
                let cm = fs::symlink_metadata(private.join(&c.rel)).unwrap();
                assert_eq!(
                    FileId::from_stat(&stat_of(&private.join(&c.rel))),
                    c.file_id
                );
                assert_eq!(cm.mode() & 0o7777, c.mode);
                seen = Some(private.to_path_buf());
                Err(io::Error::other("journal write failed"))
            },
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "journal write failed");
        assert_eq!(names(&target), Vec::<String>::new());
        assert!(!seen.unwrap().exists(), "private staging left behind");
    }

    /// A setgid target keeps its group semantics: the staging folder and
    /// the wrapper keep S_ISGID (a plain chmod would drop it, and the
    /// children would get the user's primary group).
    #[test]
    fn publication_keeps_setgid_of_the_target() {
        let (_t, root, target) = pub_fixture("pub-setgid");
        produced(&root, "0/a.md", b"a");
        let tm = fs::symlink_metadata(&target).unwrap().mode() & 0o7777;
        fs::set_permissions(&target, fs::Permissions::from_mode(tm | 0o2000)).unwrap();
        assert_ne!(
            fs::symlink_metadata(&target).unwrap().mode() & 0o2000,
            0,
            "this filesystem does not keep setgid on a folder"
        );
        let mut staging_mode = 0;
        let p = publish_beneath(
            &root,
            &rels(&["0/a.md"]),
            &target,
            "a",
            "md",
            &mut |_, private| {
                let staging = private.parent().unwrap();
                staging_mode = fs::symlink_metadata(staging).unwrap().mode() & 0o7777;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(staging_mode, 0o2700);
        let wm = fs::symlink_metadata(target.join("a")).unwrap().mode() & 0o7777;
        assert_eq!(wm, published_dir_mode(0) | 0o2000);
        assert_eq!(p.group.id.mode, wm);
        assert_eq!(postcheck(&p).unwrap().len(), 1);
    }

    /// A name taken between the check and the rename: `before_commit`
    /// runs again, for the same group and operation, with the new folder.
    #[test]
    fn publication_reruns_before_commit_for_the_next_name() {
        let (_t, root, target) = pub_fixture("pub-rerun");
        produced(&root, "0/a.md", b"a");
        let racer = target.join("a");
        let mut calls: Vec<(GroupId, OperationId, PathBuf)> = Vec::new();
        let p = with_fault(
            move |pt| {
                if pt == Point::BeforeRename && !racer.exists() {
                    fs::create_dir(&racer).unwrap();
                }
                None
            },
            || {
                publish_beneath(
                    &root,
                    &rels(&["0/a.md"]),
                    &target,
                    "a",
                    "md",
                    &mut |g, _| {
                        calls.push((g.group, g.op, g.directory.clone()));
                        Ok(())
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!((calls[0].0, calls[0].1), (calls[1].0, calls[1].1));
        assert_eq!(calls[0].2, target.join("a"));
        assert_eq!(calls[1].2, target.join("a (1)"));
        assert_eq!((p.group.group, p.group.op), (calls[1].0, calls[1].1));
        assert_eq!(p.group.directory, target.join("a (1)"));
    }

    /// An edit of the private copy after its baseline (while the journal
    /// prepares, before the commit) is no new baseline: the postcheck
    /// refuses the published folder.
    #[test]
    fn publication_edit_of_the_private_copy_before_commit_is_detected() {
        let (_t, root, target) = pub_fixture("pub-private-edit");
        produced(&root, "0/a.md", b"a");
        let mut prepared = None;
        let p = publish_beneath(
            &root,
            &rels(&["0/a.md"]),
            &target,
            "a",
            "md",
            &mut |g, private| {
                prepared = Some(g.clone());
                fs::write(private.join("a.md"), "edited").unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(prepared.unwrap().children, p.group.children);
        assert_eq!(p.group.children[0].file_id.size, 1);
        assert_eq!(postcheck(&p), Err(PublicationReason::BaselineChanged));
    }

    fn stat_of(p: &Path) -> libc::stat {
        stat_at(At {
            dir: libc::AT_FDCWD,
            name: &cstr(p).unwrap(),
        })
        .unwrap()
    }

    #[test]
    fn publication_baseline_is_the_private_one_and_ids_stay() {
        let (_t, root, target) = pub_fixture("pub-ids");
        produced(&root, "0/a.md", b"a");
        let mut prepared = None;
        let p = publish_beneath(
            &root,
            &rels(&["0/a.md"]),
            &target,
            "a",
            "md",
            &mut |g, _| {
                prepared = Some(g.clone());
                Ok(())
            },
        )
        .unwrap();
        let prepared = prepared.unwrap();
        assert_eq!((prepared.group, prepared.op), (p.group.group, p.group.op));
        assert_eq!(prepared.id, p.group.id);
        assert_eq!(prepared.children, p.group.children);
        // Comparable after the publication, and a later edit is no new
        // baseline: the postcheck sees it.
        let child = target.join("a/a.md");
        assert_eq!(
            FileId::from_stat(&stat_of(&child)),
            p.group.children[0].file_id
        );
        fs::write(&child, "edited").unwrap();
        assert_eq!(postcheck(&p), Err(PublicationReason::BaselineChanged));
        assert_eq!(prepared.children[0].file_id.size, 1);
        // Another publication: other group and operation ids.
        let q = publish(&root, &["0/a.md"], &target, "a", "md").unwrap();
        assert_ne!(q.group.group, p.group.group);
        assert_ne!(q.group.op, p.group.op);
    }

    #[test]
    fn publication_copy_is_out_of_reach_of_the_producers_fd() {
        use std::io::Write;
        let (_t, root, target) = pub_fixture("pub-oldfd");
        let src = produced(&root, "0/a.md", b"made");
        let mut old = fs::OpenOptions::new().write(true).open(&src).unwrap();
        let p = publish(&root, &["0/a.md"], &target, "a", "md").unwrap();
        old.write_all(b"LATE").unwrap();
        drop(old);
        assert_eq!(fs::read(target.join("a/a.md")).unwrap(), b"made");
        assert_eq!(postcheck(&p).unwrap().len(), 1);
    }

    #[test]
    fn publication_keeps_committed_outputs_on_sync_errors() {
        for point in [Point::SyncStagingAfter, Point::SyncParentAfter] {
            let (_t, root, target) = pub_fixture("pub-sync");
            produced(&root, "0/a.md", b"a");
            let p = with_fault(
                move |p| (p == point).then_some(libc::EIO),
                || publish(&root, &["0/a.md"], &target, "a", "md"),
            )
            .expect("a sync error after the commit is no failure");
            let Durability::Unconfirmed(why) = &p.durability else {
                panic!("{point:?}: {:?}", p.durability)
            };
            assert!(why.contains("Input/output error"), "{why}");
            // Still visible, still at the proved place.
            assert_eq!(p.location.path_at_check, Some(target.join("a")));
            assert_eq!(fs::read(target.join("a/a.md")).unwrap(), b"a");
            assert_eq!(postcheck(&p).unwrap().len(), 1);
        }
    }

    #[test]
    fn publication_with_a_moved_ancestor_has_no_proved_path() {
        let (t, root, target) = pub_fixture("pub-ancestor");
        produced(&root, "0/a.md", b"a");
        let moved = t.0.join("moved");
        let (from, to) = (target.clone(), moved.clone());
        let p = with_fault(
            move |pt| {
                if pt == Point::BeforeRename {
                    fs::rename(&from, &to).unwrap();
                }
                None
            },
            || publish(&root, &["0/a.md"], &target, "a", "md"),
        )
        .unwrap();
        // Committed into the folder it opened, now elsewhere: kept, no
        // invented path, the anchor names it.
        assert_eq!(p.location.path_at_check, None);
        assert_eq!(fs::read(moved.join("a/a.md")).unwrap(), b"a");
        let m = fs::metadata(moved.join("a")).unwrap();
        assert_eq!(
            (p.location.anchor.object_dev, p.location.anchor.object_ino),
            (m.dev(), m.ino())
        );
        assert_eq!(postcheck(&p), Err(PublicationReason::AncestorChanged));
        // A folder made at the old path is not taken for it.
        fs::create_dir_all(target.join("a")).unwrap();
        let mut q = p.clone();
        q.location.path_at_check = Some(target.join("a"));
        assert_eq!(postcheck(&q), Err(PublicationReason::AncestorChanged));
    }

    #[test]
    fn publication_refuses_links_traversal_and_special_files() {
        let (t, root, target) = pub_fixture("pub-refuse");
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        file(&outside, "secret.md", "secret");
        fs::create_dir(root.join("0")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), root.join("0/leaf.md")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("1")).unwrap();
        // SAFETY: the path is a valid NUL-terminated string.
        let fifo = cstr(&root.join("0/fifo.md")).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        produced(&root, "2/ok.md", b"ok");
        for bad in [
            "0/leaf.md",
            "1/secret.md",
            "0/fifo.md",
            "0",
            "../outside/secret.md",
            "/etc/passwd",
            "",
        ] {
            let r = publish(&root, &["2/ok.md", bad], &target, "x", "md");
            assert!(r.is_err(), "{bad}: {r:?}");
        }
        // A good output swapped for a link, a FIFO or a directory after its
        // parent was resolved: never followed, never blocked on.
        for swap in ["link", "fifo", "dir"] {
            produced(&root, "3/s.md", b"s");
            let p3 = root.join("3/s.md");
            let o = outside.join("secret.md");
            let r = with_fault(
                move |pt| {
                    if pt == Point::Open && p3.exists() {
                        fs::remove_file(&p3).unwrap();
                        match swap {
                            "link" => std::os::unix::fs::symlink(&o, &p3).unwrap(),
                            "fifo" => {
                                let c = cstr(&p3).unwrap();
                                // SAFETY: c is a valid path.
                                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
                            }
                            _ => fs::create_dir(&p3).unwrap(),
                        }
                    }
                    None
                },
                || publish(&root, &["3/s.md"], &target, "s", "md"),
            );
            assert!(r.is_err(), "{swap}: {r:?}");
            let _ = fs::remove_file(root.join("3/s.md"));
            let _ = fs::remove_dir(root.join("3/s.md"));
        }
        assert_eq!(
            fs::read_to_string(outside.join("secret.md")).unwrap(),
            "secret"
        );
        assert_eq!(names(&target), Vec::<String>::new());
    }

    #[test]
    fn publication_refuses_an_output_that_changes_while_copied() {
        let (_t, root, target) = pub_fixture("pub-grow");
        let src = produced(&root, "0/a.md", b"start");
        let e = with_fault(
            move |pt| {
                if pt == Point::Copy {
                    let mut f = fs::OpenOptions::new().append(true).open(&src).unwrap();
                    std::io::Write::write_all(&mut f, b"more").unwrap();
                }
                None
            },
            || publish(&root, &["0/a.md"], &target, "a", "md"),
        )
        .unwrap_err();
        assert!(e.to_string().contains("changed while"), "{e}");
        assert_eq!(names(&target), Vec::<String>::new());
    }

    #[test]
    fn publication_validates_text_outputs() {
        let (_t, root, target) = pub_fixture("pub-utf8");
        produced(&root, "0/a.md", b"ok \xc3\xa9");
        produced(&root, "1/a.md", b"bad \xc3");
        produced(&root, "2/a.txt", b"\xff");
        produced(&root, "3/a.png", b"\xff");
        assert!(publish(&root, &["0/a.md"], &target, "a", "md").is_ok());
        for rel in ["1/a.md", "2/a.txt"] {
            let e = publish(&root, &[rel], &target, "b", "md").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{rel}: {e:?}");
        }
        // Only text is checked as text.
        assert!(publish(&root, &["3/a.png"], &target, "c", "png").is_ok());
        assert_eq!(names(&target), ["a", "c"]);
    }

    #[test]
    fn publication_copies_across_filesystems_before_the_commit() {
        let (_t, _root, target) = pub_fixture("pub-exdev");
        let shm = Path::new("/dev/shm");
        let same = fs::metadata(shm).map(|m| m.dev() == fs::metadata(&target).unwrap().dev());
        if !matches!(same, Ok(false)) {
            eprintln!(
                "SKIPPED publication_copies_across_filesystems: no second filesystem at /dev/shm"
            );
            return;
        }
        let root = Tmp(shm.join(format!("inacircle-pub-exdev-{}", std::process::id())));
        fs::create_dir_all(&root.0).unwrap();
        produced(&root.0, "0/a.md", b"far");
        let p = publish(&root.0, &["0/a.md"], &target, "a", "md").unwrap();
        assert_eq!(fs::read(target.join("a/a.md")).unwrap(), b"far");
        assert_eq!(p.group.id.dev, fs::metadata(&target).unwrap().dev());
    }

    #[test]
    fn publication_refuses_a_rename_that_cannot_refuse_to_replace() {
        // Injected EXDEV at the commit (a mount appearing under the
        // target): refused, no copying fallback.
        let (_t, root, target) = pub_fixture("pub-unsupported");
        produced(&root, "0/a.md", b"a");
        for errno in [libc::EXDEV, libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP] {
            let e = with_fault(
                move |p| (p == Point::Rename).then_some(errno),
                || publish(&root, &["0/a.md"], &target, "a", "md"),
            )
            .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{errno}: {e:?}");
            assert_eq!(names(&target), Vec::<String>::new());
        }
        // The real syscall refused by the kernel: see
        // `filesystem_without_noreplace_is_refused`.
    }

    #[test]
    fn publication_wrapper_never_takes_our_staging_pattern() {
        let (_t, root, target) = pub_fixture("pub-pattern");
        produced(&root, "0/a.md", b"a");
        let p = publish(
            &root,
            &["0/a.md"],
            &target,
            ".inacircle-0123456789abcdef",
            "md",
        )
        .unwrap();
        let leaf = p.location.anchor.leaf.clone();
        assert!(
            !is_our_name(leaf.as_bytes()),
            "{leaf:?} would be swept as stale"
        );
        assert_eq!(leaf, ".inacircle-0123456789abcdef (1)");
    }

    #[test]
    fn filesystem_without_noreplace_is_refused() {
        if !crate::sandbox::tests::seccomp_or_skip("filesystem_without_noreplace_is_refused") {
            return;
        }
        for case in [
            "publish_renameat2_einval",
            "publish_renameat2_enosys",
            "publish_renameat2_eperm",
        ] {
            let t = Tmp::new("pub-no-noreplace");
            assert_case_ok(case, &t.0);
        }
    }

    #[test]
    fn publication_works_with_fd_0_closed() {
        let t = Tmp::new("fd0-publish");
        assert_case_ok("fd0_publish", &t.0);
    }

    #[test]
    fn publication_numbers_names_and_gives_up_after_999() {
        let (_t, root, target) = pub_fixture("pub-999");
        produced(&root, "0/a.md", b"a");
        fs::create_dir(target.join("a")).unwrap();
        for n in 1..=MAX_CONFLICT {
            fs::create_dir(target.join(format!("a ({n})"))).unwrap();
        }
        let e = publish(&root, &["0/a.md"], &target, "a", "md").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e:?}");
        assert_eq!(e.to_string(), "no free output name");
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            1000,
            "staging left behind"
        );
        fs::remove_dir(target.join("a (999)")).unwrap();
        let p = publish(&root, &["0/a.md"], &target, "a", "md").unwrap();
        assert_eq!(p.location.path_at_check, Some(target.join("a (999)")));
    }

    #[test]
    fn publication_refuses_invalid_names_and_cuts_long_ones() {
        let (_t, root, target) = pub_fixture("pub-names");
        produced(&root, "0/a.md", b"a");
        for (stem, ext) in [
            ("", "md"),
            ("a/b", "md"),
            (".", "md"),
            ("..", "md"),
            ("a", "m/d"),
            ("a", ".."),
        ] {
            let e = publish(&root, &["0/a.md"], &target, stem, ext).unwrap_err();
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidInput,
                "{stem:?}.{ext:?}: {e:?}"
            );
        }
        assert!(publish_beneath(&root, &[], &target, "a", "md", &mut |_, _| Ok(())).is_err());
        let long = "é".repeat(200);
        let p = publish(&root, &["0/a.md"], &target, &long, "md").unwrap();
        let w = p.location.path_at_check.unwrap();
        assert!(w.file_name().unwrap().len() <= NAME_MAX);
        let child = &p.group.children[0].rel;
        assert!(child.as_os_str().len() <= NAME_MAX && child.to_str().unwrap().ends_with(".md"));
        assert_eq!(names(&target).len(), 1);
    }

    #[test]
    fn publication_keeps_non_utf8_names_of_several_outputs() {
        let (_t, root, target) = pub_fixture("pub-bytes");
        let odd = OsStr::from_bytes(b"p-\xff.png");
        fs::create_dir(root.join("0")).unwrap();
        fs::write(root.join("0").join(odd), "1").unwrap();
        produced(&root, "0/q.png", b"2");
        let rels = [Path::new("0").join(odd), PathBuf::from("0/q.png")];
        let p = publish_beneath(&root, &rels, &target, "doc", "png", &mut |_, _| Ok(())).unwrap();
        assert!(target.join("doc").join(odd).exists());
        assert_eq!(p.group.children[0].rel, Path::new(odd));
    }

    /// Runs one test of this binary in a child with RLIMIT_NOFILE 256.
    #[test]
    fn publication_needs_few_fds() {
        let t = Tmp::new("pub-nofile");
        run_helper_with_nofile("output::tests::helper_publish_1100", &t.0, 256);
        assert_eq!(fs::read_dir(t.0.join("target/doc")).unwrap().count(), 1100);
    }

    /// Child side of `publication_needs_few_fds`; a no-op otherwise.
    #[test]
    fn helper_publish_1100() {
        let Some(dir) = std::env::var_os("INACIRCLE_HELPER") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (root, target) = (dir.join("root"), dir.join("target"));
        fs::create_dir(&target).unwrap();
        let rels: Vec<PathBuf> = (0..1100)
            .map(|k| {
                let rel = format!("0/p-{k:04}.png");
                produced(&root, &rel, b"x");
                PathBuf::from(rel)
            })
            .collect();
        let p = publish_beneath(&root, &rels, &target, "doc", "png", &mut |_, _| Ok(())).unwrap();
        assert_eq!(p.group.children.len(), 1100);
        assert_eq!(postcheck(&p).unwrap().len(), 1100);
    }

    #[test]
    fn publication_into_an_unwritable_or_missing_folder_fails_cleanly() {
        let (t, root, _target) = pub_fixture("pub-ro");
        produced(&root, "0/a.md", b"a");
        let missing = publish(&root, &["0/a.md"], &t.0.join("none"), "a", "md").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        let plain = file(&t.0, "plain", "p");
        assert!(publish(&root, &["0/a.md"], &plain, "a", "md").is_err());
        assert_eq!(fs::read_to_string(&plain).unwrap(), "p");
        if root_or_skip("publication_into_an_unwritable_folder") {
            return;
        }
        let ro = t.0.join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o500)).unwrap();
        let e = publish(&root, &["0/a.md"], &ro, "a", "md").unwrap_err();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e:?}");
        assert_eq!(fs::read_dir(&ro).unwrap().count(), 0);
        assert!(
            root.join("0/a.md").exists(),
            "the produced file stays where it was"
        );
    }

    #[test]
    fn publication_needs_no_read_permission_on_the_folders() {
        if root_or_skip("publication_needs_no_read_permission") {
            return;
        }
        let (_t, root, target) = pub_fixture("pub-path-fds");
        produced(&root, "sub/leaf", b"x");
        // Searchable and writable, never listable: the producer's folders
        // and the target. The target then cannot be synced: unconfirmed.
        fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o300)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o100)).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o300)).unwrap();
        let r = publish(&root, &["sub/leaf"], &target, "out", "md");
        for p in [&root, &root.join("sub"), &target] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let p = r.expect("published through non-listable folders");
        assert!(
            matches!(&p.durability, Durability::Unconfirmed(w) if w.contains("Permission denied")),
            "{:?}",
            p.durability
        );
        assert_eq!(fs::read(target.join("out/out.md")).unwrap(), b"x");
        assert_eq!(postcheck(&p).unwrap().len(), 1);
    }

    #[test]
    fn utf8_check_spans_chunk_boundaries() {
        let ok = "aé€😀".as_bytes();
        for cut in 0..=ok.len() {
            let mut u = Utf8::default();
            u.feed(&ok[..cut]).unwrap();
            u.feed(&ok[cut..]).unwrap();
            u.finish().unwrap();
        }
        // Byte by byte, too.
        let mut u = Utf8::default();
        for b in ok {
            u.feed(std::slice::from_ref(b)).unwrap();
        }
        u.finish().unwrap();
        for bad in [
            &b"\xc3"[..],
            b"\xc3(",
            b"\xe2\x82",
            b"\xed\xa0\x80",
            b"\xf5\x80\x80\x80",
            b"\xf0\x9f\x98",
        ] {
            let r = (0..=bad.len()).all(|cut| {
                let mut u = Utf8::default();
                u.feed(&bad[..cut])
                    .and_then(|_| u.feed(&bad[cut..]))
                    .and_then(|_| u.finish())
                    .is_err()
            });
            assert!(r, "{bad:?} accepted");
        }
    }

    /// A crash before the commit leaves only the private staging, under a
    /// name the stale cleanup knows (and takes after a day); one after the
    /// commit leaves the published folder, which the cleanup never takes,
    /// and at most the empty staging. A foreign folder is left alone.
    /// Published modes follow the umask like `cp` (0666/0777 & ~umask),
    /// set while still private, so the baseline holds the final mode.
    #[test]
    fn publication_modes_follow_the_umask() {
        for case in ["umask_022", "umask_077", "umask_222", "umask_100"] {
            let t = Tmp::new("pub-umask");
            assert_case_ok(case, &t.0);
        }
    }

    /// Before the final modes are set everything is 0700/0600: a crash
    /// while writing leaves only private staging, even under umask 022.
    #[test]
    fn publication_private_phase_stays_private() {
        let t = Tmp::new("pub-private");
        let out = run_case("private_modes_before_chmod", &t.0);
        assert_eq!(out.status.code(), Some(7), "{out:?}");
        let target = t.0.join("target");
        let [staging] = &names(&target)[..] else {
            panic!("{:?}", names(&target))
        };
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().mode() & 0o7777;
        let s = target.join(staging);
        assert_eq!(mode(&s), 0o700);
        assert_eq!(mode(&s.join("w")), 0o700);
        assert_eq!(mode(&s.join("w/a.md")), 0o600);
    }

    #[test]
    fn publication_crash_leaves_only_recognizable_staging() {
        for case in ["crash_before_commit", "crash_after_commit"] {
            let t = Tmp::new("pub-crash");
            let out = run_case(case, &t.0);
            assert_eq!(out.status.code(), Some(7), "{case}: {out:?}");
            let target = t.0.join("target");
            fs::create_dir(target.join(".inacircle-x")).unwrap();
            let ours: Vec<String> = names(&target)
                .into_iter()
                .filter(|n| is_our_name(n.as_bytes()))
                .collect();
            let [staging] = &ours[..] else {
                panic!("{case}: {:?}", names(&target))
            };
            let staged = names(&target.join(staging));
            if case == "crash_before_commit" {
                assert_eq!(staged, ["w"]);
                assert_eq!(names(&target.join(staging).join("w")), ["a.md"]);
            } else {
                assert!(staged.is_empty(), "{staged:?}");
                assert_eq!(fs::read(target.join("a/a.md")).unwrap(), b"a");
            }
            set_age(&target.join(staging), 25 * 3600);
            clean_stale(&target);
            let left = names(&target);
            assert!(!left.contains(staging), "{case}: {left:?}");
            assert!(
                left.contains(&".inacircle-x".to_owned()),
                "{case}: foreign removed"
            );
            assert_eq!(left.contains(&"a".to_owned()), case == "crash_after_commit");
        }
    }

    #[test]
    fn work_dir_cleanup_follows_its_own_dir_not_its_old_path() {
        let t = Tmp::new("work-anchor");
        let target = t.0.join("target");
        fs::create_dir(&target).unwrap();
        let w = make_work_dir(&target).unwrap();
        let name = w.path.file_name().unwrap().to_owned();
        file(&w.path, "scratch", "s");
        // The folder is renamed and a look-alike made at the old path.
        fs::rename(&target, t.0.join("moved")).unwrap();
        fs::create_dir_all(&w.path).unwrap();
        file(&w.path, "foreign", "f");
        drop(w);
        assert_eq!(names(&t.0.join("target").join(&name)), ["foreign"]);
        assert!(
            !t.0.join("moved").join(&name).exists(),
            "own work dir left behind"
        );
    }

    // ---- Mutation pass 2 (D-39).

    /// The identity folds seconds and nanoseconds of both times into
    /// nanoseconds, and keeps device, inode and size.
    #[test]
    fn file_id_from_stat_folds_seconds_and_nanoseconds() {
        // SAFETY: an all-zero libc::stat is valid.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        st.st_dev = 7;
        st.st_ino = 8;
        st.st_size = 9;
        st.st_mtime = 3;
        st.st_mtime_nsec = 5;
        st.st_ctime = 11;
        st.st_ctime_nsec = 13;
        assert_eq!(
            FileId::from_stat(&st),
            FileId {
                dev: 7,
                ino: 8,
                size: 9,
                mtime_ns: 3_000_000_005,
                ctime_ns: 11_000_000_013,
            }
        );
    }

    /// A staging file that cannot be created for a reason other than a
    /// name clash fails at once with that reason, instead of walking
    /// through every conflict number and reporting a clash that never was.
    #[test]
    fn a_staging_error_other_than_a_clash_is_reported_as_it_is() {
        if uid() == 0 {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let (t, root, target) = pub_fixture("pub-stage-eacces");
        produced(&root, "0/a.md", b"a");
        let tgt = target.clone();
        let r = with_fault(
            move |pt| {
                if pt == Point::Open {
                    // The staging wrapper is the only folder without
                    // subfolders under the target: make it unwritable.
                    for staging in fs::read_dir(&tgt).unwrap().flatten() {
                        for w in fs::read_dir(staging.path()).unwrap().flatten() {
                            fs::set_permissions(w.path(), fs::Permissions::from_mode(0o500))
                                .unwrap();
                        }
                    }
                }
                None
            },
            || publish(&root, &["0/a.md"], &target, "a", "md"),
        );
        // Let the cleanup and the test's own removal work.
        for staging in fs::read_dir(&target).unwrap().flatten() {
            for w in fs::read_dir(staging.path()).unwrap().flatten() {
                let _ = fs::set_permissions(w.path(), fs::Permissions::from_mode(0o700));
            }
        }
        let e = r.expect_err("an unwritable staging wrapper must fail");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        drop(t);
    }

    /// A leaf name whose existence cannot be told (the target stops being
    /// searchable) is an error: the publication goes no further, in
    /// particular never reaches the rename.
    #[test]
    fn an_unreadable_target_stops_before_the_commit() {
        if uid() == 0 {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let (_t, root, target) = pub_fixture("pub-stat-eacces");
        produced(&root, "0/a.md", b"a");
        let (tgt, seen) = (
            target.clone(),
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        );
        let log = seen.clone();
        let r = with_fault(
            move |pt| {
                log.borrow_mut().push(pt);
                if pt == Point::SyncStaging {
                    fs::set_permissions(&tgt, fs::Permissions::from_mode(0o600)).unwrap();
                }
                None
            },
            || publish(&root, &["0/a.md"], &target, "a", "md"),
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let e = r.expect_err("a target that cannot be searched must fail");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        assert!(
            !seen.borrow().contains(&Point::BeforeRename),
            "reached the rename: {:?}",
            seen.borrow()
        );
        assert!(!target.join("a").exists());
    }
}
