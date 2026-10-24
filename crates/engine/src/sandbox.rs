//! The sandbox every action runs in: Landlock (filesystem, network, scope),
//! a seccomp filter (sockets, metadata) and resource limits.
//!
//! The parent builds the ruleset before spawning ([`build`], or [`create`]
//! then [`allow_read_fd`] and [`add`]); the child applies everything
//! between fork and exec ([`apply_in_child`], from
//! `std::process::Command::pre_exec`).
//!
//! # What is covered
//!
//! - Filesystem (Landlock): read + execute the exec paths, read the read
//!   paths, everything inside the write path; nothing else. Covers write,
//!   create, remove, rename/link out of place (REFER), truncate.
//! - Network: seccomp lets `socket`/`socketpair` create only `AF_UNIX`
//!   sockets (`EACCES` otherwise), which shuts out TCP, UDP, MPTCP, SCTP,
//!   ICMP, raw, netlink, packet. Landlock additionally denies TCP (ABI 4)
//!   and UDP (ABI 10) bind/connect/send on sockets that already exist.
//! - Unix sockets: pathname sockets outside the write path (ABI 9) and
//!   abstract sockets outside the sandbox (ABI 6) are denied.
//! - Signals to processes outside the sandbox (ABI 6) are denied.
//! - Metadata: seccomp denies (`EPERM`), everywhere and by path or fd:
//!   chmod, fchmod, fchmodat, fchmodat2; chown, fchown, lchown, fchownat;
//!   utime, utimes, futimesat, utimensat (so futimens); setxattr,
//!   lsetxattr, fsetxattr, setxattrat, removexattr, lremovexattr,
//!   fremovexattr, removexattrat; file_setattr; and the ioctls
//!   FS_IOC_SETFLAGS, FS_IOC32_SETFLAGS, FS_IOC_FSSETXATTR (chattr) and
//!   FS_IOC_ENABLE_VERITY (makes a file immutable). Every other ioctl is
//!   allowed.
//! - `setsid` and `setpgid` are denied (`EPERM`; x86_64 has no `setpgrp`
//!   syscall): the filter is installed after the child's own `setsid`, so
//!   the action and every descendant stay in one killable process group.
//! - `io_uring` is denied (`EPERM`): its SOCKET and xattr ops would bypass
//!   the seccomp filter.
//! - Inherited fds: every fd >= 3 is marked close-on-exec, so nothing
//!   opened before the sandbox reaches the action.
//! - `no_new_privs`, its own session, `RLIMIT_CORE` 0, `RLIMIT_FSIZE`, and
//!   the memory net `RLIMIT_DATA` (see [`data_cap`]), with or without Landlock.
//!
//! # What is not covered
//!
//! - Per Landlock ABI: below 3, truncate of any DAC-writable file; below
//!   4, Landlock does not restrict the network (seccomp still blocks
//!   non-unix sockets); below 5, device ioctls; below 6, abstract unix
//!   sockets and signals; below 9, pathname unix sockets (e.g. D-Bus),
//!   whose peer could also pass in a network socket via SCM_RIGHTS.
//! - Without Landlock, protection from ptrace-family access (ptrace,
//!   process_vm_*, pidfd_getfd) to the user's other processes depends on
//!   Yama (`kernel.yama.ptrace_scope`).
//! - Without Landlock only seccomp, the fd sweep and the limits apply.
//! - File names outside the sandbox can still be probed (`stat`, path
//!   lookup are not restricted).
//! - `RLIMIT_FSIZE` caps each file, not their sum: many files can still
//!   fill a disk.
//! - The seccomp filter exists for x86_64 only; elsewhere [`apply_in_child`]
//!   installs none (sockets and metadata are then not restricted).
//! - No `RLIMIT_NPROC`: an action can fork-bomb until it is cancelled.
//! - `RLIMIT_DATA` is per process, not per process tree: a forking action
//!   gets the cap once per child. File mmaps and shared memory are not
//!   counted.
//! - Keyring syscalls (`keyctl`, `add_key`, `request_key`) are allowed.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Read + execute paths every action needs to start, whatever exists of them.
pub const SYSTEM_EXEC_PATHS: [&str; 4] = ["/usr", "/lib", "/lib64", "/etc/ld.so.cache"];

// Landlock uapi, checked against include/uapi/linux/landlock.h.
const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
/// ABI 1: EXECUTE ..= MAKE_SYM.
const ACCESS_FS_V1: u64 = (1 << 13) - 1;
const ACCESS_FS_REFER: u64 = 1 << 13; // ABI 2
const ACCESS_FS_TRUNCATE: u64 = 1 << 14; // ABI 3
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5
const ACCESS_FS_RESOLVE_UNIX: u64 = 1 << 16; // ABI 9
/// The rights that apply to a non-directory; the kernel rejects others.
const ACCESS_FS_FILE: u64 = ACCESS_FS_EXECUTE
    | ACCESS_FS_WRITE_FILE
    | ACCESS_FS_READ_FILE
    | ACCESS_FS_TRUNCATE
    | ACCESS_FS_IOCTL_DEV;

const ACCESS_NET_BIND_TCP: u64 = 1 << 0; // ABI 4
const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1; // ABI 4
const ACCESS_NET_BIND_UDP: u64 = 1 << 2; // ABI 10
const ACCESS_NET_CONNECT_SEND_UDP: u64 = 1 << 3; // ABI 10

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0; // ABI 6
const SCOPE_SIGNAL: u64 = 1 << 1; // ABI 6

/// `struct landlock_ruleset_attr`, up to `scoped` (ABI 6). The kernel
/// accepts a struct longer than its own when the extra fields are zero,
/// and a shorter one (the later `quiet_*` fields, ABI 10, stay zero).
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// `struct landlock_path_beneath_attr` (packed in the uapi: 12 bytes).
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// A Landlock ruleset built by the parent. The fd is `O_CLOEXEC`, so it
/// disappears from the child at exec.
#[derive(Debug)]
pub struct Ruleset {
    fd: OwnedFd,
    /// The filesystem rights it handles.
    fs: u64,
}

impl AsFd for Ruleset {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for Ruleset {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// What an action may touch.
pub struct Policy<'a> {
    /// Read + execute. A path that does not exist is skipped.
    pub exec: &'a [PathBuf],
    /// Read only, followed if a symlink (the user's inputs: a link to a
    /// file is the file the user meant). Must exist.
    pub read: &'a [PathBuf],
    /// Read only, made by the engine (the `--inputs-from` file;
    /// intermediate outputs go through [`allow_read_fd`]): a symlink is
    /// refused, never followed. Must exist.
    pub read_exact: &'a [PathBuf],
    /// Full write (the work dir). Must exist.
    pub write: &'a Path,
}

/// Resource limits for the child. `RLIMIT_CORE` is always 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_FSIZE` in bytes.
    pub fsize: u64,
    /// `RLIMIT_DATA` in bytes (soft = hard), see [`data_cap`].
    pub data: u64,
}

/// The highest Landlock ABI the running kernel supports, or `None` when
/// Landlock is unavailable (not built in, or disabled at boot).
pub fn landlock_abi() -> Option<u32> {
    // SAFETY: with a NULL attr and size 0, the VERSION query reads no memory.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    u32::try_from(v).ok().filter(|&v| v >= 1)
}

/// The rights handled at `abi`, as (filesystem, network, scope): every
/// right that ABI knows, so whatever no rule grants is denied.
fn handled(abi: u32) -> (u64, u64, u64) {
    let mut fs = ACCESS_FS_V1;
    let mut net = 0;
    let mut scoped = 0;
    if abi >= 2 {
        fs |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        fs |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 4 {
        net |= ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP;
    }
    if abi >= 5 {
        fs |= ACCESS_FS_IOCTL_DEV;
    }
    if abi >= 6 {
        scoped |= SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL;
    }
    if abi >= 9 {
        fs |= ACCESS_FS_RESOLVE_UNIX;
    }
    if abi >= 10 {
        net |= ACCESS_NET_BIND_UDP | ACCESS_NET_CONNECT_SEND_UDP;
    }
    (fs, net, scoped)
}

fn build_for_abi(policy: &Policy, abi: u32) -> io::Result<Ruleset> {
    let ruleset = create_for_abi(abi)?;
    add(&ruleset, policy)?;
    Ok(ruleset)
}

/// A ruleset handling every right `abi` knows, with no rules yet.
fn create_for_abi(abi: u32) -> io::Result<Ruleset> {
    let (fs, net, scoped) = handled(abi);
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: net,
        scoped,
    };
    // SAFETY: `attr` is a valid RulesetAttr of the size passed; the kernel
    // only reads it. A non-negative return is a new fd we own.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned to us (O_CLOEXEC by the kernel) and
    // nothing else owns it.
    Ok(Ruleset {
        fd: unsafe { OwnedFd::from_raw_fd(fd as RawFd) },
        fs,
    })
}

/// Adds the rules of `policy` to `ruleset`.
pub fn add(ruleset: &Ruleset, policy: &Policy) -> io::Result<()> {
    let fs = ruleset.fs;
    let exec = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
    for p in policy.exec {
        match allow(ruleset, p, exec & fs) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            r => r?,
        }
    }
    let read = (ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR) & fs;
    for p in policy.read {
        allow(ruleset, p, read)?;
    }
    for p in policy.read_exact {
        allow_at(ruleset, p, read, libc::O_NOFOLLOW)?;
    }
    allow(ruleset, policy.write, fs)
}

/// Grants reading the file open as `fd` (an `O_PATH` fd the engine has
/// checked; `path` only names it in errors). The rule is on that inode,
/// whatever its path resolves to by now.
pub fn allow_read_fd(ruleset: &Ruleset, fd: BorrowedFd, path: &Path) -> io::Result<()> {
    let read = (ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR) & ruleset.fs;
    allow_fd(ruleset, fd, read, path)
}

/// Adds a rule granting `access` beneath `path`, limited to the
/// file-applicable rights when `path` is not a directory.
fn allow(ruleset: &Ruleset, path: &Path, access: u64) -> io::Result<()> {
    allow_at(ruleset, path, access, 0)
}

/// [`allow`], opening `path` with `O_PATH | flags`. With `O_NOFOLLOW` a
/// symlink leaf opens as the link itself and is refused.
fn allow_at(ruleset: &Ruleset, path: &Path, access: u64, flags: libc::c_int) -> io::Result<()> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | flags)
        .open(path)?;
    allow_fd(ruleset, f.as_fd(), access, path)
}

/// [`allow`] for the file open as `fd`; a symlink is refused.
fn allow_fd(ruleset: &Ruleset, fd: BorrowedFd, access: u64, path: &Path) -> io::Result<()> {
    // SAFETY: an all-zero stat is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fd is open (O_PATH fds included) and st a writable stat.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = st.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("landlock rule for {}: is a symlink", path.display()),
        ));
    }
    let access = if kind == libc::S_IFDIR {
        access
    } else {
        access & ACCESS_FS_FILE
    };
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: fd.as_raw_fd(),
    };
    // SAFETY: both fds are open for the duration of the call and `attr` is
    // a valid PathBeneathAttr; the kernel only reads it.
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            e.kind(),
            format!("landlock rule for {}: {e}", path.display()),
        ));
    }
    Ok(())
}

/// An empty ruleset at the best available ABI, for [`add`] and
/// [`allow_read_fd`]. `Ok(None)` when Landlock is unavailable.
pub fn create() -> io::Result<Option<Ruleset>> {
    landlock_abi().map(create_for_abi).transpose()
}

/// Builds the ruleset for `policy` at the best available ABI. `Ok(None)`
/// when Landlock is unavailable.
pub fn build(policy: &Policy) -> io::Result<Option<Ruleset>> {
    match landlock_abi() {
        Some(abi) => build_for_abi(policy, abi).map(Some),
        None => Ok(None),
    }
}

/// `RLIMIT_FSIZE`: 16 x the total input size, at least 256 MiB, at most 16 GiB.
/// `RLIMIT_DATA`: [`data_cap`] of this machine's RAM (read once per call).
pub fn limits_for(inputs_total: u64) -> Limits {
    const MIN: u64 = 256 << 20;
    const MAX: u64 = 16 << 30;
    Limits {
        fsize: inputs_total.saturating_mul(16).clamp(MIN, MAX),
        data: data_cap(total_ram()),
    }
}

/// The lower bound of [`data_cap`]: 1 GiB. The Phase 6 pixel limit is
/// 180 MP (Pillow rejects above 179 MP as a decompression bomb); 180 MP
/// as RGBA8 is 720 MB, and the simplest operation (a format change) must
/// fit even on the smallest machine we support (8 GB RAM, where 1/8 is
/// exactly 1 GiB).
const DATA_CAP_MIN: u64 = 1 << 30;

/// The upper bound of [`data_cap`]: 4 GiB. A need beyond it signals a bug
/// in our own per-family limits, not a legitimate use (the largest
/// legitimate user, a 300 MP stitched panorama as RGBA8, is 1.2 GB; even
/// with 16-bit channels and a 2x operation factor it stays near 4 GiB),
/// and the net must not let a bug grow to most of a big machine's RAM.
const DATA_CAP_MAX: u64 = 4 << 30;

/// The divisor of [`data_cap`]: an action gets 1/8 of the machine's RAM.
/// Spawn-per-use means several actions can run at once, so one must not be
/// able to take a large share of the RAM. 1/8 is 1 GiB on 8 GB (12.5%),
/// 2 GiB on 16 GB and reaches the 4 GiB bound at 32 GB.
const DATA_CAP_RAM_DIVISOR: u64 = 8;

/// The `RLIMIT_DATA` of a sandboxed action on a machine with `total_ram`
/// bytes of RAM: `total_ram / 8`, clamped to 1 GiB ..= 4 GiB.
///
/// This is the last net against a faulty or missing per-family limit
/// (Phase 6 and 7 set those), not the protection itself.
///
/// - Why `DATA`, not `AS`: `RLIMIT_AS` caps the virtual address space, and
///   glibc reserves a 64 MB malloc arena per thread virtually, so a
///   multi-threaded action would fail long before it uses real memory.
///   `RLIMIT_DATA` (Linux 4.7 and later) counts the heap and private
///   writable anonymous mappings: what we want to bound. File mmaps and
///   the code of shared libraries do not count.
/// - Why a fixed, machine-dependent cap and not one derived from the
///   input: memory use does not follow the input size. A 3 MB JPEG is 48
///   MB decoded; a 50 KB PNG can be gigabytes of pixels.
/// - Why 1/8 and 1 GiB to 4 GiB: see the constants. A user with a 12 MP
///   phone photo needs 48 MB, a 61 MP DSLR frame 244 MB (488 MB at 16
///   bit), a 139 MP 1200 dpi A4 scan 557 MB (1.1 GB at 16 bit), a 200 MP
///   phone 800 MB. On 8 GB, a 180 MP progressive JPEG (about 1.26 GB)
///   does not fit and fails cleanly; from 16 GB it does.
///
/// Over the cap an allocation fails (`ENOMEM`), and the action fails with a
/// clear message; the machine stays up.
pub fn data_cap(total_ram: u64) -> u64 {
    (total_ram / DATA_CAP_RAM_DIVISOR).clamp(DATA_CAP_MIN, DATA_CAP_MAX)
}

/// The machine's RAM in bytes (`sysinfo(2)`: `totalram * mem_unit`), or 0
/// when the call fails (then [`data_cap`] gives its lower bound).
pub fn total_ram() -> u64 {
    // SAFETY: an all-zero sysinfo is a valid value.
    let mut si: libc::sysinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `si` is a writable sysinfo.
    if unsafe { libc::sysinfo(&mut si) } != 0 {
        return 0;
    }
    (si.totalram as u64).saturating_mul(u64::from(si.mem_unit))
}

/// The seccomp filter: deny non-unix sockets, metadata writes and io_uring.
/// Syscall numbers from arch/x86/entry/syscalls/syscall_64.tbl, the
/// `seccomp_data` layout from include/uapi/linux/seccomp.h.
#[cfg(target_arch = "x86_64")]
mod seccomp {
    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JGE, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
        SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS, sock_filter,
    };

    /// EM_X86_64 | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE (not in libc).
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    // Offsets in struct seccomp_data.
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    const ARG0_LOW: u32 = 16; // little endian
    const ARG1_LOW: u32 = 24; // ioctl's cmd is an unsigned int
    const SYS_SOCKET: u32 = 41;
    const SYS_SOCKETPAIR: u32 = 53;
    const SYS_IOCTL: u32 = 16;
    /// ioctl commands denied with EPERM: the inode flag setters
    /// (include/uapi/linux/fs.h) and fs-verity enabling
    /// (include/uapi/linux/fsverity.h), which all work through a read-only fd.
    const FS_IOC_SETFLAGS: u32 = 0x4008_6602; // _IOW('f', 2, long)
    const FS_IOC32_SETFLAGS: u32 = 0x4004_6602; // _IOW('f', 2, int)
    const FS_IOC_FSSETXATTR: u32 = 0x401c_5820; // _IOW('X', 32, struct fsxattr)
    const FS_IOC_ENABLE_VERITY: u32 = 0x4080_6685; // _IOW('f', 133, 128-byte arg)
    /// Denied with EPERM.
    const DENIED: [u32; 26] = [
        90,  // chmod
        91,  // fchmod
        92,  // chown
        93,  // fchown
        94,  // lchown
        109, // setpgid
        112, // setsid
        132, // utime
        188, // setxattr
        189, // lsetxattr
        190, // fsetxattr
        197, // removexattr
        198, // lremovexattr
        199, // fremovexattr
        235, // utimes
        260, // fchownat
        261, // futimesat
        268, // fchmodat
        280, // utimensat
        425, // io_uring_setup
        426, // io_uring_enter
        427, // io_uring_register
        452, // fchmodat2 (not in every libc)
        463, // setxattrat (not in libc)
        466, // removexattrat (not in libc)
        469, // file_setattr (not in libc)
    ];

    // Layout: [arch check: 3] [nr load, x32, socket, socketpair, ioctl: 5]
    // [DENIED] [ALLOW] [EPERM] [socket: load arg0, AF_UNIX?, ALLOW, EACCES]
    // [ioctl: load arg1, 4 x cmd?, ALLOW, EPERM].
    const FIRST_DENIED: usize = 8;
    const ALLOW: usize = FIRST_DENIED + DENIED.len();
    const EPERM: usize = ALLOW + 1;
    const SOCKET: usize = EPERM + 1;
    const IOCTL: usize = SOCKET + 4;
    const IOCTL_EPERM: usize = IOCTL + 6;
    const LEN: usize = IOCTL_EPERM + 1;

    const fn stmt(code: u32, k: u32) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }

    /// A conditional jump at `at` to `to` when true, to the next when false.
    const fn jump(at: usize, code: u32, k: u32, to: usize) -> sock_filter {
        sock_filter {
            code: (BPF_JMP | code | BPF_K) as u16,
            jt: (to - at - 1) as u8,
            jf: 0,
            k,
        }
    }

    const fn program() -> [sock_filter; LEN] {
        let mut p = [stmt(0, 0); LEN];
        p[0] = stmt(BPF_LD | BPF_W | BPF_ABS, ARCH);
        p[1] = sock_filter {
            code: (BPF_JMP | BPF_JEQ | BPF_K) as u16,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH_X86_64,
        };
        p[2] = stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS);
        p[3] = stmt(BPF_LD | BPF_W | BPF_ABS, NR);
        p[4] = jump(4, BPF_JGE, X32_SYSCALL_BIT, EPERM);
        p[5] = jump(5, BPF_JEQ, SYS_SOCKET, SOCKET);
        p[6] = jump(6, BPF_JEQ, SYS_SOCKETPAIR, SOCKET);
        p[7] = jump(7, BPF_JEQ, SYS_IOCTL, IOCTL);
        let mut i = 0;
        while i < DENIED.len() {
            let at = FIRST_DENIED + i;
            p[at] = jump(at, BPF_JEQ, DENIED[i], EPERM);
            i += 1;
        }
        p[ALLOW] = stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
        p[EPERM] = stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | libc::EPERM as u32);
        p[SOCKET] = stmt(BPF_LD | BPF_W | BPF_ABS, ARG0_LOW);
        p[SOCKET + 1] = sock_filter {
            code: (BPF_JMP | BPF_JEQ | BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: libc::AF_UNIX as u32,
        };
        p[SOCKET + 2] = stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
        p[SOCKET + 3] = stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | libc::EACCES as u32);
        p[IOCTL] = stmt(BPF_LD | BPF_W | BPF_ABS, ARG1_LOW);
        p[IOCTL + 1] = jump(IOCTL + 1, BPF_JEQ, FS_IOC_SETFLAGS, IOCTL_EPERM);
        p[IOCTL + 2] = jump(IOCTL + 2, BPF_JEQ, FS_IOC32_SETFLAGS, IOCTL_EPERM);
        p[IOCTL + 3] = jump(IOCTL + 3, BPF_JEQ, FS_IOC_FSSETXATTR, IOCTL_EPERM);
        p[IOCTL + 4] = jump(IOCTL + 4, BPF_JEQ, FS_IOC_ENABLE_VERITY, IOCTL_EPERM);
        p[IOCTL + 5] = stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
        p[IOCTL_EPERM] = stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | libc::EPERM as u32);
        p
    }

    pub static FILTER: [sock_filter; LEN] = program();
}

/// The seccomp filter for this architecture; `None` where there is none yet
/// (only x86_64 has one).
fn seccomp_filter() -> Option<&'static [libc::sock_filter]> {
    #[cfg(target_arch = "x86_64")]
    return Some(&seccomp::FILTER);
    #[cfg(not(target_arch = "x86_64"))]
    return None;
}

/// Puts the calling process in its own session, applies `limits`, sets
/// `no_new_privs`, enforces the ruleset if given, installs the seccomp
/// filter (even without a ruleset) and marks every fd >= 3 close-on-exec.
///
/// # Safety
///
/// Runs in the child after fork and before exec (`Command::pre_exec`):
/// it makes only async-signal-safe calls (no allocation, no locks). The
/// caller guarantees that `ruleset`, if given, is an open Landlock ruleset
/// fd (kept alive by the parent until the spawn returns).
pub unsafe fn apply_in_child(ruleset: Option<RawFd>, limits: &Limits) -> io::Result<()> {
    fn check(r: libc::c_long) -> io::Result<()> {
        // last_os_error only reads errno: no allocation.
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    let fsize = libc::rlimit {
        rlim_cur: limits.fsize,
        rlim_max: limits.fsize,
    };
    let data = libc::rlimit {
        rlim_cur: limits.data,
        rlim_max: limits.data,
    };
    let core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setsid, setrlimit, prctl and the raw syscalls are
    // async-signal-safe; the rlimit structs and `prog` live on this stack
    // frame, `prog` points at a static program of `len` instructions, and
    // the caller guarantees `fd` is an open ruleset.
    unsafe {
        check(libc::setsid().into())?;
        check(libc::setrlimit(libc::RLIMIT_FSIZE, &fsize).into())?;
        check(libc::setrlimit(libc::RLIMIT_DATA, &data).into())?;
        check(libc::setrlimit(libc::RLIMIT_CORE, &core).into())?;
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0).into())?;
        if let Some(fd) = ruleset {
            check(libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32))?;
        }
        if let Some(filter) = seccomp_filter() {
            let prog = libc::sock_fprog {
                len: filter.len() as libc::c_ushort,
                // The kernel only reads the program.
                filter: filter.as_ptr().cast_mut(),
            };
            check(libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog).into())?;
        }
        check(libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        ))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::net::{TcpListener, UdpSocket};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "inacircle-sandbox-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The running ABI, or `None` after saying the test is skipped.
    fn abi_or_skip(test: &str, min: u32) -> Option<u32> {
        match landlock_abi() {
            Some(abi) if abi >= min => Some(abi),
            Some(abi) => {
                eprintln!("SKIPPED {test}: needs Landlock ABI >= {min}, kernel has {abi}");
                None
            }
            None => {
                assert!(
                    std::env::var_os("INACIRCLE_REQUIRE_LANDLOCK").is_none_or(|v| v != "1"),
                    "{test}: Landlock unavailable but INACIRCLE_REQUIRE_LANDLOCK=1"
                );
                eprintln!("SKIPPED {test}: Landlock unavailable on this kernel");
                None
            }
        }
    }

    /// Whether this target has the seccomp filter; says the test is skipped if not.
    pub(crate) fn seccomp_or_skip(test: &str) -> bool {
        let has = seccomp_filter().is_some();
        if !has {
            eprintln!("SKIPPED {test}: no seccomp filter for this architecture");
        }
        has
    }

    fn system_exec() -> Vec<PathBuf> {
        SYSTEM_EXEC_PATHS.iter().map(PathBuf::from).collect()
    }

    fn ruleset(read: &[PathBuf], write: &Path) -> Ruleset {
        let exec = system_exec();
        let policy = Policy {
            exec: &exec,
            read,
            read_exact: &[],
            write,
        };
        build(&policy).unwrap().expect("Landlock available")
    }

    const NO_LIMIT: Limits = Limits {
        fsize: 1 << 40, // far above anything a test writes
        data: 1 << 44,  // far above any test's heap
    };

    /// Runs `/bin/sh -c script` under `ruleset` and `limits`.
    fn sh(ruleset: Option<&Ruleset>, limits: Limits, script: &str) -> Output {
        sh_with_fd9(ruleset, limits, script, -1)
    }

    /// `sh`, with `leak` (if >= 0) dup'd to fd 9 in the child before the
    /// sandbox is applied.
    fn sh_with_fd9(ruleset: Option<&Ruleset>, limits: Limits, script: &str, leak: RawFd) -> Output {
        let fd = ruleset.map(|r| r.as_raw_fd());
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        if leak >= 0 {
            // SAFETY: dup2 is async-signal-safe.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::dup2(leak, 9) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        // SAFETY: apply_in_child only makes async-signal-safe calls; the
        // ruleset outlives the spawn (borrowed for this whole function).
        unsafe {
            cmd.pre_exec(move || apply_in_child(fd, &limits));
        }
        cmd.output().unwrap()
    }

    /// How a probe runs: unsandboxed (the control), or through
    /// `apply_in_child` with an optional ruleset.
    #[derive(Clone, Copy)]
    enum Sb<'a> {
        Off,
        On(Option<&'a Ruleset>),
    }

    /// Spawns `/bin/true`; in the child, runs `before`, applies the sandbox
    /// (per `sb`), then runs `after` on what `before` returned. An error
    /// comes back from the spawn. Both must be async-signal-safe.
    fn probe_with<S, B, A>(sb: Sb, before: B, after: A) -> io::Result<()>
    where
        B: Fn() -> io::Result<S> + Send + Sync + 'static,
        A: Fn(S) -> io::Result<()> + Send + Sync + 'static,
    {
        let sandbox = match sb {
            Sb::Off => None,
            Sb::On(rs) => Some(rs.map(|r| r.as_raw_fd())),
        };
        let mut cmd = Command::new("/bin/true");
        cmd.stdin(Stdio::null());
        // SAFETY: apply_in_child and every probe below only make
        // async-signal-safe calls (raw syscalls on stack data); the ruleset
        // is borrowed for this whole function.
        unsafe {
            cmd.pre_exec(move || {
                let state = before()?;
                if let Some(fd) = sandbox {
                    apply_in_child(fd, &NO_LIMIT)?;
                }
                after(state)
            });
        }
        let mut child = cmd.spawn()?;
        assert!(child.wait()?.success());
        Ok(())
    }

    fn probe<F>(sb: Sb, f: F) -> io::Result<()>
    where
        F: Fn() -> io::Result<()> + Send + Sync + 'static,
    {
        probe_with(sb, || Ok(()), move |()| f())
    }

    fn errno(r: io::Result<()>) -> Option<i32> {
        r.err().and_then(|e| e.raw_os_error())
    }

    fn cvt(r: libc::c_int) -> io::Result<()> {
        if r == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn stderr(o: &Output) -> String {
        String::from_utf8_lossy(&o.stderr).into_owned()
    }

    #[test]
    fn abi_detected_on_this_kernel() {
        let Some(abi) = abi_or_skip("abi_detected_on_this_kernel", 1) else {
            return;
        };
        assert!(abi >= 1);
    }

    #[test]
    fn limits_follow_the_formula() {
        const MIB: u64 = 1 << 20;
        const GIB: u64 = 1 << 30;
        assert_eq!(limits_for(0).fsize, 256 * MIB);
        assert_eq!(limits_for(16 * MIB).fsize, 256 * MIB);
        assert_eq!(limits_for(100 * MIB).fsize, 1600 * MIB);
        assert_eq!(limits_for(GIB).fsize, 16 * GIB);
        assert_eq!(limits_for(2 * GIB).fsize, 16 * GIB);
        assert_eq!(limits_for(u64::MAX).fsize, 16 * GIB);
    }

    #[test]
    fn data_cap_bounds() {
        const GIB: u64 = 1 << 30;
        assert_eq!(data_cap(0), GIB);
        assert_eq!(data_cap(4 * GIB), GIB);
        assert_eq!(data_cap(8 * GIB), GIB);
        assert_eq!(data_cap(16 * GIB), 2 * GIB);
        assert_eq!(data_cap(32 * GIB), 4 * GIB);
        assert_eq!(data_cap(96 * GIB), 4 * GIB);
        assert_eq!(data_cap(u64::MAX), 4 * GIB);
    }

    #[test]
    fn handled_rights_follow_the_abi() {
        let base = (1 << 13) - 1;
        assert_eq!(handled(1), (base, 0, 0));
        assert_eq!(handled(2).0, base | ACCESS_FS_REFER);
        assert_eq!(handled(3).0, base | ACCESS_FS_REFER | ACCESS_FS_TRUNCATE);
        assert_eq!(handled(3).1, 0);
        assert_eq!(handled(4).1, ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP);
        assert_eq!(handled(4).0, handled(3).0);
        assert_eq!(handled(5).0, handled(3).0 | ACCESS_FS_IOCTL_DEV);
        assert_eq!(handled(5).2, 0);
        assert_eq!(handled(6).2, SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL);
        assert_eq!(handled(8), handled(6));
        assert_eq!(handled(9).0, handled(5).0 | ACCESS_FS_RESOLVE_UNIX);
        assert_eq!(handled(10).1, (1 << 4) - 1);
        assert_eq!(handled(11), handled(10));
    }

    #[test]
    fn write_inside_allowed() {
        if abi_or_skip("write_inside_allowed", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let script = format!(
            "set -e; cd '{0}'; mkdir d; echo hi > d/f; mv d/f g; ln -s g s; cat s; rm g s; rmdir d",
            work.0.display()
        );
        let o = sh(Some(&rs), NO_LIMIT, &script);
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(o.stdout, b"hi\n");
    }

    #[test]
    fn write_outside_denied() {
        if abi_or_skip("write_outside_denied", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let rs = ruleset(&[], &work.0);
        let target = other.0.join("f");
        let o = sh(
            Some(&rs),
            NO_LIMIT,
            &format!("echo x > '{}'", target.display()),
        );
        assert!(!o.status.success());
        assert!(stderr(&o).contains("Permission denied"), "{}", stderr(&o));
        assert!(!target.exists());
        // Control: the same write succeeds without the sandbox.
        let o = sh(None, NO_LIMIT, &format!("echo x > '{}'", target.display()));
        assert!(o.status.success(), "{}", stderr(&o));
    }

    #[test]
    fn read_of_unlisted_file_denied() {
        if abi_or_skip("read_of_unlisted_file_denied", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let listed = other.0.join("listed");
        let unlisted = other.0.join("unlisted");
        fs::write(&listed, "yes\n").unwrap();
        fs::write(&unlisted, "secret\n").unwrap();
        let rs = ruleset(std::slice::from_ref(&listed), &work.0);
        let o = sh(
            Some(&rs),
            NO_LIMIT,
            &format!("cat '{}'", unlisted.display()),
        );
        assert!(!o.status.success());
        assert!(stderr(&o).contains("Permission denied"), "{}", stderr(&o));
        assert!(o.stdout.is_empty());
        // The listed sibling is readable, the directory is not listable.
        let o = sh(Some(&rs), NO_LIMIT, &format!("cat '{}'", listed.display()));
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(o.stdout, b"yes\n");
        let o = sh(Some(&rs), NO_LIMIT, &format!("ls '{}'", other.0.display()));
        assert!(!o.status.success());
    }

    #[test]
    fn exact_read_path_refuses_a_symlink() {
        if abi_or_skip("exact_read_path_refuses_a_symlink", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let secret = other.0.join("secret");
        let link = work.0.join("link");
        fs::write(&secret, "secret\n").unwrap();
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let exec = system_exec();
        let exact = [link.clone()];
        let policy = Policy {
            exec: &exec,
            read: &[],
            read_exact: &exact,
            write: &other.0,
        };
        let e = build(&policy).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        // A regular file is granted and readable.
        let exact = [secret.clone()];
        let policy = Policy {
            exec: &exec,
            read: &[],
            read_exact: &exact,
            write: &work.0,
        };
        let rs = build(&policy).unwrap().unwrap();
        let o = sh(Some(&rs), NO_LIMIT, &format!("cat '{}'", secret.display()));
        assert_eq!(o.stdout, b"secret\n", "{}", stderr(&o));
    }

    #[test]
    fn listed_input_dir_readable_not_writable_not_executable() {
        if abi_or_skip("listed_input_dir_readable_not_writable_not_executable", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let input = Tmp::new("input");
        fs::write(input.0.join("a"), "a\n").unwrap();
        fs::copy("/bin/true", input.0.join("true")).unwrap();
        let rs = ruleset(std::slice::from_ref(&input.0), &work.0);
        let dir = input.0.display();
        let o = sh(Some(&rs), NO_LIMIT, &format!("ls '{dir}' && cat '{dir}/a'"));
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(o.stdout, b"a\ntrue\na\n");
        let o = sh(Some(&rs), NO_LIMIT, &format!("echo x >> '{dir}/a'"));
        assert!(!o.status.success());
        let o = sh(Some(&rs), NO_LIMIT, &format!("rm -f '{dir}/a'"));
        assert!(!o.status.success());
        assert_eq!(fs::read(input.0.join("a")).unwrap(), b"a\n");
        let o = sh(Some(&rs), NO_LIMIT, &format!("'{dir}/true'"));
        assert_eq!(o.status.code(), Some(126), "{}", stderr(&o));
        // Control: the copy runs without the sandbox.
        let o = sh(None, NO_LIMIT, &format!("'{dir}/true'"));
        assert!(o.status.success(), "{}", stderr(&o));
    }

    #[test]
    fn missing_exec_path_skipped() {
        if abi_or_skip("missing_exec_path_skipped", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let mut exec = system_exec();
        exec.push(work.0.join("no-such-binary"));
        let policy = Policy {
            exec: &exec,
            read: &[],
            read_exact: &[],
            write: &work.0,
        };
        let rs = build(&policy).unwrap().unwrap();
        let o = sh(Some(&rs), NO_LIMIT, "echo ok");
        assert!(o.status.success(), "{}", stderr(&o));
    }

    #[test]
    fn missing_read_or_write_path_is_an_error() {
        if abi_or_skip("missing_read_or_write_path_is_an_error", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let exec = system_exec();
        let read = [work.0.join("gone")];
        let policy = Policy {
            exec: &exec,
            read: &read,
            read_exact: &[],
            write: &work.0,
        };
        let e = build(&policy).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let gone = work.0.join("gone-dir");
        let policy = Policy {
            exec: &exec,
            read: &[],
            read_exact: &[],
            write: &gone,
        };
        assert_eq!(build(&policy).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn every_abi_level_builds_and_enforces() {
        let Some(top) = abi_or_skip("every_abi_level_builds_and_enforces", 1) else {
            return;
        };
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let exec = system_exec();
        let policy = Policy {
            exec: &exec,
            read: &[],
            read_exact: &[],
            write: &work.0,
        };
        for abi in 1..=top {
            let rs = build_for_abi(&policy, abi).unwrap();
            let inside = work.0.join(format!("in{abi}"));
            let outside = other.0.join(format!("out{abi}"));
            let script = format!(
                "echo x > '{}'; echo x > '{}'",
                inside.display(),
                outside.display()
            );
            let o = sh(Some(&rs), NO_LIMIT, &script);
            assert!(inside.exists(), "ABI {abi}: {}", stderr(&o));
            assert!(!outside.exists(), "ABI {abi}: wrote outside");
        }
    }

    #[test]
    fn fsize_limit_gives_efbig() {
        let work = Tmp::new("work");
        let out = work.0.join("big");
        let limits = Limits {
            fsize: 1 << 20,
            ..NO_LIMIT
        };
        let script = format!(
            "trap '' XFSZ; dd if=/dev/zero of='{}' bs=64k count=32 2>&1",
            out.display()
        );
        let o = sh(None, limits, &script);
        assert!(!o.status.success());
        let msg = String::from_utf8_lossy(&o.stdout);
        assert!(msg.contains("File too large"), "{msg}");
        assert_eq!(fs::metadata(&out).unwrap().len(), 1 << 20);
    }

    #[test]
    fn data_limit_is_set_soft_and_hard() {
        let limits = Limits {
            data: 2 << 30,
            ..NO_LIMIT
        };
        // Without Landlock: the net does not depend on it. `ulimit` is in KiB.
        let o = sh(None, limits, "ulimit -d; ulimit -H -d");
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(o.stdout, b"2097152\n2097152\n");
    }

    #[test]
    fn core_limit_zero() {
        let o = sh(None, NO_LIMIT, "ulimit -c; ulimit -H -c");
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(o.stdout, b"0\n0\n");
    }

    #[test]
    fn child_is_session_leader_with_no_new_privs() {
        let r = probe(Sb::On(None), || {
            // SAFETY: getsid, getpid and prctl are async-signal-safe and
            // take no pointers.
            let (sid, pid, nnp) = unsafe {
                (
                    libc::getsid(0),
                    libc::getpid(),
                    libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
                )
            };
            if sid != pid {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if nnp != 1 {
                return Err(io::Error::from_raw_os_error(libc::EPROTO));
            }
            Ok(())
        });
        r.unwrap();
    }

    fn sockaddr_in(port: u16) -> libc::sockaddr_in {
        libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be(),
            },
            sin_zero: [0; 8],
        }
    }

    fn sockaddr_in6(port: u16) -> libc::sockaddr_in6 {
        // SAFETY: sockaddr_in6 is plain data; all zeroes is a valid value.
        let mut a: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        a.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        a.sin6_port = port.to_be();
        a.sin6_addr.s6_addr = std::net::Ipv6Addr::LOCALHOST.octets();
        a
    }

    /// socket(2), async-signal-safe. The fd is left open: the probe child
    /// execs right after (and everything >= 3 is close-on-exec).
    fn socket(domain: i32, ty: i32, proto: i32) -> io::Result<RawFd> {
        // SAFETY: socket takes no pointers.
        let s = unsafe { libc::socket(domain, ty | libc::SOCK_CLOEXEC, proto) };
        cvt(s)?;
        Ok(s)
    }

    type AddrCall =
        unsafe extern "C" fn(libc::c_int, *const libc::sockaddr, libc::socklen_t) -> libc::c_int;

    /// bind(2) or connect(2) on `s` with `addr`, async-signal-safe.
    fn addr_call<T: Copy>(f: AddrCall, s: RawFd, addr: &T) -> io::Result<()> {
        // SAFETY: `f` reads exactly size_of::<T>() bytes from `addr`, a
        // valid sockaddr_* on our stack.
        cvt(unsafe {
            f(
                s,
                (addr as *const T).cast(),
                size_of::<T>() as libc::socklen_t,
            )
        })
    }

    /// socket + connect, async-signal-safe.
    fn connect<T: Copy>(domain: i32, ty: i32, addr: &T) -> io::Result<()> {
        let s = socket(domain, ty, 0)?;
        addr_call(libc::connect, s, addr)
    }

    /// A socket created before the sandbox (so seccomp does not stop it),
    /// bound to 127.0.0.1:0 first when `prebind`.
    fn early_socket(domain: i32, ty: i32, prebind: bool) -> io::Result<RawFd> {
        let s = socket(domain, ty, 0)?;
        if prebind {
            addr_call(libc::bind, s, &sockaddr_in(0))?;
        }
        Ok(s)
    }

    #[test]
    fn tcp_connect_denied() {
        if abi_or_skip("tcp_connect_denied", 4).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = sockaddr_in(l.local_addr().unwrap().port());
        let before = || early_socket(libc::AF_INET, libc::SOCK_STREAM, false);
        let after = move |s| addr_call(libc::connect, s, &addr);
        probe_with(Sb::Off, before, after).unwrap();
        let r = probe_with(Sb::On(Some(&rs)), before, after);
        assert_eq!(errno(r), Some(libc::EACCES));
    }

    #[test]
    fn tcp6_connect_denied() {
        if abi_or_skip("tcp6_connect_denied", 4).is_none() {
            return;
        }
        let Ok(l) = TcpListener::bind("[::1]:0") else {
            eprintln!("SKIPPED tcp6_connect_denied: no IPv6 loopback");
            return;
        };
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let addr = sockaddr_in6(l.local_addr().unwrap().port());
        let before = || early_socket(libc::AF_INET6, libc::SOCK_STREAM, false);
        let after = move |s| addr_call(libc::connect, s, &addr);
        probe_with(Sb::Off, before, after).unwrap();
        let r = probe_with(Sb::On(Some(&rs)), before, after);
        assert_eq!(errno(r), Some(libc::EACCES));
    }

    #[test]
    fn tcp_bind_denied() {
        if abi_or_skip("tcp_bind_denied", 4).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let before = || early_socket(libc::AF_INET, libc::SOCK_STREAM, false);
        let after = |s| addr_call(libc::bind, s, &sockaddr_in(0));
        probe_with(Sb::Off, before, after).unwrap();
        let r = probe_with(Sb::On(Some(&rs)), before, after);
        assert_eq!(errno(r), Some(libc::EACCES));
    }

    #[test]
    fn udp_bind_denied() {
        if abi_or_skip("udp_bind_denied", 10).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let before = || early_socket(libc::AF_INET, libc::SOCK_DGRAM, false);
        let after = |s| addr_call(libc::bind, s, &sockaddr_in(0));
        probe_with(Sb::Off, before, after).unwrap();
        let r = probe_with(Sb::On(Some(&rs)), before, after);
        assert_eq!(errno(r), Some(libc::EACCES));
    }

    /// The socket is bound before the sandbox, so connect cannot trip the
    /// auto-bind (BIND_UDP) check: the denial is CONNECT_SEND_UDP's.
    #[test]
    fn udp_connect_denied() {
        if abi_or_skip("udp_connect_denied", 10).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let u = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sockaddr_in(u.local_addr().unwrap().port());
        let before = || early_socket(libc::AF_INET, libc::SOCK_DGRAM, true);
        let after = move |s| addr_call(libc::connect, s, &addr);
        probe_with(Sb::Off, before, after).unwrap();
        let r = probe_with(Sb::On(Some(&rs)), before, after);
        assert_eq!(errno(r), Some(libc::EACCES));
    }

    #[test]
    fn udp_sendto_denied() {
        if abi_or_skip("udp_sendto_denied", 10).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let u = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sockaddr_in(u.local_addr().unwrap().port());
        let before = || early_socket(libc::AF_INET, libc::SOCK_DGRAM, true);
        let after = move |s| {
            // SAFETY: sendto reads 1 byte from a static and the sockaddr
            // from our stack, with their exact sizes.
            let r = unsafe {
                libc::sendto(
                    s,
                    b"x".as_ptr().cast(),
                    1,
                    0,
                    (&addr as *const libc::sockaddr_in).cast(),
                    size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            if r == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        probe_with(Sb::Off, before, after).unwrap();
        let r = probe_with(Sb::On(Some(&rs)), before, after);
        assert_eq!(errno(r), Some(libc::EACCES));
    }

    /// Seccomp: only AF_UNIX sockets can be created, with or without Landlock.
    fn assert_socket_denied(test: &str, domain: i32, ty: i32, proto: i32, needs_support: bool) {
        if !seccomp_or_skip(test) {
            return;
        }
        let attempt = move || socket(domain, ty, proto).map(drop);
        if let Err(e) = probe(Sb::Off, attempt) {
            if needs_support {
                eprintln!("SKIPPED {test}: not available on this host ({e})");
                return;
            }
            eprintln!("{test}: control fails unsandboxed too ({e})");
        }
        assert_eq!(
            errno(probe(Sb::On(None), attempt)),
            Some(libc::EACCES),
            "{test}"
        );
    }

    #[test]
    fn inet_sockets_denied() {
        for (domain, ty) in [
            (libc::AF_INET, libc::SOCK_STREAM),
            (libc::AF_INET, libc::SOCK_DGRAM),
            (libc::AF_INET6, libc::SOCK_STREAM),
            (libc::AF_INET6, libc::SOCK_DGRAM),
            (libc::AF_NETLINK, libc::SOCK_RAW),
        ] {
            assert_socket_denied("inet_sockets_denied", domain, ty, 0, false);
        }
    }

    #[test]
    fn mptcp_socket_denied() {
        let (d, t, p) = (libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_MPTCP);
        assert_socket_denied("mptcp_socket_denied", d, t, p, true);
    }

    #[test]
    fn sctp_socket_denied() {
        let (d, t, p) = (libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_SCTP);
        assert_socket_denied("sctp_socket_denied", d, t, p, true);
    }

    #[test]
    fn icmp_datagram_socket_denied() {
        let (d, t, p) = (libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP);
        assert_socket_denied("icmp_datagram_socket_denied", d, t, p, false);
    }

    /// Unsandboxed this is EPERM (no CAP_NET_RAW); EACCES proves the filter.
    #[test]
    fn raw_socket_denied() {
        let (d, t, p) = (libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP);
        assert_socket_denied("raw_socket_denied", d, t, p, false);
    }

    #[test]
    fn unix_sockets_allowed() {
        probe(Sb::On(None), || {
            socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
            socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0)?;
            let mut fds = [0; 2];
            // SAFETY: socketpair writes two ints into `fds`.
            cvt(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) })
        })
        .unwrap();
    }

    fn sockaddr_un(path: &[u8]) -> libc::sockaddr_un {
        // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
        let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        a.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, s) in a.sun_path.iter_mut().zip(path) {
            *d = *s as libc::c_char;
        }
        a
    }

    #[test]
    fn abstract_unix_socket_outside_denied() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixListener};
        if abi_or_skip("abstract_unix_socket_outside_denied", 6).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let name = format!("inacircle-sandbox-test-{}", std::process::id());
        let sa = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let _l = UnixListener::bind_addr(&sa).unwrap();
        let mut path = vec![0u8];
        path.extend_from_slice(name.as_bytes());
        let addr = sockaddr_un(&path);
        let len = 2 + path.len(); // sun_family + the name, no trailing NUL
        let attempt = move || {
            // SAFETY: as in `connect`, with the abstract address length.
            unsafe {
                let s = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                cvt(s)?;
                let r = libc::connect(
                    s,
                    (&addr as *const libc::sockaddr_un).cast(),
                    len as libc::socklen_t,
                );
                let err = io::Error::last_os_error();
                libc::close(s);
                if r == -1 { Err(err) } else { Ok(()) }
            }
        };
        probe(Sb::Off, attempt).unwrap();
        assert_eq!(errno(probe(Sb::On(Some(&rs)), attempt)), Some(libc::EPERM));
    }

    #[test]
    fn pathname_unix_socket_outside_denied() {
        use std::os::unix::net::UnixListener;
        if abi_or_skip("pathname_unix_socket_outside_denied", 9).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let rs = ruleset(&[], &work.0);
        let path = other.0.join("s");
        let _l = UnixListener::bind(&path).unwrap();
        let mut bytes = path.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0);
        let addr = sockaddr_un(&bytes);
        let attempt = move || connect(libc::AF_UNIX, libc::SOCK_STREAM, &addr);
        probe(Sb::Off, attempt).unwrap();
        assert_eq!(errno(probe(Sb::On(Some(&rs)), attempt)), Some(libc::EACCES));
    }

    #[test]
    fn signal_outside_denied() {
        if abi_or_skip("signal_outside_denied", 6).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let mut victim = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = victim.id() as libc::pid_t;
        let attempt = move || {
            // SAFETY: kill is async-signal-safe and takes no pointers.
            cvt(unsafe { libc::kill(pid, libc::SIGCONT) })
        };
        probe(Sb::Off, attempt).unwrap();
        let denied = errno(probe(Sb::On(Some(&rs)), attempt));
        victim.kill().unwrap();
        victim.wait().unwrap();
        assert_eq!(denied, Some(libc::EPERM));
    }

    fn cpath(p: &Path) -> std::ffi::CString {
        std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap()
    }

    #[test]
    fn io_uring_denied() {
        if !seccomp_or_skip("io_uring_denied") {
            return;
        }
        let attempt = || {
            let mut params = [0u32; 30]; // struct io_uring_params, 120 bytes
            // SAFETY: io_uring_setup writes at most 120 bytes into `params`.
            let r = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) };
            if r < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        if let Err(e) = probe(Sb::Off, attempt) {
            eprintln!("io_uring_denied: control fails unsandboxed too ({e})");
        }
        assert_eq!(errno(probe(Sb::On(None), attempt)), Some(libc::EPERM));
    }

    #[test]
    fn x32_syscall_denied() {
        if !seccomp_or_skip("x32_syscall_denied") {
            return;
        }
        // SAFETY: x32 getpid takes no arguments.
        let attempt = || {
            let r = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
            if r < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        // Unsandboxed: works, or ENOSYS without x32 support; never EPERM.
        assert_ne!(errno(probe(Sb::Off, attempt)), Some(libc::EPERM));
        assert_eq!(errno(probe(Sb::On(None), attempt)), Some(libc::EPERM));
    }

    /// A 32-bit syscall (int 0x80) has another audit arch: the filter kills.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn foreign_arch_syscall_kills_the_process() {
        use std::os::unix::process::ExitStatusExt;
        fn run(sandboxed: bool) -> std::process::ExitStatus {
            let mut cmd = Command::new("/bin/true");
            // SAFETY: int 0x80 with eax = 20 is the i386 getpid: no memory
            // access; r8-r11 are declared clobbered. apply_in_child is
            // async-signal-safe.
            unsafe {
                cmd.pre_exec(move || {
                    if sandboxed {
                        apply_in_child(None, &NO_LIMIT)?;
                    }
                    let r: i32;
                    std::arch::asm!(
                        "int 0x80",
                        inlateout("eax") 20 => r,
                        out("r8") _, out("r9") _, out("r10") _, out("r11") _,
                        options(nostack),
                    );
                    if r > 0 {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(-r))
                    }
                });
            }
            cmd.status().unwrap()
        }
        if !run(false).success() {
            eprintln!("SKIPPED foreign_arch_syscall_kills_the_process: no ia32 emulation");
            return;
        }
        assert_eq!(run(true).signal(), Some(libc::SIGSYS));
    }

    /// The metadata syscalls the filter denies, one per `which`, on `p`.
    fn metadata_call(which: usize, p: &std::ffi::CStr) -> io::Result<()> {
        let p = p.as_ptr();
        let name = c"user.inacircle".as_ptr();
        // SAFETY: every call reads only `p` and `name` (NUL-terminated,
        // alive for the call) and one static byte of value; NULL times
        // mean "now".
        let r = unsafe {
            match which {
                0 => libc::chmod(p, 0o600),
                1 => libc::chown(p, u32::MAX, u32::MAX),
                2 => libc::lchown(p, u32::MAX, u32::MAX),
                3 => libc::utimensat(libc::AT_FDCWD, p, std::ptr::null(), 0),
                4 => libc::utimes(p, std::ptr::null()),
                5 => libc::setxattr(p, name, b"1".as_ptr().cast(), 1, 0),
                6 => libc::removexattr(p, name),
                7 => libc::fchmodat(libc::AT_FDCWD, p, 0o600, 0),
                8 => libc::fchownat(libc::AT_FDCWD, p, u32::MAX, u32::MAX, 0),
                _ => unreachable!(),
            }
        };
        cvt(r)
    }

    #[test]
    fn metadata_changes_outside_denied() {
        if !seccomp_or_skip("metadata_changes_outside_denied") {
            return;
        }
        let other = Tmp::new("other");
        let f = other.0.join("victim");
        fs::write(&f, "v").unwrap();
        let before = fs::metadata(&f).unwrap();
        let c = cpath(&f);
        for which in 0..9 {
            let c = c.clone();
            let r = probe(Sb::On(None), move || metadata_call(which, &c));
            assert_eq!(errno(r), Some(libc::EPERM), "metadata call {which}");
        }
        let after = fs::metadata(&f).unwrap();
        assert_eq!(after.permissions(), before.permissions());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        // Control: chmod works unsandboxed.
        let c2 = c.clone();
        probe(Sb::Off, move || metadata_call(0, &c2)).unwrap();
    }

    /// The fd variants, on a read-only fd of an input the action opens.
    #[test]
    fn metadata_changes_via_input_fd_denied() {
        if !seccomp_or_skip("metadata_changes_via_input_fd_denied") {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let input = other.0.join("input");
        fs::write(&input, "i").unwrap();
        let rs = landlock_abi().map(|_| ruleset(std::slice::from_ref(&input), &work.0));
        let before = fs::metadata(&input).unwrap();
        let c = cpath(&input);
        for which in 0..4 {
            let c = c.clone();
            let r = probe(Sb::On(rs.as_ref()), move || {
                // SAFETY: open reads the NUL-terminated path; the f* calls
                // take the fd we just opened, a static name and one byte.
                let r = unsafe {
                    let fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                    cvt(fd)?;
                    match which {
                        0 => libc::fchmod(fd, 0o600),
                        1 => libc::fchown(fd, u32::MAX, u32::MAX),
                        2 => libc::futimens(fd, std::ptr::null()),
                        3 => libc::fsetxattr(
                            fd,
                            c"user.inacircle".as_ptr(),
                            b"1".as_ptr().cast(),
                            1,
                            0,
                        ),
                        _ => unreachable!(),
                    }
                };
                cvt(r)
            });
            assert_eq!(errno(r), Some(libc::EPERM), "fd metadata call {which}");
        }
        let after = fs::metadata(&input).unwrap();
        assert_eq!(after.permissions(), before.permissions());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    }

    /// Regression: an fd the parent opened without O_CLOEXEC reached the
    /// action and let it write outside the sandbox.
    #[test]
    fn inherited_fd_closed_at_exec() {
        let other = Tmp::new("other");
        let victim = other.0.join("victim");
        fs::write(&victim, "victim\n").unwrap();
        let c = cpath(&victim);
        // Force a two-digit number, as a busy parallel test run does.
        let _pad: Vec<_> = (0..12)
            .map(|_| fs::File::open("/dev/null").unwrap())
            .collect();
        // SAFETY: open reads the NUL-terminated path; we close the fd below.
        let leak = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_APPEND) };
        assert!(leak >= 3);
        // dash only takes a one-digit fd in `>&N`, but the number we got
        // depends on what the other test threads have open. The child gets
        // the leaked file as fd 9, inherited (not close-on-exec) like a real
        // leak, whatever the parent's number is.
        let script = "echo LEAKED >&9";
        let o = sh_with_fd9(None, NO_LIMIT, script, leak);
        let sandboxed = fs::read(&victim).unwrap();
        // Control: without apply_in_child the fd is inherited.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        // SAFETY: dup2 is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(leak, 9) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let control = cmd.status();
        // SAFETY: `leak` is ours and still open.
        unsafe { libc::close(leak) };
        assert!(!o.status.success());
        assert_eq!(sandboxed, b"victim\n");
        assert!(control.unwrap().success());
        assert_eq!(fs::read(&victim).unwrap(), b"victim\nLEAKED\n");
    }

    #[test]
    fn truncate_of_input_denied() {
        if abi_or_skip("truncate_of_input_denied", 3).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let input = other.0.join("input");
        fs::write(&input, "input\n").unwrap();
        let rs = ruleset(std::slice::from_ref(&input), &work.0);
        let c = cpath(&input);
        let c1 = c.clone();
        let r = probe(Sb::On(Some(&rs)), move || {
            // SAFETY: truncate reads the NUL-terminated path.
            cvt(unsafe { libc::truncate(c1.as_ptr(), 0) })
        });
        assert_eq!(errno(r), Some(libc::EACCES), "truncate(2)");
        let r = probe(Sb::On(Some(&rs)), move || {
            // SAFETY: open reads the NUL-terminated path.
            cvt(unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_TRUNC) })
        });
        assert_eq!(errno(r), Some(libc::EACCES), "O_RDONLY | O_TRUNC");
        assert_eq!(fs::read(&input).unwrap(), b"input\n");
    }

    #[test]
    fn link_or_rename_of_input_into_work_denied() {
        if abi_or_skip("link_or_rename_of_input_into_work_denied", 2).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let input = other.0.join("input");
        fs::write(&input, "input\n").unwrap();
        let rs = ruleset(std::slice::from_ref(&input), &work.0);
        let (src, hl, mv) = (
            cpath(&input),
            cpath(&work.0.join("hl")),
            cpath(&work.0.join("mv")),
        );
        let (s1, s2) = (src.clone(), src);
        let r = probe(Sb::On(Some(&rs)), move || {
            // SAFETY: link reads two NUL-terminated paths.
            cvt(unsafe { libc::link(s1.as_ptr(), hl.as_ptr()) })
        });
        assert_eq!(errno(r), Some(libc::EXDEV), "link");
        let r = probe(Sb::On(Some(&rs)), move || {
            // SAFETY: rename reads two NUL-terminated paths.
            cvt(unsafe { libc::rename(s2.as_ptr(), mv.as_ptr()) })
        });
        // Removing the source needs REMOVE_FILE there: EACCES before REFER.
        assert_eq!(errno(r), Some(libc::EACCES), "rename");
        assert!(input.exists());
        assert!(!work.0.join("hl").exists() && !work.0.join("mv").exists());
    }

    #[test]
    fn create_or_remove_outside_denied() {
        if abi_or_skip("create_or_remove_outside_denied", 1).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let victim = other.0.join("victim");
        fs::write(&victim, "v").unwrap();
        let rs = ruleset(&[], &work.0);
        let (d, l, f, v) = (
            cpath(&other.0.join("d")),
            cpath(&other.0.join("l")),
            cpath(&other.0.join("f")),
            cpath(&victim),
        );
        for which in 0..4 {
            let (d, l, f, v) = (d.clone(), l.clone(), f.clone(), v.clone());
            let r = probe(Sb::On(Some(&rs)), move || {
                // SAFETY: each call reads NUL-terminated paths only.
                cvt(unsafe {
                    match which {
                        0 => libc::mkdir(d.as_ptr(), 0o700),
                        1 => libc::symlink(c"target".as_ptr(), l.as_ptr()),
                        2 => libc::mkfifo(f.as_ptr(), 0o600),
                        3 => libc::unlink(v.as_ptr()),
                        _ => unreachable!(),
                    }
                })
            });
            assert_eq!(errno(r), Some(libc::EACCES), "call {which}");
        }
        let names: Vec<_> = fs::read_dir(&other.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["victim"]);
    }

    const FS_XFLAG_NODUMP: u32 = 0x80;
    const FS_NODUMP_FL: libc::c_long = 0x40;

    /// file_setattr(2) (x86_64 469, Linux 6.17+) setting FS_XFLAG_NODUMP on `p`.
    fn file_setattr_nodump(p: &std::ffi::CStr) -> io::Result<()> {
        // struct file_attr (FILE_ATTR_SIZE_VER0): u64 fa_xflags, u32 x4.
        let attr: [u64; 3] = [FS_XFLAG_NODUMP as u64, 0, 0];
        // SAFETY: the kernel reads the NUL-terminated path and 24 bytes of
        // `attr`, both alive for the call.
        let r = unsafe {
            libc::syscall(
                469,
                libc::AT_FDCWD,
                p.as_ptr(),
                attr.as_ptr(),
                24usize,
                0u32,
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[test]
    fn file_setattr_outside_denied() {
        if !seccomp_or_skip("file_setattr_outside_denied") {
            return;
        }
        let other = Tmp::new("other");
        let (scratch, victim) = (other.0.join("scratch"), other.0.join("victim"));
        fs::write(&scratch, "s").unwrap();
        fs::write(&victim, "v").unwrap();
        let c = cpath(&scratch);
        if let Err(e) = probe(Sb::Off, move || file_setattr_nodump(&c)) {
            eprintln!("SKIPPED file_setattr_outside_denied: unsupported here ({e})");
            return;
        }
        let c = cpath(&victim);
        let r = probe(Sb::On(None), move || file_setattr_nodump(&c));
        assert_eq!(errno(r), Some(libc::EPERM));
    }

    const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
    const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
    const FS_IOC32_SETFLAGS: libc::c_ulong = 0x4004_6602;
    const FS_IOC_FSGETXATTR: libc::c_ulong = 0x801c_581f;
    const FS_IOC_FSSETXATTR: libc::c_ulong = 0x401c_5820;

    /// Sets the nodump flag on `fd` with one of the flag ioctls.
    fn flag_ioctl(which: usize, fd: RawFd) -> io::Result<()> {
        // SAFETY: each ioctl reads or writes exactly the type its number
        // encodes (long, int, struct fsxattr = 28 bytes), all on our stack.
        unsafe {
            match which {
                0 => {
                    let mut f: libc::c_long = 0;
                    cvt(libc::ioctl(fd, FS_IOC_GETFLAGS, &mut f))?;
                    f |= FS_NODUMP_FL;
                    cvt(libc::ioctl(fd, FS_IOC_SETFLAGS, &f))
                }
                1 => {
                    let f: libc::c_int = FS_NODUMP_FL as libc::c_int;
                    cvt(libc::ioctl(fd, FS_IOC32_SETFLAGS, &f))
                }
                2 => {
                    let mut x = [0u32; 7];
                    cvt(libc::ioctl(fd, FS_IOC_FSGETXATTR, x.as_mut_ptr()))?;
                    x[0] |= FS_XFLAG_NODUMP;
                    cvt(libc::ioctl(fd, FS_IOC_FSSETXATTR, x.as_ptr()))
                }
                _ => unreachable!(),
            }
        }
    }

    /// Opens `p` read-only (as an action opens an input) and runs `flag_ioctl`.
    fn flag_ioctl_on(which: usize, p: &std::ffi::CStr) -> io::Result<()> {
        // SAFETY: open reads the NUL-terminated path.
        let fd = unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        cvt(fd)?;
        flag_ioctl(which, fd)
    }

    fn nodump_set(p: &Path) -> bool {
        let f = fs::File::open(p).unwrap();
        let mut flags: libc::c_long = 0;
        // SAFETY: FS_IOC_GETFLAGS writes one long into `flags`.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
        r == 0 && flags & FS_NODUMP_FL != 0
    }

    #[test]
    fn flag_ioctls_on_input_fd_denied() {
        if !seccomp_or_skip("flag_ioctls_on_input_fd_denied") {
            return;
        }
        let work = Tmp::new("work");
        let other = Tmp::new("other");
        let (scratch, input) = (other.0.join("scratch"), other.0.join("input"));
        fs::write(&scratch, "s").unwrap();
        fs::write(&input, "i").unwrap();
        let rs = landlock_abi().map(|_| ruleset(std::slice::from_ref(&input), &work.0));
        // Controls: the native ioctls work unsandboxed (the 32-bit compat
        // number is not a native ioctl, so it has no working control).
        for which in [0, 2] {
            let c = cpath(&scratch);
            if let Err(e) = probe(Sb::Off, move || flag_ioctl_on(which, &c)) {
                eprintln!(
                    "SKIPPED flag_ioctls_on_input_fd_denied: ioctl {which} unsupported ({e})"
                );
                return;
            }
        }
        assert!(nodump_set(&scratch));
        let c = cpath(&input);
        let control32 = {
            let c = c.clone();
            errno(probe(Sb::Off, move || flag_ioctl_on(1, &c)))
        };
        assert_ne!(control32, Some(libc::EPERM));
        for which in 0..3 {
            let c = c.clone();
            let r = probe(Sb::On(rs.as_ref()), move || flag_ioctl_on(which, &c));
            assert_eq!(errno(r), Some(libc::EPERM), "flag ioctl {which}");
        }
        assert!(!nodump_set(&input));
        // Other ioctls stay allowed.
        probe(Sb::On(rs.as_ref()), move || {
            // SAFETY: open reads the NUL-terminated path.
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            cvt(fd)?;
            let mut f: libc::c_long = 0;
            // SAFETY: FS_IOC_GETFLAGS writes one long into `f`.
            cvt(unsafe { libc::ioctl(fd, FS_IOC_GETFLAGS, &mut f) })
        })
        .unwrap();
    }

    /// `_IOW('f', 133, struct fsverity_enable_arg)`, 128-byte argument
    /// (include/uapi/linux/fsverity.h).
    const FS_IOC_ENABLE_VERITY: libc::c_ulong = 0x4080_6685;

    /// Tries to enable fs-verity on `p`, opened read-only as an input is.
    fn enable_verity(p: &std::ffi::CStr) -> io::Result<()> {
        // struct fsverity_enable_arg: version 1, FS_VERITY_HASH_ALG_SHA256,
        // block size 4096, everything else zero.
        let mut arg = [0u32; 32];
        arg[0] = 1;
        arg[1] = 1;
        arg[2] = 4096;
        // SAFETY: open reads the NUL-terminated path; the ioctl reads the
        // 128-byte `arg` on our stack.
        unsafe {
            let fd = libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            cvt(fd)?;
            cvt(libc::ioctl(fd, FS_IOC_ENABLE_VERITY, arg.as_ptr()))
        }
    }

    #[test]
    fn enable_verity_on_input_fd_denied() {
        if !seccomp_or_skip("enable_verity_on_input_fd_denied") {
            return;
        }
        let other = Tmp::new("verity");
        let (scratch, input) = (other.0.join("scratch"), other.0.join("input"));
        fs::write(&scratch, "s").unwrap();
        fs::write(&input, "i").unwrap();
        let c = cpath(&scratch);
        match errno(probe(Sb::Off, move || enable_verity(&c))) {
            None => {}
            Some(e) => {
                assert_ne!(e, libc::EPERM, "control already denied");
                eprintln!(
                    "NOTE enable_verity_on_input_fd_denied: fs-verity unsupported here \
                     (errno {e}); only the filter's EPERM is checked"
                );
            }
        }
        let c = cpath(&input);
        let r = probe(Sb::On(None), move || enable_verity(&c));
        assert_eq!(errno(r), Some(libc::EPERM));
    }

    /// Runs `f` in a forked grandchild (not a group or session leader, so
    /// setsid/setpgid would succeed there) and returns its errno.
    fn in_grandchild(f: fn() -> libc::c_int) -> io::Result<()> {
        // SAFETY: fork, waitpid and _exit are async-signal-safe; `st` is a
        // valid out-pointer on this stack.
        unsafe {
            let pid = libc::fork();
            if pid == 0 {
                let r = f();
                let e = if r < 0 { *libc::__errno_location() } else { 0 };
                libc::_exit(e);
            }
            cvt(pid)?;
            let mut st = 0;
            cvt(libc::waitpid(pid, &mut st, 0))?;
            match libc::WEXITSTATUS(st) {
                0 => Ok(()),
                e => Err(io::Error::from_raw_os_error(e)),
            }
        }
    }

    fn setsid_call() -> libc::c_int {
        // SAFETY: no pointers.
        unsafe { libc::setsid() }
    }

    fn setpgid_call() -> libc::c_int {
        // SAFETY: no pointers.
        unsafe { libc::setpgid(0, 0) }
    }

    #[test]
    fn leaving_the_process_group_denied() {
        if !seccomp_or_skip("leaving_the_process_group_denied") {
            return;
        }
        for (name, f) in [
            ("setsid", setsid_call as fn() -> _),
            ("setpgid", setpgid_call),
        ] {
            probe(Sb::Off, move || in_grandchild(f))
                .unwrap_or_else(|e| panic!("control {name}: {e}"));
            let r = probe(Sb::On(None), move || in_grandchild(f));
            assert_eq!(errno(r), Some(libc::EPERM), "{name}");
        }
    }
}
