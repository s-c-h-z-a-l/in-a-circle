//! Behaviour that depends on process-wide state (the fd table, resource
//! limits). Each test body runs in a forked child, so the changes cannot
//! disturb the other threads of this binary.

mod child;
mod common;

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use child::assert_child_ok;
use common::{Fake, Peer};
use dbus::{Conn, Error, Message, Value};

fn after(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}

fn call(member: &str) -> Message {
    Message::call(0, "org.example", "/o", "org.example.I", member, vec![])
}

fn is_open(fd: i32) -> bool {
    // SAFETY: F_GETFD takes no pointer and only reads the fd table.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Makes `fd` an open descriptor (of /dev/null), whatever it was.
fn occupy(fd: i32) {
    // SAFETY: the path is NUL-terminated; the fds are ours.
    unsafe {
        let n = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        assert!(n >= 0);
        if n != fd {
            assert_eq!(libc::dup2(n, fd), fd);
            libc::close(n);
        }
    }
}

fn set_nofile(soft: libc::rlim_t) -> libc::rlimit {
    let mut old = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `old` and `new` are valid rlimit structs.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut old), 0);
        let new = libc::rlimit {
            rlim_cur: soft,
            rlim_max: old.rlim_max,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &new), 0);
    }
    old
}

fn restore_nofile(old: libc::rlimit) {
    // SAFETY: `old` is a valid rlimit struct.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) }, 0);
}

/// A peer that answers the call with a message that has `fds` attached.
fn conn_receiving_fds(fds: usize) -> (Fake, Result<Message, Error>, Conn) {
    let target = std::env::temp_dir().join(format!("inacircle-ps-{}-{fds}", std::process::id()));
    std::fs::write(&target, b"x").unwrap();
    let t2 = target.clone();
    let fake = Fake::start(move |p| {
        p.handshake();
        let m = p.recv();
        let f = std::fs::File::open(&t2).unwrap();
        let reply = dbus::encode(&Message {
            serial: 99,
            ..Peer::reply_msg(&m, vec![])
        })
        .unwrap();
        p.send_with_fds(&reply, &vec![f.as_raw_fd(); fds]);
        drop(f);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(5000)).unwrap();
    let r = c.call(call("Ping"), after(500));
    std::fs::remove_file(&target).unwrap();
    (fake, r, c)
}

#[test]
fn closing_a_passed_fd_closes_only_that_fd() {
    assert_child_ok(|| {
        // Stdin is open and unrelated: whatever is next to the passed fd in the
        // control buffer (padding is zeros, which would read as fd 0) is not ours.
        occupy(0);
        let (fake, r, c) = conn_receiving_fds(1);
        assert!(
            matches!(r, Err(Error::Malformed("unexpected fds"))),
            "{r:?}"
        );
        assert!(is_open(0), "fd 0 was closed along with the passed fd");
        drop(c);
        fake.join();
        0
    });
}

#[test]
fn ancillary_data_that_is_not_fds_is_not_closed_as_fds() {
    assert_child_ok(|| {
        // Ask the kernel to attach the peer's credentials (pid, uid, gid as
        // three ints) to what we receive. They are ancillary data, but no fds:
        // the numbers must not be closed as if they were.
        // SAFETY: geteuid/getegid/getpid have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let mut guards = Vec::new();
        for n in [uid, gid] {
            if n >= 3 && (n as libc::rlim_t) < nofile() && !guards.contains(&(n as i32)) {
                occupy(n as i32);
                guards.push(n as i32);
            }
        }
        assert!(!guards.is_empty(), "no uid/gid usable as a guard fd");
        let fake = Fake::start(|p| {
            p.handshake();
            let m = p.recv();
            p.reply(&m, vec![Value::Str("pong".into())]);
            p.drain();
        });
        let mut c = Conn::connect(&fake.address(), after(5000)).unwrap();
        let on: libc::c_int = 1;
        // SAFETY: a valid fd and a pointer to a c_int of the size given.
        let r = unsafe {
            libc::setsockopt(
                c.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&raw const on).cast(),
                size_of_val(&on) as libc::socklen_t,
            )
        };
        assert_eq!(r, 0);
        let r = c.call(call("Ping"), after(500));
        assert!(
            matches!(r, Err(Error::Malformed("unexpected fds"))),
            "{r:?}"
        );
        for g in guards {
            assert!(is_open(g), "fd {g} was closed");
            // SAFETY: the guard is ours.
            unsafe { libc::close(g) };
        }
        drop(c);
        fake.join();
        0
    });
}

fn nofile() -> libc::rlim_t {
    let mut l = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `l` is a valid rlimit struct.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) };
    l.rlim_cur
}

#[test]
fn no_free_fd_is_an_error_not_a_panic() {
    assert_child_ok(|| {
        let fake = Fake::start(|p| p.drain());
        let old = set_nofile(0);
        let r = Conn::connect(&fake.address(), after(5000));
        restore_nofile(old);
        assert!(matches!(&r, Err(Error::Io(_))), "{:?}", r.err());
        drop(fake);
        0
    });
}

#[test]
fn fd_zero_is_a_usable_socket() {
    assert_child_ok(|| {
        let fake = Fake::start(|p| {
            p.handshake();
            p.drain();
        });
        // SAFETY: closing stdin of the test process; nothing here reads it.
        unsafe { libc::close(0) };
        let c = Conn::connect(&fake.address(), after(5000));
        let c = c.unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(c.as_raw_fd(), 0, "the socket was meant to take fd 0");
        drop(c);
        occupy(0);
        fake.join();
        0
    });
}

#[test]
fn a_failing_poll_is_reported_not_retried_until_the_deadline() {
    assert_child_ok(|| {
        let fake = Fake::start(|p| {
            p.handshake();
            p.drain();
        });
        let mut c = Conn::connect(&fake.address(), after(5000)).unwrap();
        // poll(2) fails with EINVAL when nfds exceeds RLIMIT_NOFILE.
        let old = set_nofile(0);
        let t = Instant::now();
        let r = c.recv(after(2000));
        restore_nofile(old);
        assert!(matches!(&r, Err(Error::Io(_))), "{r:?}");
        assert!(t.elapsed() < Duration::from_millis(500));
        drop(c);
        fake.join();
        0
    });
}
