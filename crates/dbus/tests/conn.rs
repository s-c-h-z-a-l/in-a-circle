//! Connection behaviour against a scripted peer (and a real `dbus-daemon`
//! when one is installed).

mod child;
mod common;

use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use child::{assert_child_ok, deny_syscalls, wait_for_bytes};
use common::{Fake, Peer};
use dbus::{Address, Conn, Error, Message, Value, parse_address};

fn after(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}

fn call(member: &str) -> Message {
    Message::call(
        0,
        "org.example",
        "/org/example",
        "org.example.I",
        member,
        vec![],
    )
}

#[test]
fn connects_and_says_hello() {
    let fake = Fake::start(|p| {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let hex: String = uid
            .to_string()
            .chars()
            .map(|c| format!("{:x}", c as u32))
            .collect();
        assert_eq!(
            p.read_line(),
            format!("\0AUTH EXTERNAL {hex}\r\n").into_bytes()
        );
        p.write(b"OK 0123456789abcdef0123456789abcdef\r\n");
        assert_eq!(p.read_line(), b"BEGIN\r\n");
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("Hello"));
        assert_eq!(m.destination.as_deref(), Some("org.freedesktop.DBus"));
        assert_eq!(m.path.as_deref(), Some("/org/freedesktop/DBus"));
        assert_eq!(m.interface.as_deref(), Some("org.freedesktop.DBus"));
        p.reply(&m, vec![Value::Str(":1.42".into())]);
        p.drain();
    });
    let c = Conn::connect(&fake.address(), after(300)).unwrap();
    assert_eq!(c.unique_name(), ":1.42");
    drop(c);
    fake.join();
}

#[test]
fn connects_over_an_abstract_socket() {
    let fake = Fake::start_abstract(|p| {
        p.handshake();
        p.drain();
    });
    assert!(matches!(fake.address(), Address::Abstract(_)));
    let c = Conn::connect(&fake.address(), after(300)).unwrap();
    assert_eq!(c.unique_name(), ":1.42");
    drop(c);
    fake.join();
}

#[test]
fn call_returns_the_matching_reply_and_queues_signals() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("Ping"));
        p.signal("/a", "a.B", "First", vec![Value::U32(7)]);
        // A reply to some other call is not ours.
        let mut stale = m.clone();
        stale.serial = m.serial + 100;
        p.reply(&stale, vec![Value::Str("stale".into())]);
        p.signal("/a", "a.B", "Second", vec![]);
        p.reply(&m, vec![Value::Str("pong".into())]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    assert!(!c.pending());
    let r = c.call(call("Ping"), after(500)).unwrap();
    assert_eq!(r.body, vec![Value::Str("pong".into())]);
    assert!(c.pending());
    let s = c.recv(after(100)).unwrap().unwrap();
    assert_eq!(s.member.as_deref(), Some("First"));
    assert_eq!(s.body, vec![Value::U32(7)]);
    let s = c.recv(after(100)).unwrap().unwrap();
    assert_eq!(s.member.as_deref(), Some("Second"));
    assert!(!c.pending());
    let t = Instant::now();
    assert!(c.recv(after(30)).unwrap().is_none());
    assert!(t.elapsed() >= Duration::from_millis(30));
    drop(c);
    fake.join();
}

#[test]
fn remote_error_is_reported() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        p.error(&m, "org.example.Error.Nope", "no way");
        // The connection stays usable.
        let m = p.recv();
        p.reply(&m, vec![]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    match c.call(call("Fail"), after(500)) {
        Err(Error::Remote(name, msg)) => {
            assert_eq!(name, "org.example.Error.Nope");
            assert_eq!(msg, "no way");
        }
        other => panic!("{other:?}"),
    }
    c.call(call("Again"), after(500)).unwrap();
    drop(c);
    fake.join();
}

#[test]
fn auth_rejected_fails() {
    let fake = Fake::start(|p| {
        p.read_line();
        p.write(b"REJECTED EXTERNAL\r\n");
        p.drain();
    });
    let r = Conn::connect(&fake.address(), after(300));
    assert!(matches!(r, Err(Error::Auth)), "{:?}", r.err());
    fake.join();
}

#[test]
fn silent_server_times_out() {
    let fake = Fake::start(|p| p.drain());
    let t = Instant::now();
    let r = Conn::connect(&fake.address(), t + Duration::from_millis(300));
    let took = t.elapsed();
    assert!(matches!(r, Err(Error::Timeout)), "{:?}", r.err());
    assert!(took <= Duration::from_millis(350), "{took:?}");
    fake.join();
}

#[test]
fn silent_call_times_out() {
    let fake = Fake::start(|p| {
        p.handshake();
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let t = Instant::now();
    let r = c.call(call("Ping"), t + Duration::from_millis(500));
    let took = t.elapsed();
    assert!(matches!(r, Err(Error::Timeout)), "{:?}", r.err());
    assert!(took >= Duration::from_millis(500), "{took:?}");
    assert!(took <= Duration::from_millis(550), "{took:?}");
    drop(c);
    fake.join();
}

#[test]
fn server_dying_midway_is_an_error() {
    // Accepts the auth, then goes away before answering Hello.
    let fake = Fake::start(|p| {
        p.auth();
        p.recv();
    });
    let t = Instant::now();
    let r = Conn::connect(&fake.address(), after(300));
    assert!(matches!(r, Err(Error::Closed)), "{:?}", r.err());
    assert!(t.elapsed() < Duration::from_millis(100));
    fake.join();
}

#[test]
fn missing_bus_is_an_error_not_a_hang() {
    let dir = std::env::temp_dir().join(format!("inacircle-nobus-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("plain");
    std::fs::write(&file, b"not a socket").unwrap();
    for addr in [
        Address::Path(dir.join("missing")),
        Address::Path(file),
        Address::Path(PathBuf::new()),
        Address::Path(PathBuf::from(format!("/{}", "x".repeat(200)))),
    ] {
        let t = Instant::now();
        let r = Conn::connect(&addr, after(300));
        assert!(r.is_err(), "{addr:?}");
        assert!(t.elapsed() < Duration::from_millis(50), "{addr:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
    // Only unix transports are used.
    assert_eq!(parse_address("tcp:host=localhost,port=4000"), None);
    assert_eq!(
        parse_address("launchd:env=DBUS_LAUNCHD_SESSION_BUS_SOCKET"),
        None
    );
}

/// Open fds of this process that point at `target`.
fn fds_pointing_at(target: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|l| l == target)
        .count()
}

fn refuses_fds(count: usize) {
    let target =
        std::env::temp_dir().join(format!("inacircle-fdpass-{}-{count}", std::process::id()));
    std::fs::write(&target, b"x").unwrap();
    let t2 = target.clone();
    let fake = Fake::start(move |p| {
        p.handshake();
        let m = p.recv();
        let f = std::fs::File::open(&t2).unwrap();
        let fds = vec![f.as_raw_fd(); count];
        let reply = dbus::encode(&Message {
            serial: 99,
            ..Peer::reply_msg(&m, vec![])
        })
        .unwrap();
        p.send_with_fds(&reply, &fds);
        drop(f);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let r = c.call(call("Ping"), after(500));
    assert!(
        matches!(r, Err(Error::Malformed("unexpected fds"))),
        "{:?}",
        r.err()
    );
    drop(c);
    fake.join();
    assert_eq!(fds_pointing_at(&target), 0);
    std::fs::remove_file(&target).unwrap();
}

#[test]
fn fd_passing_is_refused() {
    refuses_fds(1);
    // More fds than the control buffer holds: MSG_CTRUNC.
    refuses_fds(20);
}

#[test]
fn oversized_message_is_refused() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        let mut b = vec![b'l', 2, 0, 1];
        b.extend((128u32 << 10).to_le_bytes()); // body length: 128 KiB
        b.extend(m.serial.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend([0xaa; 1000]);
        p.write(&b);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let r = c.call(call("Ping"), after(500));
    assert!(matches!(r, Err(Error::TooLarge)), "{:?}", r.err());
    // Only the fixed header was read: the rest is still in the socket. Count
    // it by reading, not with FIONREAD: for a partially consumed AF_UNIX
    // stream skb the FIONREAD count depends on the kernel version (1000 on
    // some, 1016 = the consumed header included, on others).
    let mut left = Vec::new();
    loop {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is valid for `buf.len()` writable bytes.
        let n = unsafe {
            libc::recv(
                c.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            // EAGAIN: nothing more is queued.
            break;
        }
        left.extend_from_slice(&buf[..n as usize]);
    }
    assert_eq!(left.len(), 1000);
    assert!(left.iter().all(|&x| x == 0xaa));
    // The stream is out of sync now: the connection refuses further use.
    let r = c.call(call("Ping"), after(50));
    assert!(matches!(r, Err(Error::Closed)), "{:?}", r.err());
    drop(c);
    fake.join();
}

#[test]
fn real_daemon_roundtrip() {
    let child = Command::new("dbus-daemon")
        .args(["--session", "--print-address", "--nofork"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("real_daemon_roundtrip: skipped, no dbus-daemon ({e})");
            return;
        }
    };
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _daemon = Kill(child);
    let addr = parse_address(line.trim()).expect(&line);
    // Generous deadlines: this checks the protocol, not the timing.
    let mut c = Conn::connect(&addr, after(2000)).unwrap();
    assert!(c.unique_name().starts_with(":1."), "{}", c.unique_name());
    c.add_match(
        "type='signal',interface='org.freedesktop.Notifications'",
        after(2000),
    )
    .unwrap();
    let r = c
        .call(
            Message::call(
                0,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ListNames",
                vec![],
            ),
            after(2000),
        )
        .unwrap();
    let Some(Value::Array(_, names)) = r.body.first() else {
        panic!("{r:?}")
    };
    for want in ["org.freedesktop.DBus", c.unique_name()] {
        assert!(names.contains(&Value::Str(want.into())), "{want} {names:?}");
    }
    // The daemon announced our name with a signal.
    let s = c.recv(after(2000)).unwrap().unwrap();
    assert_eq!(s.member.as_deref(), Some("NameAcquired"));
}

/// `n` copies of one encoded message, for a peer that floods.
fn batch(m: Message, n: usize) -> Vec<u8> {
    let one = dbus::encode(&Message { serial: 1, ..m }).unwrap();
    one.repeat(n)
}

fn signal(member: &str, body: Vec<Value>) -> Message {
    Message {
        kind: dbus::Kind::Signal,
        path: Some("/a".into()),
        interface: Some("a.B".into()),
        member: Some(member.into()),
        reply_serial: None,
        ..Peer::reply_msg(&call("x"), body)
    }
}

#[test]
fn signal_flood_does_not_extend_a_call_deadline() {
    let fake = Fake::start(|p| {
        p.handshake();
        p.recv();
        p.flood(&batch(signal("Spam", vec![Value::U32(1)]), 64));
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let t = Instant::now();
    let r = c.call(call("Ping"), t + Duration::from_millis(200));
    let took = t.elapsed();
    assert!(matches!(r, Err(Error::Timeout)), "{:?}", r.err());
    assert!(took <= Duration::from_millis(250), "{took:?}");
    drop(c);
    fake.join();
}

#[test]
fn stale_reply_flood_does_not_extend_a_recv_deadline() {
    let fake = Fake::start(|p| {
        p.handshake();
        let mut stale = call("x");
        stale.serial = 9999;
        p.flood(&batch(Peer::reply_msg(&stale, vec![]), 64));
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let t = Instant::now();
    let r = c.recv(t + Duration::from_millis(200));
    let took = t.elapsed();
    assert!(matches!(r, Ok(None)), "{r:?}");
    assert!(took <= Duration::from_millis(250), "{took:?}");
    drop(c);
    fake.join();
}

#[test]
fn queued_signals_are_bounded_by_bytes() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        for i in 0..100u32 {
            p.signal(
                "/a",
                "a.B",
                "Big",
                vec![
                    Value::U32(i),
                    Value::Array("y".into(), vec![Value::Byte(7); 4000]),
                ],
            );
        }
        p.reply(&m, vec![Value::Str("pong".into())]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let r = c.call(call("Ping"), after(2000)).unwrap();
    assert_eq!(r.body, vec![Value::Str("pong".into())]);
    assert!(c.queued_bytes() <= 256 << 10, "{}", c.queued_bytes());
    // The newest signals survive, in order, and decode intact.
    let mut last = None;
    while let Some(s) = c.recv(after(50)).unwrap() {
        let Value::U32(i) = s.body[0] else { panic!() };
        assert!(last.is_none_or(|l| l + 1 == i));
        last = Some(i);
    }
    assert_eq!(last, Some(99));
    assert_eq!(c.queued_bytes(), 0);
    drop(c);
    fake.join();
}

#[test]
fn timed_out_call_breaks_the_connection() {
    // A late reply would be dropped anyway; the connection is not reused.
    let fake = Fake::start(|p| {
        p.handshake();
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    assert!(matches!(
        c.call(call("Ping"), after(20)),
        Err(Error::Timeout)
    ));
    let t = Instant::now();
    let r = c.call(call("Ping"), after(500));
    assert!(matches!(r, Err(Error::Closed)), "{:?}", r.err());
    assert!(t.elapsed() < Duration::from_millis(50));
    drop(c);
    fake.join();
}

#[test]
fn partial_write_timeout_breaks_the_connection() {
    // The peer stops reading: a 1 MiB call cannot be written in time and
    // leaves part of a frame on the wire.
    let fake = Fake::start(|p| {
        p.handshake();
        std::thread::sleep(Duration::from_millis(300));
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let mut big = call("Big");
    big.body = vec![Value::Str("x".repeat(1 << 20))];
    big.signature = "s".into();
    assert!(matches!(c.call(big, after(20)), Err(Error::Timeout)));
    let r = c.call(call("Ping"), after(500));
    assert!(matches!(r, Err(Error::Closed)), "{:?}", r.err());
    drop(c);
    fake.join();
}

fn readable(fd: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: p is one valid pollfd.
    unsafe { libc::poll(&mut p, 1, 0) > 0 && p.revents & libc::POLLIN != 0 }
}

fn wait_readable(fd: i32) {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: p is one valid pollfd.
    assert!(
        unsafe { libc::poll(&mut p, 1, 2000) } > 0,
        "peer sent nothing"
    );
}

#[test]
fn recv_now_does_not_block_and_clears_pollin() {
    let fake = Fake::start(|p| {
        p.handshake();
        p.signal("/a", "a.B", "One", vec![Value::U32(1)]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    wait_readable(c.as_raw_fd());
    let m = c.recv_now().unwrap().expect("a signal is waiting");
    assert_eq!(m.member.as_deref(), Some("One"));
    assert!(!readable(c.as_raw_fd()), "POLLIN must clear once consumed");
    let t = Instant::now();
    assert!(c.recv_now().unwrap().is_none());
    assert!(t.elapsed() < Duration::from_millis(50));
    drop(c);
    fake.join();
}

#[test]
fn recv_now_keeps_a_partial_frame() {
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (sent_tx, sent_rx) = std::sync::mpsc::channel::<()>();
    let fake = Fake::start(move |p| {
        p.handshake();
        let mut m = Peer::reply_msg(&call("x"), vec![Value::U32(9)]);
        m.kind = dbus::Kind::Signal;
        m.reply_serial = None;
        m.path = Some("/a".into());
        m.interface = Some("a.B".into());
        m.member = Some("Split".into());
        m.serial = 1;
        let bytes = dbus::encode(&m).unwrap();
        let (head, tail) = bytes.split_at(bytes.len() / 2);
        p.write(head);
        sent_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        p.write(tail);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    sent_rx.recv().unwrap();
    wait_readable(c.as_raw_fd());
    assert!(c.recv_now().unwrap().is_none());
    assert!(!readable(c.as_raw_fd()));
    go_tx.send(()).unwrap();
    wait_readable(c.as_raw_fd());
    let m = c.recv_now().unwrap().expect("the frame is complete now");
    assert_eq!(m.member.as_deref(), Some("Split"));
    assert_eq!(m.body, vec![Value::U32(9)]);
    drop(c);
    fake.join();
}

/// A connection whose peer answered one call without reading it and then
/// closed with that request unread: the kernel resets the stream, so the
/// next read fails with `ECONNRESET` (not an end-of-stream).
fn reset_after_one_call() -> (Fake, Conn) {
    let fake = Fake::start(|p| {
        p.handshake();
        // The client's next serial after Hello's 1; wait for the request to
        // arrive, so that it is unread when we close.
        let mut m = call("Ping");
        m.serial = 2;
        wait_readable(p.stream.as_raw_fd());
        p.reply(&m, vec![]);
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    c.call(call("Ping"), after(500)).unwrap();
    // The peer's close has landed once the socket reports the reset.
    wait_readable(c.as_raw_fd());
    (fake, c)
}

fn is_reset(r: &Result<impl std::fmt::Debug, Error>) -> bool {
    matches!(r, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionReset)
}

#[test]
fn a_reset_connection_is_an_io_error_for_recv() {
    let (fake, mut c) = reset_after_one_call();
    let t = Instant::now();
    let r = c.recv(after(1000));
    assert!(is_reset(&r), "{r:?}");
    assert!(t.elapsed() < Duration::from_millis(500));
    // And the connection is unusable from then on.
    assert!(matches!(c.recv(after(50)), Err(Error::Closed)));
    drop(c);
    fake.join();
}

#[test]
fn a_reset_connection_is_an_io_error_for_recv_now() {
    let (fake, mut c) = reset_after_one_call();
    let r = c.recv_now();
    assert!(is_reset(&r), "{r:?}");
    assert!(matches!(c.recv_now(), Err(Error::Closed)));
    drop(c);
    fake.join();
}

/// A socket file at `path` that accepts nothing; returns whether a client
/// reached it.
fn reached(path: &std::path::Path, addr: Address) -> (bool, Result<Conn, Error>) {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let r = Conn::connect(&addr, after(50));
    (listener.accept().is_ok(), r)
}

fn invalid_input(r: &Result<Conn, Error>) -> bool {
    // Refused by us, before any syscall: no OS error code.
    matches!(r, Err(Error::Io(e))
        if e.kind() == std::io::ErrorKind::InvalidInput && e.raw_os_error().is_none())
}

#[test]
fn unusable_socket_paths_are_refused_before_the_kernel_sees_them() {
    let dir = std::env::temp_dir().join(format!("inacircle-paths-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s");
    // A NUL would cut the path short: "<dir>/s\0junk" must not reach "<dir>/s".
    let mut nul = sock.clone().into_os_string().into_encoded_bytes();
    nul.extend(b"\0junk");
    let nul = PathBuf::from(os_string(nul));
    let (reached_it, r) = reached(&sock, Address::Path(nul));
    assert!(!reached_it, "connected through a path with a NUL");
    assert!(invalid_input(&r), "{:?}", r.err());
    std::fs::remove_file(&sock).unwrap();

    // An empty path and one that cannot fit in sockaddr_un.
    for p in [
        PathBuf::new(),
        PathBuf::from(format!("/{}", "x".repeat(107))),
    ] {
        let r = Conn::connect(&Address::Path(p.clone()), after(50));
        assert!(invalid_input(&r), "{p:?}: {:?}", r.err());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

fn os_string(bytes: Vec<u8>) -> std::ffi::OsString {
    std::os::unix::ffi::OsStringExt::from_vec(bytes)
}

#[test]
fn the_longest_socket_path_that_fits_is_connectable() {
    // sun_path holds 108 bytes, the last for the NUL: a 107-byte path works.
    let base = std::env::temp_dir().join(format!("inacircle-long-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let head = base.join("s").into_os_string().into_encoded_bytes();
    let pad = 107usize
        .checked_sub(head.len())
        .expect("temp dir too deep for this test");
    let mut bytes = head;
    bytes.extend(std::iter::repeat_n(b'x', pad));
    let path = PathBuf::from(os_string(bytes));
    assert_eq!(path.as_os_str().len(), 107);
    let (reached_it, r) = reached(&path, Address::Path(path.clone()));
    assert!(reached_it, "{:?}", r.err());
    // It did not fail for its length (the peer never answers: a timeout).
    assert!(matches!(r, Err(Error::Timeout)), "{:?}", r.err());
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn abstract_names_may_contain_nul_bytes() {
    use std::os::linux::net::SocketAddrExt;
    let name = format!("inacircle-nul-{}\0tail", std::process::id()).into_bytes();
    let sa = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
    let listener = std::os::unix::net::UnixListener::bind_addr(&sa).unwrap();
    listener.set_nonblocking(true).unwrap();
    let r = Conn::connect(&Address::Abstract(name), after(50));
    assert!(listener.accept().is_ok(), "{:?}", r.err());
}

#[test]
fn connect_failures_are_the_connect_errors() {
    let dir = std::env::temp_dir().join(format!("inacircle-refused-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let kind = |p: PathBuf| match Conn::connect(&Address::Path(p), after(300)) {
        Err(Error::Io(e)) => e.kind(),
        other => panic!("{:?}", other.err()),
    };
    assert_eq!(kind(dir.join("missing")), std::io::ErrorKind::NotFound);
    // A socket file nobody listens on.
    let stale = dir.join("stale");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert_eq!(kind(stale), std::io::ErrorKind::ConnectionRefused);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_error_reply_to_another_call_does_not_end_this_call() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        let mut other = m.clone();
        other.serial = m.serial + 100;
        p.error(&other, "org.example.Error.Other", "not for you");
        p.reply(&m, vec![Value::Str("mine".into())]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    let r = c.call(call("Ping"), after(500)).unwrap();
    assert_eq!(r.body, vec![Value::Str("mine".into())]);
    drop(c);
    fake.join();
}

/// A signal `Q` whose encoded form is exactly `len` bytes: a serial, then
/// padding bytes to make up the size.
fn sized_signal(len: usize) -> (Message, usize) {
    // The fixed part is small; the byte array supplies the rest.
    for n in 0..len {
        let m = signal(
            "Q",
            vec![
                Value::U32(0),
                Value::Array("y".into(), vec![Value::Byte(1); n]),
            ],
        );
        let got = dbus::encode(&m).unwrap().len();
        if got == len {
            return (m, n);
        }
    }
    panic!("no signal of {len} bytes");
}

/// The body that `sized_signal`'s message has, with `i` in the first slot.
fn sized_body(i: u32, n: usize) -> Vec<Value> {
    vec![
        Value::U32(i),
        Value::Array("y".into(), vec![Value::Byte(1); n]),
    ]
}

/// A connection that was sent `bodies` as signals (as `Q` from /a) and then
/// answered one call, so that all of them wait in the queue.
fn conn_with_queued(bodies: Vec<Vec<Value>>) -> (Fake, Conn) {
    let fake = Fake::start(move |p| {
        p.handshake();
        let m = p.recv();
        for b in bodies {
            p.signal("/a", "a.B", "Q", b);
        }
        p.reply(&m, vec![]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    c.call(call("Ping"), after(3000)).unwrap();
    (fake, c)
}

fn first_u32(m: &Message) -> u32 {
    let Value::U32(i) = m.body[0] else {
        panic!("{m:?}")
    };
    i
}

#[test]
fn the_signal_queue_holds_exactly_its_byte_budget() {
    // 64 frames of 4096 bytes are exactly 256 KiB: all stay; a 65th pushes
    // out the oldest.
    let (_, n) = sized_signal(4096);
    for (count, first) in [(64u32, 0u32), (65, 1)] {
        let (fake, mut c) = conn_with_queued((0..count).map(|i| sized_body(i, n)).collect());
        assert_eq!(c.queued_bytes(), 64 * 4096, "{count} sent");
        for i in first..count {
            let s = c.recv(after(100)).unwrap().unwrap();
            assert_eq!(first_u32(&s), i);
        }
        assert!(c.recv(after(20)).unwrap().is_none());
        assert_eq!(c.queued_bytes(), 0);
        drop(c);
        fake.join();
    }
}

#[test]
fn queue_accounting_follows_the_frames_that_come_and_go() {
    // Small frames first, then one near the 64 KiB frame cap: room is judged
    // by the bytes held, not by their product with the new frame's size.
    let (_, n) = sized_signal(1024);
    let bodies = vec![
        sized_body(0, 0),
        sized_body(1, n),
        sized_body(2, 0),
        vec![Value::U32(3), Value::Str("x".repeat(60_000))],
    ];
    let lens: Vec<usize> = bodies
        .iter()
        .map(|b| dbus::encode(&signal("Q", b.clone())).unwrap().len())
        .collect();
    let (fake, mut c) = conn_with_queued(bodies);
    let mut left: usize = lens.iter().sum();
    assert_eq!(c.queued_bytes(), left);
    for (i, len) in lens.iter().enumerate() {
        let s = c.recv(after(100)).unwrap().unwrap();
        assert_eq!(first_u32(&s), i as u32);
        left -= len;
        assert_eq!(c.queued_bytes(), left, "after signal {i}");
    }
    drop(c);
    fake.join();
}

#[test]
fn recv_now_looks_at_a_bounded_number_of_frames() {
    let fake = Fake::start(|p| {
        p.handshake();
        let mut stale = call("x");
        stale.serial = 9999;
        // 100 frames that are not signals, then a signal.
        p.write(&batch(Peer::reply_msg(&stale, vec![]), 100));
        p.signal("/a", "a.B", "Last", vec![]);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    // Everything the peer wrote is in the socket: 100 stale frames and the
    // signal.
    let stale_len = batch(Peer::reply_msg(&call("x"), vec![]), 1).len();
    let last_len = batch(signal("Last", vec![]), 1).len();
    wait_for_bytes(c.as_raw_fd(), 100 * stale_len + last_len);
    // 64 frames are looked at per call, none of them a signal.
    assert!(c.recv_now().unwrap().is_none());
    assert!(readable(c.as_raw_fd()), "the rest is still in the socket");
    // The remaining 36 stale frames and the signal fit in the next call.
    let s = c.recv_now().unwrap().expect("the signal is next");
    assert_eq!(s.member.as_deref(), Some("Last"));
    drop(c);
    fake.join();
}

#[test]
fn reads_stop_at_the_end_of_the_frame() {
    // Two signals in one write: after `recv` returns the first, the whole
    // second one is still in the socket, none of it read ahead.
    let one_len = batch(signal("One", vec![]), 1).len();
    let two = batch(signal("Two", vec![]), 1);
    let two_len = two.len();
    let fake = Fake::start(move |p| {
        p.handshake();
        let mut both = batch(signal("One", vec![]), 1);
        both.extend(two);
        p.write(&both);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    wait_for_bytes(c.as_raw_fd(), one_len + two_len);
    let s = c.recv(after(500)).unwrap().unwrap();
    assert_eq!(s.member.as_deref(), Some("One"));
    let mut left = 0;
    loop {
        let mut buf = [0u8; 4096];
        // SAFETY: `buf` is valid for `buf.len()` writable bytes.
        let n = unsafe {
            libc::recv(
                c.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            break;
        }
        left += n as usize;
    }
    assert_eq!(left, two_len);
    drop(c);
    fake.join();
}

/// A peer-sent out-of-band byte makes poll report the socket readable while a
/// plain recv finds nothing (EAGAIN): not an error, the waits run to their
/// deadlines.
#[test]
fn a_pending_oob_byte_is_not_an_error() {
    let fake = Fake::start(|p| {
        p.handshake();
        // SAFETY: valid one-byte buffer.
        let n = unsafe { libc::send(p.stream.as_raw_fd(), b"x".as_ptr().cast(), 1, libc::MSG_OOB) };
        assert_eq!(n, 1);
        p.drain();
    });
    let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
    wait_readable(c.as_raw_fd());
    let t = Instant::now();
    assert!(c.recv(t + Duration::from_millis(60)).unwrap().is_none());
    assert!(t.elapsed() >= Duration::from_millis(60));
    // Without waiting: no frame yet.
    assert!(c.recv_now().unwrap().is_none());
    // A call that gets no reply times out, rather than failing.
    let r = c.call(call("Silent"), after(60));
    assert!(matches!(r, Err(Error::Timeout)), "{:?}", r.err());
    drop(c);
    fake.join();
}

/// A signal-ish `recvmsg`/`send` that keeps failing with EINTR, in a child
/// with a seccomp filter. `libc::send` is `sendto` on x86-64, so deny both.
const SEND: [libc::c_long; 2] = [libc::SYS_sendto, libc::SYS_sendmsg];

#[test]
fn persistent_eintr_on_send_ends_at_the_deadline() {
    assert_child_ok(|| {
        let fake = Fake::start(|p| {
            p.handshake();
            p.drain();
        });
        let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
        deny_syscalls(&SEND, libc::EINTR as u32);
        let t = Instant::now();
        let r = c.call(call("Ping"), t + Duration::from_millis(100));
        let ms = t.elapsed().as_millis();
        if !matches!(r, Err(Error::Timeout)) || !(100..300).contains(&ms) {
            return 2;
        }
        // The half-sent call broke the connection.
        if !matches!(c.call(call("Ping"), after(1000)), Err(Error::Closed)) {
            return 3;
        }
        0
    });
}

#[test]
fn persistent_eintr_on_recv_ends_at_the_deadline() {
    assert_child_ok(|| {
        let fake = Fake::start(|p| {
            p.handshake();
            p.signal("/a", "a.B", "One", vec![]);
            p.drain();
        });
        let mut c = Conn::connect(&fake.address(), after(300)).unwrap();
        wait_readable(c.as_raw_fd());
        deny_syscalls(&[libc::SYS_recvmsg], libc::EINTR as u32);
        // Readable, but every recvmsg is interrupted: no frame, no spin.
        let t = Instant::now();
        let r = c.recv_now();
        if !matches!(r, Ok(None)) || t.elapsed() > Duration::from_millis(100) {
            return 2;
        }
        let t = Instant::now();
        let r = c.recv(t + Duration::from_millis(100));
        let ms = t.elapsed().as_millis();
        if !matches!(r, Ok(None)) || !(100..300).contains(&ms) {
            return 3;
        }
        0
    });
}
