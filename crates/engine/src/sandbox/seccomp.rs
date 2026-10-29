//! The seccomp policy every action runs under, one for x86_64 and aarch64.
//!
//! One table ([`POLICY`]) names each rule once, with each architecture's
//! own syscall number; the build turns it into one classic BPF program per
//! architecture ([`filter`]), so the child installs a static program and
//! builds nothing between fork and exec. A syscall an architecture does
//! not have (aarch64 has no `chmod`, only `fchmodat`) has no number there:
//! no number is ever borrowed from the other architecture.
//!
//! Syscall numbers: libc 0.2.191 (GNU LP64 tables `x86_64/not_x32.rs` and
//! `aarch64/mod.rs`); the ones libc lacks from the Linux 7.1 UAPI
//! (`asm/unistd_64.h` for x86_64, `asm-generic/unistd.h` for aarch64).
//! Every other constant names its header. The `seccomp_data` layout is
//! include/uapi/linux/seccomp.h; both architectures are little endian.
//!
//! Everything not named here is allowed: the file system, signals and the
//! network on existing sockets are Landlock's (see the parent module).

use libc::{
    BPF_ABS, BPF_ALU, BPF_AND, BPF_JEQ, BPF_JGE, BPF_JGT, BPF_JMP, BPF_JSET, BPF_K, BPF_LD,
    BPF_RET, BPF_W, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS, sock_filter,
};

/// The architectures with a policy. A binary installs only its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// The architecture this binary was built for, if it has a policy.
    pub const fn native() -> Option<Arch> {
        if cfg!(target_arch = "x86_64") {
            Some(Arch::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Aarch64)
        } else {
            None
        }
    }

    /// `AUDIT_ARCH_*` (include/uapi/linux/audit.h): the ELF machine
    /// (include/uapi/linux/elf-em.h) | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE.
    /// A syscall made through another ABI (i386 `int 0x80`, AArch32) has
    /// another value and kills the process: its numbers mean other calls.
    const fn audit(self) -> u32 {
        const AUDIT_ARCH_64BIT: u32 = 0x8000_0000;
        const AUDIT_ARCH_LE: u32 = 0x4000_0000;
        const EM_X86_64: u32 = 62;
        const EM_AARCH64: u32 = 183;
        match self {
            Arch::X86_64 => EM_X86_64 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE,
            Arch::Aarch64 => EM_AARCH64 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE,
        }
    }
}

/// The installed program for `arch`.
pub fn filter(arch: Arch) -> &'static [sock_filter] {
    match arch {
        Arch::X86_64 => &X86_64_FILTER,
        Arch::Aarch64 => &AARCH64_FILTER,
    }
}

/// What a syscall gets.
#[derive(Clone, Copy)]
enum Rule {
    Eperm,
    Eacces,
    Enosys,
    /// Only the commands in [`IOCTL_ALLOWED`].
    Ioctl,
    /// All but the commands in [`FCNTL_DENIED`] and `F_SETFL` with `O_ASYNC`.
    Fcntl,
    /// Only the flags in [`CLONE_APPROVED`] and a valid exit signal.
    Clone,
    /// Only on the caller (pid 0).
    Prlimit,
    /// All but the options in [`PRCTL_DENIED`].
    Prctl,
}

/// One rule: the syscall's number on x86_64 and on aarch64 (`None`: the
/// architecture has no such syscall).
#[derive(Clone, Copy)]
struct Sys {
    x86_64: Option<u32>,
    aarch64: Option<u32>,
    rule: Rule,
}

const fn both(x86_64: u32, aarch64: u32, rule: Rule) -> Sys {
    Sys {
        x86_64: Some(x86_64),
        aarch64: Some(aarch64),
        rule,
    }
}

const fn x86_only(x86_64: u32, rule: Rule) -> Sys {
    Sys {
        x86_64: Some(x86_64),
        aarch64: None,
        rule,
    }
}

use Rule::*;

/// The policy, by group of the hardening inventory. Each group says why
/// an offline action does not need it and what it would otherwise allow.
#[rustfmt::skip]
const POLICY: &[Sys] = &[
    // Sockets, every family (AF_UNIX too): an offline action needs none;
    // a unix socket reaches the session bus, the compositor or an agent,
    // and a socketpair could carry fds. EACCES, like Landlock's own denial.
    both(41, 198, Eacces),  // socket
    both(53, 199, Eacces),  // socketpair

    // Metadata of files an action may only read (the inputs): mode and
    // owner, by path, fd or *at, and the newer flags form fchmodat2.
    x86_only(90, Eperm),    // chmod
    both(91, 52, Eperm),    // fchmod
    both(268, 53, Eperm),   // fchmodat
    both(452, 452, Eperm),  // fchmodat2 (UAPI on aarch64)
    x86_only(92, Eperm),    // chown
    both(93, 55, Eperm),    // fchown
    x86_only(94, Eperm),    // lchown
    both(260, 54, Eperm),   // fchownat
    // Timestamps (futimens is utimensat on an fd).
    x86_only(132, Eperm),   // utime
    x86_only(235, Eperm),   // utimes
    x86_only(261, Eperm),   // futimesat
    both(280, 88, Eperm),   // utimensat
    // Extended attributes and the inode attribute setter.
    both(188, 5, Eperm),    // setxattr
    both(189, 6, Eperm),    // lsetxattr
    both(190, 7, Eperm),    // fsetxattr
    both(463, 463, Eperm),  // setxattrat (UAPI)
    both(197, 14, Eperm),   // removexattr
    both(198, 15, Eperm),   // lremovexattr
    both(199, 16, Eperm),   // fremovexattr
    both(466, 466, Eperm),  // removexattrat (UAPI)
    both(469, 469, Eperm),  // file_setattr (UAPI)

    // ioctl and fcntl: per command, see the rule constants.
    both(16, 29, Ioctl),    // ioctl
    both(72, 25, Fcntl),    // fcntl

    // Other processes' memory, fds and state: not left to Yama's
    // ptrace_scope. The self forms are denied too: no converter needs them.
    both(101, 117, Eperm),  // ptrace
    both(310, 270, Eperm),  // process_vm_readv
    both(311, 271, Eperm),  // process_vm_writev
    both(434, 434, Eperm),  // pidfd_open
    both(438, 438, Eperm),  // pidfd_getfd
    both(424, 424, Eperm),  // pidfd_send_signal
    both(440, 440, Eperm),  // process_madvise
    both(448, 448, Eperm),  // process_mrelease
    both(312, 272, Eperm),  // kcmp
    both(274, 100, Eperm),  // get_robust_list

    // The kernel keyrings: session and user secrets.
    both(250, 219, Eperm),  // keyctl
    both(248, 217, Eperm),  // add_key
    both(249, 218, Eperm),  // request_key

    // Namespaces: no leaving the sandbox's view after it is set up.
    both(272, 97, Eperm),   // unshare
    both(308, 268, Eperm),  // setns

    // clone: per flag; clone3's flags are behind a pointer the filter
    // cannot read, so it gets ENOSYS, on which glibc and musl fall back to
    // clone (EPERM would fail thread creation instead).
    both(56, 220, Clone),   // clone
    both(435, 435, Enosys), // clone3

    // The process group: the filter comes after the child's own setsid,
    // so the action and every descendant stay in one killable group.
    // x86_64 has no setpgrp syscall.
    both(112, 157, Eperm),  // setsid
    both(109, 154, Eperm),  // setpgid

    // Scheduling, priority and I/O priority, of any process (self too):
    // not needed to convert, and a lever on other processes.
    both(142, 118, Eperm),  // sched_setparam
    both(144, 119, Eperm),  // sched_setscheduler
    both(314, 274, Eperm),  // sched_setattr
    both(203, 122, Eperm),  // sched_setaffinity
    both(141, 140, Eperm),  // setpriority
    both(251, 30, Eperm),   // ioprio_set

    // Resource limits: only the caller's own (pid 0).
    both(302, 261, Prlimit), // prlimit64

    // Moving another process's pages between NUMA nodes (any pid). The
    // caller's own memory policy (set_mempolicy, mbind) stays.
    both(256, 238, Eperm),  // migrate_pages
    both(279, 239, Eperm),  // move_pages

    // prctl: per option.
    both(157, 167, Prctl),  // prctl

    // Mounts and file handles: ways around path-based (Landlock) rules.
    both(165, 40, Eperm),   // mount
    both(166, 39, Eperm),   // umount2
    both(155, 41, Eperm),   // pivot_root
    both(161, 51, Eperm),   // chroot
    both(430, 430, Eperm),  // fsopen
    both(431, 431, Eperm),  // fsconfig
    both(432, 432, Eperm),  // fsmount
    both(433, 433, Eperm),  // fspick
    both(428, 428, Eperm),  // open_tree
    both(429, 429, Eperm),  // move_mount
    both(442, 442, Eperm),  // mount_setattr
    both(467, 467, Eperm),  // open_tree_attr: open_tree with mount_setattr's attrs
    both(304, 265, Eperm),  // open_by_handle_at
    both(303, 264, Eperm),  // name_to_handle_at
    // Mount queries: they only read, but what they read is the mount
    // table, which Landlock hides (/proc is not granted); no converter
    // needs it.
    both(457, 457, Eperm),  // statmount
    both(458, 458, Eperm),  // listmount

    // File notification groups: Landlock does not mediate inotify or
    // fanotify marks, so an action could watch file activity (names,
    // opens, writes) in any directory it can search, outside its rules.
    // An offline converter needs no events; without a group fd no mark
    // can be added.
    x86_only(253, Eperm),   // inotify_init
    both(294, 26, Eperm),   // inotify_init1
    both(300, 262, Eperm),  // fanotify_init

    // Programmable and asynchronous kernel interfaces: io_uring's SOCKET
    // and xattr ops would bypass this filter; bpf, perf and userfaultfd
    // are not needed and widen the kernel surface.
    both(425, 425, Eperm),  // io_uring_setup
    both(426, 426, Eperm),  // io_uring_enter
    both(427, 427, Eperm),  // io_uring_register
    both(321, 280, Eperm),  // bpf
    both(298, 241, Eperm),  // perf_event_open
    both(323, 282, Eperm),  // userfaultfd

    // System V IPC: objects shared with every process of the user, which
    // neither Landlock nor the socket rule covers.
    both(29, 194, Eperm),   // shmget
    both(30, 196, Eperm),   // shmat
    both(67, 197, Eperm),   // shmdt
    both(31, 195, Eperm),   // shmctl
    both(64, 190, Eperm),   // semget
    both(65, 193, Eperm),   // semop
    both(220, 192, Eperm),  // semtimedop
    both(66, 191, Eperm),   // semctl
    both(68, 186, Eperm),   // msgget
    both(69, 189, Eperm),   // msgsnd
    both(70, 188, Eperm),   // msgrcv
    both(71, 187, Eperm),   // msgctl

    // POSIX message queues: the same, by name.
    both(240, 180, Eperm),  // mq_open
    both(241, 181, Eperm),  // mq_unlink
    both(242, 182, Eperm),  // mq_timedsend
    both(243, 183, Eperm),  // mq_timedreceive
    both(244, 184, Eperm),  // mq_notify
    both(245, 185, Eperm),  // mq_getsetattr
];

/// The ioctl commands allowed (asm-generic/ioctls.h, which x86_64 and
/// arm64 both use): queries, and the ioctl forms of fcntl operations the
/// fcntl rule allows. Every other command gets EPERM: a regular-file or
/// device ioctl can change what the metadata rules protect (the inode
/// flags, fs-verity, reflinks), and a TTY one can inject input.
const IOCTL_ALLOWED: [u32; 6] = [
    0x541b, // FIONREAD: bytes ready on the fd (pipes)
    0x5401, // TCGETS: isatty() and the terminal attributes
    0x5413, // TIOCGWINSZ: terminal size
    0x5451, // FIOCLEX: = fcntl(F_SETFD, FD_CLOEXEC); CPython and Rust std use it
    0x5450, // FIONCLEX: = fcntl(F_SETFD, 0)
    0x5421, // FIONBIO: = fcntl(F_SETFL, O_NONBLOCK)
];

/// The fcntl commands denied (asm-generic/fcntl.h; include/uapi/linux/fcntl.h
/// for the F_LINUX_SPECIFIC_BASE (1024) ones): signal delivery to a chosen
/// owner, leases and directory notifications (asynchronous signals on
/// other processes' file activity), and seals.
const FCNTL_DENIED: [u32; 6] = [
    8,    // F_SETOWN
    15,   // F_SETOWN_EX
    10,   // F_SETSIG
    1024, // F_SETLEASE
    1026, // F_NOTIFY
    1033, // F_ADD_SEALS
];
/// asm-generic/fcntl.h: F_SETFL, whose `O_ASYNC` (FASYNC, 020000) turns
/// on SIGIO delivery; every other flag change stays.
const F_SETFL: u32 = 4;
const O_ASYNC: u32 = 0o20000;

/// The legacy clone flags allowed (include/uapi/linux/sched.h): what
/// fork, vfork, posix_spawn and the glibc and musl thread starts pass.
/// CLONE_DETACHED is musl's, and the kernel ignores it ("Unused,
/// ignored"). Every other bit is denied, known or future: CLONE_PARENT
/// and CLONE_UNTRACED (a child outside the group's lifecycle and
/// tracing), CLONE_PTRACE, every CLONE_NEW* namespace, and the whole
/// high word (on the legacy clone the kernel drops it, but it is never a
/// legitimate flag).
const CLONE_APPROVED: u32 = 0x0000_0100 // CLONE_VM
    | 0x0000_0200 // CLONE_FS
    | 0x0000_0400 // CLONE_FILES
    | 0x0000_0800 // CLONE_SIGHAND
    | 0x0000_1000 // CLONE_PIDFD: the new child's own pidfd (pidfd syscalls are denied)
    | 0x0000_4000 // CLONE_VFORK
    | 0x0001_0000 // CLONE_THREAD
    | 0x0004_0000 // CLONE_SYSVSEM
    | 0x0008_0000 // CLONE_SETTLS
    | 0x0010_0000 // CLONE_PARENT_SETTID
    | 0x0020_0000 // CLONE_CHILD_CLEARTID
    | 0x0040_0000 // CLONE_DETACHED
    | 0x0100_0000 // CLONE_CHILD_SETTID
    | 0x8000_0000 // CLONE_IO
    | CSIGNAL;
/// The exit signal in the low byte (sched.h); at most _NSIG (64,
/// asm-generic/signal.h). 0x80 there is also CLONE_NEWTIME, which only
/// clone3 and unshare take: out of range, so denied.
const CSIGNAL: u32 = 0xff;
const NSIG: u32 = 64;
/// The escapes the inventory names (sched.h: CLONE_PARENT, CLONE_NEWNS,
/// CLONE_NEWCGROUP, CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER,
/// CLONE_NEWPID, CLONE_NEWNET, CLONE_UNTRACED); none may be approved.
const CLONE_ESCAPES: u32 = 0x7e82_8000;
const _: () = assert!(CLONE_APPROVED & CLONE_ESCAPES == 0);

/// The prctl options denied (include/uapi/linux/prctl.h): the parent-death
/// signal, the child subreaper and the Yama ptracer, all of which change
/// the supervisor's view of the process tree's lifecycle. Queries,
/// PR_SET_NAME and the no_new_privs/seccomp options (which only tighten)
/// stay.
const PRCTL_DENIED: [u32; 3] = [
    1,           // PR_SET_PDEATHSIG
    36,          // PR_SET_CHILD_SUBREAPER
    0x5961_6d61, // PR_SET_PTRACER
];

/// arch/x86/include/uapi/asm/unistd.h: marks an x32 syscall, which shares
/// x86_64's audit arch; x32 is not a supported ABI, so EPERM.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

// struct seccomp_data offsets: nr, arch, then args[6] of 8 bytes each;
// little endian, so an argument's low word comes first.
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg_low(i: u32) -> u32 {
    16 + 8 * i
}
const fn arg_high(i: u32) -> u32 {
    arg_low(i) + 4
}

const fn nr_on(s: &Sys, arch: Arch) -> Option<u32> {
    match arch {
        Arch::X86_64 => s.x86_64,
        Arch::Aarch64 => s.aarch64,
    }
}

const fn rules_on(arch: Arch) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < POLICY.len() {
        if nr_on(&POLICY[i], arch).is_some() {
            n += 1;
        }
        i += 1;
    }
    n
}

// Layout: [head: arch check, nr load, x32 check on x86_64] [one jump per
// rule] [ALLOW, EPERM, EACCES, ENOSYS] [ioctl] [fcntl] [clone] [prlimit]
// [prctl]. Each argument block ends in its own returns: a BPF jump goes
// forward only.
const fn head(arch: Arch) -> usize {
    match arch {
        Arch::X86_64 => 5,
        Arch::Aarch64 => 4,
    }
}
const IOCTL_LEN: usize = 1 + IOCTL_ALLOWED.len() + 2;
const FCNTL_LEN: usize = 1 + FCNTL_DENIED.len() + 6;
const CLONE_LEN: usize = 8;
const PRLIMIT_LEN: usize = 6;
const PRCTL_LEN: usize = 1 + PRCTL_DENIED.len() + 2;

struct Layout {
    allow: usize,
    eperm: usize,
    eacces: usize,
    enosys: usize,
    ioctl: usize,
    fcntl: usize,
    clone: usize,
    prlimit: usize,
    prctl: usize,
    len: usize,
}

const fn layout(arch: Arch) -> Layout {
    let allow = head(arch) + rules_on(arch);
    let ioctl = allow + 4;
    let fcntl = ioctl + IOCTL_LEN;
    let clone = fcntl + FCNTL_LEN;
    let prlimit = clone + CLONE_LEN;
    let prctl = prlimit + PRLIMIT_LEN;
    Layout {
        allow,
        eperm: allow + 1,
        eacces: allow + 2,
        enosys: allow + 3,
        ioctl,
        fcntl,
        clone,
        prlimit,
        prctl,
        len: prctl + PRCTL_LEN,
    }
}

const fn stmt(code: u32, k: u32) -> sock_filter {
    sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn load(offset: u32) -> sock_filter {
    stmt(BPF_LD | BPF_W | BPF_ABS, offset)
}

const fn ret(k: u32) -> sock_filter {
    stmt(BPF_RET | BPF_K, k)
}

const fn errno(e: i32) -> u32 {
    SECCOMP_RET_ERRNO | e as u32
}

/// The jump at `at` comparing with `k`: to `t` when true, to `f` when
/// false (absolute indices). `jt`/`jf` are u8 and a BPF jump goes forward
/// only: an offset that does not fit would truncate silently into a jump
/// to the wrong instruction, so this stops the build instead (the
/// programs are built at compile time).
const fn jump(at: usize, op: u32, k: u32, t: usize, f: usize) -> sock_filter {
    assert!(t > at && t - at - 1 <= u8::MAX as usize);
    assert!(f > at && f - at - 1 <= u8::MAX as usize);
    sock_filter {
        code: (BPF_JMP | op | BPF_K) as u16,
        jt: (t - at - 1) as u8,
        jf: (f - at - 1) as u8,
        k,
    }
}

const fn program<const N: usize>(arch: Arch) -> [sock_filter; N] {
    let l = layout(arch);
    assert!(N == l.len);
    let mut p = [stmt(0, 0); N];
    p[0] = load(ARCH);
    p[1] = jump(1, BPF_JEQ, arch.audit(), 3, 2);
    p[2] = ret(SECCOMP_RET_KILL_PROCESS);
    p[3] = load(NR);
    if let Arch::X86_64 = arch {
        p[4] = jump(4, BPF_JGE, X32_SYSCALL_BIT, l.eperm, 5);
    }
    let mut at = head(arch);
    let mut i = 0;
    while i < POLICY.len() {
        if let Some(nr) = nr_on(&POLICY[i], arch) {
            let to = match POLICY[i].rule {
                Eperm => l.eperm,
                Eacces => l.eacces,
                Enosys => l.enosys,
                Ioctl => l.ioctl,
                Fcntl => l.fcntl,
                Clone => l.clone,
                Prlimit => l.prlimit,
                Prctl => l.prctl,
            };
            p[at] = jump(at, BPF_JEQ, nr, to, at + 1);
            at += 1;
        }
        i += 1;
    }
    p[l.allow] = ret(SECCOMP_RET_ALLOW);
    p[l.eperm] = ret(errno(libc::EPERM));
    p[l.eacces] = ret(errno(libc::EACCES));
    p[l.enosys] = ret(errno(libc::ENOSYS));

    // ioctl: the kernel takes `unsigned int cmd`, so the low word of
    // args[1] is the whole command.
    let b = l.ioctl;
    let (b_eperm, b_allow) = (b + IOCTL_LEN - 2, b + IOCTL_LEN - 1);
    p[b] = load(arg_low(1));
    let mut i = 0;
    while i < IOCTL_ALLOWED.len() {
        let at = b + 1 + i;
        p[at] = jump(at, BPF_JEQ, IOCTL_ALLOWED[i], b_allow, at + 1);
        i += 1;
    }
    p[b_eperm] = ret(errno(libc::EPERM));
    p[b_allow] = ret(SECCOMP_RET_ALLOW);

    // fcntl: `unsigned int cmd` in args[1]; setfl() takes the flags as
    // `unsigned int` from args[2].
    let b = l.fcntl;
    let n = FCNTL_DENIED.len();
    let (setfl, allow, flags, setfl_allow, b_eperm) =
        (b + 1 + n, b + 2 + n, b + 3 + n, b + 5 + n, b + 6 + n);
    p[b] = load(arg_low(1));
    let mut i = 0;
    while i < n {
        let at = b + 1 + i;
        p[at] = jump(at, BPF_JEQ, FCNTL_DENIED[i], b_eperm, at + 1);
        i += 1;
    }
    p[setfl] = jump(setfl, BPF_JEQ, F_SETFL, flags, allow);
    p[allow] = ret(SECCOMP_RET_ALLOW);
    p[flags] = load(arg_low(2));
    p[flags + 1] = jump(flags + 1, BPF_JSET, O_ASYNC, b_eperm, setfl_allow);
    p[setfl_allow] = ret(SECCOMP_RET_ALLOW);
    p[b_eperm] = ret(errno(libc::EPERM));

    // clone: the flags are args[0] on both architectures.
    let b = l.clone;
    let b_eperm = b + CLONE_LEN - 1;
    p[b] = load(arg_high(0));
    p[b + 1] = jump(b + 1, BPF_JEQ, 0, b + 2, b_eperm);
    p[b + 2] = load(arg_low(0));
    p[b + 3] = jump(b + 3, BPF_JSET, !CLONE_APPROVED, b_eperm, b + 4);
    p[b + 4] = stmt(BPF_ALU | BPF_AND | BPF_K, CSIGNAL);
    p[b + 5] = jump(b + 5, BPF_JGT, NSIG, b_eperm, b + 6);
    p[b + 6] = ret(SECCOMP_RET_ALLOW);
    p[b_eperm] = ret(errno(libc::EPERM));

    // prlimit64: all 64 bits of the pid must be 0. The kernel reads a
    // pid_t (1 << 32 would be pid 0 to it), but anything but a plain 0
    // is no self call.
    let b = l.prlimit;
    let (b_allow, b_eperm) = (b + PRLIMIT_LEN - 2, b + PRLIMIT_LEN - 1);
    p[b] = load(arg_high(0));
    p[b + 1] = jump(b + 1, BPF_JEQ, 0, b + 2, b_eperm);
    p[b + 2] = load(arg_low(0));
    p[b + 3] = jump(b + 3, BPF_JEQ, 0, b_allow, b_eperm);
    p[b_allow] = ret(SECCOMP_RET_ALLOW);
    p[b_eperm] = ret(errno(libc::EPERM));

    // prctl: `int option` in args[0], its low word.
    let b = l.prctl;
    let (b_allow, b_eperm) = (b + PRCTL_LEN - 2, b + PRCTL_LEN - 1);
    p[b] = load(arg_low(0));
    let mut i = 0;
    while i < PRCTL_DENIED.len() {
        let at = b + 1 + i;
        p[at] = jump(at, BPF_JEQ, PRCTL_DENIED[i], b_eperm, at + 1);
        i += 1;
    }
    p[b_allow] = ret(SECCOMP_RET_ALLOW);
    p[b_eperm] = ret(errno(libc::EPERM));
    p
}

static X86_64_FILTER: [sock_filter; layout(Arch::X86_64).len] = program(Arch::X86_64);
static AARCH64_FILTER: [sock_filter; layout(Arch::Aarch64).len] = program(Arch::Aarch64);

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal constants against libc's (an independent source) on
    /// the architecture that runs the test.
    #[test]
    fn constants_match_libc() {
        let ioctls = [
            libc::FIONREAD,
            libc::TCGETS,
            libc::TIOCGWINSZ,
            libc::FIOCLEX,
            libc::FIONCLEX,
            libc::FIONBIO,
        ];
        assert_eq!(IOCTL_ALLOWED, ioctls.map(|c| c as u32));
        // libc has no F_SETOWN_EX or F_SETSIG.
        let fcntls = [
            (libc::F_SETOWN, 0),
            (libc::F_SETLEASE, 3),
            (libc::F_NOTIFY, 4),
            (libc::F_ADD_SEALS, 5),
        ];
        for (c, i) in fcntls {
            assert_eq!(FCNTL_DENIED[i], c as u32);
        }
        assert_eq!(F_SETFL, libc::F_SETFL as u32);
        assert_eq!(O_ASYNC, libc::O_ASYNC as u32);
        let prctls = [
            libc::PR_SET_PDEATHSIG,
            libc::PR_SET_CHILD_SUBREAPER,
            libc::PR_SET_PTRACER,
        ];
        assert_eq!(PRCTL_DENIED, prctls.map(|c| c as u32));
        let approved = [
            libc::CLONE_VM,
            libc::CLONE_FS,
            libc::CLONE_FILES,
            libc::CLONE_SIGHAND,
            libc::CLONE_PIDFD,
            libc::CLONE_VFORK,
            libc::CLONE_THREAD,
            libc::CLONE_SYSVSEM,
            libc::CLONE_SETTLS,
            libc::CLONE_PARENT_SETTID,
            libc::CLONE_CHILD_CLEARTID,
            libc::CLONE_DETACHED,
            libc::CLONE_CHILD_SETTID,
            libc::CLONE_IO,
        ];
        let mask = approved.iter().fold(CSIGNAL, |m, &f| m | f as u32);
        assert_eq!(CLONE_APPROVED, mask);
        let escapes = [
            libc::CLONE_PARENT,
            libc::CLONE_NEWNS,
            libc::CLONE_NEWCGROUP,
            libc::CLONE_NEWUTS,
            libc::CLONE_NEWIPC,
            libc::CLONE_NEWUSER,
            libc::CLONE_NEWPID,
            libc::CLONE_NEWNET,
            libc::CLONE_UNTRACED,
        ];
        assert_eq!(CLONE_ESCAPES, escapes.iter().fold(0, |m, &f| m | f as u32));
    }

    #[test]
    fn audit_arch_matches_the_kernel_header() {
        // AUDIT_ARCH_X86_64 / AUDIT_ARCH_AARCH64 as include/uapi/linux/audit.h
        // spells them out.
        assert_eq!(Arch::X86_64.audit(), 0xc000_003e);
        assert_eq!(Arch::Aarch64.audit(), 0xc000_00b7);
    }

    // ---- Mutation pass 2 (D-39): the program, run by a small BPF
    // ---- interpreter, on both architectures.

    /// Runs `filter(arch)` on a syscall: the verdict, and the indices of the
    /// instructions it went through.
    fn run_filter(arch: Arch, nr: u32, args: [u64; 6]) -> (u32, Vec<usize>) {
        // struct seccomp_data: nr, arch, instruction_pointer, args[6].
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&nr.to_le_bytes());
        data[4..8].copy_from_slice(&arch.audit().to_le_bytes());
        for (i, a) in args.iter().enumerate() {
            data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_le_bytes());
        }
        let prog = filter(arch);
        let (mut pc, mut a, mut seen) = (0usize, 0u32, Vec::new());
        loop {
            let ins = prog[pc];
            seen.push(pc);
            let code = u32::from(ins.code);
            if code == BPF_LD | BPF_W | BPF_ABS {
                let o = ins.k as usize;
                a = u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
            } else if code == BPF_ALU | BPF_AND | BPF_K {
                a &= ins.k;
            } else if code == BPF_RET | BPF_K {
                return (ins.k, seen);
            } else if code & 0x07 == BPF_JMP {
                let hit = match code & 0xf0 {
                    BPF_JEQ => a == ins.k,
                    BPF_JGE => a >= ins.k,
                    BPF_JGT => a > ins.k,
                    BPF_JSET => a & ins.k != 0,
                    op => panic!("unknown jump {op:#x}"),
                };
                pc += 1 + usize::from(if hit { ins.jt } else { ins.jf });
                continue;
            } else {
                panic!("unknown instruction {code:#x} at {pc}");
            }
            pc += 1;
        }
    }

    fn nr_of(rule: fn(&Rule) -> bool, arch: Arch) -> u32 {
        let s = POLICY.iter().find(|s| rule(&s.rule)).unwrap();
        nr_on(s, arch).unwrap()
    }

    const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

    fn eperm() -> u32 {
        errno(libc::EPERM)
    }

    /// Nothing is dead filler: every instruction is one the program uses
    /// (an unused slot would be the zero instruction the table starts from),
    /// so the blocks are packed as the layout says.
    #[test]
    fn the_program_has_no_unused_slots() {
        for arch in ARCHES {
            for (i, ins) in filter(arch).iter().enumerate() {
                assert_ne!(ins.code, 0, "{arch:?}: slot {i} is unused");
            }
        }
    }

    /// The ioctl block: an allowed command is allowed and a denied one gets
    /// EPERM, each decided inside the block (the verdict is the block's own
    /// return, not another block's code reached by a stray jump).
    #[test]
    fn ioctl_commands_are_decided_inside_their_own_block() {
        for arch in ARCHES {
            let (nr, l) = (nr_of(|r| matches!(r, Rule::Ioctl), arch), layout(arch));
            for cmd in IOCTL_ALLOWED {
                let (v, seen) = run_filter(arch, nr, [0, u64::from(cmd), 0, 0, 0, 0]);
                assert_eq!(v, SECCOMP_RET_ALLOW, "{arch:?} {cmd:#x}");
                assert!(
                    seen.iter().all(|&i| i < l.fcntl),
                    "{arch:?} {cmd:#x}: {seen:?}"
                );
            }
            // TIOCSTI (0x5412) injects terminal input; the upper half of the
            // argument is not part of the command.
            for cmd in [0x5412u64, 0, 0x1_0000_0000 | 0x5412] {
                let (v, seen) = run_filter(arch, nr, [0, cmd, 0, 0, 0, 0]);
                assert_eq!(v, eperm(), "{arch:?} {cmd:#x}");
                assert!(
                    seen.iter().all(|&i| i < l.fcntl),
                    "{arch:?} {cmd:#x}: {seen:?}"
                );
            }
        }
    }

    /// The fcntl block: the denied commands and `F_SETFL` with `O_ASYNC` get
    /// EPERM; other commands and `F_SETFL` without it are allowed.
    #[test]
    fn fcntl_denied_commands_and_o_async_get_eperm() {
        for arch in ARCHES {
            let nr = nr_of(|r| matches!(r, Rule::Fcntl), arch);
            let call =
                |cmd: u32, arg: u64| run_filter(arch, nr, [0, u64::from(cmd), arg, 0, 0, 0]).0;
            for cmd in FCNTL_DENIED {
                assert_eq!(call(cmd, 0), eperm(), "{arch:?} {cmd}");
            }
            assert_eq!(call(F_SETFL, u64::from(O_ASYNC)), eperm(), "{arch:?}");
            assert_eq!(
                call(F_SETFL, u64::from(O_ASYNC | 0o4000)),
                eperm(),
                "{arch:?}"
            );
            assert_eq!(call(F_SETFL, 0o4000), SECCOMP_RET_ALLOW, "{arch:?}");
            assert_eq!(call(3, 0), SECCOMP_RET_ALLOW, "{arch:?} F_GETFL");
        }
    }
}
