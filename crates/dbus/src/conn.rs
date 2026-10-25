//! Session bus connection: address, EXTERNAL auth, `Hello`, calls and
//! signals. Every wait is bounded by a caller-given deadline; nothing blocks.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::Error;
use crate::marshal::Value;
use crate::message::{Kind, Message, decode, encode, frame_len};

const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
/// Longest auth reply line ("OK " + 32 hex digits + CRLF is 37 bytes).
const MAX_LINE: usize = 512;
/// Wire bytes of signals kept until `recv`; past this the oldest are
/// dropped, so a broadcast flood cannot grow memory. Kept as raw frames: a
/// decoded frame can take several times its wire size.
const MAX_QUEUED_BYTES: usize = 256 << 10;
/// Frames `recv_now` looks at per call before it hands control back.
const NOW_FRAMES: usize = 64;
/// Control buffer for `recvmsg`, in u64 words for cmsghdr alignment: room
/// for a few fds, so the ones a peer pushes on us can be received and closed.
const CTRL_WORDS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Path(PathBuf),
    Abstract(Vec<u8>),
}

/// The session bus: the first unix address in `DBUS_SESSION_BUS_ADDRESS`,
/// or `$XDG_RUNTIME_DIR/bus` when the variable is not set or empty.
pub fn session_address() -> Option<Address> {
    address_from(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
}

fn address_from(bus: Option<&OsStr>, runtime_dir: Option<&OsStr>) -> Option<Address> {
    match bus {
        Some(s) if !s.is_empty() => parse_address(s.to_str()?),
        _ => {
            let dir = Path::new(runtime_dir?);
            dir.is_absolute().then(|| Address::Path(dir.join("bus")))
        }
    }
}

/// The first `unix:path=` or `unix:abstract=` entry of a D-Bus address
/// list. Other transports, listen-only and malformed entries are skipped.
pub fn parse_address(s: &str) -> Option<Address> {
    s.split(';')
        .find_map(|entry| unix_entry(entry.strip_prefix("unix:")?))
}

/// The keys of one `unix:` entry: exactly one `path` or `abstract`, maybe a
/// `guid`. Any other key or a bad value rejects the whole entry.
fn unix_entry(keys: &str) -> Option<Address> {
    let mut addr = None;
    for kv in keys.split(',') {
        let (k, v) = kv.split_once('=')?;
        let v = unescape(v).filter(|v| !v.is_empty())?;
        let a = match k {
            "path" => Address::Path(OsString::from_vec(v).into()),
            "abstract" => Address::Abstract(v),
            "guid" => continue,
            _ => return None,
        };
        if addr.replace(a).is_some() {
            return None;
        }
    }
    addr
}

/// Undoes the `%XX` escaping of address values.
fn unescape(v: &str) -> Option<Vec<u8>> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let (hi, lo) = (hex(*b.get(i + 1)?)?, hex(*b.get(i + 2)?)?);
            // mutants: equivalent (`|` -> `^`): `hi << 4` has its low four
            // bits clear and `lo` is at most 15, so OR and XOR agree.
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

pub struct Conn {
    fd: OwnedFd,
    name: String,
    serial: u32,
    /// Bytes received but not yet consumed: at most one partial frame (or
    /// the rest of an auth line), because reads never go past the frame.
    buf: Vec<u8>,
    /// Signals that arrived while waiting for a reply, as raw frames.
    queue: VecDeque<Vec<u8>>,
    /// Sum of the frame lengths in `queue`.
    queued: usize,
    /// When set, only signals from this sender are queued during a call.
    keep_from: Option<String>,
    /// Set by an error that leaves the stream out of sync, and by a timed
    /// out call (its reply may still come, half a request may be on the
    /// wire).
    broken: bool,
}

impl Conn {
    /// Connects, authenticates and says `Hello`, all before `deadline`.
    pub fn connect(addr: &Address, deadline: Instant) -> Result<Conn, Error> {
        let fd = socket()?;
        connect_unix(&fd, addr)?;
        let mut c = Conn {
            fd,
            name: String::new(),
            serial: 0,
            buf: Vec::new(),
            queue: VecDeque::new(),
            queued: 0,
            keep_from: None,
            broken: false,
        };
        c.auth(deadline)?;
        let hello = Message::call(0, BUS, BUS_PATH, BUS, "Hello", vec![]);
        let reply = c.call(hello, deadline)?;
        match reply.body.into_iter().next() {
            Some(Value::Str(name)) => c.name = name,
            _ => return Err(Error::Malformed("Hello reply without a name")),
        }
        Ok(c)
    }

    pub fn unique_name(&self) -> &str {
        &self.name
    }

    /// Sends `m` with the next serial and waits for its reply. Signals that
    /// arrive meanwhile are queued for `recv`; other replies are dropped.
    pub fn call(&mut self, mut m: Message, deadline: Instant) -> Result<Message, Error> {
        if self.broken {
            return Err(Error::Closed);
        }
        self.serial = self.serial.wrapping_add(1).max(1);
        m.serial = self.serial;
        let bytes = encode(&m)?;
        let r = self.exchange(&bytes, m.serial, deadline);
        self.track(r)
    }

    fn exchange(&mut self, bytes: &[u8], serial: u32, deadline: Instant) -> Result<Message, Error> {
        self.write_all(bytes, deadline)?;
        loop {
            // Checked even when data keeps coming: a flooding peer must not
            // stretch the deadline.
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            let frame = self.read_frame(deadline)?;
            let r = decode(&frame)?;
            match r.kind {
                Kind::Signal => {
                    let keep = self
                        .keep_from
                        .as_deref()
                        .is_none_or(|from| r.sender.as_deref() == Some(from));
                    drop(r);
                    if keep {
                        self.enqueue(frame);
                    }
                }
                Kind::MethodReturn if r.reply_serial == Some(serial) => return Ok(r),
                Kind::Error if r.reply_serial == Some(serial) => {
                    let text = match r.body.into_iter().next() {
                        Some(Value::Str(s)) => s,
                        _ => String::new(),
                    };
                    return Err(Error::Remote(r.error_name.unwrap_or_default(), text));
                }
                _ => {} // stale replies, calls to us
            }
        }
    }

    /// From now on, signals that arrive during a call are queued only when
    /// `sender` sent them, so a flood from others cannot push out the ones
    /// we wait for. `recv` and `recv_now` still hand out whatever they read
    /// from the socket themselves; the caller checks those.
    pub fn keep_signals_from(&mut self, sender: &str) {
        self.keep_from = Some(sender.to_owned());
    }

    pub fn add_match(&mut self, rule: &str, deadline: Instant) -> Result<(), Error> {
        let m = Message::call(
            0,
            BUS,
            BUS_PATH,
            BUS,
            "AddMatch",
            vec![Value::Str(rule.into())],
        );
        self.call(m, deadline).map(drop)
    }

    fn enqueue(&mut self, frame: Vec<u8>) {
        while self.queued + frame.len() > MAX_QUEUED_BYTES {
            let Some(old) = self.queue.pop_front() else {
                break;
            };
            self.queued -= old.len();
        }
        self.queued += frame.len();
        self.queue.push_back(frame);
    }

    /// The next signal: queued ones first. `None` when the deadline passes.
    ///
    /// A deadline that has passed never reads the socket, so a caller that
    /// polls the fd must use `recv_now`, not `recv(Instant::now())`.
    pub fn recv(&mut self, deadline: Instant) -> Result<Option<Message>, Error> {
        if let Some(frame) = self.queue.pop_front() {
            self.queued -= frame.len();
            return decode(&frame).map(Some);
        }
        if self.broken {
            return Err(Error::Closed);
        }
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.read_frame(deadline).and_then(|f| decode(&f)) {
                Ok(m) if m.kind == Kind::Signal => return Ok(Some(m)),
                Ok(_) => {}
                Err(Error::Timeout) => return Ok(None),
                Err(e) => return self.track(Err(e)),
            }
        }
    }

    /// The next signal without waiting: a queued one, else whatever complete
    /// frame the socket holds. A partial frame stays buffered and `None` is
    /// returned; reads stop at the frame, so `POLLIN` clears once the
    /// socket's data is consumed. At most `NOW_FRAMES` frames are looked at
    /// per call (`None` then, with the fd still readable).
    pub fn recv_now(&mut self) -> Result<Option<Message>, Error> {
        self.recv_within(&mut { NOW_FRAMES })
    }

    /// `recv_now` that spends one unit of `frames` per frame it looks at
    /// (queued or read) and returns `None` when it runs out.
    pub(crate) fn recv_within(&mut self, frames: &mut usize) -> Result<Option<Message>, Error> {
        if *frames == 0 {
            return Ok(None);
        }
        if let Some(frame) = self.queue.pop_front() {
            *frames -= 1;
            self.queued -= frame.len();
            return decode(&frame).map(Some);
        }
        if self.broken {
            return Err(Error::Closed);
        }
        while *frames > 0 {
            *frames -= 1;
            match self
                .try_frame()
                .and_then(|f| f.map(|f| decode(&f)).transpose())
            {
                Ok(Some(m)) if m.kind == Kind::Signal => return Ok(Some(m)),
                Ok(Some(_)) => {}
                Ok(None) => return Ok(None),
                Err(e) => return self.track(Err(e)),
            }
        }
        Ok(None)
    }

    /// True when a received message waits in the queue, so `poll` on the
    /// fd would not report it.
    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Wire bytes held by queued signals (for tests).
    #[doc(hidden)]
    pub fn queued_bytes(&self) -> usize {
        self.queued
    }

    /// Marks the connection broken on any error but a remote one: after a
    /// timeout the stream may hold half a request or a late reply.
    fn track<T>(&mut self, r: Result<T, Error>) -> Result<T, Error> {
        if let Err(e) = &r
            && !matches!(e, Error::Remote(..))
        {
            self.broken = true;
        }
        r
    }

    fn auth(&mut self, deadline: Instant) -> Result<(), Error> {
        // The bus checks this against SO_PEERCRED, the effective uid.
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        let hex: String = uid
            .to_string()
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes(), deadline)?;
        if !self.read_line(deadline)?.starts_with(b"OK ") {
            return Err(Error::Auth);
        }
        self.write_all(b"BEGIN\r\n", deadline)
    }

    fn read_line(&mut self, deadline: Instant) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = self.buf[..i].to_vec();
                self.buf.drain(..i + 2);
                return Ok(line);
            }
            if self.buf.len() >= MAX_LINE {
                return Err(Error::Auth);
            }
            self.fill(MAX_LINE - self.buf.len(), deadline)?;
        }
    }

    /// Reads exactly one frame: the fixed header first, then no more than
    /// the length it announces.
    fn read_frame(&mut self, deadline: Instant) -> Result<Vec<u8>, Error> {
        let total = loop {
            let want = match self.buf.first_chunk::<16>() {
                Some(head) => frame_len(head)?,
                None => 16,
            };
            if self.buf.len() >= want {
                break want;
            }
            self.fill(want - self.buf.len(), deadline)?;
        };
        Ok(self.buf.drain(..total).collect())
    }

    /// Like `read_frame`, but never waits: `None` when the frame is not
    /// complete yet.
    fn try_frame(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            let want = match self.buf.first_chunk::<16>() {
                Some(head) => frame_len(head)?,
                None => 16,
            };
            if self.buf.len() >= want {
                return Ok(Some(self.buf.drain(..want).collect()));
            }
            let len = self.buf.len();
            self.buf.resize(want, 0);
            let r = recv_no_fds(self.fd.as_raw_fd(), &mut self.buf[len..]);
            self.buf.truncate(len + *r.as_ref().unwrap_or(&0));
            match r {
                Ok(0) => return Err(Error::Closed),
                Ok(_) => {}
                // mutants: equivalent (guard -> false): the socket is
                // SOCK_NONBLOCK, and a non-blocking recvmsg never sleeps, so
                // a signal cannot interrupt it. Kept as defence in depth.
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Appends up to `max` bytes once the socket is readable.
    fn fill(&mut self, max: usize, deadline: Instant) -> Result<(), Error> {
        loop {
            wait(self.fd.as_raw_fd(), libc::POLLIN, deadline)?;
            let len = self.buf.len();
            self.buf.resize(len + max, 0);
            let r = recv_no_fds(self.fd.as_raw_fd(), &mut self.buf[len..]);
            self.buf.truncate(len + *r.as_ref().unwrap_or(&0));
            match r {
                Ok(0) => return Err(Error::Closed),
                Ok(_) => return Ok(()),
                // mutants: equivalent (guard -> false): `wait` just reported the
                // fd readable and this Conn is its only reader, so recvmsg
                // returns data, EOF or a real error; neither WouldBlock nor
                // (SOCK_NONBLOCK, see try_frame) Interrupted can occur. Kept
                // as defence in depth. (Guard -> true is killed by the
                // reset tests in tests/conn.rs.)
                Err(Error::Io(e))
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn write_all(&mut self, mut b: &[u8], deadline: Instant) -> Result<(), Error> {
        while !b.is_empty() {
            // SAFETY: b is a live buffer of b.len() bytes. MSG_NOSIGNAL turns
            // a hung-up peer into EPIPE instead of SIGPIPE.
            let n = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    b.as_ptr().cast(),
                    b.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            // mutants: equivalent (`<` -> `<=`): send(2) of a non-empty
            // buffer on a stream socket returns at least 1 byte or -1,
            // never 0 (the loop only runs while `b` is not empty).
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    // mutants: equivalent (delete arm): non-blocking send never
                    // sleeps, so it is never interrupted by a signal (see
                    // try_frame). Kept as defence in depth.
                    io::ErrorKind::Interrupted => {}
                    io::ErrorKind::WouldBlock => {
                        wait(self.fd.as_raw_fd(), libc::POLLOUT, deadline)?
                    }
                    _ => return Err(e.into()),
                }
                continue;
            }
            b = &b[n as usize..];
        }
        Ok(())
    }
}

impl AsRawFd for Conn {
    /// For the caller's `poll`; check `pending` first.
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Waits until `fd` reports one of `events` (or an error or hangup, which
/// the following read or write reports), or the deadline passes. A passed
/// deadline wins even over a ready fd.
fn wait(fd: RawFd, events: libc::c_short, deadline: Instant) -> Result<(), Error> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Timeout);
        }
        let left = deadline - now;
        // Round up so the deadline has passed when poll times out.
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        let mut p = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: p is one valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        // mutants: equivalent (`>` -> `>=`): a timed-out poll (0) would return
        // Ok with the fd not ready; every caller then reads or writes, gets
        // EAGAIN, and waits again, where the deadline check at the top of
        // this loop gives the same `Timeout` (poll times out only once the
        // rounded-up deadline has passed).
        if r > 0 {
            return Ok(());
        }
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }
}

fn socket() -> Result<OwnedFd, Error> {
    // mutants: equivalent (both `|` -> `^`): the three flags are distinct
    // single bits, so OR and XOR give the same value.
    // SAFETY: socket(2) has no memory preconditions.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: a non-negative result is a new fd that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Connects without blocking. A unix stream connect completes or fails at
/// once (a full backlog is `EAGAIN`, reported as an error), so there is
/// nothing to wait for here.
fn connect_unix(fd: &OwnedFd, addr: &Address) -> Result<(), Error> {
    // SAFETY: an all-zero sockaddr_un is a valid value.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // A path needs a trailing NUL, an abstract name its leading one.
    let (lead, name) = match addr {
        Address::Path(p) => (0, p.as_os_str().as_bytes()),
        Address::Abstract(n) => (1, &n[..]),
    };
    let bad_path = lead == 0 && (name.is_empty() || name.contains(&0));
    if bad_path || 1 + name.len() > sa.sun_path.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
    }
    for (d, s) in sa.sun_path[lead..].iter_mut().zip(name) {
        *d = *s as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len();
    // SAFETY: sa is an initialized sockaddr_un and len does not exceed it.
    let r = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const sa).cast(),
            len as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// `recvmsg` that refuses ancillary data. We never negotiate fd passing, so
/// any fd a peer attaches is closed and the connection treated as broken.
fn recv_no_fds(fd: RawFd, buf: &mut [u8]) -> Result<usize, Error> {
    let mut ctrl = [0u64; CTRL_WORDS];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: an all-zero msghdr is valid (no name, no iov, no control).
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.as_mut_ptr().cast();
    msg.msg_controllen = size_of_val(&ctrl) as _;
    // SAFETY: msg points at iov and ctrl, both live and of the sizes given.
    // MSG_CMSG_CLOEXEC: a received fd never leaks into a child.
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if msg.msg_controllen > 0 || msg.msg_flags & libc::MSG_CTRUNC != 0 {
        close_passed_fds(&msg);
        return Err(Error::Malformed("unexpected fds"));
    }
    Ok(n as usize)
}

/// Closes every fd carried by `SCM_RIGHTS` messages in `msg`'s control data.
fn close_passed_fds(msg: &libc::msghdr) {
    // msg_controllen is usize on glibc but u32 on musl.
    #[allow(clippy::unnecessary_cast)]
    let end = msg.msg_control as usize + msg.msg_controllen as usize;
    // SAFETY: recvmsg filled msg's control buffer and set msg_controllen to
    // the bytes used; CMSG_FIRSTHDR/CMSG_NXTHDR stay within it, and the fd
    // count is clamped to the buffer end. Each fd is ours to close.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let stop = (c as usize + (*c).cmsg_len as usize).min(end);
                let n = stop.saturating_sub(data as usize) / size_of::<RawFd>();
                for i in 0..n {
                    libc::close(data.cast::<RawFd>().add(i).read_unaligned());
                }
            }
            c = libc::CMSG_NXTHDR(msg, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> Option<Address> {
        Some(Address::Path(p.into()))
    }

    #[test]
    fn address_is_the_first_unix_entry() {
        assert_eq!(
            parse_address("unix:path=/run/user/1000/bus"),
            path("/run/user/1000/bus")
        );
        assert_eq!(
            parse_address("unix:abstract=/tmp/dbus-XYZ,guid=0123abcd"),
            Some(Address::Abstract(b"/tmp/dbus-XYZ".to_vec()))
        );
        assert_eq!(
            parse_address("unix:guid=00,path=/a"),
            path("/a"),
            "keys in any order"
        );
        assert_eq!(
            parse_address("tcp:host=localhost,port=1;unix:path=/a%20b%2c;unix:path=/c"),
            path("/a b,")
        );
        // Listen-only and malformed entries are skipped.
        assert_eq!(parse_address("unix:tmpdir=/tmp;unix:path=/d"), path("/d"));
        assert_eq!(parse_address("unix:path=/a%zz;unix:path=/e"), path("/e"));
        assert_eq!(parse_address("unix:path=/a%2;unix:path=/e"), path("/e"));
        assert_eq!(parse_address("unix:path=/a%+f;unix:path=/e"), path("/e"));
        assert_eq!(parse_address("unix:path=;unix:path=/f"), path("/f"));
        // No usable transport.
        for s in [
            "",
            ";",
            "tcp:host=x,port=1",
            "unix:",
            "unix:tmpdir=/tmp",
            "nonsense",
        ] {
            assert_eq!(parse_address(s), None, "{s:?}");
        }
    }

    #[test]
    fn missing_session_variable_falls_back_to_runtime_dir() {
        let os = |s: &'static str| Some(OsStr::new(s));
        assert_eq!(
            address_from(None, os("/run/user/7")),
            path("/run/user/7/bus")
        );
        // The variable wins, even when it names no usable transport.
        assert_eq!(
            address_from(os("unix:path=/x"), os("/run/user/7")),
            path("/x")
        );
        assert_eq!(
            address_from(os("tcp:host=x,port=1"), os("/run/user/7")),
            None
        );
        // No variable and no (absolute) runtime dir: no bus.
        assert_eq!(address_from(None, None), None);
        assert_eq!(address_from(None, os("")), None);
        assert_eq!(address_from(None, os("relative")), None);
    }

    #[test]
    fn empty_session_variable_counts_as_unset() {
        let os = |s: &'static str| Some(OsStr::new(s));
        assert_eq!(
            address_from(os(""), os("/run/user/7")),
            path("/run/user/7/bus")
        );
        assert_eq!(address_from(os(""), None), None);
    }

    #[test]
    fn unix_entry_with_any_bad_part_is_skipped_whole() {
        for bad in [
            "unix:path=%zz,abstract=/x",
            "unix:abstract=/x,path=%zz",
            "unix:path=/a,abstract=/b",
            "unix:path=/a,path=/b",
            "unix:path=/a,tmpdir=/tmp",
            "unix:path=/a,noequals",
            "unix:path=/a,guid=%zz",
        ] {
            assert_eq!(
                parse_address(&format!("{bad};unix:path=/ok")),
                path("/ok"),
                "{bad}"
            );
        }
        assert_eq!(parse_address("unix:guid=00,path=/a"), path("/a"));
    }

    /// Control data whose header claims far more fds than the buffer holds:
    /// only the fds inside `msg_controllen` may be closed. Laid out in a
    /// larger array whose tail holds the numbers of fds that must survive.
    #[test]
    fn close_passed_fds_stays_inside_the_control_buffer() {
        // SAFETY (all unsafe below): CMSG_SPACE/CMSG_LEN only compute sizes;
        // the zeroed msghdr is valid; the pointers written into it and the
        // header written into `buf` stay inside `buf`, which is 8-aligned
        // (u64 words) and larger than anything touched.
        let guard = |n: i32| {
            let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
            assert!(fd >= 0);
            assert_eq!(unsafe { libc::dup2(fd, n) }, n);
            unsafe { libc::close(fd) };
        };
        let is_open = |n: i32| unsafe { libc::fcntl(n, libc::F_GETFD) } != -1;
        let (passed, g1, g2) = (700, 701, 702);
        guard(passed);
        guard(g1);
        guard(g2);
        let mut buf = [0u64; 8];
        let space = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize;
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            // Claims ten fds, though the buffer holds one.
            (*c).cmsg_len = libc::CMSG_LEN(10 * size_of::<RawFd>() as u32) as _;
            let data = libc::CMSG_DATA(c).cast::<RawFd>();
            data.write_unaligned(passed);
            // The rest of the padded slot (not a real fd), then, just past
            // `msg_controllen`, fds that are not ours to close.
            data.add(1).write_unaligned(-1);
            data.add(2).write_unaligned(g1);
            data.add(3).write_unaligned(g2);
        }
        close_passed_fds(&msg);
        assert!(!is_open(passed), "the passed fd is closed");
        assert!(is_open(g1) && is_open(g2), "fds past the buffer are not");
        unsafe {
            libc::close(g1);
            libc::close(g2);
        }
    }

    /// A `Conn` over one end of a socketpair; the other end is returned.
    fn pair() -> (Conn, std::os::unix::net::UnixStream) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let c = Conn {
            fd: OwnedFd::from(a),
            name: String::new(),
            serial: 0,
            buf: Vec::new(),
            queue: VecDeque::new(),
            queued: 0,
            keep_from: None,
            broken: false,
        };
        (c, b)
    }

    fn signal_frame(member: &str) -> Vec<u8> {
        let mut m = Message::call(1, "a.b", "/a", "a.B", member, vec![]);
        m.kind = Kind::Signal;
        encode(&m).unwrap()
    }

    #[test]
    fn recv_within_spends_one_unit_per_queued_frame() {
        let (mut c, _peer) = pair();
        for name in ["A", "B", "C"] {
            c.enqueue(signal_frame(name));
        }
        let mut frames = 2;
        let a = c.recv_within(&mut frames).unwrap().unwrap();
        assert_eq!(a.member.as_deref(), Some("A"));
        assert_eq!(frames, 1);
        assert_eq!(c.queued_bytes(), 2 * signal_frame("A").len());
        let b = c.recv_within(&mut frames).unwrap().unwrap();
        assert_eq!(b.member.as_deref(), Some("B"));
        assert_eq!(frames, 0);
        // Out of units: nothing more is handed out, and nothing is lost.
        assert!(c.recv_within(&mut frames).unwrap().is_none());
        assert!(c.pending());
        let mut more = 1;
        let third = c.recv_within(&mut more).unwrap().unwrap();
        assert_eq!(third.member.as_deref(), Some("C"));
    }

    #[test]
    fn an_auth_line_is_read_up_to_the_limit_and_no_further() {
        use std::io::Write;
        let (mut c, mut peer) = pair();
        // 100 bytes with no line end, then far more than the limit.
        let writer = std::thread::spawn(move || {
            peer.write_all(&[b'A'; 100]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            peer.write_all(&[b'B'; 2000]).unwrap();
            peer
        });
        let r = c.read_line(Instant::now() + std::time::Duration::from_secs(2));
        assert!(matches!(r, Err(Error::Auth)), "{:?}", r.err());
        assert_eq!(c.buf.len(), MAX_LINE, "read exactly up to the limit");
        // The rest of the 2100 bytes is still in the socket.
        let _peer = writer.join().unwrap();
        let mut left = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: buf is valid for buf.len() writable bytes.
            let n = unsafe {
                libc::recv(
                    c.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
            left.extend_from_slice(&buf[..n as usize]);
        }
        assert_eq!(left.len(), 2100 - MAX_LINE);
    }
}
