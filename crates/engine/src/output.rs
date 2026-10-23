//! Output policy: where results go and how they are moved into place
//! without ever overwriting anything.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::xdg;

const WORK_PREFIX: &str = ".inacircle-";
const PROBE_PREFIX: &str = "probe-";
const MAX_CONFLICT: u32 = 999;
const NAME_MAX: usize = 255;
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// A hidden 0700 scratch directory inside the target folder, so the final
/// move is an atomic rename. Removed recursively on drop.
pub struct WorkDir {
    pub path: PathBuf,
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        remove_tree(&self.path);
    }
}

fn last_err<T>() -> io::Result<T> {
    Err(io::Error::last_os_error())
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
/// so it hits exactly the inode that was opened. Without `/proc` it falls
/// back to opening directly, which fails for a directory without `r`.
fn open_dir_for_removal(at: Option<RawFd>, name: &CString) -> io::Result<Fd> {
    let at = at.unwrap_or(libc::AT_FDCWD);
    let path_flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: name is a valid NUL-terminated string that outlives the call;
    // `at` is an open directory fd or AT_FDCWD.
    let p = unsafe { libc::openat(at, name.as_ptr(), path_flags) };
    if p < 0 {
        return last_err();
    }
    let p = Fd(p);
    let magic = CString::new(format!("/proc/self/fd/{}", p.0)).expect("no NUL in a number");
    let read_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: magic is a valid NUL-terminated path naming our O_PATH fd.
    if unsafe { libc::chmod(magic.as_ptr(), 0o700) } == 0 {
        // SAFETY: as above; the magic link resolves to the opened inode.
        let fd = unsafe { libc::openat(libc::AT_FDCWD, magic.as_ptr(), read_flags) };
        if fd >= 0 {
            return Ok(Fd(fd));
        }
    }
    // SAFETY: as for the O_PATH open.
    let fd = unsafe { libc::openat(at, name.as_ptr(), read_flags | libc::O_NOFOLLOW) };
    if fd < 0 {
        return last_err();
    }
    let fd = Fd(fd);
    // SAFETY: fd.0 is an open fd.
    unsafe { libc::fchmod(fd.0, 0o700) };
    Ok(fd)
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
    for _ in 0..16 {
        let path = target_dir.join(format!("{WORK_PREFIX}{:016x}", random_u64()?));
        match mkdir_mode(&path, 0o700) {
            Ok(()) => {
                // Independent of the umask. The WorkDir guard removes the
                // directory again if this fails.
                let dir = WorkDir { path };
                fs::set_permissions(&dir.path, fs::Permissions::from_mode(0o700))?;
                return Ok(dir);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
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

/// A move source or destination: a name relative to a directory fd
/// (`AT_FDCWD` for a plain path).
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

/// What `place_many_beneath` made: the fresh folder and the files in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManyPlaced {
    pub folder: PathBuf,
    pub files: Vec<(PathBuf, FileId)>,
}

fn stat_at(at: At) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is valid storage; at.name is NUL-terminated and
    // at.dir an open directory fd or AT_FDCWD.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::fstatat(at.dir, at.name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if r == 0 { Ok(st) } else { last_err() }
}

/// Fallback for filesystems without `RENAME_NOREPLACE`: `linkat` fails with
/// `EEXIST` instead of replacing, then the old name is dropped. If that
/// unlink fails the output is already complete at `to`, so this still
/// returns `Ok`; the work directory's removal deals with `from`.
/// Directories cannot be hard-linked and are rejected before linking.
fn link_then_unlink_at(from: At, to: At) -> io::Result<()> {
    if stat_at(from)?.st_mode & libc::S_IFMT == libc::S_IFDIR {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem cannot move a directory without replacing",
        ));
    }
    // SAFETY: both names are NUL-terminated, both dirs open fds or
    // AT_FDCWD; flags 0 never follows a symlink.
    if unsafe { libc::linkat(from.dir, from.name.as_ptr(), to.dir, to.name.as_ptr(), 0) } != 0 {
        return last_err();
    }
    // SAFETY: as above.
    unsafe { libc::unlinkat(from.dir, from.name.as_ptr(), 0) };
    Ok(())
}

#[cfg(test)]
fn link_then_unlink(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    let cwd = |name| At {
        dir: libc::AT_FDCWD,
        name,
    };
    link_then_unlink_at(cwd(&f), cwd(&t))
}

/// No-replace move of `from` to `to`. `AlreadyExists` when `to` is taken.
/// Neither name's last component is followed if it is a symlink.
fn move_noreplace(from: At, to: At) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and outlive the call; both dirs
    // are open directory fds or AT_FDCWD.
    let r = unsafe {
        libc::renameat2(
            from.dir,
            from.name.as_ptr(),
            to.dir,
            to.name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) => link_then_unlink_at(from, to),
        _ => Err(err),
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

/// `<stem><suffix> (n).<ext>` as bytes. The stem is cut (on a UTF-8
/// boundary) so the whole name fits in `NAME_MAX`.
fn candidate(dir: &Path, stem: &[u8], suffix: &[u8], ext: &[u8], n: u32) -> io::Result<PathBuf> {
    let mut tail = suffix.to_vec();
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
    Ok(dir.join(OsString::from_vec(name)))
}

/// Moves `from` to `dir/<stem><suffix>.<ext>` (numbered on conflict).
/// With `dir_fd`, the destination is named relative to that fd (which must
/// be `dir`), else by its full path.
fn place_bytes(
    from: At,
    dir: &Path,
    dir_fd: Option<RawFd>,
    stem: &[u8],
    ext: &[u8],
    suffix: &[u8],
) -> io::Result<PathBuf> {
    if stem.is_empty() {
        return Err(invalid("empty output stem"));
    }
    check_part(stem)?;
    check_part(ext)?;
    check_part(suffix)?;
    for n in 0..=MAX_CONFLICT {
        let target = candidate(dir, stem, suffix, ext, n)?;
        let to = match dir_fd {
            Some(_) => CString::new(target.file_name().unwrap_or_default().as_bytes())
                .map_err(|_| invalid("invalid output name"))?,
            None => cstr(&target)?,
        };
        let to_at = At {
            dir: dir_fd.unwrap_or(libc::AT_FDCWD),
            name: &to,
        };
        match move_noreplace(from, to_at) {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free output name",
    ))
}

/// Moves `produced` to `dir/<stem><suffix>.<ext>`; a taken name becomes
/// `name (1).ext` ... `name (999).ext`. Each candidate is tried with an
/// atomic no-replace move, so nothing is ever overwritten, even under a race.
pub fn place(
    produced: &Path,
    dir: &Path,
    stem: &str,
    ext: &str,
    suffix: Option<&str>,
) -> io::Result<PathBuf> {
    let from = cstr(produced)?;
    let from = At {
        dir: libc::AT_FDCWD,
        name: &from,
    };
    place_bytes(
        from,
        dir,
        None,
        stem.as_bytes(),
        ext.as_bytes(),
        suffix.unwrap_or("").as_bytes(),
    )
}

/// Splits at the last dot (a leading dot is part of the stem).
fn split_name(name: &OsStr) -> (&[u8], &[u8]) {
    let b = name.as_bytes();
    match b.iter().rposition(|&c| c == b'.') {
        Some(i) if i > 0 => (&b[..i], &b[i + 1..]),
        _ => (b, &[]),
    }
}

/// Several outputs from one input: moves them into a fresh `dir/<stem>/`
/// (a taken folder name becomes `<stem> (1)/` ...), files keep their names.
/// If one move fails, the files already moved and the folder are removed.
pub fn place_many(produced: &[PathBuf], dir: &Path, stem: &str) -> io::Result<Vec<PathBuf>> {
    let placed = place_many_with(produced.len(), dir, stem, false, |i, folder, _| {
        let p = &produced[i];
        let name = p.file_name().ok_or_else(|| invalid("output has no name"))?;
        let (s, e) = split_name(name);
        let from = cstr(p)?;
        let from = At {
            dir: libc::AT_FDCWD,
            name: &from,
        };
        let path = place_bytes(from, folder, None, s, e, b"")?;
        let name = cstr(&path)?;
        let st = stat_at(At {
            dir: libc::AT_FDCWD,
            name: &name,
        })?;
        Ok((path, FileId::from_stat(&st)))
    })?;
    Ok(placed.files.into_iter().map(|(p, _)| p).collect())
}

/// Creates the fresh `dir/<stem>/` folder for `count` outputs and calls
/// `one(i, folder, folder fd)` for each in turn (the fd is opened only with
/// `with_fd`). If one fails, the files already moved and the folder are
/// removed.
fn place_many_with(
    count: usize,
    dir: &Path,
    stem: &str,
    with_fd: bool,
    mut one: impl FnMut(usize, &Path, Option<RawFd>) -> io::Result<(PathBuf, FileId)>,
) -> io::Result<ManyPlaced> {
    if count == 0 {
        return Ok(ManyPlaced {
            folder: PathBuf::new(),
            files: Vec::new(),
        });
    }
    if stem.is_empty() {
        return Err(invalid("empty output stem"));
    }
    check_part(stem.as_bytes())?;
    let mut folder = None;
    for n in 0..=MAX_CONFLICT {
        let cand = candidate(dir, stem.as_bytes(), b"", b"", n)?;
        match mkdir_mode(&cand, 0o777) {
            Ok(()) => {
                folder = Some(cand);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let folder = folder.ok_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "no free output folder name")
    })?;
    let folder_fd = if with_fd {
        match cstr(&folder).and_then(|c| open_path(libc::AT_FDCWD, &c, libc::O_NOFOLLOW)) {
            Ok(fd) => Some(fd),
            Err(e) => {
                let _ = fs::remove_dir(&folder);
                return Err(e);
            }
        }
    } else {
        None
    };
    let mut placed = Vec::with_capacity(count);
    for i in 0..count {
        match one(i, &folder, folder_fd.as_ref().map(|f| f.0)) {
            Ok(t) => placed.push(t),
            Err(e) => {
                for (t, _) in &placed {
                    let _ = fs::remove_file(t);
                }
                let _ = fs::remove_dir(&folder);
                return Err(e);
            }
        }
    }
    Ok(ManyPlaced {
        folder,
        files: placed,
    })
}

/// `openat(at, name, O_PATH | O_DIRECTORY | extra)`.
fn open_path(at: RawFd, name: &CStr, extra: libc::c_int) -> io::Result<Fd> {
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
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
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

/// The placed name must still be the regular file we moved (a leaf swapped
/// for a symlink between the check and the rename would land as a link):
/// otherwise it is removed again.
fn check_placed(dir_fd: RawFd, placed: &Path) -> io::Result<FileId> {
    let name = cstr(Path::new(placed.file_name().unwrap_or_default()))?;
    let at = At {
        dir: dir_fd,
        name: &name,
    };
    if let Ok(st) = stat_at(at)
        && st.st_mode & libc::S_IFMT == libc::S_IFREG
    {
        return Ok(FileId::from_stat(&st));
    }
    // SAFETY: name is NUL-terminated, dir_fd open.
    unsafe { libc::unlinkat(dir_fd, name.as_ptr(), 0) };
    Err(invalid("output changed while being placed"))
}

/// Like [`place`], for the file `rel` beneath `root` (a work dir), found
/// without following any symlink: every directory on the way is opened
/// with `O_NOFOLLOW` from the one before, the leaf must be a regular file,
/// and the move is relative to those fds.
pub fn place_beneath(
    root: &Path,
    rel: &Path,
    dir: &Path,
    stem: &str,
    ext: &str,
    suffix: Option<&str>,
) -> io::Result<(PathBuf, FileId)> {
    let (parent, leaf) = resolve_beneath(root, rel)?;
    let dir_fd = open_path(libc::AT_FDCWD, &cstr(dir)?, 0)?;
    let from = At {
        dir: parent.0,
        name: &leaf,
    };
    let placed = place_bytes(
        from,
        dir,
        Some(dir_fd.0),
        stem.as_bytes(),
        ext.as_bytes(),
        suffix.unwrap_or("").as_bytes(),
    )?;
    let id = check_placed(dir_fd.0, &placed)?;
    Ok((placed, id))
}

/// Like [`place_many`], for files beneath `root`, resolved as in
/// [`place_beneath`]. One file at a time is resolved, checked and moved,
/// so at most a few fds are open whatever the number of outputs.
pub fn place_many_beneath(
    root: &Path,
    rels: &[PathBuf],
    dir: &Path,
    stem: &str,
) -> io::Result<ManyPlaced> {
    place_many_with(rels.len(), dir, stem, true, |i, folder, folder_fd| {
        let folder_fd = folder_fd.expect("opened with_fd");
        let (parent, leaf) = resolve_beneath(root, &rels[i])?;
        let (s, e) = split_name(OsStr::from_bytes(leaf.as_bytes()));
        let from = At {
            dir: parent.0,
            name: &leaf,
        };
        let placed = place_bytes(from, folder, Some(folder_fd), s, e, b"")?;
        let id = check_placed(folder_fd, &placed)?;
        Ok((placed, id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xdg::tests::with_env;
    use std::sync::atomic::{AtomicU32, Ordering};

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
    fn places_next_to_source() {
        let t = Tmp::new("next");
        let src = file(&t.0, "tmp.bin", "x");
        let out = place(&src, &t.0, "a", "md", None).unwrap();
        assert_eq!(out, t.0.join("a.md"));
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(out).unwrap(), "x");
    }

    #[test]
    fn conflict_numbering() {
        let t = Tmp::new("num");
        file(&t.0, "a.md", "orig");
        let s1 = file(&t.0, "p1", "one");
        let s2 = file(&t.0, "p2", "two");
        assert_eq!(
            place(&s1, &t.0, "a", "md", None).unwrap(),
            t.0.join("a (1).md")
        );
        assert_eq!(
            place(&s2, &t.0, "a", "md", None).unwrap(),
            t.0.join("a (2).md")
        );
        assert_eq!(fs::read_to_string(t.0.join("a.md")).unwrap(), "orig");
        assert_eq!(fs::read_to_string(t.0.join("a (1).md")).unwrap(), "one");
    }

    #[test]
    fn fails_after_999() {
        let t = Tmp::new("max");
        file(&t.0, "a.md", "o");
        for n in 1..=999 {
            file(&t.0, &format!("a ({n}).md"), "o");
        }
        let s = file(&t.0, "p", "new");
        assert!(place(&s, &t.0, "a", "md", None).is_err());
        assert!(s.exists());
        assert_eq!(fs::read_to_string(t.0.join("a (999).md")).unwrap(), "o");
    }

    #[test]
    fn never_overwrites_under_race() {
        for round in 0..50 {
            let t = Tmp::new("race");
            let src = file(&t.0, "p", "mine");
            let dir = t.0.clone();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (s2, d2) = (stop.clone(), dir.clone());
            // Creates the first names as fast as it can, racing the move.
            let h = std::thread::spawn(move || {
                let mut n = 0;
                while !s2.load(Ordering::Relaxed) && n < 3 {
                    let name = if n == 0 {
                        "a.md".into()
                    } else {
                        format!("a ({n}).md")
                    };
                    let _ = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(d2.join(name))
                        .map(|mut f| std::io::Write::write_all(&mut f, b"theirs"));
                    n += 1;
                }
            });
            let out = place(&src, &dir, "a", "md", None).unwrap();
            stop.store(true, Ordering::Relaxed);
            h.join().unwrap();
            assert_eq!(fs::read_to_string(&out).unwrap(), "mine", "round {round}");
            for n in ["a.md", "a (1).md", "a (2).md"] {
                let p = dir.join(n);
                if p.exists() && p != out {
                    assert_eq!(fs::read_to_string(p).unwrap(), "theirs");
                }
            }
        }
    }

    #[test]
    fn taken_name_in_the_middle_of_a_move_is_skipped() {
        // Deterministic version of the race: the free-name "check" a naive
        // implementation would do is stale; the move itself must refuse.
        let t = Tmp::new("stale");
        let src = file(&t.0, "p", "mine");
        let target = t.0.join("a.md");
        file(&t.0, "a.md", "theirs");
        let (f, t2) = (cstr(&src).unwrap(), cstr(&target).unwrap());
        let cwd = |name| At {
            dir: libc::AT_FDCWD,
            name,
        };
        let e = move_noreplace(cwd(&f), cwd(&t2)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(target).unwrap(), "theirs");
        assert!(src.exists());
    }

    #[test]
    fn link_then_unlink_moves_to_free_target() {
        let t = Tmp::new("link-free");
        let src = file(&t.0, "p", "mine");
        let to = t.0.join("a.md");
        link_then_unlink(&src, &to).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(to).unwrap(), "mine");
    }

    #[test]
    fn link_then_unlink_never_overwrites() {
        let t = Tmp::new("link-taken");
        let src = file(&t.0, "p", "mine");
        let taken = file(&t.0, "a.md", "theirs");
        let e = link_then_unlink(&src, &taken).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(taken).unwrap(), "theirs");
        assert_eq!(fs::read_to_string(src).unwrap(), "mine");
    }

    #[test]
    fn link_then_unlink_rejects_directories() {
        let t = Tmp::new("link-dir");
        let d = t.0.join("d");
        fs::create_dir(&d).unwrap();
        assert!(link_then_unlink(&d, &t.0.join("e")).is_err());
        assert!(d.exists() && !t.0.join("e").exists());
    }

    #[test]
    fn invalid_names_rejected() {
        let t = Tmp::new("names");
        let s = file(&t.0, "p", "x");
        for (stem, ext, suf) in [
            ("", "md", ""),
            ("a/b", "md", ""),
            ("a\0b", "md", ""),
            (".", "md", ""),
            ("..", "md", ""),
            ("a", "m/d", ""),
            ("a", "..", ""),
            ("a", "md", "/x"),
        ] {
            let e = place(&s, &t.0, stem, ext, Some(suf)).unwrap_err();
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidInput,
                "{stem:?} {ext:?} {suf:?}"
            );
        }
        assert!(s.exists());
        assert!(place_many(std::slice::from_ref(&s), &t.0, "a/b").is_err());
        assert!(place_many(std::slice::from_ref(&s), &t.0, "").is_err());
        assert!(s.exists());
    }

    #[test]
    fn long_stem_truncated_to_name_max() {
        let t = Tmp::new("long");
        file(&t.0, &format!("{}.md", "a".repeat(250)), "o");
        let stem = "a".repeat(250);
        let s1 = file(&t.0, "p1", "1");
        let out = place(&s1, &t.0, &stem, "md", None).unwrap();
        assert!(out.file_name().unwrap().len() <= 255);
        assert_eq!(fs::read_to_string(&out).unwrap(), "1");
        // Multi-byte stem: cut must land on a character boundary.
        let wide = "é".repeat(130);
        let s2 = file(&t.0, "p2", "2");
        let out = place(&s2, &t.0, &wide, "md", None).unwrap();
        let name = out.file_name().unwrap().to_str().unwrap();
        assert!(name.len() <= 255 && name.ends_with(".md"));
    }

    #[test]
    fn place_many_empty_and_rollback() {
        let t = Tmp::new("rollback");
        assert!(place_many(&[], &t.0, "doc").unwrap().is_empty());
        assert!(!t.0.join("doc").exists());
        let a = file(&t.0, "a.png", "1");
        let missing = t.0.join("missing.png");
        assert!(place_many(&[a.clone(), missing], &t.0, "doc").is_err());
        assert!(!t.0.join("doc").exists(), "folder rolled back");
        assert!(!t.0.join("doc/a.png").exists());
        assert!(!a.exists(), "moved file was removed by the rollback");
    }

    #[test]
    fn place_many_keeps_non_utf8_names() {
        let t = Tmp::new("bytes");
        let name = OsString::from_vec(b"x\xff.png".to_vec());
        let a = t.0.join(&name);
        fs::write(&a, "1").unwrap();
        let out = place_many(&[a], &t.0, "doc").unwrap();
        assert_eq!(out[0], t.0.join("doc").join(&name));
        assert!(out[0].exists());
    }

    #[test]
    fn numbering_goes_before_last_extension() {
        let t = Tmp::new("ext");
        let a = file(&t.0, "x.tar.gz", "1");
        let b = file(&t.0, "y.tar.gz", "2");
        let out = place_many(&[a], &t.0, "d").unwrap();
        assert_eq!(out[0], t.0.join("d/x.tar.gz"));
        let out2 = place_many(&[b], &t.0, "d").unwrap();
        assert_eq!(out2[0], t.0.join("d (1)/y.tar.gz"));
        let c = file(&t.0, "z.tar.gz", "3");
        file(&t.0.join("d"), "z.tar.gz", "o");
        // Direct conflict inside an existing folder numbers before ".gz".
        let r = place(&c, &t.0.join("d"), "z.tar", "gz", None).unwrap();
        assert_eq!(r, t.0.join("d/z.tar (1).gz"));
    }

    #[test]
    fn suffix_used() {
        let t = Tmp::new("suffix");
        let s1 = file(&t.0, "p1", "1");
        let s2 = file(&t.0, "p2", "2");
        assert_eq!(
            place(&s1, &t.0, "pic", "png", Some("-small")).unwrap(),
            t.0.join("pic-small.png")
        );
        assert_eq!(
            place(&s2, &t.0, "pic", "png", Some("-small")).unwrap(),
            t.0.join("pic-small (1).png")
        );
    }

    #[test]
    fn many_outputs_into_folder_with_conflict() {
        let t = Tmp::new("many");
        fs::create_dir(t.0.join("doc")).unwrap();
        file(&t.0.join("doc"), "keep.txt", "k");
        let a = file(&t.0, "doc-01.png", "1");
        let b = file(&t.0, "doc-02.png", "2");
        let out = place_many(&[a, b], &t.0, "doc").unwrap();
        assert_eq!(
            out,
            vec![
                t.0.join("doc (1)/doc-01.png"),
                t.0.join("doc (1)/doc-02.png")
            ]
        );
        assert_eq!(fs::read_to_string(t.0.join("doc/keep.txt")).unwrap(), "k");
        assert_eq!(fs::read_to_string(&out[1]).unwrap(), "2");
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

    #[test]
    fn place_beneath_moves_regular_files() {
        let t = Tmp::new("beneath");
        let w = make_work_dir(&t.0).unwrap();
        let target = t.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::create_dir_all(w.path.join("0/sub")).unwrap();
        file(&w.path.join("0/sub"), "a.md", "a");
        file(&target, "a.md", "taken");
        let (out, id) =
            place_beneath(&w.path, Path::new("0/sub/a.md"), &target, "a", "md", None).unwrap();
        assert_eq!(out, target.join("a (1).md"));
        let st = stat_at(At {
            dir: libc::AT_FDCWD,
            name: &cstr(&out).unwrap(),
        })
        .unwrap();
        assert_eq!(id, FileId::from_stat(&st));
        assert_eq!(id.size, 1);
        assert_eq!(fs::read_to_string(&out).unwrap(), "a");
        assert_eq!(fs::read_to_string(target.join("a.md")).unwrap(), "taken");
        fs::create_dir(w.path.join("1")).unwrap();
        file(&w.path.join("1"), "p-1.png", "1");
        file(&w.path.join("1"), "p-2.png", "2");
        let rels = [PathBuf::from("1/p-1.png"), PathBuf::from("1/p-2.png")];
        let outs = place_many_beneath(&w.path, &rels, &target, "doc").unwrap();
        assert_eq!(outs.folder, target.join("doc"));
        let names: Vec<_> = outs.files.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            names,
            [target.join("doc/p-1.png"), target.join("doc/p-2.png")]
        );
        // One bad entry: what was moved is removed again, no folder is left.
        fs::create_dir(w.path.join("2")).unwrap();
        file(&w.path.join("2"), "ok.png", "1");
        let rels = [PathBuf::from("2/ok.png"), PathBuf::from("2/missing.png")];
        assert!(place_many_beneath(&w.path, &rels, &target, "two").is_err());
        assert!(!target.join("two").exists());
        for bad in ["", "/abs", "0/../0/sub/a.md", "./0", "0/sub"] {
            assert!(
                place_beneath(&w.path, Path::new(bad), &target, "x", "md", None).is_err(),
                "{bad}"
            );
        }
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
    fn place_many_beneath_needs_few_fds() {
        let t = Tmp::new("nofile");
        run_helper_with_nofile("output::tests::helper_place_many_beneath_2000", &t.0, 256);
        let n = fs::read_dir(t.0.join("target/doc")).unwrap().count();
        assert_eq!(n, 2000);
    }

    /// Child side of `place_many_beneath_needs_few_fds`; a no-op otherwise.
    #[test]
    fn helper_place_many_beneath_2000() {
        let Some(dir) = std::env::var_os("INACIRCLE_HELPER") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let w = make_work_dir(&dir).unwrap();
        fs::create_dir(w.path.join("0")).unwrap();
        let rels: Vec<PathBuf> = (0..2000)
            .map(|k| {
                let rel = PathBuf::from(format!("0/p-{k:04}.png"));
                fs::write(w.path.join(&rel), "x").unwrap();
                rel
            })
            .collect();
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();
        let placed = place_many_beneath(&w.path, &rels, &target, "doc").unwrap();
        assert_eq!(placed.files.len(), 2000);
    }

    #[test]
    fn place_beneath_never_follows_a_swapped_in_symlink() {
        let t = Tmp::new("swap");
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        file(&outside, "victim.md", "secret");
        let target = t.0.join("target");
        fs::create_dir(&target).unwrap();
        let w = make_work_dir(&t.0).unwrap();
        fs::create_dir(w.path.join("0")).unwrap();
        file(&w.path.join("0"), "victim.md", "fake");
        // Validated by path here; then a directory on the way is swapped.
        assert!(
            fs::symlink_metadata(w.path.join("0/victim.md"))
                .unwrap()
                .is_file()
        );
        fs::rename(w.path.join("0"), w.path.join("0x")).unwrap();
        std::os::unix::fs::symlink(&outside, w.path.join("0")).unwrap();
        let r = place_beneath(&w.path, Path::new("0/victim.md"), &target, "v", "md", None);
        assert!(r.is_err(), "followed the symlinked directory: {r:?}");
        let r = place_many_beneath(&w.path, &[PathBuf::from("0/victim.md")], &target, "v");
        assert!(r.is_err(), "place_many followed the symlinked directory");
        // The leaf itself swapped for a symlink.
        fs::create_dir(w.path.join("1")).unwrap();
        std::os::unix::fs::symlink(outside.join("victim.md"), w.path.join("1/l.md")).unwrap();
        assert!(place_beneath(&w.path, Path::new("1/l.md"), &target, "l", "md", None).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("victim.md")).unwrap(),
            "secret"
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0, "nothing placed");
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
    /// (which denies chmod), with `INACIRCLE_HELPER` set.
    fn run_helper_under_seccomp(name: &str, dir: &Path) {
        use std::os::unix::process::CommandExt;
        let limits = crate::sandbox::limits_for(0);
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("INACIRCLE_HELPER", dir)
            .stdin(std::process::Stdio::null());
        // SAFETY: apply_in_child only makes async-signal-safe calls; no
        // ruleset fd is passed.
        unsafe {
            cmd.pre_exec(move || crate::sandbox::apply_in_child(None, &limits));
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
}
