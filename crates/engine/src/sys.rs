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

/// `(st_dev, st_ino)`; their widths differ between targets.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn ident(st: &libc::stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}
