//! Running a test body in a forked child, for tests that change process-wide
//! state (the fd table, rlimits) or install a seccomp filter, which must not
//! touch the parallel tests of the same binary.
#![allow(dead_code)]

/// Runs `f` in a forked child and returns its wait status. `f` returns the
/// child's exit code (0: all held); a panic is code 99 and an alarm bounds a
/// hang. Only the forking thread exists in the child: build servers (threads)
/// inside `f`, not before.
pub fn in_child(f: impl FnOnce() -> i32) -> libc::c_int {
    // SAFETY: fork(2) in a multithreaded process: the child is NOT
    // async-signal-safe, because it allocates and may spawn threads. That is
    // acceptable in test-only code: glibc makes malloc fork-safe, and a std
    // lock held by another thread at the instant of the fork could deadlock
    // the child, which the alarm below bounds (a spurious failure, never a
    // hang). The child leaves with `_exit`, never returning into the harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // SAFETY: close_range(2) is a plain syscall (fork-safe) taking
        // integers. The child inherited copies of every fd of the parent's
        // parallel tests (server sockets); keeping them would delay the EOF
        // those tests wait for until this child exits. Only stdin, stdout and
        // stderr stay; `f` creates what it needs itself.
        unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) };
        // SAFETY: plain alarm(2): a hung child dies by SIGALRM.
        unsafe { libc::alarm(30) };
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(99);
        // SAFETY: leave the forked child without running the parent's
        // destructors or harness.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    // SAFETY: waits for the child forked above.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    status
}

/// `in_child` that asserts the child exited with code 0.
pub fn assert_child_ok(f: impl FnOnce() -> i32) {
    let st = in_child(f);
    assert!(
        libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0,
        "child status {st:#x} (exit code {}, 99 = panic)",
        libc::WEXITSTATUS(st)
    );
}

/// Makes the calling thread's `nrs` syscalls fail with `errno` (a seccomp
/// filter; threads started later inherit it, existing ones do not). Child
/// side only: the filter cannot be removed. Native architecture, so no arch
/// check.
pub fn deny_syscalls(nrs: &[libc::c_long], errno: u32) {
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
    // SAFETY: prctl and seccomp take plain integers and a pointer to `fprog`,
    // which points at `prog`; both outlive the calls.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, 0, &fprog),
            0
        );
    }
}

/// Bytes waiting in `fd`'s receive queue (FIONREAD).
pub fn bytes_waiting(fd: i32) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int through the pointer.
    let r = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) };
    assert_eq!(r, 0);
    n as usize
}

/// Waits (bounded) until `fd` holds at least `want` bytes.
pub fn wait_for_bytes(fd: i32, want: usize) {
    let t = std::time::Instant::now();
    while bytes_waiting(fd) < want {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "peer data did not arrive: {} of {want}",
            bytes_waiting(fd)
        );
        std::thread::yield_now();
    }
}
