//! The few raw syscall helpers shared by the private state folders
//! (`journal`) and the output publication (`output`). One copy, so the
//! SAFETY reasoning is stated once.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

pub(crate) fn last_err<T>() -> io::Result<T> {
    Err(io::Error::last_os_error())
}

pub(crate) fn c_name(name: &[u8]) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a name"))
}

pub(crate) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is valid storage; fd is an open descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } == 0 {
        Ok(st)
    } else {
        last_err()
    }
}

/// `fstatat` without following a symlink at `name`.
pub(crate) fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is valid storage; name is NUL-terminated and dir
    // an open directory fd or AT_FDCWD.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(st)
    } else {
        last_err()
    }
}

/// `openat(dir, name, flags | O_NOFOLLOW | O_CLOEXEC, mode)`; EINTR is
/// retried.
pub(crate) fn open_at(
    dir: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    // mutants: equivalent: distinct flag bits, so `|` and `^` agree.
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    loop {
        // SAFETY: name is NUL-terminated; dir is an open dir fd or AT_FDCWD.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags, libc::c_uint::from(mode)) };
        if fd >= 0 {
            // SAFETY: fd was just opened and nothing else owns it.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// `open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)`, following symlinks in
/// `path`: for a base the user configured (a parent path, `$XDG_DATA_HOME`);
/// what is opened is checked by identity afterwards.
pub(crate) fn open_dir_follow(path: &[u8]) -> io::Result<OwnedFd> {
    let c = c_name(path)?;
    // SAFETY: c is NUL-terminated.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return last_err();
    }
    // SAFETY: fd was just opened and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `renameat2(from_dir, from, to_dir, to, RENAME_NOREPLACE)`: never
/// replaces what is at `to` (`EEXIST`).
pub(crate) fn rename_noreplace(
    from_dir: RawFd,
    from: &CStr,
    to_dir: RawFd,
    to: &CStr,
) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated, both fds open directories.
    let r = unsafe {
        libc::renameat2(
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 { Ok(()) } else { last_err() }
}

pub(crate) fn fsync(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd is an open descriptor.
    if unsafe { libc::fsync(fd) } == 0 {
        Ok(())
    } else {
        last_err()
    }
}

/// The names in the open directory `dir` (without `.` and `..`), read
/// through a duplicate of the descriptor, never by path. More than `max`
/// names is an error (`max` is the caller's bound, so a planted folder of
/// millions of entries costs at most `max + 1` reads).
pub(crate) fn read_dir(dir: RawFd, max: usize) -> io::Result<Vec<CString>> {
    // SAFETY: dir is an open fd; the duplicate is fdopendir's to own.
    let dup = unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return last_err();
    }
    // SAFETY: dup is an open directory fd; on success the DIR owns it.
    let d = unsafe { libc::fdopendir(dup) };
    if d.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so dup is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    // The stream starts where the shared offset is: rewind it.
    // SAFETY: d is a valid DIR.
    unsafe { libc::rewinddir(d) };
    let mut names = Vec::new();
    let r = loop {
        // SAFETY: errno is thread-local; cleared to tell the end from an
        // error after readdir.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: d is a valid DIR until closedir below.
        let ent = unsafe { libc::readdir(d) };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            break if e.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(e)
            };
        }
        // SAFETY: readdir returned a valid dirent whose d_name is
        // NUL-terminated; it is copied before the next readdir call.
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if names.len() == max {
            break Err(io::Error::other("more entries than expected"));
        }
        names.push(name.to_owned());
    };
    // SAFETY: d is a valid DIR, closed exactly once (this closes dup).
    unsafe { libc::closedir(d) };
    r.map(|()| names)
}

/// `(st_dev, st_ino)`; their widths differ between targets.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn ident(st: &libc::stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Makes a FIFO at `path`.
    pub(crate) fn mkfifo(path: &Path) {
        let c = super::c_name(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).unwrap();
        // SAFETY: c is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Runs `f` on a thread and fails the test if it does not return in
    /// `secs`: the check for "never blocks on a FIFO". (A blocked thread is
    /// left behind; the test process ends anyway.)
    pub(crate) fn within<R: Send + 'static>(
        secs: u64,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> R {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs))
            .expect("blocked: no result in time")
    }
}
