//! Connection behaviour against a scripted peer (and a real `dbus-daemon`
//! when one is installed).

mod common;

use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
