//! Undo into the freedesktop.org Trash (Trash Specification 1.0): a
//! published input group is moved there whole, by one no-replace rename,
//! and nothing published is ever unlinked, overwritten or copied across
//! filesystems.
//!
//! Per group (`undo_group`): the wrapper is opened (never through a
//! symlink at its own name) and checked against its private baseline, the
//! wrapper's identity and every child's, with the exact child count; one
//! changed, missing or extra child keeps the whole group. Then a Trash on
//! the same mount and device is chosen (the home Trash, else the volume's
//! `$topdir/.Trash/$uid` or `$topdir/.Trash-$uid`; each component opened
//! without following a symlink and checked; nothing is `chmod`ed), a free
//! name is picked, the recovery intent is written and synced, the
//! `.trashinfo` is created exclusively (`O_EXCL`), written, read back and
//! synced, and the wrapper is moved with `renameat2(RENAME_NOREPLACE)`:
//! the linearisation point. After the move the group and our info are
//! checked again; on any difference the object is moved back (no-replace),
//! and if its way back is taken it stays in the Trash with its place and
//! reason. Our `.trashinfo` is authentic only while it names our moved
//! group: on every other ending it is closed (neutralised through its fd,
//! then renamed off its `.trashinfo` name after a device/inode check, the
//! rename running even when the neutralisation failed).
//!
//! A place is reported only when proved at its check point: the known path
//! still reaches the object, or the object's fd resolves (`/proc/self/fd`,
//! then `openat2(RESOLVE_NO_SYMLINKS)` and an identity check, at most
//! [`RESOLVE_ATTEMPTS`] times) to a symlink-free path. Otherwise the place
//! is `None` with an anchor, and the bytes are kept. Later renames by other
//! programs are not followed: the object has no stable name once another
//! program moves it, and chasing it would risk acting on the wrong file.

use crate::output::{
    AnchorHint, CheckPoint, ChildBaseline, DirectoryId, Durability, FileId, GroupId,
    LocationEvidence, NodeKind, OperationId,
};
use crate::sys::{
    c_name, fstat, fsync, ident, last_err, open_at, open_dir_follow, read_dir, rename_noreplace,
    stat_at,
};
use crate::xdg;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt};
use std::path::{Path, PathBuf};

/// Trash names tried per group (`<leaf>`, `<leaf>.2` … `<leaf>.64`): a
/// finite budget against earlier trashed items of the same name, not a
/// probability estimate. Past it the group is kept (`NameTaken`). 64, not 8, because with 8 the ninth undo
/// of an output with the same name would be refused; 63 taken names plus
/// the free one cost ~18 us (measured: 17.7 us
/// median on tmpfs, 18.1 us on btrfs), far under the action-start budget.
pub const TRASH_NAME_ATTEMPTS: u32 = 64;
/// The largest `.trashinfo` written or read back: a 4096-byte path
/// percent-encoded is at most 12288 B, plus 52 B of header and date
/// (12340 B). Past it no info is written and the group is kept
/// (`TooLong`). See `docs/resource-limits.md`.
pub const TRASHINFO_MAX_BYTES: usize = 16384;
/// The largest recovery-intent record: two `%XX`-encoded paths of at most
/// 4096 raw bytes (12288 B each) plus at most 32 scalar fields of at most
/// 64 + 32 B (28672 B in all). Past it nothing is written or moved
/// (`TooLong`). See `docs/resource-limits.md`.
pub const RECOVERY_INTENT_MAX_BYTES: usize = 32768;
/// fd→path attempts: each is one `readlink` and one `openat2`. A renamer racing every
/// attempt wins; the place is then `None` (bytes kept, anchor reported),
/// never guessed. Four tolerates a few ordinary renames during the undo.
pub const RESOLVE_ATTEMPTS: u32 = 4;
/// `PATH_MAX`: a `readlink` result that fills the buffer is cut off.
const PATH_MAX: usize = 4096;
const INFO_EXT: &[u8] = b".trashinfo";
/// `<base>.trashinfo` must fit `NAME_MAX` (255 B).
const BASE_MAX: usize = 255 - INFO_EXT.len();
/// A mountinfo line holds two escaped paths (`\ooo`: at most 4 x 4096 B
/// each), the options and the source: 64 KiB covers it; a longer line is
/// not read and the volume Trash is refused (the group is kept).
const MOUNTINFO_LINE_MAX: u64 = 64 * 1024;
/// Mountinfo lines read before giving up on the volume Trash: hosts with
/// many containers have thousands of mounts; 100 000 bounds the scan of a
/// pathological table (refusal, the group kept).
const MOUNTINFO_LINES_MAX: usize = 100_000;

/// Why a group ended as it did. The errno is that of the failed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoReason {
    /// The wrapper or a child changed, went missing or gained a sibling
    /// since publication (the whole group is kept).
    Changed,
    /// The group's folder is not at its recorded place.
    Missing,
    /// A place could not be proved: an ancestor was renamed.
    AncestorChanged,
    /// No Trash on the same filesystem passed its checks.
    NoTrash,
    /// Every Trash name tried was taken.
    NameTaken,
    /// The path is too long for the Trash metadata or the history.
    TooLong,
    /// The recovery intent could not be written.
    HistoryNotWritten(i32),
    /// The `.trashinfo` could not be written.
    InfoNotWritten(i32),
    /// The move into the Trash failed (`EXDEV`, `EACCES`, `EEXIST`, …).
    MoveRefused(i32),
    /// After the move, the object was not the published group.
    PostMoveMismatch,
    /// After the move, our `.trashinfo` name held another file.
    InfoChanged,
    /// The way back was taken by a new object.
    RestoreCollision,
    /// The way back failed otherwise.
    RestoreFailed,
    /// Moved, but the history could not record it.
    CommitNotRecorded,
    /// Moved, but a sync failed.
    NotDurable,
    /// Our `.trashinfo` could not be neutralised (its errno).
    ContentNotNeutralized(i32),
    /// An earlier undo of this job was interrupted before this group moved.
    Interrupted,
}

impl UndoReason {
    /// The record name.
    pub fn name(self) -> &'static str {
        use UndoReason::*;
        match self {
            Changed => "Changed",
            Missing => "Missing",
            AncestorChanged => "AncestorChanged",
            NoTrash => "NoTrash",
            NameTaken => "NameTaken",
            TooLong => "TooLong",
            HistoryNotWritten(_) => "HistoryNotWritten",
            InfoNotWritten(_) => "InfoNotWritten",
            MoveRefused(_) => "MoveRefused",
            PostMoveMismatch => "PostMoveMismatch",
            InfoChanged => "InfoChanged",
            RestoreCollision => "RestoreCollision",
            RestoreFailed => "RestoreFailed",
            CommitNotRecorded => "CommitNotRecorded",
            NotDurable => "NotDurable",
            ContentNotNeutralized(_) => "ContentNotNeutralized",
            Interrupted => "Interrupted",
        }
    }

    /// The errno it carries, if any.
    pub fn errno(self) -> Option<i32> {
        use UndoReason::*;
        match self {
            HistoryNotWritten(e)
            | InfoNotWritten(e)
            | MoveRefused(e)
            | ContentNotNeutralized(e) => Some(e),
            _ => None,
        }
    }

    /// The inverse of [`UndoReason::name`] and [`UndoReason::errno`].
    pub fn parse(name: &str, errno: Option<i32>) -> Option<UndoReason> {
        use UndoReason::*;
        let unit = match name {
            "Changed" => Changed,
            "Missing" => Missing,
            "AncestorChanged" => AncestorChanged,
            "NoTrash" => NoTrash,
            "NameTaken" => NameTaken,
            "TooLong" => TooLong,
            "PostMoveMismatch" => PostMoveMismatch,
            "InfoChanged" => InfoChanged,
            "RestoreCollision" => RestoreCollision,
            "RestoreFailed" => RestoreFailed,
            "CommitNotRecorded" => CommitNotRecorded,
            "NotDurable" => NotDurable,
            "Interrupted" => Interrupted,
            _ => {
                let e = errno?;
                return Some(match name {
                    "HistoryNotWritten" => HistoryNotWritten(e),
                    "InfoNotWritten" => InfoNotWritten(e),
                    "MoveRefused" => MoveRefused(e),
                    "ContentNotNeutralized" => ContentNotNeutralized(e),
                    _ => return None,
                });
            }
        };
        errno.is_none().then_some(unit)
    }
}

/// How one input group's undo ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupOutcome {
    /// Provably not moved: it is where it was.
    Kept {
        group: GroupId,
        location: LocationEvidence,
        reason: UndoReason,
    },
    /// Moved into the Trash, checked, and the move recorded.
    MovedToTrash {
        group: GroupId,
        location: LocationEvidence,
        info_leaf: OsString,
        durability: Durability,
    },
    /// Moved, then moved back (no-replace) because of `reason`.
    Restored {
        group: GroupId,
        location: LocationEvidence,
        reason: UndoReason,
        durability: Durability,
    },
    /// Kept whole at `location`, but the usual ending did not complete.
    RecoveryKept {
        group: GroupId,
        location: LocationEvidence,
        reason: UndoReason,
        errno: Option<i32>,
        durability: Durability,
    },
    /// A place or the metadata could not be proved; nothing was deleted.
    Uncertain {
        group: GroupId,
        op: OperationId,
        locations: Vec<LocationEvidence>,
        reason: UndoReason,
    },
}

/// An undo's per-group outcomes and its warnings (left-over metadata, a
/// history that could not record the undo).
#[derive(Debug, Default)]
pub struct UndoResult {
    pub groups: Vec<GroupOutcome>,
    pub warnings: Vec<String>,
}

/// One committed group as the history holds it.
pub(crate) struct Group<'a> {
    pub group: GroupId,
    /// The committed place of the wrapper.
    pub place: &'a [u8],
    pub wrapper: DirectoryId,
    pub children: &'a [ChildBaseline],
}

/// The recovery intent, written and synced before the `.trashinfo` and
/// the move. `info_rel` is where `Path=` starts in `source` (0 for the home
/// Trash, else past the volume's topdir).
pub(crate) struct Intent<'a> {
    pub op: OperationId,
    pub group: GroupId,
    pub source: &'a [u8],
    pub target: &'a [u8],
    pub info_rel: usize,
    pub source_parent: (u64, u64),
    pub target_parent: (u64, u64),
    pub wrapper: (u64, u64),
    pub mount: u64,
    pub date: &'a str,
}

/// The history, as the undo writes it.
pub(crate) trait Log {
    /// The intent; an error means nothing may move.
    fn intent(&mut self, i: &Intent) -> io::Result<()>;
    /// The closing record of `op`.
    fn close(&mut self, op: OperationId, o: &GroupOutcome) -> io::Result<()>;
}

/// Points where the tests inject a fault (an errno) or stage a race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    /// Before the intent is written.
    Intent,
    /// Before the `.trashinfo` bytes are written (an errno: a short write).
    InfoWrite,
    InfoSync,
    /// After the info is synced, before the move.
    BeforeMove,
    /// The move itself (an errno replaces it).
    Rename,
    /// Right after the move, before the postcheck.
    AfterMove,
    /// The move back (an errno replaces it).
    Restore,
    /// After a failed move back, before the place is proved.
    AfterRestoreFail,
    /// Between the `readlink` and the `openat2` of each resolve attempt.
    Resolve,
    NeutralWrite,
    NeutralSync,
    /// After the info's identity check, before its detaching rename (an
    /// errno replaces the rename).
    Detach,
    /// Before a foreign file caught at our detached name is put back (an
    /// errno replaces the rename).
    PutBack,
    /// The parents' syncs after the move.
    Sync,
    /// Before the home Trash is looked at (an `EXDEV` makes the home Trash
    /// another filesystem's, so the volume Trash is used).
    Home,
    /// Before the closing record of a moved group.
    Commit,
}

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

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn path_of(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b))
}

// ---- the .trashinfo ----

/// `[Trash Info]`, `Path=` (the bytes percent-encoded: everything but the
/// unreserved set and `/`), `DeletionDate=`. Longer than
/// [`TRASHINFO_MAX_BYTES`] is `TooLong`.
pub(crate) fn trashinfo(path: &[u8], date: &str) -> Option<Vec<u8>> {
    let mut s = Vec::with_capacity(52 + path.len());
    s.extend_from_slice(b"[Trash Info]\nPath=");
    for &b in path {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b);
        } else {
            s.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
    s.extend_from_slice(b"\nDeletionDate=");
    s.extend_from_slice(date.as_bytes());
    s.push(b'\n');
    (s.len() <= TRASHINFO_MAX_BYTES).then_some(s)
}

/// Local time as `YYYY-MM-DDThh:mm:ss` (the spec's `DeletionDate`).
fn local_date() -> String {
    let mut out = [0u8; 32];
    // SAFETY: time accepts NULL; tm is valid storage for localtime_r; the
    // format is NUL-terminated and strftime writes at most out.len() bytes.
    let n = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "1970-01-01T00:00:00".to_owned();
        }
        libc::strftime(
            out.as_mut_ptr().cast(),
            out.len(),
            c"%Y-%m-%dT%H:%M:%S".as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&out[..n]).into_owned()
}

/// The `k`th name for `leaf`: `leaf`, then `leaf.<k+1>`, the leaf cut (on
/// a UTF-8 boundary) so `<name>.trashinfo` fits `NAME_MAX`.
pub(crate) fn candidate(leaf: &[u8], k: u32) -> Option<CString> {
    let tail = if k == 0 {
        String::new()
    } else {
        format!(".{}", k + 1)
    };
    let mut cut = leaf.len().min(BASE_MAX - tail.len());
    while cut > 0 && cut < leaf.len() && leaf[cut] & 0xC0 == 0x80 {
        cut -= 1;
    }
    if cut == 0 {
        return None;
    }
    let mut name = leaf[..cut].to_vec();
    name.extend_from_slice(tail.as_bytes());
    CString::new(name).ok()
}

fn info_name(base: &CStr) -> CString {
    let mut n = base.to_bytes().to_vec();
    n.extend_from_slice(INFO_EXT);
    CString::new(n).expect("no NUL in a name")
}

/// Our detached info's name: `.<op>.inacircle-detached` (84 B at most): in
/// the info folder, without the `.trashinfo` ending, so no `files/` entry
/// pairs with it.
fn detached_name(op: OperationId) -> CString {
    CString::new(format!(".{:016x}.inacircle-detached", op.0)).expect("no NUL")
}

/// What a closed info of ours holds: not a `.trashinfo`.
fn neutral(op: OperationId) -> Vec<u8> {
    format!("inacircle-detached-trashinfo op={:016x}\n", op.0).into_bytes()
}

/// Whether `b` is what our info holds while being neutralised: the
/// neutral text written over the first `k` bytes of the authentic one
/// (`pwrite` cut short, or whole but before the `ftruncate`). Such a file
/// is still ours: it never pairs with data and is never put back.
fn partly_neutral(b: &[u8], ours: &[u8], op: OperationId) -> bool {
    let n = neutral(op);
    (1..=n.len()).any(|k| {
        let tail = ours.get(k..).unwrap_or_default();
        b.len() == k + tail.len() && b[..k] == n[..k] && b[k..] == *tail
    })
}

/// The first of [`TRASH_NAME_ATTEMPTS`] names free in both `files` and
/// `info`.
pub(crate) fn choose_name(files: RawFd, info: RawFd, leaf: &[u8]) -> Option<CString> {
    let free =
        |dir, n: &CStr| matches!(stat_at(dir, n), Err(e) if e.kind() == io::ErrorKind::NotFound);
    (0..TRASH_NAME_ATTEMPTS)
        .filter_map(|k| candidate(leaf, k))
        .find(|b| free(files, b) && free(info, &info_name(b)))
}

/// Our reserved `.trashinfo`, held by fd for the whole operation.
pub(crate) struct Info {
    fd: OwnedFd,
    id: (u64, u64),
    name: CString,
    detached: CString,
    op: OperationId,
}

fn pwrite_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut off = 0;
    while off < bytes.len() {
        // SAFETY: the slice is valid for its length; fd is open.
        let n = unsafe {
            libc::pwrite(
                fd,
                bytes[off..].as_ptr().cast(),
                bytes.len() - off,
                off as libc::off_t,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
        }
        off += n as usize;
    }
    Ok(())
}

/// `openat(info, name, O_CREAT | O_EXCL | O_NOFOLLOW, 0600)`, the whole
/// content written, read back through the same fd and synced, then the
/// folder synced. An error after the inode exists hands it back for
/// closing.
pub(crate) fn reserve(
    dir: RawFd,
    name: CString,
    op: OperationId,
    bytes: &[u8],
) -> Result<Info, (i32, Option<Info>)> {
    let fd = open_at(
        dir,
        &name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )
    .map_err(|e| (errno(&e), None))?;
    let info = match fstat(fd.as_raw_fd()) {
        Ok(st) => Info {
            id: ident(&st),
            fd,
            name,
            detached: detached_name(op),
            op,
        },
        // Ours but unidentifiable: never touched again by name.
        Err(e) => return Err((errno(&e), None)),
    };
    let raw = info.fd.as_raw_fd();
    let written = (|| {
        if let Err(e) = at(Point::InfoWrite) {
            // The injected fault is a short write: half the bytes landed.
            let _ = pwrite_all(raw, &bytes[..bytes.len() / 2]);
            return Err(e);
        }
        pwrite_all(raw, bytes)?;
        let f = fs::File::from(info.fd.try_clone()?);
        let mut back = vec![0u8; bytes.len()];
        f.read_exact_at(&mut back, 0)?;
        if back != bytes || f.metadata()?.len() != bytes.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "read back differs",
            ));
        }
        at(Point::InfoSync)?;
        fsync(raw)?;
        fsync(dir)
    })();
    match written {
        Ok(()) => Ok(info),
        Err(e) => Err((errno(&e), Some(info))),
    }
}

/// What happened to our info's name when it was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InfoState {
    /// Renamed to the detached name, checked to be ours.
    Detached,
    /// The `.trashinfo` name was not ours: left alone (our inode was still
    /// neutralised through its fd).
    ForeignUntouched,
    /// A foreign file took the name between the check and the rename; it
    /// was put back.
    ForeignRestored,
    /// As above, but its name was taken again: it stays at the detached
    /// name (errno of the failed put-back).
    ForeignKeptDetached(i32),
    /// The detaching rename failed: our inode stays under the
    /// `.trashinfo` name.
    NeutralizedInPlace(i32),
}

pub(crate) struct Closure {
    pub state: InfoState,
    /// The neutralisation's errno, when it failed.
    pub neutral: Option<i32>,
}

/// Step 1 of closing: our inode's content replaced through its fd.
fn neutralise(info: &Info) -> Result<(), i32> {
    let n = neutral(info.op);
    let raw = info.fd.as_raw_fd();
    at(Point::NeutralWrite).map_err(|e| errno(&e))?;
    pwrite_all(raw, &n).map_err(|e| errno(&e))?;
    // SAFETY: raw is our open, writable file.
    if unsafe { libc::ftruncate(raw, n.len() as libc::off_t) } != 0 {
        return Err(errno(&io::Error::last_os_error()));
    }
    at(Point::NeutralSync).map_err(|e| errno(&e))?;
    fsync(raw).map_err(|e| errno(&e))
}

/// Closes our info: neutralised through its fd, then (even when that
/// failed) renamed off its `.trashinfo` name if the name is still our
/// inode, checked after the rename, a foreign file put back. Nothing is
/// unlinked.
pub(crate) fn close_info(dir: RawFd, info: &Info) -> Closure {
    let neutral = neutralise(info).err();
    let ours = stat_at(dir, &info.name).is_ok_and(|st| ident(&st) == info.id);
    let state = if !ours {
        InfoState::ForeignUntouched
    } else {
        let moved =
            at(Point::Detach).and_then(|()| rename_noreplace(dir, &info.name, dir, &info.detached));
        match moved {
            Err(e) if e.kind() == io::ErrorKind::NotFound => InfoState::ForeignUntouched,
            Err(e) => InfoState::NeutralizedInPlace(errno(&e)),
            Ok(()) if stat_at(dir, &info.detached).is_ok_and(|st| ident(&st) == info.id) => {
                InfoState::Detached
            }
            Ok(()) => match at(Point::PutBack)
                .and_then(|()| rename_noreplace(dir, &info.detached, dir, &info.name))
            {
                Ok(()) => InfoState::ForeignRestored,
                Err(e) => InfoState::ForeignKeptDetached(errno(&e)),
            },
        }
    };
    let _ = fsync(dir);
    Closure { state, neutral }
}

// ---- places ----

/// A path proved to reach an object, or why not.
#[derive(Debug)]
pub(crate) enum Resolved {
    At(PathBuf),
    Unverified { last: Option<PathBuf> },
}

/// `openat2(path, flags, RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`.
fn open_no_symlinks(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: open_how is plain data; all-zero is valid, then set.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
    // SAFETY: path and how live through the call; the size is how's own.
    let r = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if r < 0 {
        return last_err();
    }
    // SAFETY: r is a new fd nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(r as RawFd) })
}

/// The bounded fd→path resolution: `readlink("/proc/self/fd/N")` into a
/// `PATH_MAX` buffer (a full buffer is cut off), then the path reopened
/// with no symlink at all and its identity compared, at most
/// [`RESOLVE_ATTEMPTS`] times. Proves the path at that moment only.
pub(crate) fn resolve(fd: RawFd) -> Resolved {
    let Ok(want) = fstat(fd).map(|st| ident(&st)) else {
        return Resolved::Unverified { last: None };
    };
    let Ok(link) = CString::new(format!("/proc/self/fd/{fd}")) else {
        return Resolved::Unverified { last: None };
    };
    let mut last = None;
    for _ in 0..RESOLVE_ATTEMPTS {
        let mut buf = [0u8; PATH_MAX];
        // SAFETY: link is NUL-terminated; readlink writes at most buf.len().
        let n = unsafe { libc::readlink(link.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 || n as usize >= PATH_MAX {
            return Resolved::Unverified { last };
        }
        let p = &buf[..n as usize];
        if !p.starts_with(b"/") || p.ends_with(b" (deleted)") {
            return Resolved::Unverified {
                last: Some(path_of(p)),
            };
        }
        let reopened = at(Point::Resolve)
            .and_then(|()| c_name(p))
            .and_then(|c| open_no_symlinks(&c, libc::O_PATH));
        if let Ok(g) = reopened
            && fstat(g.as_raw_fd()).is_ok_and(|st| ident(&st) == want)
        {
            return Resolved::At(path_of(p));
        }
        last = Some(path_of(p));
    }
    Resolved::Unverified { last }
}

/// Where `leaf` in `dir` (the object `obj`) is, checked at `cp`: the known
/// folder path while it still reaches `dir` and `leaf` there is `obj`,
/// else the resolved path of `obj` (`PostResolve`), else `None` with the
/// anchor and the last path seen.
pub(crate) fn locate(
    known: Option<&Path>,
    dir: RawFd,
    leaf: &CStr,
    obj: RawFd,
    op: OperationId,
    cp: CheckPoint,
) -> LocationEvidence {
    let p = fstat(dir).map(|st| ident(&st)).unwrap_or((0, 0));
    let o = fstat(obj).map(|st| ident(&st)).unwrap_or((0, 0));
    let anchor = AnchorHint {
        parent_dev: p.0,
        parent_ino: p.1,
        object_dev: o.0,
        object_ino: o.1,
        leaf: OsStr::from_bytes(leaf.to_bytes()).to_owned(),
        op,
    };
    let leaf = OsStr::from_bytes(leaf.to_bytes());
    if let Some(k) = known {
        let same_dir = c_name(k.as_os_str().as_bytes()).is_ok_and(|c| {
            // SAFETY: zeroed stat is valid storage; c is NUL-terminated.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            (unsafe { libc::stat(c.as_ptr(), &mut st) }) == 0 && ident(&st) == p
        });
        let same_obj = c_name(leaf.as_bytes())
            .and_then(|l| stat_at(dir, &l))
            .is_ok_and(|st| ident(&st) == o);
        if same_dir && same_obj {
            return LocationEvidence {
                path_at_check: Some(k.join(leaf)),
                last_unverified_path: None,
                anchor,
                checked_at: cp,
            };
        }
    }
    match resolve(obj) {
        Resolved::At(path) => LocationEvidence {
            path_at_check: Some(path),
            last_unverified_path: None,
            anchor,
            checked_at: CheckPoint::PostResolve,
        },
        Resolved::Unverified { last } => LocationEvidence {
            path_at_check: None,
            last_unverified_path: last.or_else(|| known.map(|k| k.join(leaf))),
            anchor,
            checked_at: CheckPoint::PostResolve,
        },
    }
}

/// No place known: only the recorded path, as a hint.
fn unknown_place(place: &Path, op: OperationId, cp: CheckPoint) -> LocationEvidence {
    LocationEvidence {
        path_at_check: None,
        last_unverified_path: Some(place.to_owned()),
        anchor: AnchorHint {
            parent_dev: 0,
            parent_ino: 0,
            object_dev: 0,
            object_ino: 0,
            leaf: place.file_name().unwrap_or_default().to_owned(),
            op,
        },
        checked_at: cp,
    }
}

// ---- choosing a Trash ----

/// `(mount id, st_dev)`: a rename stays within one mount and one device
/// (a Btrfs subvolume has its own device under one mount).
fn domain(fd: RawFd) -> io::Result<(u64, u64)> {
    // SAFETY: statx is plain data; all-zero is valid storage.
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: the empty path with AT_EMPTY_PATH names fd itself; stx is
    // valid storage. The raw syscall needs no libc wrapper (musl).
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut stx as *mut libc::statx,
        )
    };
    if r != 0 {
        return last_err();
    }
    if stx.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no mount id (kernel before 5.8)",
        ));
    }
    Ok((stx.stx_mnt_id, ident(&fstat(fd)?).0))
}

/// Filesystems whose rename and ownership semantics are another host's:
/// NFS, SMB/CIFS (both magics), AFS, Ceph, Coda, FUSE. A group there is
/// kept rather than trashed under unknown rules.
const REMOTE_FS: &[u32] = &[
    0x6969,
    0x517B,
    0xFF53_4D42,
    0xFE53_4D42,
    0x5346_414F,
    0x00C3_6400,
    0x7375_7245,
    0x6573_5546,
];

fn local_fs(fd: RawFd) -> bool {
    // SAFETY: statfs is plain data; all-zero is valid storage.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: fd is open; st is valid storage.
    if unsafe { libc::fstatfs(fd, &mut st) } != 0 {
        return false;
    }
    // f_type's width differs between targets; magics fit 32 bits.
    #[allow(clippy::unnecessary_cast)]
    let magic = st.f_type as u32;
    !remote_magic(magic)
}

/// Whether a `statfs` magic is one of [`REMOTE_FS`] (apart from
/// `local_fs` so the table can be tested without a remote mount).
fn remote_magic(magic: u32) -> bool {
    REMOTE_FS.contains(&magic)
}

/// A Trash's `files` and `info` folders, with the paths they were opened
/// by (for the reported places).
pub(crate) struct TrashDir {
    files: OwnedFd,
    info: OwnedFd,
    files_path: PathBuf,
    info_path: PathBuf,
    /// The volume's topdir (`Path=` is relative to it), or `None` for the
    /// home Trash (`Path=` absolute).
    top: Option<PathBuf>,
}

/// A private Trash folder: a directory of this user with mode 0700, never
/// a symlink. A wrong one is refused, never `chmod`ed or `chown`ed.
pub(crate) fn private_ok(st: &libc::stat, uid: u32) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR && st.st_uid == uid && st.st_mode & 0o7777 == 0o700
}

/// Opens (making it if missing, mode 0700) and checks `name` in `at`.
fn private_dir(at: RawFd, name: &CStr, uid: u32) -> io::Result<OwnedFd> {
    // SAFETY: name is NUL-terminated; at is an open directory.
    if unsafe { libc::mkdirat(at, name.as_ptr(), 0o700) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    let fd = open_at(at, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    if !private_ok(&fstat(fd.as_raw_fd())?, uid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a Trash folder that is not private",
        ));
    }
    Ok(fd)
}

/// `files` and `info` inside the Trash folder `root`.
fn trash_in(root: RawFd, path: &Path, uid: u32, top: Option<&Path>) -> io::Result<TrashDir> {
    Ok(TrashDir {
        files: private_dir(root, c"files", uid)?,
        info: private_dir(root, c"info", uid)?,
        files_path: path.join("files"),
        info_path: path.join("info"),
        top: top.map(Path::to_owned),
    })
}

/// The home Trash, `$XDG_DATA_HOME/Trash`, if on the same mount and
/// device as the source (`dom`). `$XDG_DATA_HOME` itself is made if
/// missing (as file managers do) and followed if a symlink (the user's
/// own setting); `Trash`, `files` and `info` never are.
fn home_trash(dom: (u64, u64)) -> io::Result<TrashDir> {
    at(Point::Home)?;
    let data = xdg::data_home()?;
    #[cfg(test)]
    if !under_test_root(&data) {
        return Err(io::Error::other(NOT_A_TEST_TRASH));
    }
    // Its nearest existing ancestor tells the domain before anything is
    // made: a home on another filesystem gets no new folders.
    let mut probe = data.as_path();
    let base = loop {
        match open_dir_follow(probe.as_os_str().as_bytes()) {
            Ok(fd) => break fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                probe = probe.parent().ok_or(e)?;
            }
            Err(e) => return Err(e),
        }
    };
    if domain(base.as_raw_fd())? != dom {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data)?;
    let d = open_dir_follow(data.as_os_str().as_bytes())?;
    if domain(d.as_raw_fd())? != dom {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    let t = private_dir(d.as_raw_fd(), c"Trash", euid())?;
    trash_in(t.as_raw_fd(), &data.join("Trash"), euid(), None)
}

/// `\ooo` escapes of a mountinfo field undone.
fn unescape(f: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(f.len());
    let mut i = 0;
    while i < f.len() {
        if f[i] == b'\\'
            && i + 3 < f.len()
            && f[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = f[i + 1..i + 4]
                .iter()
                .fold(0u32, |a, c| a * 8 + u32::from(c - b'0'));
            out.push(v as u8);
            i += 4;
        } else {
            out.push(f[i]);
            i += 1;
        }
    }
    out
}

/// A mountinfo line's mount id and mount point.
pub(crate) fn mountinfo_line(line: &[u8]) -> Option<(u64, PathBuf)> {
    let mut f = line.split(|&b| b == b' ');
    let id = std::str::from_utf8(f.next()?).ok()?.parse().ok()?;
    let point = f.nth(3)?;
    Some((id, path_of(&unescape(point))))
}

/// The mount point of mount `mnt`, from `/proc/self/mountinfo` (bounded).
fn mount_point(mnt: u64) -> io::Result<PathBuf> {
    let f = fs::File::open("/proc/self/mountinfo")?;
    let mut rd = BufReader::new(f);
    let mut line = Vec::new();
    for _ in 0..MOUNTINFO_LINES_MAX {
        line.clear();
        let n = (&mut rd)
            .take(MOUNTINFO_LINE_MAX)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            return Err(io::Error::other("mountinfo line too long"));
        }
        line.pop();
        if let Some((id, point)) = mountinfo_line(&line)
            && id == mnt
        {
            return Ok(point);
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "mount not found"))
}

/// The volume Trash under `top`: `$topdir/.Trash/$uid` if `.Trash` is a
/// real directory with the sticky bit (never a symlink), else
/// `$topdir/.Trash-$uid`. Each private folder is checked as in
/// [`private_ok`].
pub(crate) fn volume_trash_at(top: RawFd, top_path: &Path, uid: u32) -> io::Result<TrashDir> {
    let shared = open_at(top, c".Trash", libc::O_RDONLY | libc::O_DIRECTORY, 0).ok();
    if let Some(s) = shared
        && fstat(s.as_raw_fd()).is_ok_and(|st| {
            // Sticky, and made by the administrator (or by us): another
            // user's `.Trash` could rename our `$uid` folder away.
            st.st_mode & libc::S_IFMT == libc::S_IFDIR
                && st.st_mode & libc::S_ISVTX != 0
                && (st.st_uid == 0 || st.st_uid == uid)
        })
    {
        let u = CString::new(uid.to_string()).expect("digits");
        let path = top_path.join(".Trash").join(uid.to_string());
        if let Ok(t) = private_dir(s.as_raw_fd(), &u, uid)
            .and_then(|d| trash_in(d.as_raw_fd(), &path, uid, Some(top_path)))
        {
            return Ok(t);
        }
    }
    let name = format!(".Trash-{uid}");
    let d = private_dir(top, &CString::new(name.clone()).expect("no NUL"), uid)?;
    trash_in(d.as_raw_fd(), &top_path.join(name), uid, Some(top_path))
}

/// Under test, a Trash outside the test's own temp folder is refused
/// (fail closed: a test never makes or fills a real Trash).
#[cfg(test)]
const NOT_A_TEST_TRASH: &str = "a test reached a Trash outside its own folder";

/// Whether `p` (resolved through symlinks, as `open_dir_follow` will
/// resolve it) is under the test's own temp folder. Its nearest existing
/// ancestor is resolved, so a link planted below `/tmp` to a real data
/// home is caught where a lexical `starts_with` is not. (Integration
/// tests link the engine without `cfg(test)`: there the guard is the
/// fixtures' own `XDG_DATA_HOME` under their root, by discipline.)
#[cfg(test)]
fn under_test_root(p: &Path) -> bool {
    let Ok(root) = fs::canonicalize(std::env::temp_dir()) else {
        return false;
    };
    let mut probe = p;
    loop {
        match fs::canonicalize(probe) {
            Ok(real) => {
                let tail = p.strip_prefix(probe).unwrap_or(Path::new(""));
                return real.join(tail).starts_with(&root);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => match probe.parent() {
                Some(up) => probe = up,
                None => return false,
            },
            Err(_) => return false,
        }
    }
}

/// The volume Trash of mount `dom.0`.
fn volume_trash(dom: (u64, u64)) -> io::Result<TrashDir> {
    #[cfg(test)]
    let point = tests::TOPDIR
        .with_borrow(Clone::clone)
        .ok_or_else(|| io::Error::other(NOT_A_TEST_TRASH))?;
    #[cfg(not(test))]
    let point = mount_point(dom.0)?;
    let top = open_no_symlinks(
        &c_name(point.as_os_str().as_bytes())?,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    if domain(top.as_raw_fd())?.0 != dom.0 {
        return Err(io::Error::other("mount point moved"));
    }
    volume_trash_at(top.as_raw_fd(), &point, euid())
}

/// A Trash on the source's mount and device: the home Trash when it is
/// on the same one (a home Trash there that fails its checks refuses the
/// group: no fallback past a rejected folder), else the volume's.
fn trash_for(src: RawFd) -> io::Result<(TrashDir, u64)> {
    if !local_fs(src) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "remote filesystem",
        ));
    }
    let dom = domain(src)?;
    let t = match home_trash(dom) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => volume_trash(dom)?,
        r => r?,
    };
    // Telling the two folders apart needs a second mount: the
    // `trash_for_refuses_a_trash_folder_on_another_mount` test makes one
    // in a user and mount namespace and kills the `||` -> `&&` mutant.
    if domain(t.files.as_raw_fd())? != dom || domain(t.info.as_raw_fd())?.0 != dom.0 {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok((t, dom.0))
}

// ---- checking a group ----

/// Whether the open wrapper `w` is the group: the wrapper's identity
/// (device, inode, type, owner, mode), exactly as many entries as children,
/// and each child matching its baseline (device, inode, size, mtime,
/// ctime, type, owner, mode, link count). The children's names are unique,
/// so equal counts and all present make equal name sets.
fn group_matches(w: RawFd, g: &Group) -> bool {
    let Ok(st) = fstat(w) else { return false };
    let now = DirectoryId {
        dev: ident(&st).0,
        ino: ident(&st).1,
        kind: NodeKind::Directory,
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
    };
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR || now != g.wrapper {
        return false;
    }
    match read_dir(w, g.children.len()) {
        Ok(names) if names.len() == g.children.len() => {}
        _ => return false,
    }
    g.children.iter().all(|c| {
        let Ok(st) = c_name(c.rel.as_os_str().as_bytes()).and_then(|n| stat_at(w, &n)) else {
            return false;
        };
        let want = match c.kind {
            NodeKind::File => libc::S_IFREG,
            NodeKind::Directory => libc::S_IFDIR,
        };
        // st_nlink's width differs between targets.
        #[allow(clippy::unnecessary_cast)]
        let same = st.st_mode & libc::S_IFMT == want
            && st.st_uid == c.uid
            && st.st_mode & 0o7777 == c.mode
            && st.st_nlink as u64 == c.nlink
            && FileId::from_stat(&st) == c.file_id;
        same
    })
}

// ---- one group ----

/// A moved (or maybe moved) group's handles.
struct Moved<'a> {
    g: &'a Group<'a>,
    op: OperationId,
    /// The source's parent and its proved path; `None` after a crash when
    /// it cannot be opened (no way back).
    src: Option<(OwnedFd, PathBuf)>,
    leaf: CString,
    files: RawFd,
    files_path: &'a Path,
    info_dir: RawFd,
    info_path: &'a Path,
    base: CString,
    /// Our info, when we hold it.
    info: Option<Info>,
}

fn sync_all(fds: &[RawFd]) -> Durability {
    for &fd in fds {
        if let Err(e) = at(Point::Sync).and_then(|()| fsync(fd)) {
            return Durability::Unconfirmed(format!("sync failed: {e}"));
        }
    }
    Durability::Synced
}

/// Closes `info` and turns a neutralisation failure into `Uncertain`
/// (with where our bytes now are), leftovers into warnings.
fn closed(m: &Moved, info: &Info, o: GroupOutcome, warn: &mut Vec<String>) -> GroupOutcome {
    let c = close_info(m.info_dir, info);
    let at_name = |n: &CStr| m.info_path.join(OsStr::from_bytes(n.to_bytes()));
    match c.state {
        InfoState::NeutralizedInPlace(e) => warn.push(format!(
            "Trash metadata left neutralised at {} (rename failed: {})",
            at_name(&info.name).display(),
            io::Error::from_raw_os_error(e)
        )),
        InfoState::ForeignKeptDetached(_) => warn.push(format!(
            "another program's Trash metadata kept at {}",
            at_name(&info.detached).display()
        )),
        _ => {}
    }
    let Some(e) = c.neutral else { return o };
    let name = match c.state {
        InfoState::Detached => &info.detached,
        _ => &info.name,
    };
    let info_loc = locate(
        Some(m.info_path),
        m.info_dir,
        name,
        info.fd.as_raw_fd(),
        m.op,
        CheckPoint::PostMove,
    );
    let (group, mut locations) = match o {
        GroupOutcome::Kept {
            group, location, ..
        }
        | GroupOutcome::MovedToTrash {
            group, location, ..
        }
        | GroupOutcome::Restored {
            group, location, ..
        }
        | GroupOutcome::RecoveryKept {
            group, location, ..
        } => (group, vec![location]),
        GroupOutcome::Uncertain {
            group, locations, ..
        } => (group, locations),
    };
    locations.push(info_loc);
    GroupOutcome::Uncertain {
        group,
        op: m.op,
        locations,
        reason: UndoReason::ContentNotNeutralized(e),
    }
}

/// Records the ending (a failure is a warning: the intent stays open and
/// the next undo of the job recovers it) and returns it.
fn end(
    log: &mut dyn Log,
    op: OperationId,
    o: GroupOutcome,
    warn: &mut Vec<String>,
) -> GroupOutcome {
    if let Err(e) = log.close(op, &o) {
        warn.push(format!("history: an undo step not recorded: {e}"));
    }
    o
}

/// Everything after the move: the postcheck, then the commit or the move
/// back.
fn after_move(mut m: Moved, log: &mut dyn Log, warn: &mut Vec<String>) -> GroupOutcome {
    let (g, op) = (m.g, m.op);
    let Ok(obj) = open_at(m.files, &m.base, libc::O_PATH, 0) else {
        // Gone from the Trash before we could look: nothing of ours to
        // anchor, nothing deleted by us. Our info no longer describes
        // `files/<base>`, so it is closed: whatever appears there later
        // never pairs with it.
        let o = GroupOutcome::Uncertain {
            group: g.group,
            op,
            locations: vec![unknown_place(
                &m.files_path.join(OsStr::from_bytes(m.base.to_bytes())),
                op,
                CheckPoint::PostMove,
            )],
            reason: UndoReason::AncestorChanged,
        };
        let o = match m.info.take() {
            Some(i) => closed(&m, &i, o, warn),
            None => o,
        };
        return end(log, op, o, warn);
    };
    let ours = open_at(m.files, &m.base, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .ok()
        .filter(|w| {
            fstat(w.as_raw_fd()).ok().map(|s| ident(&s))
                == fstat(obj.as_raw_fd()).ok().map(|s| ident(&s))
        })
        .is_some_and(|w| group_matches(w.as_raw_fd(), g));
    let info_ok = m
        .info
        .as_ref()
        .is_some_and(|i| stat_at(m.info_dir, &i.name).is_ok_and(|st| ident(&st) == i.id));
    let mismatch = if !ours {
        Some(UndoReason::PostMoveMismatch)
    } else if !info_ok {
        Some(UndoReason::InfoChanged)
    } else {
        None
    };
    let Some(reason) = mismatch else {
        let mut fds = vec![m.files];
        if let Some((s, _)) = &m.src {
            fds.insert(0, s.as_raw_fd());
        }
        let durability = sync_all(&fds);
        let location = locate(
            Some(m.files_path),
            m.files,
            &m.base,
            obj.as_raw_fd(),
            op,
            CheckPoint::Commit,
        );
        let info = m.info.as_ref().expect("checked above");
        let o = if location.path_at_check.is_none() {
            GroupOutcome::Uncertain {
                group: g.group,
                op,
                locations: vec![location],
                reason: UndoReason::AncestorChanged,
            }
        } else if durability != Durability::Synced {
            GroupOutcome::RecoveryKept {
                group: g.group,
                location,
                reason: UndoReason::NotDurable,
                errno: None,
                durability,
            }
        } else {
            GroupOutcome::MovedToTrash {
                group: g.group,
                location,
                info_leaf: OsStr::from_bytes(info.name.to_bytes()).to_owned(),
                durability,
            }
        };
        return match at(Point::Commit).and_then(|()| log.close(op, &o)) {
            Ok(()) => o,
            Err(e) => {
                warn.push(format!("history: the move to Trash not recorded: {e}"));
                match o {
                    GroupOutcome::MovedToTrash {
                        group, location, ..
                    }
                    | GroupOutcome::RecoveryKept {
                        group, location, ..
                    } => GroupOutcome::RecoveryKept {
                        group,
                        location,
                        reason: UndoReason::CommitNotRecorded,
                        errno: e.raw_os_error(),
                        durability: Durability::Unconfirmed(format!("history: {e}")),
                    },
                    other => other,
                }
            }
        };
    };
    // Back where it came from, never over what is there now.
    let back = match &m.src {
        Some((s, _)) => at(Point::Restore)
            .and_then(|()| rename_noreplace(m.files, &m.base, s.as_raw_fd(), &m.leaf)),
        None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
    };
    match back {
        Ok(()) => {
            let (s, dir) = m.src.as_ref().expect("restored into it");
            let durability = sync_all(&[s.as_raw_fd(), m.files]);
            let location = locate(
                Some(dir),
                s.as_raw_fd(),
                &m.leaf,
                obj.as_raw_fd(),
                op,
                CheckPoint::PostRestore,
            );
            // Restored, but nowhere it can be shown to be: as for the
            // commit, the typed outcome says so (`None` only in Uncertain).
            let o = if location.path_at_check.is_none() {
                GroupOutcome::Uncertain {
                    group: g.group,
                    op,
                    locations: vec![location],
                    reason: UndoReason::AncestorChanged,
                }
            } else {
                GroupOutcome::Restored {
                    group: g.group,
                    location,
                    reason,
                    durability,
                }
            };
            let o = match m.info.take() {
                Some(i) => closed(&m, &i, o, warn),
                None => o,
            };
            end(log, op, o, warn)
        }
        Err(e) => {
            let _ = at(Point::AfterRestoreFail);
            let e = errno(&e);
            // Our info stays only while its name still pairs with our data.
            let paired = mismatch_keeps_pair(&m, reason);
            let location = locate(
                Some(m.files_path),
                m.files,
                &m.base,
                obj.as_raw_fd(),
                op,
                CheckPoint::PostMove,
            );
            let durability = sync_all(&[m.files]);
            let o = if location.path_at_check.is_none() {
                GroupOutcome::Uncertain {
                    group: g.group,
                    op,
                    locations: vec![location],
                    reason: UndoReason::AncestorChanged,
                }
            } else {
                GroupOutcome::RecoveryKept {
                    group: g.group,
                    location,
                    reason: if e == libc::EEXIST {
                        UndoReason::RestoreCollision
                    } else {
                        UndoReason::RestoreFailed
                    },
                    errno: Some(e),
                    durability,
                }
            };
            let o = match (paired, m.info.take()) {
                (false, Some(i)) => closed(&m, &i, o, warn),
                _ => o,
            };
            end(log, op, o, warn)
        }
    }
}

/// After a failed move back, our info stays (a correct pair) only if its
/// name is still ours and the data at `files/<base>` is what we moved
/// (whatever it is, it came from the source name we described).
fn mismatch_keeps_pair(m: &Moved, reason: UndoReason) -> bool {
    reason != UndoReason::InfoChanged
        && m.info
            .as_ref()
            .is_some_and(|i| stat_at(m.info_dir, &i.name).is_ok_and(|st| ident(&st) == i.id))
}

/// Undoes one committed group into the Trash (see the module doc).
pub(crate) fn undo_group(g: &Group, log: &mut dyn Log, warn: &mut Vec<String>) -> GroupOutcome {
    let op = OperationId(crate::output::random_u64().unwrap_or_else(|_| {
        // The clock and pid: unique enough for a name never shared
        // with another of our operations at the same time.
        (u64::from(std::process::id()) << 32)
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
    }));
    let place = path_of(g.place);
    let kept = |location, reason| GroupOutcome::Kept {
        group: g.group,
        location,
        reason,
    };
    let missing = || {
        kept(
            unknown_place(&place, op, CheckPoint::Precheck),
            UndoReason::Missing,
        )
    };
    let (Some(parent), Some(leaf)) = (place.parent(), place.file_name()) else {
        return missing();
    };
    // These opens and the `stat`s below have no deadline of their own: a
    // hung network or FUSE mount at the group's place blocks them. Undo
    // runs in the supervisor's Undo window or the CLI, never in the
    // overlay, so it cannot lock the desktop; the history lock it holds
    // makes a concurrent undo give up after its own bounded wait.
    let (Ok(src), Ok(cleaf)) = (
        open_dir_follow(parent.as_os_str().as_bytes()),
        c_name(leaf.as_bytes()),
    ) else {
        return missing();
    };
    let w = match open_at(
        src.as_raw_fd(),
        &cleaf,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    ) {
        Ok(w) => w,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return missing(),
        Err(_) => {
            let o = open_at(src.as_raw_fd(), &cleaf, libc::O_PATH, 0);
            let loc = match &o {
                Ok(o) => locate(
                    Some(parent),
                    src.as_raw_fd(),
                    &cleaf,
                    o.as_raw_fd(),
                    op,
                    CheckPoint::Precheck,
                ),
                Err(_) => unknown_place(&place, op, CheckPoint::Precheck),
            };
            return kept(loc, UndoReason::Changed);
        }
    };
    let src_loc = locate(
        Some(parent),
        src.as_raw_fd(),
        &cleaf,
        w.as_raw_fd(),
        op,
        CheckPoint::Precheck,
    );
    if !group_matches(w.as_raw_fd(), g) {
        return kept(src_loc, UndoReason::Changed);
    }
    // The source's real path, for `Path=` and the way back.
    let Resolved::At(src_dir) = resolve(src.as_raw_fd()) else {
        return kept(src_loc, UndoReason::AncestorChanged);
    };
    let Ok((trash, mount)) = trash_for(src.as_raw_fd()) else {
        return kept(src_loc, UndoReason::NoTrash);
    };
    let source = src_dir.join(leaf);
    let sb = source.as_os_str().as_bytes();
    let info_rel = match &trash.top {
        None => 0,
        Some(top) => match source.strip_prefix(top) {
            Ok(rel) => sb.len() - rel.as_os_str().len(),
            Err(_) => return kept(src_loc, UndoReason::NoTrash),
        },
    };
    let date = local_date();
    let Some(bytes) = trashinfo(&sb[info_rel..], &date) else {
        return kept(src_loc, UndoReason::TooLong);
    };
    let (files, info_dir) = (trash.files.as_raw_fd(), trash.info.as_raw_fd());
    let Some(base) = choose_name(files, info_dir, leaf.as_bytes()) else {
        return kept(src_loc, UndoReason::NameTaken);
    };
    let target = trash.files_path.join(OsStr::from_bytes(base.to_bytes()));
    let id = |fd: RawFd| fstat(fd).map(|st| ident(&st)).unwrap_or((0, 0));
    let intent = Intent {
        op,
        group: g.group,
        source: sb,
        target: target.as_os_str().as_bytes(),
        info_rel,
        source_parent: id(src.as_raw_fd()),
        target_parent: id(files),
        wrapper: (g.wrapper.dev, g.wrapper.ino),
        mount,
        date: &date,
    };
    if let Err(e) = at(Point::Intent).and_then(|()| log.intent(&intent)) {
        let r = if e.kind() == io::ErrorKind::InvalidInput {
            UndoReason::TooLong
        } else {
            UndoReason::HistoryNotWritten(errno(&e))
        };
        return kept(src_loc, r);
    }
    let mut m = Moved {
        g,
        op,
        src: Some((src, src_dir)),
        leaf: cleaf,
        files,
        files_path: &trash.files_path,
        info_dir,
        info_path: &trash.info_path,
        base: base.clone(),
        info: None,
    };
    let info = match reserve(info_dir, info_name(&base), op, &bytes) {
        Ok(i) => i,
        Err((e, held)) => {
            // EEXIST here is a race: `choose_name` saw both names free a
            // moment ago. The group is kept (the user can undo again, the
            // job stays open); no second candidate is tried, so a name
            // fought over by another writer never loops.
            let r = if e == libc::EEXIST {
                UndoReason::NameTaken
            } else {
                UndoReason::InfoNotWritten(e)
            };
            let o = kept(src_loc, r);
            let o = match held {
                Some(i) => closed(&m, &i, o, warn),
                None => o,
            };
            return end(log, op, o, warn);
        }
    };
    let _ = at(Point::BeforeMove);
    let (s, _) = m.src.as_ref().expect("set above");
    if let Err(e) =
        at(Point::Rename).and_then(|()| rename_noreplace(s.as_raw_fd(), &m.leaf, files, &base))
    {
        let o = kept(src_loc, UndoReason::MoveRefused(errno(&e)));
        let o = closed(&m, &info, o, warn);
        return end(log, op, o, warn);
    }
    let _ = at(Point::AfterMove);
    m.info = Some(info);
    after_move(m, log, warn)
}

/// The recorded intent of an operation that has no ending (a crash).
pub(crate) struct Open<'a> {
    pub op: OperationId,
    pub source: &'a [u8],
    pub target: &'a [u8],
    pub info_rel: usize,
    pub source_parent: (u64, u64),
    pub target_parent: (u64, u64),
    pub date: &'a str,
}

/// A regular file `name` in `dir`, opened for writing (never through a
/// symlink, never blocking), with its first [`TRASHINFO_MAX_BYTES`].
fn read_info(dir: RawFd, name: &CStr) -> Option<(OwnedFd, Vec<u8>)> {
    let fd = open_at(dir, name, libc::O_RDWR | libc::O_NONBLOCK, 0).ok()?;
    let st = fstat(fd.as_raw_fd()).ok()?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG || st.st_size as u64 > TRASHINFO_MAX_BYTES as u64
    {
        return None;
    }
    let mut b = vec![0u8; st.st_size as usize];
    let f = fs::File::from(fd.try_clone().ok()?);
    f.read_exact_at(&mut b, 0).ok()?;
    Some((fd, b))
}

/// Recovers an operation a crash left open, with no Trash scan: only the
/// intent's target and our two info names are looked at. If the group is
/// in the Trash under the target, the undo resumes after the move. If not,
/// our info (authentic or neutral) is closed under both names, a foreign
/// file found at the detached name is put back, the operation is closed,
/// and `None` lets a fresh undo run.
pub(crate) fn recover(
    g: &Group,
    o: &Open,
    log: &mut dyn Log,
    warn: &mut Vec<String>,
) -> Option<GroupOutcome> {
    let op = o.op;
    let target = path_of(o.target);
    let source = path_of(o.source);
    let (Some(files_path), Some(base)) = (target.parent(), target.file_name()) else {
        return Some(end(log, op, uncertain_at(g, op, &target), warn));
    };
    let Some(trash_root) = files_path.parent() else {
        return Some(end(log, op, uncertain_at(g, op, &target), warn));
    };
    let info_path = trash_root.join("info");
    let files = open_dir_follow(files_path.as_os_str().as_bytes())
        .ok()
        .filter(|f| fstat(f.as_raw_fd()).is_ok_and(|st| ident(&st) == o.target_parent));
    // `info` is reached through the verified `files` (its `..` is the
    // Trash root that really holds it), never by its own path, and must be
    // a private folder of ours like the one the move made.
    let info_dir = files.as_ref().and_then(|f| {
        let fd = open_at(
            f.as_raw_fd(),
            c"../info",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .ok()?;
        private_ok(&fstat(fd.as_raw_fd()).ok()?, euid()).then_some(fd)
    });
    let (Ok(cbase), Some(rel)) = (c_name(base.as_bytes()), o.source.get(o.info_rel..)) else {
        return Some(end(log, op, uncertain_at(g, op, &target), warn));
    };
    let ours = trashinfo(rel, o.date).unwrap_or_default();
    let name = info_name(&cbase);
    let detached = detached_name(op);
    // The source folder is the one the intent recorded, not whatever now
    // stands at its path: a restore never goes into a look-alike.
    let src = source.parent().and_then(|p| {
        open_dir_follow(p.as_os_str().as_bytes())
            .ok()
            .filter(|fd| fstat(fd.as_raw_fd()).is_ok_and(|st| ident(&st) == o.source_parent))
            .map(|fd| (fd, p.to_owned()))
    });
    let Ok(leaf) = c_name(source.file_name().unwrap_or_default().as_bytes()) else {
        return Some(end(log, op, uncertain_at(g, op, &target), warn));
    };
    let in_trash = files.as_ref().is_some_and(|f| {
        stat_at(f.as_raw_fd(), &cbase).is_ok_and(|st| {
            st.st_mode & libc::S_IFMT == libc::S_IFDIR
                && ident(&st) == (g.wrapper.dev, g.wrapper.ino)
        })
    });
    let (Some(files), Some(info_dir)) = (files, info_dir) else {
        // The Trash is not where the intent says. At the source, our info
        // (if any) cannot be reached: a warning, and a fresh undo runs.
        let at_source = src.as_ref().is_some_and(|(s, _)| {
            stat_at(s.as_raw_fd(), &leaf)
                .is_ok_and(|st| ident(&st) == (g.wrapper.dev, g.wrapper.ino))
        });
        if at_source {
            warn.push(format!(
                "an interrupted undo's Trash metadata could not be checked (Trash moved from {})",
                files_path.display()
            ));
            let o = GroupOutcome::Kept {
                group: g.group,
                location: unknown_place(&source, op, CheckPoint::Precheck),
                reason: UndoReason::Interrupted,
            };
            end(log, op, o, warn);
            return None;
        }
        return Some(end(log, op, uncertain_at(g, op, &target), warn));
    };
    let (files, info_dir) = (files.as_raw_fd(), info_dir.as_raw_fd());
    let mut m = Moved {
        g,
        op,
        src,
        leaf,
        files,
        files_path,
        info_dir,
        info_path: &info_path,
        base: cbase,
        info: None,
    };
    if in_trash {
        m.info = read_info(info_dir, &name)
            .filter(|(_, b)| *b == ours)
            .and_then(|(fd, _)| {
                let id = ident(&fstat(fd.as_raw_fd()).ok()?);
                Some(Info {
                    fd,
                    id,
                    name: name.clone(),
                    detached: detached.clone(),
                    op,
                })
            });
        return Some(after_move(m, log, warn));
    }
    // Never moved (or moved elsewhere by someone): close what is ours.
    let mut uncertain = None;
    // Ours: authentic, neutral, or cut short while being written (a
    // prefix of the authentic bytes, empty included: we made the name
    // with O_EXCL).
    // An empty file counts only when no detached info shows our info was
    // ever closed: after that, an empty file at the name is someone else's.
    let detached_exists = stat_at(info_dir, &detached).is_ok();
    if let Some((fd, b)) = read_info(info_dir, &name)
        && ((ours.starts_with(&b) && !(b.is_empty() && detached_exists))
            || b == neutral(op)
            || partly_neutral(&b, &ours, op))
        && let Ok(st) = fstat(fd.as_raw_fd())
    {
        let i = Info {
            id: ident(&st),
            fd,
            name: name.clone(),
            detached: detached.clone(),
            op,
        };
        let c = close_info(info_dir, &i);
        if let Some(e) = c.neutral {
            uncertain = Some(e);
        }
    }
    if let Some((fd, b)) = read_info(info_dir, &detached) {
        if b == ours || partly_neutral(&b, &ours, op) {
            let i = Info {
                id: fstat(fd.as_raw_fd()).map_or((0, 0), |st| ident(&st)),
                fd,
                name: name.clone(),
                detached: detached.clone(),
                op,
            };
            if let Err(e) = neutralise(&i) {
                uncertain = Some(e);
            }
        } else if b != neutral(op)
            && let Err(e) = rename_noreplace(info_dir, &detached, info_dir, &name)
        {
            warn.push(format!(
                "another program's Trash metadata kept at {} ({e})",
                info_path
                    .join(OsStr::from_bytes(detached.to_bytes()))
                    .display()
            ));
        }
    }
    let _ = fsync(info_dir);
    if let Some(e) = uncertain {
        let mut locations = vec![unknown_place(&source, op, CheckPoint::PostMove)];
        locations.push(unknown_place(
            &info_path.join(OsStr::from_bytes(detached.to_bytes())),
            op,
            CheckPoint::PostMove,
        ));
        let o = GroupOutcome::Uncertain {
            group: g.group,
            op,
            locations,
            reason: UndoReason::ContentNotNeutralized(e),
        };
        return Some(end(log, op, o, warn));
    }
    let o = GroupOutcome::Kept {
        group: g.group,
        location: unknown_place(&source, op, CheckPoint::Precheck),
        reason: UndoReason::Interrupted,
    };
    end(log, op, o, warn);
    None
}

fn uncertain_at(g: &Group, op: OperationId, target: &Path) -> GroupOutcome {
    GroupOutcome::Uncertain {
        group: g.group,
        op,
        locations: vec![unknown_place(target, op, CheckPoint::PostMove)],
        reason: UndoReason::AncestorChanged,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::xdg::tests::with_env;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU32, Ordering};

    type Fault = Option<Box<dyn FnMut(Point) -> Option<i32>>>;

    thread_local! {
        /// Called at each [`Point`]: an errno to inject there, or `None`
        /// (after any race the test stages).
        pub(crate) static FAULT: std::cell::RefCell<Fault> =
            const { std::cell::RefCell::new(None) };
        /// The volume Trash's topdir in place of the real mount point (a
        /// test never makes a Trash at a real mount's root).
        pub(crate) static TOPDIR: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Installs `f` at every [`Point`] until the guard drops.
    pub(crate) struct Faults;
    impl Drop for Faults {
        fn drop(&mut self) {
            FAULT.with_borrow_mut(|f| *f = None);
        }
    }
    pub(crate) fn fault(f: impl FnMut(Point) -> Option<i32> + 'static) -> Faults {
        FAULT.with_borrow_mut(|s| *s = Some(Box::new(f)));
        Faults
    }

    /// A private folder per test, removed (made writable first) at the end.
    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Dir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "engine-trash-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn fd(&self, rel: &str) -> OwnedFd {
            open_dir_follow(self.0.join(rel).as_os_str().as_bytes()).unwrap()
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::process::Command::new("chmod")
                .args(["-R", "u+rwx"])
                .arg(&self.0)
                .status();
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    const OP: OperationId = OperationId(0x0123_4567_89ab_cdef);

    #[test]
    fn trashinfo_is_the_spec_format_and_bounded() {
        let date = "2026-10-27T10:00:00";
        assert_eq!(
            trashinfo("/a b/%\u{fc}~x".as_bytes(), date).unwrap(),
            b"[Trash Info]\nPath=/a%20b/%25%C3%BC~x\nDeletionDate=2026-10-27T10:00:00\n"
        );
        // 52 B of frame: 16332 path bytes fill the limit exactly, one more
        // is refused.
        assert_eq!(
            trashinfo(&[b'a'; 16332], date).unwrap().len(),
            TRASHINFO_MAX_BYTES
        );
        assert_eq!(trashinfo(&[b'a'; 16333], date), None);
        // The worst 4096-byte path (all escaped) fits.
        assert!(trashinfo(&[0xff; 4096], date).is_some());
    }

    #[test]
    fn candidate_names_fit_name_max() {
        assert_eq!(candidate(b"a", 0).unwrap().as_bytes(), b"a");
        assert_eq!(candidate(b"a", 1).unwrap().as_bytes(), b"a.2");
        assert_eq!(candidate(b"a", 7).unwrap().as_bytes(), b"a.8");
        let long = [b'x'; 300];
        let n = candidate(&long, 3).unwrap();
        assert_eq!(n.as_bytes().len() + INFO_EXT.len(), 255);
        // Never cut inside a character.
        let wide = "\u{e9}".repeat(200);
        let n = candidate(wide.as_bytes(), 0).unwrap();
        assert!(std::str::from_utf8(n.as_bytes()).is_ok());
        assert!(n.as_bytes().len() <= BASE_MAX);
    }

    /// 64 names are tried (`trash_name_attempts`): with 63 taken the 64th
    /// is found, with all 64 taken (in either folder) there is none.
    #[test]
    fn choose_name_tries_a_bounded_number_of_names() {
        let d = Dir::new("names");
        fs::create_dir_all(d.0.join("files")).unwrap();
        fs::create_dir_all(d.0.join("info")).unwrap();
        let take = |k: u32| {
            let n = String::from_utf8(candidate(b"g", k).unwrap().into_bytes()).unwrap();
            if k.is_multiple_of(2) {
                fs::create_dir(d.0.join("files").join(&n)).unwrap();
            } else {
                fs::write(d.0.join("info").join(format!("{n}.trashinfo")), "x").unwrap();
            }
        };
        for k in 0..63 {
            take(k);
        }
        let (f, i) = (d.fd("files"), d.fd("info"));
        let got = choose_name(f.as_raw_fd(), i.as_raw_fd(), b"g").unwrap();
        assert_eq!(got.as_bytes(), b"g.64");
        take(63);
        assert_eq!(choose_name(f.as_raw_fd(), i.as_raw_fd(), b"g"), None);
    }

    #[test]
    fn reserve_is_exclusive_and_read_back() {
        let d = Dir::new("reserve");
        let info = d.fd("");
        let bytes = trashinfo(b"/x", "2026-10-27T10:00:00").unwrap();
        let i = reserve(info.as_raw_fd(), c("x.trashinfo"), OP, &bytes)
            .unwrap_or_else(|_| panic!("reserve"));
        assert_eq!(fs::read(d.0.join("x.trashinfo")).unwrap(), bytes);
        let mode = fs::metadata(d.0.join("x.trashinfo"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(i);
        // Never over an existing name, ours or not.
        match reserve(info.as_raw_fd(), c("x.trashinfo"), OP, &bytes) {
            Err((e, None)) => assert_eq!(e, libc::EEXIST),
            _ => panic!("reserved over an existing name"),
        }
        // A symlink at the name is not followed.
        let victim = d.0.join("victim");
        symlink(&victim, d.0.join("y.trashinfo")).unwrap();
        assert!(reserve(info.as_raw_fd(), c("y.trashinfo"), OP, &bytes).is_err());
        assert!(!victim.exists());
    }

    /// A short write (labelled injection: half the bytes, then ENOSPC)
    /// hands the inode back for closing.
    #[test]
    fn a_short_info_write_is_handed_back_and_closed() {
        let d = Dir::new("short");
        let info = d.fd("");
        let bytes = trashinfo(b"/x", "2026-10-27T10:00:00").unwrap();
        let _f = fault(|p| (p == Point::InfoWrite).then_some(libc::ENOSPC));
        let Err((e, Some(i))) = reserve(info.as_raw_fd(), c("x.trashinfo"), OP, &bytes) else {
            panic!("no fault")
        };
        assert_eq!(e, libc::ENOSPC);
        assert_eq!(
            fs::read(d.0.join("x.trashinfo")).unwrap(),
            bytes[..bytes.len() / 2]
        );
        let cl = close_info(info.as_raw_fd(), &i);
        assert_eq!((cl.state, cl.neutral), (InfoState::Detached, None));
        assert!(!d.0.join("x.trashinfo").exists());
        let det = d.0.join(".0123456789abcdef.inacircle-detached");
        assert_eq!(fs::read(det).unwrap(), neutral(OP));
    }

    fn reserved(d: &Dir) -> (OwnedFd, Info) {
        let info = d.fd("");
        let bytes = trashinfo(b"/x", "2026-10-27T10:00:00").unwrap();
        let i = reserve(info.as_raw_fd(), c("x.trashinfo"), OP, &bytes)
            .unwrap_or_else(|_| panic!("reserve"));
        (info, i)
    }

    #[test]
    fn close_info_neutralises_and_detaches() {
        let d = Dir::new("close");
        let (info, i) = reserved(&d);
        let cl = close_info(info.as_raw_fd(), &i);
        assert_eq!((cl.state, cl.neutral), (InfoState::Detached, None));
        let names: Vec<_> = fs::read_dir(&d.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            [OsString::from(".0123456789abcdef.inacircle-detached")]
        );
        assert_eq!(
            fs::read(d.0.join(".0123456789abcdef.inacircle-detached")).unwrap(),
            neutral(OP)
        );
    }

    /// Our name taken by a foreign info (ours renamed away first): the
    /// foreign file is left byte-identical, ours neutralised through its
    /// fd wherever it is.
    #[test]
    fn close_info_leaves_a_foreign_name_alone() {
        let d = Dir::new("foreign");
        let (info, i) = reserved(&d);
        fs::rename(d.0.join("x.trashinfo"), d.0.join("moved")).unwrap();
        let foreign = b"[Trash Info]\nPath=/foreign\nDeletionDate=2026-01-01T00:00:00\n";
        fs::write(d.0.join("x.trashinfo"), foreign).unwrap();
        let cl = close_info(info.as_raw_fd(), &i);
        assert_eq!(cl.state, InfoState::ForeignUntouched);
        assert_eq!(fs::read(d.0.join("x.trashinfo")).unwrap(), foreign);
        assert_eq!(fs::read(d.0.join("moved")).unwrap(), neutral(OP));
    }

    /// N1: a failed neutralisation (labelled injection at the write and at
    /// the sync) still detaches the name, and says so.
    #[test]
    fn close_info_detaches_even_when_neutralising_fails() {
        for (at_point, e) in [
            (Point::NeutralWrite, libc::ENOSPC),
            (Point::NeutralSync, libc::EIO),
        ] {
            let d = Dir::new("n1");
            let (info, i) = reserved(&d);
            let _f = fault(move |p| (p == at_point).then_some(e));
            let cl = close_info(info.as_raw_fd(), &i);
            assert_eq!((cl.state, cl.neutral), (InfoState::Detached, Some(e)));
            assert!(!d.0.join("x.trashinfo").exists(), "{at_point:?}");
        }
    }

    /// A foreign file swapped in between the identity check and the
    /// detaching rename is put back under its name.
    #[test]
    fn close_info_puts_back_a_foreign_file_that_raced_the_detach() {
        let d = Dir::new("race");
        let (info, i) = reserved(&d);
        let root = d.0.clone();
        let _f = fault(move |p| {
            if p == Point::Detach {
                fs::rename(root.join("x.trashinfo"), root.join("ours")).unwrap();
                fs::write(root.join("x.trashinfo"), "foreign").unwrap();
            }
            None
        });
        let cl = close_info(info.as_raw_fd(), &i);
        assert_eq!(cl.state, InfoState::ForeignRestored);
        assert_eq!(
            fs::read_to_string(d.0.join("x.trashinfo")).unwrap(),
            "foreign"
        );
        assert_eq!(fs::read(d.0.join("ours")).unwrap(), neutral(OP));
    }

    /// A failed detaching rename (real EACCES: the folder made read-only)
    /// leaves our neutralised inode in place, reported.
    #[test]
    fn close_info_neutralises_in_place_when_the_rename_fails() {
        if euid() == 0 {
            return; // root ignores the folder's mode
        }
        let d = Dir::new("eacces");
        fs::create_dir(d.0.join("info")).unwrap();
        let info = d.fd("info");
        let bytes = trashinfo(b"/x", "2026-10-27T10:00:00").unwrap();
        let i = reserve(info.as_raw_fd(), c("x.trashinfo"), OP, &bytes)
            .unwrap_or_else(|_| panic!("reserve"));
        fs::set_permissions(d.0.join("info"), fs::Permissions::from_mode(0o500)).unwrap();
        let cl = close_info(info.as_raw_fd(), &i);
        assert_eq!(cl.state, InfoState::NeutralizedInPlace(libc::EACCES));
        assert_eq!(fs::read(d.0.join("info/x.trashinfo")).unwrap(), neutral(OP));
    }

    #[test]
    fn resolve_proves_a_renamed_ancestor_never_a_symlink() {
        let d = Dir::new("resolve");
        fs::create_dir_all(d.0.join("a/b")).unwrap();
        let b = d.fd("a/b");
        fs::rename(d.0.join("a"), d.0.join("moved")).unwrap();
        symlink(d.0.join("moved"), d.0.join("a")).unwrap();
        let Resolved::At(p) = resolve(b.as_raw_fd()) else {
            panic!("not resolved")
        };
        assert_eq!(p, fs::canonicalize(d.0.join("moved/b")).unwrap());
    }

    /// A renamer that wins every attempt: the place stays unproved after
    /// exactly RESOLVE_ATTEMPTS tries, with the last path seen.
    #[test]
    fn resolve_gives_up_after_bounded_attempts() {
        let d = Dir::new("race-resolve");
        fs::create_dir_all(d.0.join("t0/x")).unwrap();
        let x = d.fd("t0/x");
        let root = d.0.clone();
        let n = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let seen = n.clone();
        let _f = fault(move |p| {
            if p == Point::Resolve {
                let k = seen.get();
                fs::rename(root.join(format!("t{k}")), root.join(format!("t{}", k + 1))).unwrap();
                seen.set(k + 1);
            }
            None
        });
        let Resolved::Unverified { last } = resolve(x.as_raw_fd()) else {
            panic!("resolved under a racing renamer")
        };
        assert_eq!(n.get(), RESOLVE_ATTEMPTS);
        let last = last.unwrap();
        assert!(
            last.ends_with(format!("t{}/x", RESOLVE_ATTEMPTS - 1)),
            "{last:?}"
        );
        assert!(d.0.join(format!("t{RESOLVE_ATTEMPTS}/x")).is_dir());
    }

    #[test]
    fn resolve_of_a_removed_folder_is_unverified() {
        let d = Dir::new("gone");
        fs::create_dir(d.0.join("x")).unwrap();
        let x = d.fd("x");
        fs::remove_dir(d.0.join("x")).unwrap();
        assert!(matches!(
            resolve(x.as_raw_fd()),
            Resolved::Unverified { .. }
        ));
    }

    #[test]
    fn private_ok_checks_type_owner_and_mode() {
        // SAFETY: zeroed stat is a valid value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        st.st_mode = libc::S_IFDIR | 0o700;
        st.st_uid = 1000;
        assert!(private_ok(&st, 1000));
        assert!(!private_ok(&st, 1001), "another owner");
        st.st_mode = libc::S_IFDIR | 0o755;
        assert!(!private_ok(&st, 1000), "not private");
        st.st_mode = libc::S_IFDIR | 0o1700;
        assert!(!private_ok(&st, 1000), "sticky private folder");
        st.st_mode = libc::S_IFLNK | 0o700;
        assert!(!private_ok(&st, 1000), "a symlink");
    }

    /// The home Trash is made private when missing; a symlinked `Trash`
    /// or a `files` folder open to others is refused and left as it is.
    #[test]
    fn home_trash_is_private_and_never_followed() {
        let d = Dir::new("home");
        let data = d.0.join("share");
        with_env(&[("XDG_DATA_HOME", Some(data.to_str().unwrap()))], || {
            let dom = domain(d.fd("").as_raw_fd()).unwrap();
            let t = home_trash(dom).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(t.files_path, data.join("Trash/files"));
            assert_eq!(t.top, None);
            for p in ["Trash", "Trash/files", "Trash/info"] {
                let m = fs::metadata(data.join(p)).unwrap().permissions().mode();
                assert_eq!(m & 0o777, 0o700, "{p}");
            }
            fs::set_permissions(data.join("Trash/files"), fs::Permissions::from_mode(0o755))
                .unwrap();
            assert!(home_trash(dom).is_err());
            let m = fs::metadata(data.join("Trash/files"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o755, "never chmod-ed");
            fs::rename(data.join("Trash"), data.join("Real")).unwrap();
            symlink(data.join("Real"), data.join("Trash")).unwrap();
            assert!(home_trash(dom).is_err());
        });
    }

    /// The volume Trash: `.Trash/$uid` under a sticky `.Trash`; else
    /// `.Trash-$uid`; a `.Trash` that is a symlink or not sticky is passed
    /// over, a `.Trash-$uid` that is a symlink or open to others refuses.
    #[test]
    fn volume_trash_methods_and_their_checks() {
        let uid = euid();
        let d = Dir::new("volume");
        let top = d.fd("");
        let t = volume_trash_at(top.as_raw_fd(), &d.0, uid).unwrap();
        assert_eq!(t.files_path, d.0.join(format!(".Trash-{uid}/files")));
        assert_eq!(t.top.as_deref(), Some(d.0.as_path()));

        let d = Dir::new("volume-sticky");
        fs::create_dir(d.0.join(".Trash")).unwrap();
        fs::set_permissions(d.0.join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        let t = volume_trash_at(d.fd("").as_raw_fd(), &d.0, uid).unwrap();
        assert_eq!(t.files_path, d.0.join(format!(".Trash/{uid}/files")));

        let d = Dir::new("volume-plain");
        fs::create_dir(d.0.join(".Trash")).unwrap();
        fs::set_permissions(d.0.join(".Trash"), fs::Permissions::from_mode(0o777)).unwrap();
        let t = volume_trash_at(d.fd("").as_raw_fd(), &d.0, uid).unwrap();
        assert_eq!(t.files_path, d.0.join(format!(".Trash-{uid}/files")));
        assert!(
            !d.0.join(format!(".Trash/{uid}")).exists(),
            "no sticky bit: unused"
        );

        let d = Dir::new("volume-link");
        let real = d.0.join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o1777)).unwrap();
        symlink(&real, d.0.join(".Trash")).unwrap();
        symlink(&real, d.0.join(format!(".Trash-{uid}"))).unwrap();
        assert!(volume_trash_at(d.fd("").as_raw_fd(), &d.0, uid).is_err());
        assert_eq!(
            fs::read_dir(&real).unwrap().count(),
            0,
            "nothing made through a link"
        );

        let d = Dir::new("volume-open");
        fs::create_dir(d.0.join(format!(".Trash-{uid}"))).unwrap();
        fs::set_permissions(
            d.0.join(format!(".Trash-{uid}")),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(volume_trash_at(d.fd("").as_raw_fd(), &d.0, uid).is_err());
    }

    /// A test never reaches a real Trash: without its own volume topdir,
    /// or with a data home outside the temp folder, the Trash is refused
    /// (not `EXDEV`, so no fallback either) before anything is made.
    #[test]
    fn a_test_without_its_own_trash_fails_closed() {
        let d = Dir::new("closed");
        let dom = domain(d.fd("").as_raw_fd()).unwrap();
        TOPDIR.with_borrow_mut(|p| *p = None);
        let e = volume_trash(dom).err().expect("a real volume Trash");
        assert_eq!(e.to_string(), NOT_A_TEST_TRASH);
        let e = with_env(&[("XDG_DATA_HOME", Some("/proc/inacircle-never"))], || {
            home_trash(dom).err().expect("a Trash outside the test")
        });
        assert_eq!(e.to_string(), NOT_A_TEST_TRASH);
    }

    /// The test guard resolves symlinks: a link below the temp folder to
    /// somewhere else is outside it (a lexical prefix check passes it).
    #[test]
    fn the_test_guard_resolves_links() {
        let d = Dir::new("guard-link");
        symlink("/proc", d.0.join("out")).unwrap();
        let dom = domain(d.fd("").as_raw_fd()).unwrap();
        let data = d.0.join("out/inacircle-never");
        let e = with_env(&[("XDG_DATA_HOME", Some(data.to_str().unwrap()))], || {
            home_trash(dom).err().expect("a Trash outside the test")
        });
        assert_eq!(e.to_string(), NOT_A_TEST_TRASH);
        assert!(under_test_root(&d.0.join("inside/not/yet")));
    }

    /// A shared `.Trash` that is sticky but belongs to another user is
    /// passed over (its owner could rename our `$uid` folder away): the
    /// `$uid` folder is never made in it. Ownership is checked against the
    /// `uid` given, so a `.Trash` of ours is "another user's" for `uid+1`.
    #[test]
    fn a_shared_trash_of_another_owner_is_not_used() {
        if euid() == 0 {
            return; // root owns every .Trash it makes
        }
        let other = euid() + 1;
        let d = Dir::new("volume-foreign");
        fs::create_dir(d.0.join(".Trash")).unwrap();
        fs::set_permissions(d.0.join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        let _ = volume_trash_at(d.fd("").as_raw_fd(), &d.0, other);
        assert!(!d.0.join(format!(".Trash/{other}")).exists());
    }

    #[test]
    fn mountinfo_lines_and_this_mount() {
        let l = b"36 35 98:0 /mnt1 /mnt\\040x\\134y rw,noatime master:1 - ext3 /dev/root rw";
        assert_eq!(mountinfo_line(l), Some((36, PathBuf::from("/mnt x\\y"))));
        assert_eq!(mountinfo_line(b"x 1 2:3 / /"), None);
        // The fixture's own mount is found, and holds the fixture.
        let d = Dir::new("mount");
        let fd = d.fd("");
        let (mnt, _) = domain(fd.as_raw_fd()).unwrap();
        let point = mount_point(mnt).unwrap();
        assert!(
            fs::canonicalize(&d.0).unwrap().starts_with(&point),
            "{point:?}"
        );
        assert!(local_fs(fd.as_raw_fd()));
    }

    #[test]
    fn unescape_takes_only_whole_octal_escapes() {
        assert_eq!(unescape(b"/a\\040b"), b"/a b");
        assert_eq!(unescape(b"/a\\040"), b"/a ");
        assert_eq!(unescape(b"/a\\04"), b"/a\\04", "cut off at the end");
        assert_eq!(unescape(b"/a\\x41"), b"/a\\x41", "not octal");
        assert_eq!(unescape(b"\\134"), b"\\");
    }

    #[test]
    fn candidate_of_a_leaf_of_continuation_bytes() {
        let odd = [0x80u8; 300];
        assert_eq!(candidate(&odd, 0), None);
        assert_eq!(candidate(&[0x80u8; 3], 0).unwrap().as_bytes(), [0x80u8; 3]);
    }

    #[test]
    fn mount_point_is_the_mount_of_the_fixture() {
        let d = Dir::new("mountpoint");
        let (mnt, _) = domain(d.fd("").as_raw_fd()).unwrap();
        let point = mount_point(mnt).unwrap();
        let top = open_dir_follow(point.as_os_str().as_bytes()).unwrap();
        assert_eq!(domain(top.as_raw_fd()).unwrap().0, mnt, "{point:?}");
    }

    /// Only a regular file of at most TRASHINFO_MAX_BYTES is read; a FIFO
    /// at the name is never read (nor blocks).
    #[test]
    fn read_info_is_bounded_and_regular_only() {
        let d = Dir::new("readinfo");
        let dir = d.fd("");
        fs::write(d.0.join("max"), vec![b'a'; TRASHINFO_MAX_BYTES]).unwrap();
        fs::write(d.0.join("over"), vec![b'a'; TRASHINFO_MAX_BYTES + 1]).unwrap();
        crate::sys::tests::mkfifo(&d.0.join("fifo"));
        assert_eq!(
            read_info(dir.as_raw_fd(), c"max").unwrap().1.len(),
            TRASHINFO_MAX_BYTES
        );
        assert!(read_info(dir.as_raw_fd(), c"over").is_none());
        assert!(read_info(dir.as_raw_fd(), c"fifo").is_none());
    }

    /// The known folder still reaches the folder fd, but the leaf names
    /// another object: the place comes from the object's own fd.
    #[test]
    fn locate_never_takes_a_name_that_is_another_object() {
        let d = Dir::new("locate");
        fs::create_dir_all(d.0.join("p/x")).unwrap();
        let (p, x) = (d.fd("p"), d.fd("p/x"));
        fs::rename(d.0.join("p/x"), d.0.join("p/moved")).unwrap();
        fs::create_dir(d.0.join("p/x")).unwrap();
        let l = locate(
            Some(&d.0.join("p")),
            p.as_raw_fd(),
            c"x",
            x.as_raw_fd(),
            OP,
            CheckPoint::PostMove,
        );
        assert_eq!(l.checked_at, CheckPoint::PostResolve);
        assert_eq!(
            l.path_at_check,
            Some(fs::canonicalize(d.0.join("p/moved")).unwrap())
        );
        let l = locate(
            Some(&d.0.join("p")),
            p.as_raw_fd(),
            c"moved",
            x.as_raw_fd(),
            OP,
            CheckPoint::PostMove,
        );
        assert_eq!(l.checked_at, CheckPoint::PostMove);
        assert_eq!(l.path_at_check, Some(d.0.join("p/moved")));
    }

    /// The known folder renamed away and a lookalike made at its path
    /// (holding a same-named leaf): the lookalike is never the place; the
    /// object's own fd resolves the real one.
    #[test]
    fn locate_never_takes_a_lookalike_at_the_known_path() {
        let d = Dir::new("lookalike");
        fs::create_dir_all(d.0.join("p/x")).unwrap();
        let (p, x) = (d.fd("p"), d.fd("p/x"));
        fs::rename(d.0.join("p"), d.0.join("real")).unwrap();
        fs::create_dir_all(d.0.join("p/x")).unwrap();
        let l = locate(
            Some(&d.0.join("p")),
            p.as_raw_fd(),
            c"x",
            x.as_raw_fd(),
            OP,
            CheckPoint::PostRestore,
        );
        assert_eq!(l.checked_at, CheckPoint::PostResolve);
        assert_eq!(
            l.path_at_check,
            Some(fs::canonicalize(d.0.join("real/x")).unwrap())
        );
    }

    /// The path read from `/proc` turned into a symlink to the same folder
    /// before the reopen: that path is refused (no symlink on the way),
    /// and the next attempt proves the real one.
    #[test]
    fn resolve_refuses_a_path_through_a_symlink() {
        let d = Dir::new("resolve-link");
        fs::create_dir_all(d.0.join("a/b")).unwrap();
        let b = d.fd("a/b");
        let root = d.0.clone();
        let n = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let seen = n.clone();
        let _f = fault(move |p| {
            if p == Point::Resolve {
                if seen.get() == 0 {
                    fs::rename(root.join("a"), root.join("moved")).unwrap();
                    symlink(root.join("moved"), root.join("a")).unwrap();
                }
                seen.set(seen.get() + 1);
            }
            None
        });
        let Resolved::At(p) = resolve(b.as_raw_fd()) else {
            panic!("not resolved")
        };
        assert_eq!(p, fs::canonicalize(d.0.join("moved/b")).unwrap());
        assert_eq!(n.get(), 2);
    }

    /// A removed folder or a path longer than PATH_MAX is unverified at
    /// once: no reopen is tried.
    #[test]
    fn resolve_gives_up_at_once_on_a_removed_or_too_long_path() {
        let d = Dir::new("noreopen");
        let tries = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let t2 = tries.clone();
        let _f = fault(move |p| {
            if p == Point::Resolve {
                t2.set(t2.get() + 1);
            }
            None
        });
        fs::create_dir(d.0.join("x")).unwrap();
        let x = d.fd("x");
        fs::remove_dir(d.0.join("x")).unwrap();
        assert!(matches!(
            resolve(x.as_raw_fd()),
            Resolved::Unverified { .. }
        ));
        // Deeper than PATH_MAX, built relative to fds.
        let mut cur = d.fd("");
        let part = CString::new("d".repeat(200)).unwrap();
        for _ in 0..25 {
            // SAFETY: cur is an open folder; part is NUL-terminated.
            assert_eq!(
                unsafe { libc::mkdirat(cur.as_raw_fd(), part.as_ptr(), 0o700) },
                0
            );
            cur = open_at(
                cur.as_raw_fd(),
                &part,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .unwrap();
        }
        assert!(matches!(
            resolve(cur.as_raw_fd()),
            Resolved::Unverified { last: None }
        ));
        assert_eq!(tries.get(), 0);
    }

    /// Our name gone between the identity check and the detaching rename:
    /// nothing else is touched.
    #[test]
    fn close_info_when_its_name_vanishes_before_the_detach() {
        let d = Dir::new("vanish");
        let (info, i) = reserved(&d);
        let root = d.0.clone();
        let _f = fault(move |p| {
            if p == Point::Detach {
                fs::rename(root.join("x.trashinfo"), root.join("elsewhere")).unwrap();
            }
            None
        });
        let cl = close_info(info.as_raw_fd(), &i);
        assert_eq!(cl.state, InfoState::ForeignUntouched);
        assert_eq!(fs::read(d.0.join("elsewhere")).unwrap(), neutral(OP));
    }

    #[test]
    fn reasons_roundtrip_through_their_names() {
        use UndoReason::*;
        for r in [
            Changed,
            Missing,
            AncestorChanged,
            NoTrash,
            NameTaken,
            TooLong,
            HistoryNotWritten(28),
            InfoNotWritten(122),
            MoveRefused(18),
            PostMoveMismatch,
            InfoChanged,
            RestoreCollision,
            RestoreFailed,
            CommitNotRecorded,
            NotDurable,
            ContentNotNeutralized(5),
            Interrupted,
        ] {
            assert_eq!(UndoReason::parse(r.name(), r.errno()), Some(r));
        }
        assert_eq!(UndoReason::parse("MoveRefused", None), None);
        assert_eq!(UndoReason::parse("Changed", Some(1)), None);
        assert_eq!(UndoReason::parse("Nope", None), None);
    }

    // ---- mutation pass 3 ----

    /// The mountinfo line bound is 64 KiB (the derivation at the constant);
    /// no boundary test can see it, so it is pinned. Kills `*` -> `+`.
    #[test]
    fn the_mountinfo_line_bound_is_64_kib() {
        assert_eq!(MOUNTINFO_LINE_MAX, 65_536);
    }

    /// The neutral text over the first `k` bytes of our info, and nothing
    /// else, is ours still. Kills both `==` -> `!=` of `partly_neutral`.
    #[test]
    fn partly_neutral_accepts_exactly_a_neutral_prefix_over_our_text() {
        let ours = trashinfo(b"/some/place/x", "2026-10-27T10:00:00").unwrap();
        let n = neutral(OP);
        for k in 1..=n.len() {
            let mut b = n[..k].to_vec();
            b.extend_from_slice(&ours[k.min(ours.len())..]);
            assert!(partly_neutral(&b, &ours, OP), "k={k}");
            // One byte of the tail changed: someone else's.
            if k < ours.len() {
                let mut other = b.clone();
                *other.last_mut().unwrap() ^= 1;
                assert!(!partly_neutral(&other, &ours, OP), "k={k} tail");
            }
            // One byte of the neutral part changed.
            let mut other = b.clone();
            other[0] ^= 1;
            assert!(!partly_neutral(&other, &ours, OP), "k={k} head");
            // A length that fits no k.
            let mut longer = b.clone();
            longer.push(b'x');
            assert!(!partly_neutral(&longer, &ours, OP), "k={k} length");
        }
        assert!(!partly_neutral(&ours, &ours, OP));
        assert!(!partly_neutral(b"", &ours, OP));
    }

    /// `pwrite_all` returns the error of a write that fails (here: a
    /// descriptor open for reading). Kills `<` -> `==` on the result (the
    /// error would be counted as progress and reported as success).
    #[test]
    fn pwrite_all_reports_a_failed_write() {
        let d = Dir::new("pwrite");
        fs::write(d.0.join("f"), "x").unwrap();
        let f = fs::File::open(d.0.join("f")).unwrap();
        let e = pwrite_all(f.as_raw_fd(), b"abc").unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EBADF));
        let w = fs::OpenOptions::new()
            .write(true)
            .open(d.0.join("f"))
            .unwrap();
        pwrite_all(w.as_raw_fd(), b"abc").unwrap();
        assert_eq!(fs::read(d.0.join("f")).unwrap(), b"abc");
    }

    /// A reserved info whose file has more than we wrote (read back as
    /// equal over our length) is refused: the length is checked too. Kills
    /// `||` -> `&&` in `reserve`.
    #[test]
    fn reserve_refuses_a_file_longer_than_what_was_written() {
        let d = Dir::new("reserve-long");
        let info = d.fd("");
        let bytes = trashinfo(b"/x", "2026-10-27T10:00:00").unwrap();
        let path = d.0.join("x.trashinfo");
        let len = bytes.len();
        let _f = fault(move |p| {
            if p == Point::InfoWrite {
                fs::write(&path, vec![b'Z'; 4 * len]).unwrap();
            }
            None
        });
        let Err((_, Some(_))) = reserve(info.as_raw_fd(), c("x.trashinfo"), OP, &bytes) else {
            panic!("a longer file was accepted");
        };
    }

    /// The reopened descriptor is a path descriptor with close-on-exec.
    /// Kills `|` -> `&` in both places of `open_no_symlinks`' flags.
    #[test]
    fn open_no_symlinks_keeps_the_path_flag_and_close_on_exec() {
        let d = Dir::new("opath");
        let fd = open_no_symlinks(&c(d.0.to_str().unwrap()), libc::O_PATH).unwrap();
        // SAFETY: fd is open.
        let fl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(fl & libc::O_PATH, 0);
        // SAFETY: fd is open.
        let fdfl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(fdfl & libc::FD_CLOEXEC, 0);
    }

    /// The remote filesystems' magics are refused, the usual local ones
    /// are not; and a real local folder is local.
    #[test]
    fn remote_filesystem_magics_are_refused() {
        for m in REMOTE_FS {
            assert!(remote_magic(*m), "{m:#x}");
        }
        for local in [0xEF53, 0x0102_1994, 0x9123_683E, 0x5846_5342, 0x2FC1_2FC1] {
            assert!(!remote_magic(local), "{local:#x}");
        }
        let d = Dir::new("local");
        assert!(local_fs(d.fd("").as_raw_fd()));
    }

    /// A Trash folder name that is a FIFO is refused at once; the open
    /// must not wait for a writer. Kills `O_RDONLY | O_DIRECTORY` -> `&`
    /// in `private_dir`. Bounded wait on a thread; a blocked reader is
    /// freed by opening the write end.
    #[test]
    fn a_fifo_in_place_of_a_trash_folder_is_refused_without_waiting() {
        use std::os::unix::fs::OpenOptionsExt;
        let d = Dir::new("fifo");
        let fifo = d.0.join("files");
        let cp = c(fifo.to_str().unwrap());
        // SAFETY: cp is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(cp.as_ptr(), 0o700) }, 0);
        let root = d.fd("");
        let uid = euid();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(private_dir(root.as_raw_fd(), c"files", uid).is_err());
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(20));
        if got.is_err() {
            let _ = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo);
        }
        assert_eq!(got, Ok(true));
    }

    /// `\ooo` is octal, three digits.
    #[test]
    fn unescape_decodes_octal_escapes() {
        assert_eq!(unescape(b"a\\040b"), b"a b");
        assert_eq!(unescape(b"\\134x"), b"\\x");
        assert_eq!(unescape(b"\\377!"), [0xff, b'!']);
        assert_eq!(unescape(b"\\0408"), b" 8");
    }

    /// The shared `.Trash` of a volume that is a FIFO is refused at once
    /// and the per-user folder is used. Kills `O_RDONLY | O_DIRECTORY` ->
    /// `&` in `volume_trash_at`.
    #[test]
    fn a_fifo_named_dot_trash_is_not_waited_on() {
        let d = Dir::new("vol-fifo");
        crate::sys::tests::mkfifo(&d.0.join(".Trash"));
        let path = d.0.clone();
        let ok = crate::sys::tests::within(60, move || {
            let top = open_dir_follow(path.as_os_str().as_bytes()).unwrap();
            volume_trash_at(top.as_raw_fd(), &path, euid()).is_ok()
        });
        assert!(ok);
        assert!(d.0.join(format!(".Trash-{}", euid())).is_dir());
    }

    /// A mount point that is a FIFO is refused, not opened for reading.
    /// Kills `O_RDONLY | O_DIRECTORY` -> `&` in `volume_trash`.
    #[test]
    fn a_fifo_as_mount_point_is_not_waited_on() {
        let d = Dir::new("mnt-fifo");
        let fifo = d.0.join("mnt");
        crate::sys::tests::mkfifo(&fifo);
        let failed = crate::sys::tests::within(60, move || {
            TOPDIR.with_borrow_mut(|t| *t = Some(fifo));
            let r = volume_trash((1, 1)).is_err();
            TOPDIR.with_borrow_mut(|t| *t = None);
            r
        });
        assert!(failed);
    }

    /// A Trash whose `files` or `info` folder is another mount than the
    /// source (a second tmpfs in a user and mount namespace) is refused
    /// with `EXDEV`; one on the source's mount is not. Kills `||` -> `&&`
    /// in `trash_for` (a mutant an earlier review found surviving). Runs the
    /// same test as a helper under `unshare -Urm`; skipped where user
    /// namespaces are not available.
    #[test]
    fn trash_for_refuses_a_trash_folder_on_another_mount() {
        const NAME: &str = "trash::tests::trash_for_refuses_a_trash_folder_on_another_mount";
        if let Some(root) = std::env::var_os("INACIRCLE_SECOND_MOUNT") {
            second_mount_cases(Path::new(&root));
            println!("ran second mount helper");
            return;
        }
        let usable = std::process::Command::new("unshare")
            .args(["-Urm", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if !usable {
            // A safety test must not pass by skipping where the kernel
            // controls are required (CI sets this).
            assert!(
                std::env::var_os("INACIRCLE_REQUIRE_KERNEL_CONTROLS").is_none_or(|v| v != "1"),
                "INACIRCLE_REQUIRE_KERNEL_CONTROLS=1 but no user and mount namespaces"
            );
            eprintln!("skipped: no user and mount namespaces here");
            return;
        }
        let d = Dir::new("second-mount");
        let out = std::process::Command::new("unshare")
            .arg("-Urm")
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("INACIRCLE_SECOND_MOUNT", &d.0)
            .output()
            .unwrap();
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("ran second"),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Inside the namespace: a home Trash with `files` (then `info`) on a
    /// tmpfs of its own, and a control without a mount.
    fn second_mount_cases(root: &Path) {
        for which in ["none", "files", "info"] {
            let base = root.join(which);
            let trash = base.join("data/Trash");
            for sub in ["files", "info", "src"] {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(if sub == "src" {
                        base.join(sub)
                    } else {
                        trash.join(sub)
                    })
                    .unwrap();
            }
            if which != "none" {
                let target = c(trash.join(which).to_str().unwrap());
                // SAFETY: all arguments are NUL-terminated strings.
                let r = unsafe {
                    libc::mount(
                        c"tmpfs".as_ptr(),
                        target.as_ptr(),
                        c"tmpfs".as_ptr(),
                        0,
                        c"mode=0700".as_ptr().cast(),
                    )
                };
                assert_eq!(r, 0, "mount: {}", io::Error::last_os_error());
            }
            let data = base.join("data");
            let src = open_dir_follow(base.join("src").as_os_str().as_bytes()).unwrap();
            let r = with_env(&[("XDG_DATA_HOME", Some(data.to_str().unwrap()))], || {
                trash_for(src.as_raw_fd()).map(|_| ())
            });
            match (which, r) {
                ("none", Ok(())) => {}
                (w, Err(e)) if w != "none" => {
                    assert_eq!(e.raw_os_error(), Some(libc::EXDEV), "{w}: {e}");
                }
                (w, r) => panic!("{w}: {r:?}"),
            }
        }
    }
}
