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
/// Signals kept until `recv`; past this the oldest are dropped, so a
/// broadcast flood cannot grow memory.
const MAX_QUEUED: usize = 64;
/// Control buffer for `recvmsg`, in u64 words for cmsghdr alignment: room
/// for a few fds, so the ones a peer pushes on us can be received and closed.
const CTRL_WORDS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Path(PathBuf),
    Abstract(Vec<u8>),
}

/// The session bus: the first unix address in `DBUS_SESSION_BUS_ADDRESS`,
/// or `$XDG_RUNTIME_DIR/bus` when the variable is not set.
pub fn session_address() -> Option<Address> {
    address_from(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
}

fn address_from(bus: Option<&OsStr>, runtime_dir: Option<&OsStr>) -> Option<Address> {
    match bus {
        Some(s) => parse_address(s.to_str()?),
        None => {
            let dir = Path::new(runtime_dir?);
            dir.is_absolute().then(|| Address::Path(dir.join("bus")))
        }
    }
}

/// The first `unix:path=` or `unix:abstract=` entry of a D-Bus address
/// list. Other transports and listen-only entries are skipped.
pub fn parse_address(s: &str) -> Option<Address> {
    s.split(';').find_map(|entry| {
        let rest = entry.strip_prefix("unix:")?;
        rest.split(',').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let v = unescape(v).filter(|v| !v.is_empty())?;
            match k {
                "path" => Some(Address::Path(OsString::from_vec(v).into())),
                "abstract" => Some(Address::Abstract(v)),
                _ => None,
            }
        })
    })
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
    /// Signals that arrived while waiting for a reply.
    queue: VecDeque<Message>,
    /// Set by an error that leaves the stream out of sync.
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
            let r = self.read_message(deadline)?;
            match r.kind {
                Kind::Signal => {
                    if self.queue.len() == MAX_QUEUED {
                        self.queue.pop_front();
                    }
                    self.queue.push_back(r);
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

    /// The next signal: queued ones first. `None` when the deadline passes.
    pub fn recv(&mut self, deadline: Instant) -> Result<Option<Message>, Error> {
        if let Some(m) = self.queue.pop_front() {
            return Ok(Some(m));
        }
        if self.broken {
            return Err(Error::Closed);
        }
        loop {
            match self.read_message(deadline) {
                Ok(m) if m.kind == Kind::Signal => return Ok(Some(m)),
                Ok(_) => {}
                Err(Error::Timeout) => return Ok(None),
                Err(e) => return self.track(Err(e)),
            }
        }
    }

    /// True when a received message waits in the queue, so `poll` on the
    /// fd would not report it.
    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Marks the connection broken on errors that desynchronize the stream.
    fn track<T>(&mut self, r: Result<T, Error>) -> Result<T, Error> {
        if let Err(e) = &r
            && !matches!(e, Error::Timeout | Error::Remote(..))
        {
            self.broken = true;
        }
        r
    }

    fn auth(&mut self, deadline: Instant) -> Result<(), Error> {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
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
    fn read_message(&mut self, deadline: Instant) -> Result<Message, Error> {
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
        let m = decode(&self.buf[..total]);
        self.buf.drain(..total);
        m
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
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
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
/// the following read or write reports), or the deadline passes.
fn wait(fd: RawFd, events: libc::c_short, deadline: Instant) -> Result<(), Error> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // Round up so the deadline has passed when poll times out.
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        let mut p = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: p is one valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r > 0 {
            return Ok(());
        }
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        } else if Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
    }
}

fn socket() -> Result<OwnedFd, Error> {
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
}
