//! The Notifications proxy against a scripted peer.

mod common;

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use common::{Fake, Peer};
use dbus::notifications::{Event, Notice, Notifications};
use dbus::{Message, Value};

const IFACE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

fn after(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}

fn strings(v: &[&str]) -> Value {
    Value::Array(
        "s".into(),
        v.iter().map(|s| Value::Str((*s).into())).collect(),
    )
}

/// Handshake, the two `AddMatch` calls and `GetCapabilities`.
fn serve_open(p: &mut Peer, caps: &[&str]) {
    p.handshake();
    for member in ["ActionInvoked", "NotificationClosed"] {
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("AddMatch"));
        assert_eq!(
            m.body,
            vec![Value::Str(format!(
                "type='signal',interface='{IFACE}',path='{PATH}',member='{member}'"
            ))]
        );
        p.reply(&m, vec![]);
    }
    let m = p.recv();
    assert_eq!(m.destination.as_deref(), Some(IFACE));
    assert_eq!(m.path.as_deref(), Some(PATH));
    assert_eq!(m.member.as_deref(), Some("GetCapabilities"));
    p.reply(&m, vec![strings(caps)]);
}

fn expect_notify(p: &mut Peer) -> Message {
    let m = p.recv();
    assert_eq!(m.member.as_deref(), Some("Notify"));
    m
}

fn action_invoked(p: &mut Peer, id: u32, key: &str) {
    p.signal(
        PATH,
        IFACE,
        "ActionInvoked",
        vec![Value::U32(id), Value::Str(key.into())],
    );
}

fn closed(p: &mut Peer, id: u32, reason: u32) {
    p.signal(
        PATH,
        IFACE,
        "NotificationClosed",
        vec![Value::U32(id), Value::U32(reason)],
    );
}

#[test]
fn notify_sends_the_expected_call() {
    let fake = Fake::start(|p| {
        serve_open(p, &["body", "actions"]);
        let m = expect_notify(p);
        assert_eq!(m.destination.as_deref(), Some(IFACE));
        assert_eq!(m.path.as_deref(), Some(PATH));
        assert_eq!(m.interface.as_deref(), Some(IFACE));
        assert_eq!(m.signature, "susssasa{sv}i");
        assert_eq!(m.body[0], Value::Str("In.A.Circle".into()));
        assert_eq!(m.body[1], Value::U32(5));
        assert_eq!(m.body[2], Value::Str(String::new()));
        assert_eq!(m.body[3], Value::Str("Markdown".into()));
        assert_eq!(m.body[4], Value::Str("1 / 3 files".into()));
        assert_eq!(m.body[5], strings(&["cancel", "Cancel"]));
        assert_eq!(
            m.body[6],
            Value::Dict(
                "s".into(),
                "v".into(),
                vec![(
                    Value::Str("value".into()),
                    Value::Variant(Box::new(Value::I32(33)))
                )]
            )
        );
        assert_eq!(m.body[7], Value::I32(-1));
        p.reply(&m, vec![Value::U32(7)]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    assert!(n.has("actions") && n.has("body") && !n.has("icon"));
    let id = n
        .notify(
            &Notice {
                replaces: 5,
                summary: "Markdown",
                body: "1 / 3 files",
                actions: &[("cancel", "Cancel")],
                progress: Some(33),
            },
            after(500),
        )
        .unwrap();
    assert_eq!(id, 7);
    drop(n);
    fake.join();
}

#[test]
fn no_actions_capability_sends_no_buttons() {
    let fake = Fake::start(|p| {
        serve_open(p, &["body"]);
        let m = expect_notify(p);
        assert_eq!(m.body[5], Value::Array("s".into(), vec![]));
        assert_eq!(m.body[6], Value::Dict("s".into(), "v".into(), vec![]));
        p.reply(&m, vec![Value::U32(1)]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    assert!(!n.has("actions"));
    n.notify(
        &Notice {
            replaces: 0,
            summary: "s",
            body: "b",
            actions: &[("undo", "Undo")],
            progress: None,
        },
        after(500),
    )
    .unwrap();
    drop(n);
    fake.join();
}

#[test]
fn close_calls_close_notification() {
    let fake = Fake::start(|p| {
        serve_open(p, &[]);
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("CloseNotification"));
        assert_eq!(m.body, vec![Value::U32(7)]);
        p.reply(&m, vec![]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    n.close(7, after(500)).unwrap();
    drop(n);
    fake.join();
}

#[test]
fn action_invoked_for_our_id() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        action_invoked(p, 7, "undo");
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    let e = n.next_event(7, after(1000)).unwrap();
    assert_eq!(e, Event::Action("undo".into()));
    drop(n);
    fake.join();
}

#[test]
fn foreign_action_is_ignored() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        action_invoked(p, 99, "undo");
        p.signal(
            PATH,
            IFACE,
            "ActivationToken",
            vec![Value::U32(7), Value::Str("t".into())],
        );
        p.signal(PATH, IFACE, "SomethingNew", vec![]);
        closed(p, 99, 1);
        closed(p, 7, 2);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    assert_eq!(n.next_event(7, after(1000)).unwrap(), Event::Closed(2));
    drop(n);
    fake.join();
}

#[test]
fn our_action_survives_a_foreign_flood() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        for _ in 0..500 {
            action_invoked(p, 99, "x");
        }
        action_invoked(p, 7, "undo");
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    assert_eq!(
        n.next_event(7, after(2000)).unwrap(),
        Event::Action("undo".into())
    );
    drop(n);
    fake.join();
}

#[test]
fn poll_event_ignores_foreign_ids() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let fake = Fake::start(move |p| {
        serve_open(p, &["actions"]);
        action_invoked(p, 99, "undo");
        closed(p, 98, 1);
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        action_invoked(p, 7, "cancel");
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    // Only foreign traffic so far: nothing for us, and the fd is drained.
    assert_eq!(n.poll_event(7).unwrap(), None);
    let mut p = libc::pollfd {
        fd: n.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: p is one valid pollfd.
    assert!(unsafe { libc::poll(&mut p, 1, 2000) } > 0);
    assert_eq!(
        n.poll_event(7).unwrap(),
        Some(Event::Action("cancel".into()))
    );
    assert_eq!(n.poll_event(7).unwrap(), None);
    drop(n);
    fake.join();
}

/// CPU time of the calling thread only: other tests run in parallel.
fn thread_cpu() -> Duration {
    // SAFETY: an all-zero rusage is valid; getrusage fills it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: ru is a valid out-pointer.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) }, 0);
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

#[test]
fn next_event_times_out_without_wakeups() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    let (cpu, t) = (thread_cpu(), Instant::now());
    let e = n.next_event(7, after(2000)).unwrap();
    let (waited, used) = (t.elapsed(), thread_cpu() - cpu);
    assert_eq!(e, Event::Timeout);
    let slack = Duration::from_millis(50);
    assert!(
        waited >= Duration::from_millis(2000) && waited <= Duration::from_millis(2000) + slack,
        "{waited:?}"
    );
    assert!(used < Duration::from_millis(5), "spun for {used:?}");
    drop(n);
    fake.join();
}
