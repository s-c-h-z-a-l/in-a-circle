//! The Notifications proxy against a scripted peer.

mod common;

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use common::{Fake, Peer};
use dbus::notifications::{Event, Notice, Notifications};
use dbus::{Message, Value};

const IFACE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// The unique name the fake server owns `IFACE` under.
const OWNER: &str = ":1.7";
const STRANGER: &str = ":1.99";

fn after(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}

fn strings(v: &[&str]) -> Value {
    Value::Array(
        "s".into(),
        v.iter().map(|s| Value::Str((*s).into())).collect(),
    )
}

/// Handshake, `GetCapabilities` (which activates a service on a real bus),
/// `GetNameOwner` and the `AddMatch`.
fn serve_open(p: &mut Peer, caps: &[&str]) {
    p.handshake();
    let m = p.recv();
    assert_eq!(m.destination.as_deref(), Some(IFACE));
    assert_eq!(m.path.as_deref(), Some(PATH));
    assert_eq!(m.member.as_deref(), Some("GetCapabilities"));
    p.reply(&m, vec![strings(caps)]);
    let m = p.recv();
    assert_eq!(m.destination.as_deref(), Some("org.freedesktop.DBus"));
    assert_eq!(m.member.as_deref(), Some("GetNameOwner"));
    assert_eq!(m.body, vec![Value::Str(IFACE.into())]);
    p.reply(&m, vec![Value::Str(OWNER.into())]);
    let m = p.recv();
    assert_eq!(m.member.as_deref(), Some("AddMatch"));
    assert_eq!(
        m.body,
        vec![Value::Str(format!(
            "type='signal',sender='{OWNER}',interface='{IFACE}',path='{PATH}'"
        ))]
    );
    p.reply(&m, vec![]);
}

fn expect_notify(p: &mut Peer) -> Message {
    let m = p.recv();
    assert_eq!(m.member.as_deref(), Some("Notify"));
    m
}

fn action_invoked(p: &mut Peer, id: u32, key: &str) {
    p.signal_from(
        OWNER,
        PATH,
        IFACE,
        "ActionInvoked",
        vec![Value::U32(id), Value::Str(key.into())],
    );
}

fn closed(p: &mut Peer, id: u32, reason: u32) {
    p.signal_from(
        OWNER,
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
        p.signal_from(
            OWNER,
            PATH,
            IFACE,
            "ActivationToken",
            vec![Value::U32(7), Value::Str("t".into())],
        );
        p.signal_from(OWNER, PATH, IFACE, "SomethingNew", vec![]);
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

#[test]
fn spoofed_signals_are_ignored() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        let ours = || vec![Value::U32(7), Value::Str("undo".into())];
        // Right id and member, wrong sender (unicast ignores match rules).
        p.signal_from(STRANGER, PATH, IFACE, "ActionInvoked", ours());
        // The owner, but not on the Notifications path.
        p.signal_from(OWNER, "/x", IFACE, "ActionInvoked", ours());
        closed(p, 7, 3);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    assert_eq!(n.next_event(7, after(1000)).unwrap(), Event::Closed(3));
    drop(n);
    fake.join();
}

const NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";

#[test]
fn activation_then_owner() {
    // Like a D-Bus activated daemon: no owner until the first call to the
    // well-known name started it.
    let fake = Fake::start(|p| {
        p.handshake();
        let mut started = false;
        loop {
            let m = p.recv();
            match m.member.as_deref().unwrap() {
                "GetCapabilities" => {
                    started = true;
                    p.reply(&m, vec![strings(&["actions"])]);
                }
                "GetNameOwner" if !started => p.error(&m, NO_OWNER, "no owner"),
                "GetNameOwner" => p.reply(&m, vec![Value::Str(OWNER.into())]),
                "AddMatch" => {
                    p.reply(&m, vec![]);
                    break;
                }
                other => panic!("{other}"),
            }
        }
        p.drain();
    });
    let n = Notifications::open(&fake.address(), after(300)).unwrap();
    assert!(n.has("actions"));
    drop(n);
    fake.join();
}

#[test]
fn open_fails_without_a_name_owner() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("GetCapabilities"));
        p.reply(&m, vec![strings(&[])]);
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("GetNameOwner"));
        p.error(&m, NO_OWNER, "no owner");
        p.drain();
    });
    let r = Notifications::open(&fake.address(), after(300));
    assert!(matches!(r, Err(dbus::Error::Remote(..))), "{:?}", r.err());
    fake.join();
}

#[test]
fn owner_name_is_validated() {
    for bad in [":1.7',sender='x", "1.7", "", ":1.7 ", ":1,7", ":1.7\\"] {
        let fake = Fake::start(move |p| {
            p.handshake();
            let m = p.recv();
            p.reply(&m, vec![strings(&[])]);
            let m = p.recv();
            p.reply(&m, vec![Value::Str(bad.into())]);
            p.drain();
        });
        let r = Notifications::open(&fake.address(), after(300));
        assert!(
            matches!(r, Err(dbus::Error::Malformed("GetNameOwner reply"))),
            "{bad:?}: {:?}",
            r.err()
        );
        fake.join();
    }
}

#[test]
fn our_action_survives_a_flood_during_a_call() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        let m = expect_notify(p);
        action_invoked(p, 7, "cancel");
        // Well over the 256 KiB the connection queues.
        for _ in 0..3000 {
            p.signal_from(
                STRANGER,
                PATH,
                IFACE,
                "ActionInvoked",
                vec![Value::U32(99), Value::Str("x".into())],
            );
        }
        p.reply(&m, vec![Value::U32(7)]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    let notice = Notice {
        replaces: 0,
        summary: "s",
        body: "b",
        actions: &[],
        progress: Some(1),
    };
    assert_eq!(n.notify(&notice, after(2000)).unwrap(), 7);
    assert_eq!(
        n.next_event(7, after(1000)).unwrap(),
        Event::Action("cancel".into())
    );
    drop(n);
    fake.join();
}

#[test]
fn progress_is_clamped_to_100() {
    let fake = Fake::start(|p| {
        serve_open(p, &[]);
        let m = expect_notify(p);
        let Value::Dict(_, _, hints) = &m.body[6] else {
            panic!("{:?}", m.body[6]);
        };
        assert_eq!(hints[0].1, Value::Variant(Box::new(Value::I32(100))));
        p.reply(&m, vec![Value::U32(1)]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    let notice = Notice {
        replaces: 0,
        summary: "s",
        body: "b",
        actions: &[],
        progress: Some(250),
    };
    n.notify(&notice, after(500)).unwrap();
    drop(n);
    fake.join();
}

#[test]
fn poll_event_within_spends_one_budget() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let fake = Fake::start(move |p| {
        serve_open(p, &["actions"]);
        for _ in 0..10 {
            action_invoked(p, 7, "bogus");
        }
        tx.send(()).unwrap();
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let mut frames = 3;
    let mut got = 0;
    while let Some(e) = n.poll_event_within(7, &mut frames).unwrap() {
        assert_eq!(e, Event::Action("bogus".into()));
        got += 1;
    }
    // Three frames, three events: the budget is spent across calls.
    assert_eq!((got, frames), (3, 0));
    drop(n);
    fake.join();
}

#[test]
fn open_within_shares_one_deadline() {
    let fake = Fake::start(|p| {
        p.handshake();
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("GetCapabilities"));
        std::thread::sleep(Duration::from_millis(200));
        p.reply(&m, vec![strings(&["actions"])]);
        let m = p.recv();
        assert_eq!(m.member.as_deref(), Some("GetNameOwner"));
        std::thread::sleep(Duration::from_millis(200));
        p.reply(&m, vec![Value::Str(OWNER.into())]);
        // The match rule never gets an answer.
        p.drain();
    });
    let start = Instant::now();
    let r = Notifications::open_within(&fake.address(), after(500));
    let took = start.elapsed();
    assert!(matches!(r, Err(dbus::Error::Timeout)), "{:?}", r.err());
    // Each call alone is in time; together they are not.
    assert!(took < Duration::from_millis(650), "{took:?}");
    fake.join();
}

#[test]
fn notify_reply_id_0_is_malformed() {
    let fake = Fake::start(|p| {
        serve_open(p, &["actions"]);
        let m = expect_notify(p);
        p.reply(&m, vec![Value::U32(0)]);
        p.drain();
    });
    let mut n = Notifications::open(&fake.address(), after(300)).unwrap();
    let notice = Notice {
        replaces: 0,
        summary: "s",
        body: "",
        actions: &[],
        progress: None,
    };
    // The spec never hands out 0: it would read as "no notification yet".
    let r = n.notify(&notice, after(500));
    assert!(matches!(r, Err(dbus::Error::Malformed(_))), "{r:?}");
    drop(n);
    fake.join();
}
