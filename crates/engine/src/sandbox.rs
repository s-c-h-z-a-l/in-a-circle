//! The sandbox every action runs in: Landlock (filesystem, network, scope)
//! plus resource limits.
//!
//! The parent builds the ruleset before spawning ([`build`]); the child
//! applies it between fork and exec ([`apply_in_child`], from
//! `std::process::Command::pre_exec`).

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
    /// Read only (the inputs, the `--inputs-from` file). Must exist.
    pub read: &'a [PathBuf],
    /// Full write (the work dir). Must exist.
    pub write: &'a Path,
}

/// Resource limits for the child. `RLIMIT_CORE` is always 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_FSIZE` in bytes.
    pub fsize: u64,
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
    let ruleset = Ruleset {
        fd: unsafe { OwnedFd::from_raw_fd(fd as RawFd) },
    };
    let exec = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
    for p in policy.exec {
        match allow(&ruleset, p, exec & fs) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            r => r?,
        }
    }
    for p in policy.read {
        allow(&ruleset, p, (ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR) & fs)?;
    }
    allow(&ruleset, policy.write, fs)?;
    Ok(ruleset)
}

/// Adds a rule granting `access` beneath `path`, limited to the
/// file-applicable rights when `path` is not a directory.
fn allow(ruleset: &Ruleset, path: &Path, access: u64) -> io::Result<()> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)?;
    let access = if f.metadata()?.is_dir() {
        access
    } else {
        access & ACCESS_FS_FILE
    };
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: f.as_raw_fd(),
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

/// Builds the ruleset for `policy` at the best available ABI. `Ok(None)`
/// when Landlock is unavailable.
pub fn build(policy: &Policy) -> io::Result<Option<Ruleset>> {
    match landlock_abi() {
        Some(abi) => build_for_abi(policy, abi).map(Some),
        None => Ok(None),
    }
}

/// `RLIMIT_FSIZE`: 16 x the total input size, at least 256 MiB, at most 16 GiB.
pub fn limits_for(inputs_total: u64) -> Limits {
    const MIN: u64 = 256 << 20;
    const MAX: u64 = 16 << 30;
    Limits {
        fsize: inputs_total.saturating_mul(16).clamp(MIN, MAX),
    }
}

/// Puts the calling process in its own session, applies `limits`, sets
/// `no_new_privs` and, given a ruleset, enforces it.
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
    let core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setsid, setrlimit, prctl and the Landlock syscall are
    // async-signal-safe; the rlimit structs live on this stack frame and
    // the caller guarantees `fd` is an open ruleset.
    unsafe {
        check(libc::setsid().into())?;
        check(libc::setrlimit(libc::RLIMIT_FSIZE, &fsize).into())?;
        check(libc::setrlimit(libc::RLIMIT_CORE, &core).into())?;
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0).into())?;
        if let Some(fd) = ruleset {
            check(libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
                eprintln!("SKIPPED {test}: Landlock unavailable on this kernel");
                None
            }
        }
    }

    fn system_exec() -> Vec<PathBuf> {
        SYSTEM_EXEC_PATHS.iter().map(PathBuf::from).collect()
    }

    fn ruleset(read: &[PathBuf], write: &Path) -> Ruleset {
        let exec = system_exec();
        let policy = Policy {
            exec: &exec,
            read,
            write,
        };
        build(&policy).unwrap().expect("Landlock available")
    }

    const NO_LIMIT: Limits = Limits {
        fsize: 1 << 40, // far above anything a test writes
    };

    /// Runs `/bin/sh -c script` under `ruleset` and `limits`.
    fn sh(ruleset: Option<&Ruleset>, limits: Limits, script: &str) -> Output {
        let fd = ruleset.map(|r| r.as_raw_fd());
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        // SAFETY: apply_in_child only makes async-signal-safe calls; the
        // ruleset outlives the spawn (borrowed for this whole function).
        unsafe {
            cmd.pre_exec(move || apply_in_child(fd, &limits));
        }
        cmd.output().unwrap()
    }

    /// Spawns `/bin/true` with the sandbox applied, then runs `probe` in the
    /// child before exec; the probe's error comes back from the spawn.
    /// `probe` must be async-signal-safe.
    fn probe<F>(ruleset: Option<&Ruleset>, probe: F) -> io::Result<()>
    where
        F: Fn() -> io::Result<()> + Send + Sync + 'static,
    {
        let fd = ruleset.map(|r| r.as_raw_fd());
        let mut cmd = Command::new("/bin/true");
        cmd.stdin(Stdio::null());
        // SAFETY: apply_in_child and every probe below only make
        // async-signal-safe calls (raw syscalls on stack data).
        unsafe {
            cmd.pre_exec(move || {
                apply_in_child(fd, &NO_LIMIT)?;
                probe()
            });
        }
        let mut child = cmd.spawn()?;
        assert!(child.wait()?.success());
        Ok(())
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
            write: &work.0,
        };
        let e = build(&policy).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let gone = work.0.join("gone-dir");
        let policy = Policy {
            exec: &exec,
            read: &[],
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
        let limits = Limits { fsize: 1 << 20 };
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
    fn core_limit_zero() {
        let o = sh(None, NO_LIMIT, "ulimit -c; ulimit -H -c");
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(o.stdout, b"0\n0\n");
    }

    #[test]
    fn child_is_session_leader_with_no_new_privs() {
        let r = probe(None, || {
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

    /// socket + connect, async-signal-safe.
    fn connect<T: Copy>(domain: i32, ty: i32, addr: &T) -> io::Result<()> {
        // SAFETY: socket takes no pointers; connect reads exactly
        // size_of::<T>() bytes from `addr`, a valid sockaddr_* on our stack.
        unsafe {
            let s = libc::socket(domain, ty | libc::SOCK_CLOEXEC, 0);
            cvt(s)?;
            let r = libc::connect(
                s,
                (addr as *const T).cast(),
                size_of::<T>() as libc::socklen_t,
            );
            let err = io::Error::last_os_error();
            libc::close(s);
            if r == -1 { Err(err) } else { Ok(()) }
        }
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
        let attempt = move || connect(libc::AF_INET, libc::SOCK_STREAM, &addr);
        probe(None, attempt).unwrap();
        assert_eq!(errno(probe(Some(&rs), attempt)), Some(libc::EACCES));
    }

    #[test]
    fn udp_connect_denied() {
        if abi_or_skip("udp_connect_denied", 10).is_none() {
            return;
        }
        let work = Tmp::new("work");
        let rs = ruleset(&[], &work.0);
        let u = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sockaddr_in(u.local_addr().unwrap().port());
        let attempt = move || connect(libc::AF_INET, libc::SOCK_DGRAM, &addr);
        probe(None, attempt).unwrap();
        assert_eq!(errno(probe(Some(&rs), attempt)), Some(libc::EACCES));
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
        probe(None, attempt).unwrap();
        assert_eq!(errno(probe(Some(&rs), attempt)), Some(libc::EPERM));
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
        probe(None, attempt).unwrap();
        assert_eq!(errno(probe(Some(&rs), attempt)), Some(libc::EACCES));
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
        probe(None, attempt).unwrap();
        let denied = errno(probe(Some(&rs), attempt));
        victim.kill().unwrap();
        victim.wait().unwrap();
        assert_eq!(denied, Some(libc::EPERM));
    }
}
