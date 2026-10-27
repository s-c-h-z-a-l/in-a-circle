//! The hardening boundary of the action sandbox: the seccomp policy that
//! x86_64 and aarch64 share (`engine::sandbox::seccomp`).
//!
//! Three kinds of evidence, each with a positive control in this binary,
//! so a broken fixture cannot pass:
//! - An evaluator runs both architectures' filters, on any host, on every
//!   syscall number and on argument cases, against `POLICY` below (the T0
//!   inventory, written independently of the implementation).
//! - Real-kernel probes on the native architecture: every denied syscall
//!   is first made without the sandbox (it must reach the kernel and not
//!   return the deny errno), then under `apply_in_child` (it must).
//! - A real workload (threads, small allocations, fork + exec, pipes, work
//!   files) runs under the filter in a re-executed copy of this binary.

use engine::sandbox::seccomp::{self, Arch};
use engine::sandbox::{self, Limits, MIN_LANDLOCK_ABI, Policy, Ruleset};
use libc::{c_long, sock_filter};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

// ---- the policy under test (T0 inventory) ----

/// The verdict a syscall gets: a fixed one, or one that depends on its
/// arguments (checked by the argument tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V {
    Allow,
    Eperm,
    Eacces,
    Enosys,
    Ioctl,
    Fcntl,
    Clone,
    Prlimit,
    Prctl,
}
use V::*;

/// Name, x86_64 number, aarch64 number (`None`: the architecture has no
/// such syscall, its `*at` form is a row of its own; no number is borrowed
/// from the other architecture), verdict.
type Row = (&'static str, Option<u32>, Option<u32>, V);

/// Every syscall of the T0 inventory, grouped as there. Numbers: libc
/// 0.2.191 GNU LP64 tables (`x86_64/not_x32.rs`, `aarch64/mod.rs`); the
/// ones libc lacks (setxattrat, removexattrat, file_setattr, io_pgetevents,
/// open_tree_attr, statmount, listmount; fchmodat2 on aarch64) from the Linux 7.1 UAPI `asm/unistd_64.h` and
/// `asm-generic/unistd.h`. `seccomp_native_numbers_match_libc` checks the
/// native column against libc on every test run.
const POLICY: &[Row] = &[
    // landlock_gate
    ("landlock_create_ruleset", Some(444), Some(444), Allow),
    ("landlock_add_rule", Some(445), Some(445), Allow),
    ("landlock_restrict_self", Some(446), Some(446), Allow),
    ("prctl", Some(157), Some(167), Prctl),
    ("seccomp", Some(317), Some(277), Allow),
    // socket_creation
    ("socket", Some(41), Some(198), Eacces),
    ("socketpair", Some(53), Some(199), Eacces),
    // metadata_mode_owner
    ("chmod", Some(90), None, Eperm),
    ("fchmod", Some(91), Some(52), Eperm),
    ("fchmodat", Some(268), Some(53), Eperm),
    ("fchmodat2", Some(452), Some(452), Eperm),
    ("chown", Some(92), None, Eperm),
    ("fchown", Some(93), Some(55), Eperm),
    ("lchown", Some(94), None, Eperm),
    ("fchownat", Some(260), Some(54), Eperm),
    // metadata_time
    ("utime", Some(132), None, Eperm),
    ("utimes", Some(235), None, Eperm),
    ("futimesat", Some(261), None, Eperm),
    ("utimensat", Some(280), Some(88), Eperm),
    // metadata_xattr
    ("setxattr", Some(188), Some(5), Eperm),
    ("lsetxattr", Some(189), Some(6), Eperm),
    ("fsetxattr", Some(190), Some(7), Eperm),
    ("setxattrat", Some(463), Some(463), Eperm),
    ("removexattr", Some(197), Some(14), Eperm),
    ("lremovexattr", Some(198), Some(15), Eperm),
    ("fremovexattr", Some(199), Some(16), Eperm),
    ("removexattrat", Some(466), Some(466), Eperm),
    ("file_setattr", Some(469), Some(469), Eperm),
    // ioctl_closed_queries
    ("ioctl", Some(16), Some(29), Ioctl),
    // fcntl_async_metadata
    ("fcntl", Some(72), Some(25), Fcntl),
    // ptrace_process_memory
    ("ptrace", Some(101), Some(117), Eperm),
    ("process_vm_readv", Some(310), Some(270), Eperm),
    ("process_vm_writev", Some(311), Some(271), Eperm),
    ("pidfd_open", Some(434), Some(434), Eperm),
    ("pidfd_getfd", Some(438), Some(438), Eperm),
    ("pidfd_send_signal", Some(424), Some(424), Eperm),
    ("process_madvise", Some(440), Some(440), Eperm),
    ("process_mrelease", Some(448), Some(448), Eperm),
    ("kcmp", Some(312), Some(272), Eperm),
    ("get_robust_list", Some(274), Some(100), Eperm),
    // keyring
    ("keyctl", Some(250), Some(219), Eperm),
    ("add_key", Some(248), Some(217), Eperm),
    ("request_key", Some(249), Some(218), Eperm),
    // namespace
    ("unshare", Some(272), Some(97), Eperm),
    ("setns", Some(308), Some(268), Eperm),
    // clone_legacy
    ("clone", Some(56), Some(220), Clone),
    // clone3
    ("clone3", Some(435), Some(435), Enosys),
    // group_escape
    ("setsid", Some(112), Some(157), Eperm),
    ("setpgid", Some(109), Some(154), Eperm),
    // signal_scope
    ("kill", Some(62), Some(129), Allow),
    ("tkill", Some(200), Some(130), Allow),
    ("tgkill", Some(234), Some(131), Allow),
    ("rt_sigqueueinfo", Some(129), Some(138), Allow),
    ("rt_tgsigqueueinfo", Some(297), Some(240), Allow),
    // process_setters
    ("sched_setparam", Some(142), Some(118), Eperm),
    ("sched_setscheduler", Some(144), Some(119), Eperm),
    ("sched_setattr", Some(314), Some(274), Eperm),
    ("sched_setaffinity", Some(203), Some(122), Eperm),
    ("setpriority", Some(141), Some(140), Eperm),
    ("ioprio_set", Some(251), Some(30), Eperm),
    // prlimit_pid
    ("prlimit64", Some(302), Some(261), Prlimit),
    // memory_policy
    ("migrate_pages", Some(256), Some(238), Eperm),
    ("move_pages", Some(279), Some(239), Eperm),
    // prctl_lifecycle
    // mount_handles
    ("mount", Some(165), Some(40), Eperm),
    ("umount2", Some(166), Some(39), Eperm),
    ("pivot_root", Some(155), Some(41), Eperm),
    ("chroot", Some(161), Some(51), Eperm),
    ("fsopen", Some(430), Some(430), Eperm),
    ("fsconfig", Some(431), Some(431), Eperm),
    ("fsmount", Some(432), Some(432), Eperm),
    ("fspick", Some(433), Some(433), Eperm),
    ("open_tree", Some(428), Some(428), Eperm),
    ("move_mount", Some(429), Some(429), Eperm),
    ("mount_setattr", Some(442), Some(442), Eperm),
    ("open_tree_attr", Some(467), Some(467), Eperm),
    ("open_by_handle_at", Some(304), Some(265), Eperm),
    ("name_to_handle_at", Some(303), Some(264), Eperm),
    ("statmount", Some(457), Some(457), Eperm),
    ("listmount", Some(458), Some(458), Eperm),
    // notify_groups
    ("inotify_init", Some(253), None, Eperm),
    ("inotify_init1", Some(294), Some(26), Eperm),
    ("fanotify_init", Some(300), Some(262), Eperm),
    // programmable_async
    ("io_uring_setup", Some(425), Some(425), Eperm),
    ("io_uring_enter", Some(426), Some(426), Eperm),
    ("io_uring_register", Some(427), Some(427), Eperm),
    ("bpf", Some(321), Some(280), Eperm),
    ("perf_event_open", Some(298), Some(241), Eperm),
    ("userfaultfd", Some(323), Some(282), Eperm),
    // system_v_ipc
    ("shmget", Some(29), Some(194), Eperm),
    ("shmat", Some(30), Some(196), Eperm),
    ("shmdt", Some(67), Some(197), Eperm),
    ("shmctl", Some(31), Some(195), Eperm),
    ("semget", Some(64), Some(190), Eperm),
    ("semop", Some(65), Some(193), Eperm),
    ("semtimedop", Some(220), Some(192), Eperm),
    ("semctl", Some(66), Some(191), Eperm),
    ("msgget", Some(68), Some(186), Eperm),
    ("msgsnd", Some(69), Some(189), Eperm),
    ("msgrcv", Some(70), Some(188), Eperm),
    ("msgctl", Some(71), Some(187), Eperm),
    // posix_mq
    ("mq_open", Some(240), Some(180), Eperm),
    ("mq_unlink", Some(241), Some(181), Eperm),
    ("mq_timedsend", Some(242), Some(182), Eperm),
    ("mq_timedreceive", Some(243), Some(183), Eperm),
    ("mq_notify", Some(244), Some(184), Eperm),
    ("mq_getsetattr", Some(245), Some(185), Eperm),
    // legitimate_core
    ("read", Some(0), Some(63), Allow),
    ("readv", Some(19), Some(65), Allow),
    ("pread64", Some(17), Some(67), Allow),
    ("write", Some(1), Some(64), Allow),
    ("writev", Some(20), Some(66), Allow),
    ("pwrite64", Some(18), Some(68), Allow),
    ("openat", Some(257), Some(56), Allow),
    ("openat2", Some(437), Some(437), Allow),
    ("close", Some(3), Some(57), Allow),
    ("close_range", Some(436), Some(436), Allow),
    ("fstat", Some(5), Some(80), Allow),
    ("newfstatat", Some(262), Some(79), Allow),
    ("statx", Some(332), Some(291), Allow),
    ("mmap", Some(9), Some(222), Allow),
    ("mprotect", Some(10), Some(226), Allow),
    ("munmap", Some(11), Some(215), Allow),
    ("mremap", Some(25), Some(216), Allow),
    ("madvise", Some(28), Some(233), Allow),
    ("brk", Some(12), Some(214), Allow),
    ("futex", Some(202), Some(98), Allow),
    ("futex_waitv", Some(449), Some(449), Allow),
    ("set_robust_list", Some(273), Some(99), Allow),
    ("rseq", Some(334), Some(293), Allow),
    ("getrandom", Some(318), Some(278), Allow),
    ("clock_gettime", Some(228), Some(113), Allow),
    ("poll", Some(7), None, Allow),
    ("ppoll", Some(271), Some(73), Allow),
    ("epoll_wait", Some(232), None, Allow),
    ("epoll_pwait", Some(281), Some(22), Allow),
    ("eventfd2", Some(290), Some(19), Allow),
    ("pipe2", Some(293), Some(59), Allow),
    ("dup", Some(32), Some(23), Allow),
    ("dup3", Some(292), Some(24), Allow),
    ("flock", Some(73), Some(32), Allow),
    ("fork", Some(57), None, Allow),
    ("vfork", Some(58), None, Allow),
    ("execve", Some(59), Some(221), Allow),
    ("execveat", Some(322), Some(281), Allow),
    ("wait4", Some(61), Some(260), Allow),
    ("waitid", Some(247), Some(95), Allow),
    ("rt_sigaction", Some(13), Some(134), Allow),
    ("rt_sigprocmask", Some(14), Some(135), Allow),
    ("rt_sigreturn", Some(15), Some(139), Allow),
    ("sigaltstack", Some(131), Some(132), Allow),
    ("getpid", Some(39), Some(172), Allow),
    ("gettid", Some(186), Some(178), Allow),
    ("set_mempolicy", Some(238), Some(237), Allow),
    ("mbind", Some(237), Some(235), Allow),
    ("set_mempolicy_home_node", Some(450), Some(450), Allow),
    ("get_mempolicy", Some(239), Some(236), Allow),
    ("getcpu", Some(309), Some(168), Allow),
    ("memfd_create", Some(319), Some(279), Allow),
    ("io_setup", Some(206), Some(0), Allow),
    ("io_destroy", Some(207), Some(1), Allow),
    ("io_submit", Some(209), Some(2), Allow),
    ("io_getevents", Some(208), Some(4), Allow),
    ("io_cancel", Some(210), Some(3), Allow),
    ("io_pgetevents", Some(333), Some(292), Allow),
];

const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

fn nr_on(row: &Row, arch: Arch) -> Option<u32> {
    match arch {
        Arch::X86_64 => row.1,
        Arch::Aarch64 => row.2,
    }
}

fn number(name: &str, arch: Arch) -> u32 {
    POLICY
        .iter()
        .find(|r| r.0 == name)
        .and_then(|r| nr_on(r, arch))
        .unwrap_or_else(|| panic!("{name} has no number on {arch:?}"))
}

// ---- the evaluator ----

// include/uapi/linux/audit.h and include/uapi/linux/elf-em.h.
const AUDIT_ARCH_64BIT: u32 = 0x8000_0000;
const AUDIT_ARCH_LE: u32 = 0x4000_0000;
const EM_386: u32 = 3;
const EM_ARM: u32 = 40;
const EM_X86_64: u32 = 62;
const EM_AARCH64: u32 = 183;
/// arch/x86/include/uapi/asm/unistd.h: marks an x32 syscall number.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
/// Every number up to this is evaluated: Linux 7.1's highest syscall is
/// below 480 on both architectures, so 1024 leaves room for numbers the
/// kernel does not know yet (they must stay ALLOW, the kernel's ENOSYS)
/// and covers x86_64's x32-only range 512-547 without the x32 bit.
const SWEEP: u32 = 1024;

fn audit_arch(arch: Arch) -> u32 {
    match arch {
        Arch::X86_64 => EM_X86_64 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE,
        Arch::Aarch64 => EM_AARCH64 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE,
    }
}

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
const EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
const EACCES: u32 = libc::SECCOMP_RET_ERRNO | libc::EACCES as u32;
const ENOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;

/// The verdict for all-zero arguments: ioctl 0 is no allowed query,
/// fcntl 0 is F_DUPFD, clone 0 is a plain clone without exit signal,
/// prlimit64 pid 0 is the caller, prctl 0 is no lifecycle setter.
fn zero_args(v: V) -> u32 {
    match v {
        Allow | Fcntl | Clone | Prlimit | Prctl => ALLOW,
        Eperm | Ioctl => EPERM,
        Eacces => EACCES,
        Enosys => ENOSYS,
    }
}

/// Runs `prog` on a `struct seccomp_data` (include/uapi/linux/seccomp.h:
/// nr, arch, instruction_pointer, args[6]; little endian on both
/// architectures) the way the kernel would, for the classic BPF subset a
/// seccomp filter may use. Anything else panics, as does a jump or load
/// out of range.
fn eval(prog: &[sock_filter], nr: u32, arch: u32, args: [u64; 6]) -> u32 {
    let mut data = [0u8; 64];
    data[0..4].copy_from_slice(&nr.to_le_bytes());
    data[4..8].copy_from_slice(&arch.to_le_bytes());
    for (i, a) in args.iter().enumerate() {
        data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_le_bytes());
    }
    let mut acc = 0u32;
    let mut pc = 0usize;
    loop {
        let ins = prog[pc];
        let code = u32::from(ins.code);
        let k = ins.k;
        pc += 1;
        if code == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS {
            assert!(k.is_multiple_of(4) && k < 64, "load at {k}");
            let k = k as usize;
            acc = u32::from_le_bytes(data[k..k + 4].try_into().unwrap());
        } else if code == libc::BPF_ALU | libc::BPF_AND | libc::BPF_K {
            acc &= k;
        } else if code == libc::BPF_RET | libc::BPF_K {
            return k;
        } else if code & 0x07 == libc::BPF_JMP && code & 0x08 == libc::BPF_K {
            let taken = match code & 0xf0 {
                libc::BPF_JA => {
                    pc += k as usize;
                    continue;
                }
                libc::BPF_JEQ => acc == k,
                libc::BPF_JGE => acc >= k,
                libc::BPF_JGT => acc > k,
                libc::BPF_JSET => acc & k != 0,
                op => panic!("jump op {op:#x}"),
            };
            pc += usize::from(if taken { ins.jt } else { ins.jf });
        } else {
            panic!("opcode {code:#x} at {}", pc - 1);
        }
    }
}

fn run(arch: Arch, name: &str, args: [u64; 6]) -> u32 {
    eval(
        seccomp::filter(arch),
        number(name, arch),
        audit_arch(arch),
        args,
    )
}

fn args(v: &[u64]) -> [u64; 6] {
    let mut a = [0; 6];
    a[..v.len()].copy_from_slice(v);
    a
}

fn name_of(v: u32) -> String {
    match v {
        ALLOW => "ALLOW".into(),
        KILL => "KILL_PROCESS".into(),
        _ if v & libc::SECCOMP_RET_ACTION_FULL == libc::SECCOMP_RET_ERRNO => {
            format!("ERRNO({})", v & libc::SECCOMP_RET_DATA)
        }
        _ => format!("{v:#x}"),
    }
}

#[test]
fn seccomp_eval_filters_are_well_formed() {
    for arch in ARCHES {
        let p = seccomp::filter(arch);
        assert!(
            !p.is_empty() && p.len() <= libc::BPF_MAXINSNS as usize,
            "{arch:?}: {} instructions",
            p.len()
        );
        for (i, ins) in p.iter().enumerate() {
            let code = u32::from(ins.code);
            if code & 0x07 == libc::BPF_JMP {
                let far = if code & 0xf0 == libc::BPF_JA {
                    ins.k as usize
                } else {
                    usize::from(ins.jt.max(ins.jf))
                };
                assert!(i + 1 + far < p.len(), "{arch:?}: jump at {i} leaves");
            }
        }
        let last = u32::from(p[p.len() - 1].code);
        assert_eq!(last, libc::BPF_RET | libc::BPF_K, "{arch:?}");
    }
}

/// Evaluator-only for AArch32 on aarch64: the kill branch for
/// `AUDIT_ARCH_ARM` has no real-kernel test, because the native arm64 CI
/// runners (Neoverse) cannot execute AArch32 EL0 code, so no compat
/// syscall can be made there. The i386 branch on x86_64 does have one
/// (`sandbox::tests::foreign_arch_syscall_kills_the_process`). On an
/// AArch32-capable arm64 machine this stays a manual check.
#[test]
fn seccomp_eval_foreign_arch_is_killed() {
    let i386 = EM_386 | AUDIT_ARCH_LE;
    let arm = EM_ARM | AUDIT_ARCH_LE;
    for arch in ARCHES {
        let other = ARCHES.into_iter().find(|&a| a != arch).unwrap();
        let p = seccomp::filter(arch);
        for foreign in [i386, arm, audit_arch(other), 0, u32::MAX] {
            for nr in [0, 1, number("socket", arch), number("execve", arch)] {
                assert_eq!(
                    name_of(eval(p, nr, foreign, [0; 6])),
                    name_of(KILL),
                    "{arch:?} filter, audit arch {foreign:#x}, nr {nr}"
                );
            }
        }
        // Control: the native arch is not killed.
        let read = number("read", arch);
        assert_eq!(eval(p, read, audit_arch(arch), [0; 6]), ALLOW, "{arch:?}");
    }
}

#[test]
fn seccomp_eval_x32_bit_is_denied_on_x86_64() {
    let x86 = seccomp::filter(Arch::X86_64);
    let arm = seccomp::filter(Arch::Aarch64);
    for nr in [0, 1, 41, 53, 59, 435, 512, 547, 0x3fff_ffff] {
        let with = nr | X32_SYSCALL_BIT;
        let at = audit_arch(Arch::X86_64);
        assert_eq!(
            name_of(eval(x86, with, at, [0; 6])),
            "ERRNO(1)",
            "x32 nr {nr}"
        );
        // aarch64 has no x32 ABI: the number is just unknown there (the
        // kernel's own ENOSYS), not borrowed x86 semantics.
        let at = audit_arch(Arch::Aarch64);
        assert_eq!(eval(arm, with, at, [0; 6]), ALLOW, "aarch64 nr {with:#x}");
    }
}

/// Every number up to `SWEEP` with all-zero arguments gets the verdict
/// of its `POLICY` row, or ALLOW when it has none.
fn sweep(arch: Arch) {
    let p = seccomp::filter(arch);
    let mut wrong = Vec::new();
    for nr in 0..SWEEP {
        let row = POLICY.iter().find(|r| nr_on(r, arch) == Some(nr));
        let want = row.map_or(ALLOW, |r| zero_args(r.3));
        let got = eval(p, nr, audit_arch(arch), [0; 6]);
        if got != want {
            let name = row.map_or("(not in the inventory)", |r| r.0);
            wrong.push(format!(
                "{nr} {name}: want {}, got {}",
                name_of(want),
                name_of(got)
            ));
        }
    }
    assert!(wrong.is_empty(), "{arch:?}:\n{}", wrong.join("\n"));
}

#[test]
fn seccomp_eval_x86_64_matches_policy() {
    sweep(Arch::X86_64);
}

#[test]
fn seccomp_eval_aarch64_matches_policy() {
    sweep(Arch::Aarch64);
}

/// Values that select a branch somewhere in the policy: ioctl commands,
/// fcntl commands and flags, clone flags, pids, prctl options, and
/// high-word variants of them.
fn interesting() -> Vec<u64> {
    let mut v: Vec<u64> = vec![
        0,
        1,
        2,
        4,
        8,
        10,
        15,
        17,
        36,
        37,
        64,
        65,
        128,
        255,
        1024,
        1026,
        1030,
        1033,
        0x2000,
        0x2800,
        0x5401,
        0x5412,
        0x5413,
        0x541b,
        0x5421,
        0x5450,
        0x5451,
        0x5452,
        0x4008_6602,
        0x5961_6d61,
        i32::MAX as u64,
        u32::MAX as u64,
        u64::MAX,
    ];
    for bit in 0..64 {
        v.push(1 << bit);
    }
    let high: Vec<u64> = v.iter().map(|x| x | 1 << 32).collect();
    v.extend(high);
    v
}

#[test]
fn seccomp_eval_architectures_agree() {
    let values = interesting();
    let mut wrong = Vec::new();
    for row in POLICY {
        let (Some(_), Some(_)) = (row.1, row.2) else {
            continue;
        };
        for &x in &values {
            for slot in 0..3 {
                let mut a = [0u64; 6];
                a[slot] = x;
                if slot == 2 {
                    a[1] = 4; // F_SETFL: x is its flag word
                }
                let on_x86 = run(Arch::X86_64, row.0, a);
                let on_arm = run(Arch::Aarch64, row.0, a);
                if on_x86 != on_arm {
                    wrong.push(format!(
                        "{} {a:x?}: x86_64 {}, aarch64 {}",
                        row.0,
                        name_of(on_x86),
                        name_of(on_arm)
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

// asm-generic/ioctls.h (x86_64 and arm64 both use it).
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TIOCSTI: u64 = 0x5412;
const TIOCGWINSZ: u64 = 0x5413;
const FIONREAD: u64 = 0x541b;
const TIOCLINUX: u64 = 0x541c;
const FIONBIO: u64 = 0x5421;
const FIONCLEX: u64 = 0x5450;
const FIOCLEX: u64 = 0x5451;
const FIOASYNC: u64 = 0x5452;
// include/uapi/linux/fs.h, include/uapi/linux/fsverity.h.
const FS_IOC_GETFLAGS: u64 = 0x8008_6601;
const FS_IOC_SETFLAGS: u64 = 0x4008_6602;
const FS_IOC32_SETFLAGS: u64 = 0x4004_6602;
const FS_IOC_FSSETXATTR: u64 = 0x401c_5820;
const FS_IOC_ENABLE_VERITY: u64 = 0x4080_6685;
const FICLONE: u64 = 0x4004_9409;

#[test]
fn seccomp_eval_ioctl_allows_only_queries() {
    // The kernel's ioctl takes `unsigned int cmd`: bits above 32 are
    // dropped, so the verdict must follow the low word both ways.
    let high = 0xdead_beef_0000_0000;
    let allowed = [FIONREAD, TCGETS, TIOCGWINSZ, FIOCLEX, FIONCLEX, FIONBIO];
    let denied = [
        FS_IOC_SETFLAGS,
        FS_IOC32_SETFLAGS,
        FS_IOC_FSSETXATTR,
        FS_IOC_ENABLE_VERITY,
        FS_IOC_GETFLAGS,
        FICLONE,
        FIOASYNC,
        TIOCSTI,
        TCSETS,
        TIOCLINUX,
        0,
        u32::MAX as u64,
    ];
    for arch in ARCHES {
        for cmd in allowed {
            assert_eq!(
                run(arch, "ioctl", args(&[0, cmd])),
                ALLOW,
                "{arch:?} {cmd:#x}"
            );
            let a = args(&[0, cmd | high]);
            assert_eq!(run(arch, "ioctl", a), ALLOW, "{arch:?} {cmd:#x} high");
        }
        for cmd in denied {
            assert_eq!(
                run(arch, "ioctl", args(&[0, cmd])),
                EPERM,
                "{arch:?} {cmd:#x}"
            );
            let a = args(&[0, cmd | high]);
            assert_eq!(run(arch, "ioctl", a), EPERM, "{arch:?} {cmd:#x} high");
        }
    }
}

// asm-generic/fcntl.h (x86_64 and arm64 both use it).
const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_SETLK: u64 = 6;
const F_SETOWN: u64 = 8;
const F_GETOWN: u64 = 9;
const F_SETSIG: u64 = 10;
const F_SETOWN_EX: u64 = 15;
const F_OFD_SETLK: u64 = 37;
const F_SETLEASE: u64 = 1024;
const F_NOTIFY: u64 = 1026;
const F_DUPFD_CLOEXEC: u64 = 1030;
const F_ADD_SEALS: u64 = 1033;
/// include/uapi/linux/fcntl.h: F_NOTIFY on reads.
const DN_ACCESS: c_long = 1;
const O_NONBLOCK: u64 = 0o4000;
const O_ASYNC: u64 = 0o20000;

#[test]
fn seccomp_eval_fcntl_denies_async_and_lease() {
    let high = 1 << 40;
    for arch in ARCHES {
        let f = |a: &[u64]| run(arch, "fcntl", args(a));
        for cmd in [
            F_SETOWN,
            F_SETOWN_EX,
            F_SETSIG,
            F_SETLEASE,
            F_NOTIFY,
            F_ADD_SEALS,
        ] {
            assert_eq!(f(&[0, cmd, 1]), EPERM, "{arch:?} cmd {cmd}");
            // `unsigned int cmd`: the high word does not change the command.
            assert_eq!(f(&[0, cmd | high, 1]), EPERM, "{arch:?} cmd {cmd} high");
        }
        assert_eq!(f(&[0, F_SETFL, O_ASYNC]), EPERM, "{arch:?}");
        assert_eq!(f(&[0, F_SETFL, O_ASYNC | O_NONBLOCK]), EPERM, "{arch:?}");
        assert_eq!(f(&[0, F_SETFL, O_NONBLOCK]), ALLOW, "{arch:?}");
        // setfl() takes the flags as `unsigned int`: O_ASYNC in the high
        // word is not O_ASYNC.
        assert_eq!(f(&[0, F_SETFL, O_ASYNC << 32]), ALLOW, "{arch:?}");
        for cmd in [
            F_DUPFD,
            F_GETFD,
            F_SETFD,
            F_GETFL,
            F_SETLK,
            F_GETOWN,
            F_OFD_SETLK,
            F_DUPFD_CLOEXEC,
        ] {
            assert_eq!(f(&[0, cmd, O_ASYNC]), ALLOW, "{arch:?} cmd {cmd}");
        }
    }
}

// include/uapi/linux/sched.h.
const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_PTRACE: u64 = 0x2000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x1_0000;
const CLONE_NEWNS: u64 = 0x2_0000;
const CLONE_SYSVSEM: u64 = 0x4_0000;
const CLONE_SETTLS: u64 = 0x8_0000;
const CLONE_PARENT_SETTID: u64 = 0x10_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
const CLONE_DETACHED: u64 = 0x40_0000;
const CLONE_UNTRACED: u64 = 0x80_0000;
const CLONE_CHILD_SETTID: u64 = 0x100_0000;
const CLONE_NEWCGROUP: u64 = 0x200_0000;
const CLONE_NEWUTS: u64 = 0x400_0000;
const CLONE_NEWIPC: u64 = 0x800_0000;
const CLONE_NEWUSER: u64 = 0x1000_0000;
const CLONE_NEWPID: u64 = 0x2000_0000;
const CLONE_NEWNET: u64 = 0x4000_0000;
const CLONE_IO: u64 = 0x8000_0000;
const SIGCHLD: u64 = 17;

/// The flags real runtimes pass to the legacy clone.
const RUNTIME_CLONES: [(&str, u64); 5] = [
    // glibc fork(): arch_fork.
    (
        "glibc fork",
        CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | SIGCHLD,
    ),
    // glibc pthread_create (clone3 fails with ENOSYS, then clone).
    (
        "glibc pthread",
        CLONE_VM
            | CLONE_FS
            | CLONE_FILES
            | CLONE_SIGHAND
            | CLONE_THREAD
            | CLONE_SYSVSEM
            | CLONE_SETTLS
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_CLEARTID,
    ),
    // glibc posix_spawn and vfork.
    ("glibc posix_spawn", CLONE_VM | CLONE_VFORK | SIGCHLD),
    // musl pthread_create: the glibc set plus CLONE_DETACHED, which the
    // kernel ignores on the legacy clone ("Unused, ignored", sched.h).
    (
        "musl pthread",
        CLONE_VM
            | CLONE_FS
            | CLONE_FILES
            | CLONE_SIGHAND
            | CLONE_THREAD
            | CLONE_SYSVSEM
            | CLONE_SETTLS
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_CLEARTID
            | CLONE_DETACHED,
    ),
    (
        "own child pidfd, io context",
        CLONE_PIDFD | CLONE_IO | SIGCHLD,
    ),
];

#[test]
fn seccomp_eval_clone_flags() {
    let approved = CLONE_VM
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_PIDFD
        | CLONE_VFORK
        | CLONE_THREAD
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED
        | CLONE_CHILD_SETTID
        | CLONE_IO;
    let escapes = [
        CLONE_PARENT,
        CLONE_NEWNS,
        CLONE_NEWCGROUP,
        CLONE_NEWUTS,
        CLONE_NEWIPC,
        CLONE_NEWUSER,
        CLONE_NEWPID,
        CLONE_NEWNET,
        CLONE_UNTRACED,
        CLONE_PTRACE,
    ];
    for arch in ARCHES {
        let c = |flags: u64| run(arch, "clone", args(&[flags]));
        for (what, flags) in RUNTIME_CLONES {
            assert_eq!(c(flags), ALLOW, "{arch:?} {what}");
        }
        for e in escapes {
            assert_eq!(c(e | SIGCHLD), EPERM, "{arch:?} {e:#x}");
            assert_eq!(c(e | RUNTIME_CLONES[1].1), EPERM, "{arch:?} {e:#x}");
        }
        // Every flag bit outside the approved set, known or future.
        for bit in 8..32 {
            let flag = 1u64 << bit;
            let want = if approved & flag != 0 { ALLOW } else { EPERM };
            assert_eq!(c(flag | SIGCHLD), want, "{arch:?} bit {bit}");
        }
        // Any high-word bit (the kernel drops it on the legacy clone, but
        // a set bit is never a legitimate flag).
        for bit in 32..64 {
            assert_eq!(c(1 << bit | SIGCHLD), EPERM, "{arch:?} bit {bit}");
        }
        // CSIGNAL (low byte): a signal number up to _NSIG (64), or 0.
        for sig in [0, 1, SIGCHLD, 64] {
            assert_eq!(c(sig), ALLOW, "{arch:?} signal {sig}");
        }
        // 128 is also CLONE_NEWTIME's value, a clone3/unshare-only flag.
        for sig in [65, 127, 128, 255] {
            assert_eq!(c(sig), EPERM, "{arch:?} signal {sig}");
        }
        // clone3's struct is behind a pointer the filter cannot read.
        for a in [args(&[]), args(&[1, 88]), args(&[u64::MAX, 88])] {
            assert_eq!(run(arch, "clone3", a), ENOSYS, "{arch:?}");
        }
    }
}

#[test]
fn seccomp_eval_prlimit_self_only() {
    for arch in ARCHES {
        let p = |pid: u64| run(arch, "prlimit64", args(&[pid, 9]));
        assert_eq!(p(0), ALLOW, "{arch:?}");
        // 1 << 32 is pid 0 for the kernel (pid_t), but not a plain self.
        for pid in [
            1,
            4_194_304,
            i32::MAX as u64,
            u32::MAX as u64,
            1 << 32,
            u64::MAX,
        ] {
            assert_eq!(p(pid), EPERM, "{arch:?} pid {pid:#x}");
        }
    }
}

#[test]
fn seccomp_eval_prctl_lifecycle() {
    let set = [
        libc::PR_SET_PDEATHSIG,
        libc::PR_SET_CHILD_SUBREAPER,
        libc::PR_SET_PTRACER,
    ];
    let other = [
        libc::PR_GET_PDEATHSIG,
        libc::PR_GET_CHILD_SUBREAPER,
        libc::PR_SET_NAME,
        libc::PR_GET_NAME,
        libc::PR_SET_NO_NEW_PRIVS,
        libc::PR_SET_SECCOMP,
        libc::PR_GET_DUMPABLE,
    ];
    for arch in ARCHES {
        let p = |opt: u64, arg: u64| run(arch, "prctl", args(&[opt, arg]));
        for opt in set {
            let opt = opt as u64;
            assert_eq!(p(opt, 0), EPERM, "{arch:?} {opt:#x}");
            assert_eq!(p(opt, 15), EPERM, "{arch:?} {opt:#x}");
            // `int option`: the high word does not change the option.
            assert_eq!(p(opt | 1 << 32, 0), EPERM, "{arch:?} {opt:#x} high");
        }
        for opt in other {
            assert_eq!(p(opt as u64, 0), ALLOW, "{arch:?} {opt}");
        }
    }
}

#[test]
fn seccomp_eval_fixed_verdicts_ignore_arguments() {
    for arch in ARCHES {
        for row in POLICY {
            if nr_on(row, arch).is_none() || !matches!(row.3, Allow | Eperm | Eacces | Enosys) {
                continue;
            }
            for a in [[u64::MAX; 6], [1; 6], args(&[libc::AF_UNIX as u64, 1])] {
                assert_eq!(
                    run(arch, row.0, a),
                    zero_args(row.3),
                    "{arch:?} {} {a:x?}",
                    row.0
                );
            }
        }
    }
}

#[test]
fn seccomp_eval_policy_rows_are_consistent() {
    for (i, row) in POLICY.iter().enumerate() {
        assert!(row.1.is_some() || row.2.is_some(), "{}", row.0);
        for other in &POLICY[i + 1..] {
            assert_ne!(row.0, other.0, "duplicate row");
            for arch in ARCHES {
                let (a, b) = (nr_on(row, arch), nr_on(other, arch));
                assert!(a.is_none() || a != b, "{arch:?}: {} and {}", row.0, other.0);
            }
        }
    }
}

/// The native column against libc's own tables (an independent source:
/// a typo in `POLICY` fails here on the architecture that runs it; CI runs
/// both). Rows libc does not export come from the UAPI headers only.
#[test]
fn seccomp_native_numbers_match_libc() {
    let Some(arch) = Arch::native() else {
        panic!("no seccomp policy for this architecture");
    };
    #[rustfmt::skip]
    let common: &[(&str, c_long)] = &[
        ("landlock_create_ruleset", libc::SYS_landlock_create_ruleset),
        ("landlock_add_rule", libc::SYS_landlock_add_rule),
        ("landlock_restrict_self", libc::SYS_landlock_restrict_self),
        ("prctl", libc::SYS_prctl), ("seccomp", libc::SYS_seccomp),
        ("socket", libc::SYS_socket), ("socketpair", libc::SYS_socketpair),
        ("fchmod", libc::SYS_fchmod), ("fchmodat", libc::SYS_fchmodat),
        ("fchown", libc::SYS_fchown), ("fchownat", libc::SYS_fchownat),
        ("utimensat", libc::SYS_utimensat),
        ("setxattr", libc::SYS_setxattr), ("lsetxattr", libc::SYS_lsetxattr),
        ("fsetxattr", libc::SYS_fsetxattr), ("removexattr", libc::SYS_removexattr),
        ("lremovexattr", libc::SYS_lremovexattr),
        ("fremovexattr", libc::SYS_fremovexattr),
        ("ioctl", libc::SYS_ioctl), ("fcntl", libc::SYS_fcntl),
        ("ptrace", libc::SYS_ptrace), ("process_vm_readv", libc::SYS_process_vm_readv),
        ("process_vm_writev", libc::SYS_process_vm_writev),
        ("pidfd_open", libc::SYS_pidfd_open), ("pidfd_getfd", libc::SYS_pidfd_getfd),
        ("pidfd_send_signal", libc::SYS_pidfd_send_signal),
        ("process_madvise", libc::SYS_process_madvise),
        ("process_mrelease", libc::SYS_process_mrelease),
        ("kcmp", libc::SYS_kcmp), ("get_robust_list", libc::SYS_get_robust_list),
        ("keyctl", libc::SYS_keyctl), ("add_key", libc::SYS_add_key),
        ("request_key", libc::SYS_request_key),
        ("unshare", libc::SYS_unshare), ("setns", libc::SYS_setns),
        ("clone", libc::SYS_clone), ("clone3", libc::SYS_clone3),
        ("setsid", libc::SYS_setsid), ("setpgid", libc::SYS_setpgid),
        ("kill", libc::SYS_kill), ("tkill", libc::SYS_tkill), ("tgkill", libc::SYS_tgkill),
        ("rt_sigqueueinfo", libc::SYS_rt_sigqueueinfo),
        ("rt_tgsigqueueinfo", libc::SYS_rt_tgsigqueueinfo),
        ("sched_setparam", libc::SYS_sched_setparam),
        ("sched_setscheduler", libc::SYS_sched_setscheduler),
        ("sched_setattr", libc::SYS_sched_setattr),
        ("sched_setaffinity", libc::SYS_sched_setaffinity),
        ("setpriority", libc::SYS_setpriority), ("ioprio_set", libc::SYS_ioprio_set),
        ("prlimit64", libc::SYS_prlimit64),
        ("migrate_pages", libc::SYS_migrate_pages), ("move_pages", libc::SYS_move_pages),
        ("mount", libc::SYS_mount), ("umount2", libc::SYS_umount2),
        ("pivot_root", libc::SYS_pivot_root), ("chroot", libc::SYS_chroot),
        ("fsopen", libc::SYS_fsopen), ("fsconfig", libc::SYS_fsconfig),
        ("fsmount", libc::SYS_fsmount), ("fspick", libc::SYS_fspick),
        ("open_tree", libc::SYS_open_tree), ("move_mount", libc::SYS_move_mount),
        ("mount_setattr", libc::SYS_mount_setattr),
        ("inotify_init1", libc::SYS_inotify_init1), ("fanotify_init", libc::SYS_fanotify_init),
        ("open_by_handle_at", libc::SYS_open_by_handle_at),
        ("name_to_handle_at", libc::SYS_name_to_handle_at),
        ("io_uring_setup", libc::SYS_io_uring_setup),
        ("io_uring_enter", libc::SYS_io_uring_enter),
        ("io_uring_register", libc::SYS_io_uring_register),
        ("bpf", libc::SYS_bpf), ("perf_event_open", libc::SYS_perf_event_open),
        ("userfaultfd", libc::SYS_userfaultfd),
        ("shmget", libc::SYS_shmget), ("shmat", libc::SYS_shmat),
        ("shmdt", libc::SYS_shmdt), ("shmctl", libc::SYS_shmctl),
        ("semget", libc::SYS_semget), ("semop", libc::SYS_semop),
        ("semtimedop", libc::SYS_semtimedop), ("semctl", libc::SYS_semctl),
        ("msgget", libc::SYS_msgget), ("msgsnd", libc::SYS_msgsnd),
        ("msgrcv", libc::SYS_msgrcv), ("msgctl", libc::SYS_msgctl),
        ("mq_open", libc::SYS_mq_open), ("mq_unlink", libc::SYS_mq_unlink),
        ("mq_timedsend", libc::SYS_mq_timedsend),
        ("mq_timedreceive", libc::SYS_mq_timedreceive),
        ("mq_notify", libc::SYS_mq_notify), ("mq_getsetattr", libc::SYS_mq_getsetattr),
        ("read", libc::SYS_read), ("readv", libc::SYS_readv), ("pread64", libc::SYS_pread64),
        ("write", libc::SYS_write), ("writev", libc::SYS_writev),
        ("pwrite64", libc::SYS_pwrite64), ("openat", libc::SYS_openat),
        ("openat2", libc::SYS_openat2), ("close", libc::SYS_close),
        ("close_range", libc::SYS_close_range), ("fstat", libc::SYS_fstat),
        ("newfstatat", libc::SYS_newfstatat), ("statx", libc::SYS_statx),
        ("mmap", libc::SYS_mmap), ("mprotect", libc::SYS_mprotect),
        ("munmap", libc::SYS_munmap), ("mremap", libc::SYS_mremap),
        ("madvise", libc::SYS_madvise), ("brk", libc::SYS_brk),
        ("futex", libc::SYS_futex), ("futex_waitv", libc::SYS_futex_waitv),
        ("set_robust_list", libc::SYS_set_robust_list), ("rseq", libc::SYS_rseq),
        ("getrandom", libc::SYS_getrandom), ("clock_gettime", libc::SYS_clock_gettime),
        ("ppoll", libc::SYS_ppoll), ("epoll_pwait", libc::SYS_epoll_pwait),
        ("eventfd2", libc::SYS_eventfd2), ("pipe2", libc::SYS_pipe2),
        ("dup", libc::SYS_dup), ("dup3", libc::SYS_dup3), ("flock", libc::SYS_flock),
        ("execve", libc::SYS_execve), ("execveat", libc::SYS_execveat),
        ("wait4", libc::SYS_wait4), ("waitid", libc::SYS_waitid),
        ("rt_sigaction", libc::SYS_rt_sigaction),
        ("rt_sigprocmask", libc::SYS_rt_sigprocmask),
        ("rt_sigreturn", libc::SYS_rt_sigreturn), ("sigaltstack", libc::SYS_sigaltstack),
        ("getpid", libc::SYS_getpid), ("gettid", libc::SYS_gettid),
        ("set_mempolicy", libc::SYS_set_mempolicy), ("mbind", libc::SYS_mbind),
        ("set_mempolicy_home_node", libc::SYS_set_mempolicy_home_node),
        ("get_mempolicy", libc::SYS_get_mempolicy), ("getcpu", libc::SYS_getcpu),
        ("memfd_create", libc::SYS_memfd_create),
        ("io_setup", libc::SYS_io_setup), ("io_destroy", libc::SYS_io_destroy),
        ("io_submit", libc::SYS_io_submit), ("io_getevents", libc::SYS_io_getevents),
        ("io_cancel", libc::SYS_io_cancel),
    ];
    // The legacy syscalls only x86_64 has.
    #[cfg(target_arch = "x86_64")]
    #[rustfmt::skip]
    let only: &[(&str, c_long)] = &[
        ("chmod", libc::SYS_chmod), ("chown", libc::SYS_chown), ("lchown", libc::SYS_lchown),
        ("fchmodat2", libc::SYS_fchmodat2),
        ("utime", libc::SYS_utime), ("utimes", libc::SYS_utimes),
        ("futimesat", libc::SYS_futimesat), ("poll", libc::SYS_poll),
        ("epoll_wait", libc::SYS_epoll_wait), ("fork", libc::SYS_fork),
        ("vfork", libc::SYS_vfork), ("inotify_init", libc::SYS_inotify_init),
    ];
    #[cfg(not(target_arch = "x86_64"))]
    let only: &[(&str, c_long)] = &[];
    let known: Vec<(&str, c_long)> = common.iter().chain(only).copied().collect();
    let uapi_only = [
        "setxattrat",
        "removexattrat",
        "file_setattr",
        "io_pgetevents",
        "fchmodat2",
        "open_tree_attr",
        "statmount",
        "listmount",
    ];
    let mut wrong = Vec::new();
    for row in POLICY {
        let Some(nr) = nr_on(row, arch) else {
            continue;
        };
        match known.iter().find(|k| k.0 == row.0) {
            Some(&(_, libc_nr)) if libc_nr as u32 == nr => {}
            Some(&(_, libc_nr)) => wrong.push(format!("{}: {nr} vs libc {libc_nr}", row.0)),
            None if uapi_only.contains(&row.0) => {}
            None => wrong.push(format!("{}: not checked against libc", row.0)),
        }
    }
    assert!(wrong.is_empty(), "{arch:?}:\n{}", wrong.join("\n"));
}

// ---- real-kernel probes ----

const NO_LIMIT: Limits = Limits {
    fsize: 1 << 40, // far above anything a probe writes
    data: 1 << 44,  // far above any probe's heap
};
/// A pid that never names a process: above PID_MAX_LIMIT (4 Mi,
/// include/linux/threads.h), so the kernel answers ESRCH, not EPERM.
const NO_PID: c_long = i32::MAX as c_long;
const BAD_FD: c_long = -1;
const NULL: c_long = 0;
/// Added to a `connect` step's errno so it cannot pass for the `socket`
/// step's (errnos are below 4096, include/linux/err.h MAX_ERRNO).
const CONNECT_STEP: i32 = 1 << 16;
/// Added to a fixture setup failure's errno (unshare of the user
/// namespace), as above.
const SETUP_STEP: i32 = 1 << 17;

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "inacircle-hardening-{tag}-{}-{}",
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

fn landlock_required() -> bool {
    std::env::var_os("INACIRCLE_REQUIRE_LANDLOCK").is_some_and(|v| v == "1")
}

/// Whether the kernel has the Landlock ABI the policy needs; says the
/// test is skipped if not (an error under INACIRCLE_REQUIRE_LANDLOCK=1).
fn landlock_or_skip(test: &str) -> bool {
    match sandbox::landlock_abi() {
        Some(abi) if abi >= MIN_LANDLOCK_ABI => true,
        abi => {
            assert!(
                !landlock_required(),
                "{test}: Landlock ABI {abi:?} < {MIN_LANDLOCK_ABI} but INACIRCLE_REQUIRE_LANDLOCK=1"
            );
            eprintln!("SKIPPED {test}: Landlock ABI {abi:?} < {MIN_LANDLOCK_ABI}");
            false
        }
    }
}

fn ruleset(exec: &[PathBuf], write: &Path) -> Ruleset {
    let mut all: Vec<PathBuf> = sandbox::SYSTEM_EXEC_PATHS
        .iter()
        .map(PathBuf::from)
        .collect();
    all.extend_from_slice(exec);
    let policy = Policy {
        exec: &all,
        read: &[],
        read_exact: &[],
        write: Some(write),
    };
    sandbox::build(&policy).unwrap()
}

/// The least restrictive ruleset the API can express (everything beneath
/// `/` writable; the scopes and the network stay handled), for the seccomp
/// tests: every spawn now runs under a ruleset. Panics when the gate
/// refuses (Landlock ABI < 6 or a privileged parent): nothing runs then.
fn open_ruleset() -> Ruleset {
    sandbox::build(&Policy {
        exec: &[],
        read: &[],
        read_exact: &[],
        write: Some(Path::new("/")),
    })
    .expect("the sandbox gate passes (Landlock ABI >= 6, unprivileged)")
}

/// The ruleset fd `sb` applies, opening an [`open_ruleset`] into `keep`
/// for `Sb::On(None)`; `None` for `Sb::Off`.
fn sandbox_fd(sb: Sb, keep: &mut Option<Ruleset>) -> Option<RawFd> {
    match sb {
        Sb::Off => None,
        Sb::On(Some(fd)) => Some(fd),
        Sb::On(None) => Some(keep.insert(open_ruleset()).as_raw_fd()),
    }
}

/// How a probe runs: unsandboxed (the control), or through
/// `apply_in_child` with a Landlock ruleset (`None`: [`open_ruleset`]).
#[derive(Clone, Copy, Debug)]
enum Sb {
    Off,
    On(Option<RawFd>),
}

/// Spawns `/bin/true`; in the child, runs `before`, applies the sandbox
/// (per `sb`), then `after` on what `before` returned. An error comes back
/// from the spawn as its raw errno. Both must be async-signal-safe.
fn probe_with<S, B, A>(sb: Sb, before: B, after: A) -> io::Result<()>
where
    B: Fn() -> io::Result<S> + Send + Sync + 'static,
    A: Fn(S) -> io::Result<()> + Send + Sync + 'static,
{
    let mut keep = None;
    let fd = sandbox_fd(sb, &mut keep);
    let mut cmd = Command::new("/bin/true");
    cmd.stdin(Stdio::null());
    // SAFETY: apply_in_child and every probe below only make
    // async-signal-safe calls (raw syscalls on stack or pre-built data);
    // a ruleset fd is kept open (by the caller, or `keep`) for the whole
    // spawn.
    unsafe {
        cmd.pre_exec(move || {
            let state = before()?;
            if let Some(fd) = fd {
                sandbox::apply_in_child(fd, &NO_LIMIT)?;
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
    r.err().map(|e| e.raw_os_error().unwrap_or(-1))
}

fn cvt(r: c_long) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sys(nr: u32, a: [c_long; 6]) -> io::Result<()> {
    // SAFETY: every probe passes NULL, an invalid fd/pid, or pointers to
    // its own live stack data; the kernel validates the rest.
    cvt(unsafe { libc::syscall(c_long::from(nr), a[0], a[1], a[2], a[3], a[4], a[5]) })
}

/// Runs `f` in a forked child of the probe process and returns its
/// result (the errno travels in the exit status, errnos are < 256).
fn in_fork(f: impl Fn() -> io::Result<()>) -> io::Result<()> {
    // SAFETY: fork, _exit and waitpid are async-signal-safe; the probe
    // process is single-threaded.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            let code = match f() {
                Ok(()) => 0,
                Err(e) => e.raw_os_error().unwrap_or(255),
            };
            libc::_exit(code);
        }
        let mut status = 0;
        if libc::waitpid(pid, &mut status, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        match libc::WEXITSTATUS(status) {
            0 => Ok(()),
            e => Err(io::Error::from_raw_os_error(e)),
        }
    }
}

/// Enters a new user + mount namespace, in which the caller holds every
/// capability: a syscall that checks one first then reaches its argument
/// checks, so the control can tell the kernel from the filter.
fn enter_userns() -> io::Result<()> {
    // SAFETY: unshare takes no pointers.
    let r = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
    if r < 0 {
        let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(io::Error::from_raw_os_error(SETUP_STEP + e));
    }
    Ok(())
}

/// How a denied syscall's control is made.
#[derive(Clone, Copy, Debug, PartialEq)]
enum How {
    /// Directly in the probe process.
    Plain,
    /// In a forked child (setsid/setpgid: the probe process is already a
    /// session leader under the sandbox).
    Forked,
    /// In a new user namespace: the kernel checks a capability first.
    Userns,
    /// The kernel itself may answer with the deny errno first, for the
    /// stated reason; then only the evaluator proves the filter.
    KernelMayDeny(&'static str),
}

/// Harmless arguments for each denied syscall: NULL, a bad fd, a pid
/// that does not exist or an invalid flag, so the unsandboxed call fails
/// (or does nothing) with an errno other than the deny one.
fn denial_case(name: &str) -> Option<([c_long; 6], How)> {
    use How::*;
    let a = |v: &[c_long]| {
        let mut x = [0; 6];
        x[..v.len()].copy_from_slice(v);
        x
    };
    let unix = c_long::from(libc::AF_UNIX);
    let stream = c_long::from(libc::SOCK_STREAM | libc::SOCK_CLOEXEC);
    Some(match name {
        "socket" => (a(&[unix, stream, 0]), Plain),
        "socketpair" => (a(&[unix, stream, 0, NULL]), Plain),
        "chmod" | "chown" | "lchown" | "utime" | "utimes" | "setxattr" | "lsetxattr"
        | "removexattr" | "lremovexattr" | "chroot" | "mq_open" | "mq_unlink" | "add_key"
        | "request_key" => (a(&[NULL, -1, -1]), Plain),
        "fchmod" | "fchown" | "fsetxattr" | "fremovexattr" | "setns" | "pidfd_getfd"
        | "pidfd_send_signal" | "process_madvise" | "process_mrelease" | "io_uring_enter"
        | "io_uring_register" | "mq_timedsend" | "mq_timedreceive" | "mq_notify"
        | "mq_getsetattr" | "futimesat" | "utimensat" => (a(&[BAD_FD]), Plain),
        "fchmodat" | "fchmodat2" | "fchownat" | "setxattrat" | "removexattrat" | "file_setattr"
        | "name_to_handle_at" | "open_by_handle_at" => (a(&[BAD_FD]), Plain),
        // PTRACE_PEEKDATA (2) on no process: ESRCH.
        "ptrace" => (a(&[2, NO_PID]), Plain),
        // flags 1: EINVAL before any access.
        "process_vm_readv" | "process_vm_writev" => (a(&[NO_PID, 0, 0, 0, 0, 1]), Plain),
        "pidfd_open" | "get_robust_list" | "migrate_pages" | "move_pages" => (a(&[NO_PID]), Plain),
        "kcmp" => (a(&[NO_PID, NO_PID]), Plain),
        // An unknown keyctl operation: EOPNOTSUPP.
        "keyctl" => (a(&[0xffff]), Plain),
        // unshare(0) changes nothing.
        "unshare" => (a(&[0]), Plain),
        "setsid" => (a(&[]), Forked),
        "setpgid" => (a(&[0, 0]), Forked),
        "sched_setparam" | "sched_setscheduler" | "sched_setattr" | "sched_setaffinity" => {
            (a(&[NO_PID]), Plain)
        }
        // PRIO_PROCESS / IOPRIO_WHO_PROCESS (both 1 or 0 select a pid).
        "setpriority" => (a(&[c_long::from(libc::PRIO_PROCESS), NO_PID]), Plain),
        "ioprio_set" => (a(&[1, NO_PID]), Plain),
        // NULL target: EFAULT in the path lookup, before may_mount().
        "mount" => (a(&[]), Plain),
        // Unknown flags: EINVAL first.
        "umount2" => (a(&[NULL, 0x7fff_0000]), Plain),
        "open_tree" | "mount_setattr" | "open_tree_attr" => {
            (a(&[BAD_FD, NULL, 0x7fff_0000]), Plain)
        }
        // Unknown flags: EINVAL before the request is read.
        "statmount" | "listmount" => (a(&[NULL, NULL, 0, 0x7fff_0000]), Plain),
        // inotify_init makes a group fd (gone with the probe process);
        // inotify_init1 with unknown flags is EINVAL first.
        "inotify_init" => (a(&[]), Plain),
        "inotify_init1" => (a(&[0x7fff_0000]), Plain),
        // FAN_REPORT_FID (0x200), what an unprivileged group needs (5.13+);
        // without that support the kernel answers EPERM first.
        "fanotify_init" => (
            a(&[0x200, c_long::from(libc::O_RDONLY)]),
            KernelMayDeny("unprivileged fanotify"),
        ),
        "fsconfig" => (a(&[BAD_FD, 0xffff]), Plain),
        "pivot_root" | "fsopen" | "fsmount" | "fspick" | "move_mount" => {
            (a(&[NULL, NULL, BAD_FD]), Userns)
        }
        // 0 entries: EINVAL; but a kernel with io_uring disabled
        // (kernel.io_uring_disabled = 2) answers EPERM first.
        "io_uring_setup" => (a(&[0]), KernelMayDeny("kernel.io_uring_disabled")),
        // An unknown command; unprivileged bpf is off on most kernels
        // (kernel.unprivileged_bpf_disabled), which answers EPERM first.
        "bpf" => (
            a(&[0xffff]),
            KernelMayDeny("kernel.unprivileged_bpf_disabled"),
        ),
        "perf_event_open" => (a(&[NULL, 0, -1, -1, 0]), Plain),
        // UFFD_USER_MODE_ONLY (1) passes vm.unprivileged_userfaultfd, the
        // unknown flag then gives EINVAL.
        "userfaultfd" => (a(&[1 | 1 << 20]), Plain),
        // IPC_PRIVATE with size 0 / nsems -1: EINVAL, nothing created.
        "shmget" => (a(&[0, 0, 0]), Plain),
        "semget" => (a(&[0, -1, 0]), Plain),
        // A key without IPC_CREAT never creates.
        "msgget" => (a(&[0x1ac1_7c1e, 0]), Plain),
        "shmat" | "shmdt" | "shmctl" | "semop" | "semtimedop" | "semctl" | "msgsnd" | "msgrcv"
        | "msgctl" => (a(&[-1]), Plain),
        "clone3" => (a(&[NULL, 0]), Plain),
        _ => return None,
    })
}

fn deny_errno(v: V) -> i32 {
    match v {
        Eperm => libc::EPERM,
        Eacces => libc::EACCES,
        Enosys => libc::ENOSYS,
        _ => unreachable!(),
    }
}

/// Runs one denied syscall without and with the sandbox.
fn denied_call(sb: Sb, nr: u32, a: [c_long; 6], how: How) -> Option<i32> {
    let call = move || sys(nr, a);
    let r = match how {
        How::Forked => probe(sb, move || in_fork(call)),
        How::Userns => probe_with(sb, enter_userns, move |()| call()),
        How::Plain | How::KernelMayDeny(_) => probe(sb, call),
    };
    errno(r)
}

#[test]
fn seccomp_denied_syscalls_reach_the_kernel_only_without_the_filter() {
    let arch = Arch::native().expect("a supported architecture");
    let mut wrong = Vec::new();
    let mut notes = Vec::new();
    for row in POLICY {
        let Some(nr) = nr_on(row, arch) else {
            continue;
        };
        if !matches!(row.3, Eperm | Eacces | Enosys) {
            continue;
        }
        let Some((a, mut how)) = denial_case(row.0) else {
            wrong.push(format!("{}: no kernel case", row.0));
            continue;
        };
        let deny = deny_errno(row.3);
        let mut control = denied_call(Sb::Off, nr, a, how);
        if how == How::Userns && control.is_some_and(|e| e >= SETUP_STEP) {
            notes.push(format!("{}: no user namespace for the control", row.0));
            how = How::KernelMayDeny("user namespaces unavailable");
            control = denied_call(Sb::Off, nr, a, how);
        }
        if control == Some(deny) {
            match how {
                How::KernelMayDeny(why) => {
                    notes.push(format!("{}: kernel denies first ({why})", row.0));
                }
                // The namespace exists but grants no capability: Ubuntu's
                // AppArmor (kernel.apparmor_restrict_unprivileged_userns).
                How::Userns => notes.push(format!(
                    "{}: user namespace without capabilities, control unprovable here",
                    row.0
                )),
                _ => wrong.push(format!("{}: control already gives {deny}", row.0)),
            }
        }
        let got = denied_call(Sb::On(None), nr, a, how);
        if got != Some(deny) {
            wrong.push(format!(
                "{}: sandboxed {got:?} (control {control:?}), want {deny}",
                row.0
            ));
        }
    }
    for n in &notes {
        eprintln!("note: {n}");
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // On a runner qualified for it, an unprovable control is a failure
    // here, not a line for a log grep (parallel tests interleave output).
    assert!(
        !kernel_controls_required() || notes.is_empty(),
        "unprovable kernel controls under {REQUIRE_KERNEL_CONTROLS}=1:\n{}",
        notes.join("\n")
    );
}

/// Set on the native CI runners, which allow unprivileged user namespaces
/// with capabilities and unprivileged io_uring/bpf answers: every kernel
/// control must be provable there.
const REQUIRE_KERNEL_CONTROLS: &str = "INACIRCLE_REQUIRE_KERNEL_CONTROLS";

fn kernel_controls_required() -> bool {
    std::env::var_os(REQUIRE_KERNEL_CONTROLS).is_some_and(|v| v == "1")
}

// ---- sockets ----

fn sockaddr_un(path: &[u8]) -> (libc::sockaddr_un, libc::socklen_t) {
    // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in a.sun_path.iter_mut().zip(path) {
        *d = *s as libc::c_char;
    }
    // sun_family, then the path (abstract: no trailing NUL counted).
    let len = std::mem::size_of::<libc::sa_family_t>() + path.len();
    (a, len as libc::socklen_t)
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

/// `socket(domain, ty)` then `connect` to `addr`: the socket step's errno
/// as is, the connect step's plus `CONNECT_STEP`.
fn connect_to<T: Copy>(domain: i32, ty: i32, addr: &T, len: libc::socklen_t) -> io::Result<()> {
    // SAFETY: `addr` is a live sockaddr of `len` bytes; the fd is closed.
    unsafe {
        let s = libc::socket(domain, ty | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return Err(io::Error::last_os_error());
        }
        let r = libc::connect(s, (addr as *const T).cast(), len);
        let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        libc::close(s);
        if r < 0 {
            return Err(io::Error::from_raw_os_error(CONNECT_STEP + e));
        }
    }
    Ok(())
}

/// The unix socket listeners of the socket tests (pathname and abstract,
/// stream and datagram), and an attempt on each.
struct UnixListeners {
    _dir: Tmp,
    _held: Vec<Box<dyn std::any::Any>>,
    targets: Vec<(&'static str, i32, libc::sockaddr_un, libc::socklen_t)>,
}

fn unix_listeners() -> UnixListeners {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener};
    let dir = Tmp::new("unix");
    let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
    let mut targets = Vec::new();
    let path = |n: &str| {
        let mut b = dir.0.join(n).into_os_string().into_encoded_bytes();
        b.push(0);
        b
    };
    held.push(Box::new(UnixListener::bind(dir.0.join("s")).unwrap()));
    let (a, l) = sockaddr_un(&path("s"));
    targets.push(("pathname stream", libc::SOCK_STREAM, a, l));
    held.push(Box::new(UnixDatagram::bind(dir.0.join("d")).unwrap()));
    let (a, l) = sockaddr_un(&path("d"));
    targets.push(("pathname datagram", libc::SOCK_DGRAM, a, l));
    for (kind, ty) in [
        ("stream", libc::SOCK_STREAM),
        ("datagram", libc::SOCK_DGRAM),
    ] {
        // The temp dir's name is unique per process and call.
        let tag = dir.0.file_name().unwrap().to_string_lossy();
        let name = format!("{tag}-{kind}");
        let sa = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        if ty == libc::SOCK_STREAM {
            held.push(Box::new(UnixListener::bind_addr(&sa).unwrap()));
        } else {
            held.push(Box::new(UnixDatagram::bind_addr(&sa).unwrap()));
        }
        let mut b = vec![0u8];
        b.extend_from_slice(name.as_bytes());
        let (a, l) = sockaddr_un(&b);
        let what = if ty == libc::SOCK_STREAM {
            "abstract stream"
        } else {
            "abstract datagram"
        };
        targets.push((what, ty, a, l));
    }
    UnixListeners {
        _dir: dir,
        _held: held,
        targets,
    }
}

/// Every unix target: reachable without the sandbox, `EACCES` at
/// `socket()` with it.
fn assert_unix_denied(sb: Sb) {
    let ls = unix_listeners();
    for &(what, ty, addr, len) in &ls.targets {
        let attempt = move || connect_to(libc::AF_UNIX, ty, &addr, len);
        assert_eq!(errno(probe(Sb::Off, attempt)), None, "control {what}");
        assert_eq!(
            errno(probe(sb, attempt)),
            Some(libc::EACCES),
            "{what} under {sb:?}"
        );
    }
}

#[test]
fn seccomp_unix_sockets_denied() {
    assert_unix_denied(Sb::On(None));
}

/// TCP and UDP to our own loopback listeners, IPv6 when the host has it,
/// netlink and packet sockets.
fn assert_inet_denied(sb: Sb) {
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    for (what, ty, port) in [
        ("tcp", libc::SOCK_STREAM, tcp.local_addr().unwrap().port()),
        ("udp", libc::SOCK_DGRAM, udp.local_addr().unwrap().port()),
    ] {
        let addr = sockaddr_in(port);
        let attempt = move || connect_to(libc::AF_INET, ty, &addr, len);
        assert_eq!(errno(probe(Sb::Off, attempt)), None, "control {what}");
        assert_eq!(errno(probe(sb, attempt)), Some(libc::EACCES), "{what}");
    }
    for (what, domain, ty, proto) in [
        ("inet6 tcp", libc::AF_INET6, libc::SOCK_STREAM, 0),
        ("inet6 udp", libc::AF_INET6, libc::SOCK_DGRAM, 0),
        (
            "netlink",
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_ROUTE,
        ),
        // Without CAP_NET_RAW the kernel says EPERM, so EACCES is the filter.
        ("packet", libc::AF_PACKET, libc::SOCK_RAW, 0),
    ] {
        let attempt = move || {
            // SAFETY: socket takes no pointers; the fd is closed.
            let s = unsafe { libc::socket(domain, ty | libc::SOCK_CLOEXEC, proto) };
            cvt(c_long::from(s))?;
            // SAFETY: `s` is our open fd.
            unsafe { libc::close(s) };
            Ok(())
        };
        let control = errno(probe(Sb::Off, attempt));
        assert_ne!(control, Some(libc::EACCES), "control {what}");
        assert_eq!(errno(probe(sb, attempt)), Some(libc::EACCES), "{what}");
    }
}

#[test]
fn seccomp_inet_sockets_denied() {
    assert_inet_denied(Sb::On(None));
}

#[test]
fn seccomp_socketpair_denied_pipes_still_work() {
    for ty in [libc::SOCK_STREAM, libc::SOCK_DGRAM, libc::SOCK_SEQPACKET] {
        let attempt = move || {
            let mut fds = [0; 2];
            // SAFETY: socketpair writes two ints into `fds`.
            let r = unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) };
            cvt(c_long::from(r))
        };
        assert_eq!(errno(probe(Sb::Off, attempt)), None, "control type {ty}");
        assert_eq!(
            errno(probe(Sb::On(None), attempt)),
            Some(libc::EACCES),
            "type {ty}"
        );
    }
    // The IPC an offline action keeps: a pipe.
    let pipe = || {
        let mut fds = [0; 2];
        let mut buf = [0u8; 4];
        // SAFETY: pipe2 writes two ints; write/read use the 4-byte buffers.
        unsafe {
            cvt(c_long::from(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC)))?;
            cvt(libc::write(fds[1], b"ping".as_ptr().cast(), 4) as c_long)?;
            cvt(libc::read(fds[0], buf.as_mut_ptr().cast(), 4) as c_long)?;
        }
        if &buf == b"ping" {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
    };
    probe(Sb::On(None), pipe).unwrap();
}

#[test]
fn seccomp_full_sandbox_denies_sockets() {
    if !landlock_or_skip("seccomp_full_sandbox_denies_sockets") {
        return;
    }
    let work = Tmp::new("work");
    let rs = ruleset(&[], &work.0);
    let sb = Sb::On(Some(rs.as_raw_fd()));
    assert_unix_denied(sb);
    assert_inet_denied(sb);
}

// ---- argument-dependent rules on the real kernel ----

fn ioctl_on_pipe(cmd: u64, arg: c_long) -> impl Fn(RawFd) -> io::Result<()> {
    move |fd| {
        let mut word: c_long = arg;
        let mut buf = [0u8; 256];
        let p: *mut libc::c_void = if arg < 0 {
            buf.as_mut_ptr().cast()
        } else {
            (&mut word as *mut c_long).cast()
        };
        // SAFETY: every command probed reads or writes at most one int or
        // a `struct winsize`/`termios`-sized region; `buf` is larger.
        cvt(unsafe { libc::syscall(libc::SYS_ioctl, c_long::from(fd), cmd as c_long, p) })
    }
}

fn pipe_fd() -> io::Result<RawFd> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes two ints into `fds`.
    cvt(c_long::from(unsafe {
        libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC)
    }))?;
    Ok(fds[0])
}

#[test]
fn seccomp_ioctl_allows_queries_only() {
    // arg -1: a scratch buffer; otherwise a pointer to that int.
    let allowed: [(&str, u64, c_long, Option<i32>); 6] = [
        ("FIONREAD", FIONREAD, -1, None),
        ("TCGETS", TCGETS, -1, Some(libc::ENOTTY)),
        ("TIOCGWINSZ", TIOCGWINSZ, -1, Some(libc::ENOTTY)),
        ("FIOCLEX", FIOCLEX, 0, None),
        ("FIONCLEX", FIONCLEX, 0, None),
        ("FIONBIO", FIONBIO, 1, None),
    ];
    for (what, cmd, arg, want) in allowed {
        let r = probe_with(Sb::On(None), pipe_fd, ioctl_on_pipe(cmd, arg));
        assert_eq!(errno(r), want, "{what}");
        // The kernel drops the high word of the command: so does the filter.
        let r = probe_with(Sb::On(None), pipe_fd, ioctl_on_pipe(cmd | 1 << 32, arg));
        assert_eq!(errno(r), want, "{what} high");
    }
    let denied: [(&str, u64, c_long); 5] = [
        ("FIOASYNC", FIOASYNC, 1),
        ("TIOCSTI", TIOCSTI, -1),
        ("FS_IOC_GETFLAGS", FS_IOC_GETFLAGS, -1),
        ("FS_IOC_SETFLAGS", FS_IOC_SETFLAGS, 0),
        ("FS_IOC_SETFLAGS high", FS_IOC_SETFLAGS | 1 << 32, 0),
    ];
    for (what, cmd, arg) in denied {
        let control = errno(probe_with(Sb::Off, pipe_fd, ioctl_on_pipe(cmd, arg)));
        assert_ne!(control, Some(libc::EPERM), "control {what}");
        let r = probe_with(Sb::On(None), pipe_fd, ioctl_on_pipe(cmd, arg));
        assert_eq!(errno(r), Some(libc::EPERM), "{what}");
    }
}

/// A pipe and a sealable memfd, for the fcntl probes.
fn fcntl_fds() -> io::Result<(RawFd, RawFd)> {
    let p = pipe_fd()?;
    // SAFETY: the name is a static NUL-terminated string.
    let m = unsafe { libc::memfd_create(c"inacircle".as_ptr(), libc::MFD_ALLOW_SEALING) };
    cvt(c_long::from(m))?;
    Ok((p, m))
}

fn fcntl(fd: RawFd, cmd: u64, arg: c_long) -> io::Result<()> {
    // SAFETY: the probed commands take an int, or (F_SETOWN_EX) a pointer
    // that the callers below pass to their own stack data.
    cvt(unsafe { libc::syscall(libc::SYS_fcntl, c_long::from(fd), cmd as c_long, arg) })
}

#[test]
fn seccomp_fcntl_denies_async_lease_and_seals() {
    type Call = fn((RawFd, RawFd)) -> io::Result<()>;
    let denied: [(&str, Call); 7] = [
        ("F_SETOWN", |(p, _)| {
            // SAFETY: getpid takes no arguments.
            fcntl(p, F_SETOWN, c_long::from(unsafe { libc::getpid() }))
        }),
        ("F_SETOWN_EX", |(p, _)| {
            // struct f_owner_ex { int type; pid_t pid; }, F_OWNER_PID = 1.
            // SAFETY: getpid takes no arguments.
            let owner: [i32; 2] = [1, unsafe { libc::getpid() }];
            fcntl(p, F_SETOWN_EX, owner.as_ptr() as c_long)
        }),
        ("F_SETSIG", |(p, _)| fcntl(p, F_SETSIG, 0)),
        // A pipe: EINVAL / ENOTDIR without the filter.
        ("F_SETLEASE", |(p, _)| {
            fcntl(p, F_SETLEASE, c_long::from(libc::F_RDLCK))
        }),
        ("F_NOTIFY", |(p, _)| fcntl(p, F_NOTIFY, DN_ACCESS)),
        ("F_ADD_SEALS", |(_, m)| {
            fcntl(m, F_ADD_SEALS, c_long::from(libc::F_SEAL_SEAL))
        }),
        ("F_SETFL O_ASYNC", |(p, _)| {
            fcntl(p, F_SETFL, O_ASYNC as c_long)
        }),
    ];
    for (what, call) in denied {
        let control = errno(probe_with(Sb::Off, fcntl_fds, call));
        assert_ne!(control, Some(libc::EPERM), "control {what}");
        let r = probe_with(Sb::On(None), fcntl_fds, call);
        assert_eq!(errno(r), Some(libc::EPERM), "{what}");
    }
    let allowed: [(&str, Call); 6] = [
        ("F_GETFD", |(p, _)| fcntl(p, F_GETFD, 0)),
        ("F_SETFD", |(p, _)| {
            fcntl(p, F_SETFD, c_long::from(libc::FD_CLOEXEC))
        }),
        ("F_GETFL", |(p, _)| fcntl(p, F_GETFL, 0)),
        ("F_SETFL O_NONBLOCK", |(p, _)| {
            fcntl(p, F_SETFL, O_NONBLOCK as c_long)
        }),
        ("F_DUPFD_CLOEXEC", |(p, _)| fcntl(p, F_DUPFD_CLOEXEC, 0)),
        ("F_GETOWN", |(p, _)| fcntl(p, F_GETOWN, 0)),
    ];
    for (what, call) in allowed {
        assert_eq!(
            errno(probe_with(Sb::On(None), fcntl_fds, call)),
            None,
            "{what}"
        );
    }
}

fn clone_raw(flags: u64) -> io::Result<()> {
    // Every flag set probed here is invalid for the kernel (CLONE_THREAD
    // without CLONE_SIGHAND: EINVAL in copy_process before anything is
    // created), so no process or thread ever starts: the errno tells
    // whether the kernel saw the call.
    sys(libc::SYS_clone as u32, [flags as c_long, 0, 0, 0, 0, 0])
}

#[test]
fn seccomp_clone_rejects_escapes_keeps_fork() {
    let escapes: [(&str, u64); 8] = [
        ("CLONE_PARENT", CLONE_PARENT),
        ("CLONE_NEWUSER", CLONE_NEWUSER),
        ("CLONE_NEWNS", CLONE_NEWNS),
        ("CLONE_NEWPID", CLONE_NEWPID),
        ("CLONE_NEWNET", CLONE_NEWNET),
        ("high word", 1 << 32),
        ("signal 65", 65),
        ("CLONE_PTRACE", CLONE_PTRACE),
    ];
    for (what, flags) in escapes {
        let f = flags | CLONE_THREAD;
        assert_eq!(
            errno(probe(Sb::Off, move || clone_raw(f))),
            Some(libc::EINVAL),
            "control {what}"
        );
        assert_eq!(
            errno(probe(Sb::On(None), move || clone_raw(f))),
            Some(libc::EPERM),
            "{what}"
        );
    }
    // Approved flags reach the kernel (its own EINVAL), and a real fork
    // works and is reaped.
    let f = CLONE_DETACHED | CLONE_THREAD;
    assert_eq!(
        errno(probe(Sb::On(None), move || clone_raw(f))),
        Some(libc::EINVAL)
    );
    probe(Sb::On(None), || in_fork(|| Ok(()))).unwrap();
    // clone3: EINVAL from the kernel without the filter, ENOSYS with it
    // (glibc then falls back to clone, see the workload test).
    let nr = libc::SYS_clone3 as u32;
    let c3 = move || sys(nr, [NULL, 0, 0, 0, 0, 0]);
    assert_eq!(errno(probe(Sb::Off, c3)), Some(libc::EINVAL));
    assert_eq!(errno(probe(Sb::On(None), c3)), Some(libc::ENOSYS));
}

#[test]
fn seccomp_prlimit_only_on_self() {
    let call = |pid: c_long| {
        move || {
            let mut old = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let p = (&mut old as *mut libc::rlimit) as c_long;
            sys(
                libc::SYS_prlimit64 as u32,
                [pid, libc::RLIMIT_DATA as c_long, NULL, p, 0, 0],
            )
        }
    };
    assert_eq!(errno(probe(Sb::On(None), call(0))), None, "pid 0");
    let own = move || {
        // SAFETY: getpid takes no arguments.
        let pid = c_long::from(unsafe { libc::getpid() });
        call(pid)()
    };
    assert_eq!(errno(probe(Sb::Off, own)), None, "control own pid");
    assert_eq!(errno(probe(Sb::On(None), own)), Some(libc::EPERM));
    // pid_t truncation: the kernel reads 1 << 32 as pid 0.
    assert_eq!(errno(probe(Sb::Off, call(1 << 32))), None, "control");
    assert_eq!(errno(probe(Sb::On(None), call(1 << 32))), Some(libc::EPERM));
    assert_eq!(errno(probe(Sb::Off, call(NO_PID))), Some(libc::ESRCH));
    assert_eq!(errno(probe(Sb::On(None), call(NO_PID))), Some(libc::EPERM));
}

#[test]
fn seccomp_prctl_lifecycle_setters_denied() {
    let nr = libc::SYS_prctl as u32;
    let call = move |opt: c_long, arg: c_long| move || sys(nr, [opt, arg, 0, 0, 0, 0]);
    let pdeathsig = c_long::from(libc::PR_SET_PDEATHSIG);
    let set = [
        ("PR_SET_PDEATHSIG 0", pdeathsig, 0),
        (
            "PR_SET_PDEATHSIG SIGTERM",
            pdeathsig,
            c_long::from(libc::SIGTERM),
        ),
        (
            "PR_SET_CHILD_SUBREAPER",
            c_long::from(libc::PR_SET_CHILD_SUBREAPER),
            0,
        ),
        ("PR_SET_PTRACER", c_long::from(libc::PR_SET_PTRACER), 0),
        // `int option`: the kernel drops the high word.
        ("PR_SET_PDEATHSIG high", pdeathsig | 1 << 32, 0),
    ];
    for (what, opt, arg) in set {
        let control = errno(probe(Sb::Off, call(opt, arg)));
        assert_ne!(control, Some(libc::EPERM), "control {what}");
        let r = probe(Sb::On(None), call(opt, arg));
        assert_eq!(errno(r), Some(libc::EPERM), "{what}");
    }
    // The queries and PR_SET_NAME stay.
    let name = c"inacircle-probe".as_ptr() as c_long;
    let set_name = call(c_long::from(libc::PR_SET_NAME), name);
    assert_eq!(errno(probe(Sb::On(None), set_name)), None, "PR_SET_NAME");
    for opt in [libc::PR_GET_PDEATHSIG, libc::PR_GET_CHILD_SUBREAPER] {
        let get = move || {
            let mut out = 0i32;
            let p = (&mut out as *mut i32) as c_long;
            sys(nr, [c_long::from(opt), p, 0, 0, 0, 0])
        };
        assert_eq!(errno(probe(Sb::On(None), get)), None, "option {opt}");
    }
}

// ---- the real workload ----

const CHILD_ENV: &str = "INACIRCLE_HARDENING_CHILD";
const WORK_ENV: &str = "INACIRCLE_HARDENING_WORK";
const CHILD_TAG: &str = "HARDENING-CHILD ";
/// The fd the canary is inherited on.
const CANARY_FD: RawFd = 9;

/// Re-runs this test binary as `child_main` in `mode`, under `sb`, with
/// `canary` (if >= 0) dup'd to `CANARY_FD` before the sandbox; returns
/// what the child reported.
fn run_child(mode: &str, sb: Sb, work: &Path, canary: RawFd) -> String {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(&exe);
    cmd.args(["--exact", "child_main", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, mode)
        .env(WORK_ENV, work)
        .stdin(Stdio::null());
    let mut keep = None;
    let fd = sandbox_fd(sb, &mut keep);
    // SAFETY: dup2, fcntl and apply_in_child are async-signal-safe; the ruleset
    // (by the caller, or `keep`) and canary fds stay open for the whole
    // spawn.
    unsafe {
        cmd.pre_exec(move || {
            // The ruleset may itself be fd 9: move it out of the canary's
            // way first (above it, close-on-exec), or dup2 replaces it.
            let fd = match fd {
                Some(CANARY_FD) if canary >= 0 && canary != CANARY_FD => {
                    let moved = libc::fcntl(CANARY_FD, libc::F_DUPFD_CLOEXEC, CANARY_FD + 1);
                    if moved < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Some(moved)
                }
                fd => fd,
            };
            // dup2 onto itself changes nothing, so a canary that already
            // is fd 9 would keep its O_CLOEXEC: clear it instead.
            let inherited = match canary {
                c if c < 0 => 0,
                CANARY_FD => libc::fcntl(CANARY_FD, libc::F_SETFD, 0),
                c => libc::dup2(c, CANARY_FD),
            };
            if inherited < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(fd) = fd {
                sandbox::apply_in_child(fd, &NO_LIMIT)?;
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // libtest prints `test child_main ... ` on the same line first.
    match stdout
        .lines()
        .find_map(|l| l.split_once(CHILD_TAG).map(|(_, r)| r))
    {
        Some(r) => r.to_owned(),
        None => panic!(
            "no child report ({:?})\nstdout:\n{stdout}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

// ---- the musl build ----

/// Set by the CI step that builds and runs this binary for
/// `<arch>-unknown-linux-musl`: the run must really be musl's, so its
/// thread start (musl's own clone flags, CLONE_DETACHED included) and
/// fork + exec go through the filter in the workload tests above.
const EXPECT_MUSL: &str = "INACIRCLE_EXPECT_MUSL";

#[test]
fn musl_run_is_really_musl() {
    let is_musl = cfg!(target_env = "musl");
    if std::env::var_os(EXPECT_MUSL).is_some_and(|v| v == "1") {
        assert!(is_musl, "{EXPECT_MUSL}=1 but not a musl build");
    }
}

// ---- real privileged parents ----

/// Set on the native CI runners (passwordless `sudo -n`): the privileged
/// cases must run there. Elsewhere the test is skipped, so a developer's
/// suite never runs anything as root unasked.
const REQUIRE_PRIVILEGED: &str = "INACIRCLE_REQUIRE_PRIVILEGED_TEST";
const PRIV_ENV: &str = "INACIRCLE_HARDENING_PRIV";
const PRIV_IDS_ENV: &str = "INACIRCLE_HARDENING_IDS";
const PRIV_TAG: &str = "HARDENING-PRIV ";

/// The gate against real (not injected) privileged parents: this binary
/// re-run as root by `sudo -n`, then left as root, with a saved uid 0
/// (setuid-like), or as the user with permitted capabilities kept
/// (`PR_SET_KEEPCAPS`). Each is refused and no ruleset is built. Positive
/// control: the same root child, fully dropped to the user, passes.
#[test]
fn sandbox_gate_refuses_real_privileged_parents() {
    if std::env::var_os(REQUIRE_PRIVILEGED).is_none_or(|v| v != "1") {
        eprintln!(
            "SKIPPED sandbox_gate_refuses_real_privileged_parents: {REQUIRE_PRIVILEGED} unset"
        );
        return;
    }
    assert!(landlock_or_skip(
        "sandbox_gate_refuses_real_privileged_parents"
    ));
    let exe = std::env::current_exe().unwrap();
    // SAFETY: getuid/getgid have no preconditions.
    let ids = format!("{}:{}", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    });
    for (mode, want) in [
        ("root", "refused"),
        ("saved-root", "refused"),
        ("kept-caps", "refused"),
        ("dropped", "passed"),
    ] {
        let out = Command::new("sudo")
            .arg("-n")
            .arg("env")
            .arg(format!("{PRIV_ENV}={mode}"))
            .arg(format!("{PRIV_IDS_ENV}={ids}"))
            .arg(&exe)
            .args([
                "--exact",
                "privileged_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let got = stdout
            .lines()
            .find_map(|l| l.split_once(PRIV_TAG).map(|(_, r)| r.to_owned()))
            .unwrap_or_else(|| {
                panic!(
                    "{mode}: no report ({}): {stdout}{}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        assert!(got.starts_with(want), "{mode}: {got}");
    }
}

/// Child side of `sandbox_gate_refuses_real_privileged_parents`, run as
/// root; a no-op without its env.
#[test]
fn privileged_child() {
    let Ok(mode) = std::env::var(PRIV_ENV) else {
        return;
    };
    let ids = std::env::var(PRIV_IDS_ENV).unwrap();
    let (uid, gid) = ids.split_once(':').unwrap();
    let (uid, gid): (libc::uid_t, libc::gid_t) = (uid.parse().unwrap(), gid.parse().unwrap());
    let r = (|| -> io::Result<String> {
        // SAFETY: the id calls and prctl take plain integers; glibc's
        // setres[ug]id apply to every thread of this process.
        unsafe {
            match mode.as_str() {
                "root" => {}
                "saved-root" => {
                    cvt(libc::setresgid(gid, gid, gid) as c_long)?;
                    cvt(libc::setresuid(uid, uid, 0) as c_long)?;
                }
                "kept-caps" => {
                    cvt(libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) as c_long)?;
                    cvt(libc::setresgid(gid, gid, gid) as c_long)?;
                    cvt(libc::setresuid(uid, uid, uid) as c_long)?;
                }
                "dropped" => {
                    cvt(libc::setgroups(0, std::ptr::null()) as c_long)?;
                    cvt(libc::setresgid(gid, gid, gid) as c_long)?;
                    cvt(libc::setresuid(uid, uid, uid) as c_long)?;
                }
                m => return Err(io::Error::other(format!("unknown mode {m}"))),
            }
        }
        let snap = sandbox::PrivilegeSnapshot::current()?;
        let policy = Policy {
            exec: &[],
            read: &[],
            read_exact: &[],
            write: None,
        };
        match (
            sandbox::validate_privilege(&snap),
            sandbox::ready(),
            sandbox::build(&policy),
        ) {
            (Ok(()), Ok(_), Ok(_)) => Ok(format!("passed {snap:?}")),
            (Err(e), Err(g), Err(b))
                if [e.kind(), g.kind(), b.kind()] == [io::ErrorKind::PermissionDenied; 3] =>
            {
                Ok(format!("refused {e} ({snap:?})"))
            }
            (v, g, b) => Ok(format!(
                "inconsistent: validate {v:?}, ready {:?}, build {:?} ({snap:?})",
                g.map(|_| ()),
                b.map(|_| ())
            )),
        }
    })();
    match r {
        Ok(s) => println!("{PRIV_TAG}{s}"),
        Err(e) => println!("{PRIV_TAG}error {e}"),
    }
    let _ = io::stdout().flush();
    std::process::exit(0);
}

/// The child side of `run_child`; a no-op in a normal test run.
#[test]
fn child_main() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let work = PathBuf::from(std::env::var_os(WORK_ENV).unwrap());
    let r = match mode.as_str() {
        "core" => core_workload(&work),
        "canary" => {
            // SAFETY: F_GETFD takes no pointer.
            let open = unsafe { libc::fcntl(CANARY_FD, libc::F_GETFD) } >= 0;
            Ok(if open { "open" } else { "closed" }.to_owned())
        }
        m => Err(format!("unknown mode {m}")),
    };
    match r {
        Ok(s) => println!("{CHILD_TAG}ok {s}"),
        Err(e) => println!("{CHILD_TAG}fail {e}"),
    }
    let _ = io::stdout().flush();
    std::process::exit(0);
}

/// What a normal offline action does: threads with small allocations,
/// fork + exec, a pipe, work files with a lock, randomness, its own limits
/// and name, anonymous and memfd memory.
fn core_workload(work: &Path) -> Result<String, String> {
    const THREADS: usize = 8;
    const SHARE: usize = 1 << 20; // 1 MiB each: small, but every page touched
    let e = |what: &str| {
        let what = what.to_owned();
        move |err: io::Error| format!("{what}: {err}")
    };
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            std::thread::Builder::new()
                .spawn(move || {
                    let mut v = vec![0u8; SHARE];
                    for p in (0..SHARE).step_by(4096) {
                        v[p] = i as u8 + 1;
                    }
                    v.iter().map(|&b| u64::from(b)).sum::<u64>()
                })
                .map_err(e("thread spawn"))
        })
        .collect::<Result<_, _>>()?;
    let mut sum = 0;
    for h in handles {
        sum += h.join().map_err(|_| "thread panicked".to_owned())?;
    }
    let status = Command::new("/bin/true")
        .status()
        .map_err(e("spawn /bin/true"))?;
    if !status.success() {
        return Err(format!("/bin/true: {status}"));
    }
    let (mut r, mut w) = io::pipe().map_err(e("pipe"))?;
    w.write_all(b"ping").map_err(e("pipe write"))?;
    drop(w);
    let mut got = String::new();
    r.read_to_string(&mut got).map_err(e("pipe read"))?;
    let file = work.join("core.txt");
    fs::write(&file, "converted").map_err(e("work write"))?;
    let f = fs::File::open(&file).map_err(e("work open"))?;
    f.lock().map_err(e("flock"))?;
    let back = fs::read_to_string(&file).map_err(e("work read"))?;
    let mut rnd = [0u8; 16];
    // SAFETY: getrandom writes at most 16 bytes into `rnd`.
    cvt(unsafe { libc::getrandom(rnd.as_mut_ptr().cast(), rnd.len(), 0) } as c_long)
        .map_err(e("getrandom"))?;
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a writable rlimit.
    cvt(c_long::from(unsafe {
        libc::getrlimit(libc::RLIMIT_DATA, &mut lim)
    }))
    .map_err(e("getrlimit"))?;
    // SAFETY: a static NUL-terminated name.
    cvt(c_long::from(unsafe {
        libc::prctl(libc::PR_SET_NAME, c"core".as_ptr())
    }))
    .map_err(e("PR_SET_NAME"))?;
    // SAFETY: an anonymous private mapping, unmapped again; a static name.
    unsafe {
        let len = 1 << 16;
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", io::Error::last_os_error()));
        }
        libc::munmap(p, len);
        let m = libc::memfd_create(c"core".as_ptr(), libc::MFD_CLOEXEC);
        cvt(c_long::from(m)).map_err(e("memfd_create"))?;
        libc::close(m);
    }
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    if got != "ping" || back != "converted" {
        return Err(format!("pipe {got:?}, file {back:?}"));
    }
    Ok(format!("threads={THREADS} sum={sum} cpus={cpus}"))
}

/// The sum the workload's threads report: page bytes of each thread.
fn workload_sum() -> u64 {
    (1..=8u64).map(|i| i * (1 << 20) / 4096).sum()
}

#[test]
fn seccomp_core_workload_runs_under_the_filter() {
    let work = Tmp::new("core");
    let r = run_child("core", Sb::On(None), &work.0, -1);
    assert!(
        r.starts_with(&format!("ok threads=8 sum={}", workload_sum())),
        "{r}"
    );
}

#[test]
fn seccomp_core_workload_runs_in_the_full_sandbox() {
    if !landlock_or_skip("seccomp_core_workload_runs_in_the_full_sandbox") {
        return;
    }
    let work = Tmp::new("core-full");
    let exe = std::env::current_exe().unwrap();
    let rs = ruleset(&[exe], &work.0);
    let r = run_child("core", Sb::On(Some(rs.as_raw_fd())), &work.0, -1);
    assert!(
        r.starts_with(&format!("ok threads=8 sum={}", workload_sum())),
        "{r}"
    );
}

#[test]
fn sandbox_inherited_fd_canary_closed_at_exec() {
    let work = Tmp::new("canary");
    let canary = fs::File::create(work.0.join("canary")).unwrap();
    let fd = canary.as_raw_fd();
    assert_eq!(
        run_child("canary", Sb::Off, &work.0, fd),
        "ok open",
        "control"
    );
    assert_eq!(run_child("canary", Sb::On(None), &work.0, fd), "ok closed");
}
