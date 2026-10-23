//! `org.freedesktop.Notifications`: show, replace and close notifications,
//! and the signals about our own.

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use crate::marshal::Value;
use crate::message::{Kind, Message};
use crate::{Address, Conn, Error};

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
/// How long each call after the connection is up may take.
const CALL_WAIT: Duration = Duration::from_millis(500);
const APP_NAME: &str = "In.A.Circle";
/// Frames (queued or read, signals or not) one `poll_event` call looks at
/// before it hands control back (the fd stays readable, so the caller's
/// `poll` returns at once).
const POLL_FRAMES: usize = 256;

pub struct Notice<'a> {
    /// 0 for a new notification, else the id to replace.
    pub replaces: u32,
    pub summary: &'a str,
    pub body: &'a str,
    /// `(key, label)` buttons; sent only when the server has `actions`.
    pub actions: &'a [(&'a str, &'a str)],
    /// 0-100, sent as the `value` hint.
    pub progress: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A button was pressed: its key.
    Action(String),
    /// The notification closed: the reason code.
    Closed(u32),
    /// The deadline passed (`next_event` only).
    Timeout,
}

pub struct Notifications {
    conn: Conn,
    /// Unique name of the server; only its signals count.
    owner: String,
    caps: Vec<String>,
}

impl Notifications {
    /// Connects, authenticates and says `Hello` before `deadline`; then
    /// reads the capabilities, finds the server's unique name and subscribes
    /// to its signals, each call within `CALL_WAIT` of its start.
    pub fn open(addr: &Address, deadline: Instant) -> Result<Notifications, Error> {
        // Activation may take longer than the connect deadline: every call
        // after it has its own `CALL_WAIT`.
        Notifications::open_with(addr, deadline, || Instant::now() + CALL_WAIT)
    }

    /// `open` with one deadline for all of it, calls included: for a caller
    /// that cannot wait long (no time for activation).
    pub fn open_within(addr: &Address, deadline: Instant) -> Result<Notifications, Error> {
        Notifications::open_with(addr, deadline, || deadline)
    }

    /// Connects before `deadline`, then each call before `wait()`.
    fn open_with(
        addr: &Address,
        deadline: Instant,
        wait: impl Fn() -> Instant,
    ) -> Result<Notifications, Error> {
        let mut conn = Conn::connect(addr, deadline)?;
        // To the well-known name, so a D-Bus activated daemon gets started.
        let r = conn.call(call("GetCapabilities", vec![]), wait())?;
        let caps = match r.body.into_iter().next() {
            Some(Value::Array(_, items)) => items
                .into_iter()
                .filter_map(|v| match v {
                    Value::Str(s) => Some(s),
                    _ => None,
                })
                .collect(),
            _ => return Err(Error::Malformed("GetCapabilities reply")),
        };
        let q = Message::call(
            0,
            BUS,
            BUS_PATH,
            BUS,
            "GetNameOwner",
            vec![Value::Str(SERVICE.into())],
        );
        let owner = match conn.call(q, wait())?.body.into_iter().next() {
            Some(Value::Str(name)) if is_unique_name(&name) => name,
            _ => return Err(Error::Malformed("GetNameOwner reply")),
        };
        conn.add_match(
            &format!("type='signal',sender='{owner}',interface='{SERVICE}',path='{PATH}'"),
            wait(),
        )?;
        // Nothing can concern us before our first `Notify`, so no signal
        // that arrived before this point is lost.
        conn.keep_signals_from(&owner);
        Ok(Notifications { conn, owner, caps })
    }

    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    /// Shows or replaces a notification; returns its id.
    pub fn notify(&mut self, n: &Notice, deadline: Instant) -> Result<u32, Error> {
        let actions = if self.has("actions") { n.actions } else { &[] };
        let actions = actions
            .iter()
            .flat_map(|(k, l)| [Value::Str((*k).into()), Value::Str((*l).into())])
            .collect();
        let hints = n
            .progress
            .map(|p| {
                (
                    Value::Str("value".into()),
                    Value::Variant(Box::new(Value::I32(p.min(100).into()))),
                )
            })
            .into_iter()
            .collect();
        let body = vec![
            Value::Str(APP_NAME.into()),
            Value::U32(n.replaces),
            Value::Str(String::new()),
            Value::Str(n.summary.into()),
            Value::Str(n.body.into()),
            Value::Array("s".into(), actions),
            Value::Dict("s".into(), "v".into(), hints),
            Value::I32(-1),
        ];
        let r = self.conn.call(call("Notify", body), deadline)?;
        match r.body.first() {
            Some(Value::U32(id)) => Ok(*id),
            _ => Err(Error::Malformed("Notify reply")),
        }
    }

    pub fn close(&mut self, id: u32, deadline: Instant) -> Result<(), Error> {
        let m = call("CloseNotification", vec![Value::U32(id)]);
        self.conn.call(m, deadline).map(drop)
    }

    /// The next signal about `id`, or `Event::Timeout` at the deadline.
    /// Signals about other notifications are read and dropped as they come.
    pub fn next_event(&mut self, id: u32, deadline: Instant) -> Result<Event, Error> {
        loop {
            match self.conn.recv(deadline)? {
                None => return Ok(Event::Timeout),
                Some(m) => {
                    if let Some(e) = event_for(&m, id, &self.owner) {
                        return Ok(e);
                    }
                }
            }
        }
    }

    /// Like `next_event` but never waits, for callers that `poll` the fd:
    /// `None` when nothing for `id` is available right now.
    pub fn poll_event(&mut self, id: u32) -> Result<Option<Event>, Error> {
        self.poll_event_within(id, &mut { POLL_FRAMES })
    }

    /// `poll_event` that spends one unit of `frames` per frame it looks at
    /// and returns `None` when it runs out, so one budget can span calls.
    pub fn poll_event_within(
        &mut self,
        id: u32,
        frames: &mut usize,
    ) -> Result<Option<Event>, Error> {
        loop {
            match self.conn.recv_within(frames)? {
                None => return Ok(None),
                Some(m) => {
                    if let Some(e) = event_for(&m, id, &self.owner) {
                        return Ok(Some(e));
                    }
                }
            }
        }
    }

    /// For the caller's `poll`; check `pending` first.
    pub fn pending(&self) -> bool {
        self.conn.pending()
    }
}

impl AsRawFd for Notifications {
    fn as_raw_fd(&self) -> RawFd {
        self.conn.as_raw_fd()
    }
}

/// A bus-assigned name (`:1.42`): safe to put in a match rule.
fn is_unique_name(s: &str) -> bool {
    s.starts_with(':')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

fn call(member: &str, body: Vec<Value>) -> Message {
    Message::call(0, SERVICE, PATH, SERVICE, member, body)
}

/// The event a signal means for notification `id`; `None` for everything
/// else (other ids, members, interfaces, paths or senders: unicast signals
/// ignore match rules, so the sender is checked here).
fn event_for(m: &Message, id: u32, owner: &str) -> Option<Event> {
    if m.kind != Kind::Signal
        || m.interface.as_deref() != Some(SERVICE)
        || m.path.as_deref() != Some(PATH)
        || m.sender.as_deref() != Some(owner)
    {
        return None;
    }
    match (m.member.as_deref()?, &m.body[..]) {
        ("ActionInvoked", [Value::U32(i), Value::Str(key)]) if *i == id => {
            Some(Event::Action(key.clone()))
        }
        ("NotificationClosed", [Value::U32(i), Value::U32(reason)]) if *i == id => {
            Some(Event::Closed(*reason))
        }
        _ => None,
    }
}
