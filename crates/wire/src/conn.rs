//! The connection: socket, outgoing requests, incoming bytes and fds, event
//! framing. Every wait is bounded by a caller-given deadline.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::event::Event;
use crate::proto::{Iface, event_fds};
use crate::{Error, MAX_MESSAGE};

/// The `wl_display` object id.
pub const DISPLAY: u32 = 1;
/// Fds one `recvmsg` (and one `sendmsg`) carries at most, as libwayland's
/// `MAX_FDS_OUT`. More in one batch truncates the control data.
const FDS_PER_MSG: usize = 28;
/// Fds received but not yet taken by their events; past this the peer is
/// pushing fds that no event claims, and the connection is broken.
const MAX_QUEUED_FDS: usize = 64;
/// First id the server allocates.
const SERVER_IDS: u32 = 0xff00_0000;
/// Bytes read but not yet framed into events, at most.
const MAX_INB: usize = 2 * MAX_MESSAGE;

pub enum Arg<'a> {
    U(u32),
    I(i32),
    S(&'a str),
    /// Null string or null object: a zero word.
    Null,
    Fd(BorrowedFd<'a>),
}

pub struct Conn {
    sock: OwnedFd,
    out: Vec<u8>,
    out_fds: Vec<OwnedFd>,
    inb: Vec<u8>,
    in_fds: VecDeque<OwnedFd>,
    /// The fd of the last event returned, until `take_fd` or the next event.
    cur_fd: Option<OwnedFd>,
    next_id: u32,
    /// Interface of client-allocated ids, indexed by id.
    ids: Vec<Option<Iface>>,
    /// Server-allocated ids, e.g. data offers.
    server_ids: Vec<(u32, Iface)>,
    /// Set by an error that leaves the stream out of sync.
    broken: Option<&'static str>,
}

/// The header of one framed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    obj: u32,
    op: u16,
    /// Header and arguments, in bytes: 8..=`MAX_MESSAGE`, a multiple of 4.
    size: usize,
}

/// Reads the header of the first message in `buf`. `None`: the header or
/// the body is not all there yet. A size below 8, above [`MAX_MESSAGE`] or
/// not a multiple of 4 is `BadSize`: the stream is out of sync. Pure: the
/// connection state (known objects, fds) is checked by the caller.
fn frame(buf: &[u8]) -> Result<Option<Frame>, Error> {
    let Some(head) = buf.get(..8) else {
        return Ok(None);
    };
    let obj = u32::from_ne_bytes([head[0], head[1], head[2], head[3]]);
    let word = u32::from_ne_bytes([head[4], head[5], head[6], head[7]]);
    let (size, op) = ((word >> 16) as usize, word as u16);
    if !(8..=MAX_MESSAGE).contains(&size) || !size.is_multiple_of(4) {
        return Err(Error::BadSize(size));
    }
    if buf.len() < size {
        return Ok(None);
    }
    Ok(Some(Frame { obj, op, size }))
}

/// Fuzz entry point: the events a compositor could send, as bytes. Runs the
/// real `Conn::event` over every object kind (client id `n + 1` and server
/// id `SERVER_IDS + n` for the interface at index `n` of `Iface::ALL`) and
/// every argument reader on what comes out. One fd is queued, so the first
/// `wl_keyboard.keymap` finds its fd and a second one is refused as
/// `MissingFd`.
#[cfg(fuzzing)]
pub fn fuzz_events(bytes: &[u8]) {
    use crate::proto::Iface;
    // SAFETY: plain open(2) of /dev/null; the result is checked.
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return;
    }
    // SAFETY: fresh fd we own.
    let mut c = Conn::from_fd(unsafe { OwnedFd::from_raw_fd(fd) });
    for i in Iface::ALL.iter().skip(1) {
        c.new_id(*i);
    }
    for (n, i) in Iface::ALL.iter().enumerate() {
        c.register_server_id(SERVER_IDS + n as u32, *i);
    }
    // SAFETY: as above.
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd >= 0 {
        // SAFETY: fresh fd we own.
        c.in_fds.push_back(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    // `read` never grows `inb` past this, so neither may the fuzz input.
    c.inb.extend_from_slice(&bytes[..bytes.len().min(MAX_INB)]);
    while let Ok(Some(mut ev)) = c.event() {
        // Each reader on the whole argument block, then the readers as a
        // consumer chains them, until one fails.
        let all = ev.clone_args();
        for kind in 0..5u8 {
            let mut e = Event::new(ev.obj, ev.iface, ev.op, all.clone());
            while match kind {
                0 => e.u32().is_ok(),
                1 => e.i32().is_ok(),
                2 => e.fixed().is_ok(),
                3 => e.bytes().is_ok(),
                _ => e.string().is_ok(),
            } {}
        }
        let _ = (ev.u32(), ev.string(), ev.fixed(), ev.bytes());
    }
}

/// `$WAYLAND_DISPLAY` (default `wayland-0`): an absolute path, or a name
/// under `$XDG_RUNTIME_DIR`.
fn socket_path(display: Option<&OsStr>, runtime_dir: Option<&OsStr>) -> io::Result<PathBuf> {
    let name = Path::new(match display {
        Some(d) if !d.is_empty() => d,
        _ => OsStr::new("wayland-0"),
    });
    if name.is_absolute() {
        return Ok(name.into());
    }
    match runtime_dir.map(Path::new) {
        Some(dir) if dir.is_absolute() => Ok(dir.join(name)),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is not set or not absolute",
        )),
    }
}

/// Room for `FDS_PER_MSG` fds in a control buffer, in u64 words for
/// cmsghdr alignment.
const CTRL_WORDS: usize = 32;

fn ctrl_space(nfds: usize) -> usize {
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE((nfds * 4) as u32) } as usize;
    debug_assert!(space <= CTRL_WORDS * 8);
    space
}

fn broken_io(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, Error::Broken(why))
}

impl Conn {
    /// Connects to the compositor named by `WAYLAND_DISPLAY`.
    /// `WAYLAND_SOCKET` is not supported.
    pub fn connect() -> io::Result<Conn> {
        let path = socket_path(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        )?;
        Conn::connect_to(&path)
    }

    fn connect_to(path: &Path) -> io::Result<Conn> {
        // SAFETY: plain socket(2); result checked. Non-blocking: every wait
        // goes through `poll` with a deadline, connect included.
        // mutants: equivalent (`|` -> `^` in the flags) - the flag bits are
        // disjoint, so `^` and `|` give the same value.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh fd we own.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: a zeroed sockaddr_un is valid.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        let p = path.as_os_str().as_encoded_bytes();
        if p.len() >= addr.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Wayland socket path too long",
            ));
        }
        for (d, s) in addr.sun_path.iter_mut().zip(p) {
            *d = *s as _;
        }
        // SAFETY: addr is a valid sockaddr_un of the given size. A unix
        // socket connects at once or fails (EAGAIN: backlog full).
        let r = unsafe {
            libc::connect(
                sock.as_raw_fd(),
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_un>() as u32,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Conn::from_fd(sock))
    }

    /// A connection over `sock`, which must be a connected, non-blocking
    /// stream socket. For tests that play the compositor on the other end
    /// of a `socketpair`; real clients use [`Conn::connect`].
    #[doc(hidden)]
    pub fn from_fd(sock: OwnedFd) -> Conn {
        Conn {
            sock,
            out: Vec::with_capacity(MAX_MESSAGE),
            out_fds: Vec::new(),
            inb: Vec::with_capacity(MAX_MESSAGE),
            in_fds: VecDeque::new(),
            cur_fd: None,
            next_id: 2,
            ids: vec![None, Some(Iface::Display)],
            server_ids: Vec::new(),
            broken: None,
        }
    }

    pub fn new_id(&mut self, iface: Iface) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.ids.push(Some(iface));
        id
    }

    /// Records the interface of an id the server allocated (a `new_id`
    /// argument of an event). A reused id gets the new interface. An id
    /// outside the server's range breaks the connection: requests sent to
    /// it would reach one of our own objects.
    pub fn register_server_id(&mut self, id: u32, iface: Iface) {
        if id < SERVER_IDS {
            self.broken = Some("server new_id in the client range");
            return;
        }
        match self.server_ids.iter_mut().find(|(i, _)| *i == id) {
            Some(e) => e.1 = iface,
            None => self.server_ids.push((id, iface)),
        }
    }

    fn iface_of(&self, id: u32) -> Option<Iface> {
        if id >= SERVER_IDS {
            self.server_ids
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, f)| *f)
        } else {
            self.ids.get(id as usize).copied().flatten()
        }
    }

    /// Queues a request. A request over [`MAX_MESSAGE`] bytes, a string with
    /// a NUL in it, or an fd that cannot be duplicated breaks the
    /// connection: the next `flush` reports it. So do more than
    /// `FDS_PER_MSG` fds queued for one flush.
    pub fn req(&mut self, obj: u32, op: u16, args: &[Arg]) {
        if self.broken.is_some() {
            return;
        }
        let start = self.out.len();
        let fds_before = self.out_fds.len();
        self.out.extend([0u8; 8]);
        for a in args {
            match a {
                Arg::U(v) => self.out.extend(v.to_ne_bytes()),
                Arg::I(v) => self.out.extend(v.to_ne_bytes()),
                Arg::Null => self.out.extend(0u32.to_ne_bytes()),
                Arg::S(s) => {
                    if s.len() >= MAX_MESSAGE || s.as_bytes().contains(&0) {
                        return self.refuse(start, fds_before, "bad string argument");
                    }
                    let len = s.len() + 1;
                    self.out.extend((len as u32).to_ne_bytes());
                    self.out.extend(s.as_bytes());
                    self.out
                        .resize(self.out.len() + len.next_multiple_of(4) - s.len(), 0);
                }
                Arg::Fd(fd) => match fd.try_clone_to_owned() {
                    Ok(fd) => self.out_fds.push(fd),
                    Err(_) => return self.refuse(start, fds_before, "cannot duplicate an fd"),
                },
            }
        }
        let size = self.out.len() - start;
        if size > MAX_MESSAGE {
            return self.refuse(start, fds_before, "request too large");
        }
        if self.out_fds.len() > FDS_PER_MSG {
            return self.refuse(start, fds_before, "too many fds in one flush");
        }
        self.out[start..start + 4].copy_from_slice(&obj.to_ne_bytes());
        // mutants: equivalent (`|` -> `^`) - `size << 16` has zero low 16 bits
        // and `op` is a u16, so the operands are disjoint.
        let word = ((size as u32) << 16) | op as u32;
        self.out[start + 4..start + 8].copy_from_slice(&word.to_ne_bytes());
    }

    fn refuse(&mut self, start: usize, fds_before: usize, why: &'static str) {
        self.out.truncate(start);
        self.out_fds.truncate(fds_before);
        self.broken = Some(why);
    }

    fn check(&self) -> io::Result<()> {
        match self.broken {
            Some(why) => Err(broken_io(why)),
            None => Ok(()),
        }
    }

    /// Waits for `events` on the socket until `deadline`; false when it
    /// passed. Readiness includes hang-up and error, which the next
    /// send or receive reports.
    fn wait(&self, events: i16, deadline: Instant) -> io::Result<bool> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd: self.sock.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            // mutants: equivalent (`>` -> `>=`) - r == 0 is poll's timeout, which
            // is rounded up to whole ms, so the deadline has passed; reporting
            // ready then only makes the caller retry its non-blocking syscall,
            // get WouldBlock, and come back here to `left.is_zero()`: the same
            // result one iteration later.
            if r > 0 {
                return Ok(true);
            }
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }

    /// One `sendmsg` of the queued bytes; the fds go with the first one.
    fn send(&mut self) -> io::Result<usize> {
        let mut iov = libc::iovec {
            iov_base: self.out.as_mut_ptr().cast(),
            iov_len: self.out.len(),
        };
        let mut cbuf = [0u64; CTRL_WORDS];
        // SAFETY: a zeroed msghdr is valid; pointers set below outlive sendmsg.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        let nfds = self.out_fds.len();
        // mutants: equivalent (`>` -> `>=`) - with 0 fds this would attach a
        // SCM_RIGHTS cmsg of zero fds, which the kernel delivers as no ancillary
        // data at all: the peer sees the same bytes and no fds.
        if nfds > 0 {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = ctrl_space(nfds) as _;
            // SAFETY: the control buffer holds CMSG_SPACE(nfds * 4) bytes
            // (nfds <= FDS_PER_MSG, checked in `req`).
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN((nfds * 4) as u32) as _;
                let data = libc::CMSG_DATA(c) as *mut RawFd;
                for (i, f) in self.out_fds.iter().enumerate() {
                    *data.add(i) = f.as_raw_fd();
                }
            }
        }
        // SAFETY: msg describes valid buffers.
        let n = unsafe {
            libc::sendmsg(
                self.sock.as_raw_fd(),
                &msg,
                // mutants: equivalent (`|` -> `^`) - disjoint flag bits.
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        // mutants: equivalent (`<` -> `<=`) - `flush` calls `send` only with a
        // non-empty `out`, and sendmsg on a stream socket with iov_len > 0
        // returns >= 1 or -1, never 0.
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // Sent: the peer has its own copies of the fds; close ours.
        self.out_fds.clear();
        Ok(n as usize)
    }

    /// Sends the queued requests, waiting for socket space until `deadline`.
    /// A timeout breaks the connection: half a request may be on the wire.
    pub fn flush(&mut self, deadline: Instant) -> io::Result<()> {
        self.check()?;
        while !self.out.is_empty() {
            match self.send() {
                Ok(n) => drop(self.out.drain(..n)),
                // mutants: equivalent (guard -> false) - sendmsg with MSG_DONTWAIT
                // on a unix stream socket never sleeps, so it cannot be
                // interrupted by a signal (EINTR comes only from the sleeping
                // path); kept for portability. Not reachable by a test.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let ready = self.wait(libc::POLLOUT, deadline);
                    if !matches!(ready, Ok(true)) {
                        self.broken = Some("flush failed");
                        return Err(ready.err().unwrap_or_else(|| {
                            io::Error::new(io::ErrorKind::TimedOut, "Wayland flush timed out")
                        }));
                    }
                }
                Err(e) => {
                    self.broken = Some("write failed");
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Waits until `deadline` for data, then reads what is there.
    /// `Ok(false)` means the deadline passed with nothing to read.
    /// A deadline already passed returns `Ok(false)` without reading.
    /// End of stream, truncated fds and too many unclaimed fds are errors
    /// that break the connection.
    ///
    /// The required loop: call `event` until it returns `None`, then
    /// `read`. Unframed bytes are capped at two messages; a `read` with more
    /// than one message's worth still unframed breaks the connection.
    pub fn read(&mut self, deadline: Instant) -> io::Result<bool> {
        self.check()?;
        loop {
            if !self.wait(libc::POLLIN, deadline)? {
                return Ok(false);
            }
            match self.recv() {
                Ok(()) => return Ok(true),
                // mutants: equivalent (guard -> false) - `wait` returned ready, so
                // data, hang-up or an error is pending and the non-blocking
                // recvmsg neither finds the queue empty nor sleeps (no EINTR);
                // nobody else reads this socket (`&mut self`). Kept for
                // spurious wakeups; not reachable by a test.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn recv(&mut self) -> io::Result<()> {
        // Drained as documented, `inb` holds less than one message, so there
        // is always room for a full read.
        let room = MAX_INB.saturating_sub(self.inb.len()).min(MAX_MESSAGE);
        if room == 0 {
            self.broken = Some("events not drained before read");
            return Err(broken_io("events not drained before read"));
        }
        let mut buf = [0u8; MAX_MESSAGE];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: room,
        };
        let mut cbuf = [0u64; CTRL_WORDS];
        // SAFETY: a zeroed msghdr is valid; pointers set below outlive recvmsg.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        // Exactly FDS_PER_MSG fds fit: more sets MSG_CTRUNC.
        msg.msg_controllen = ctrl_space(FDS_PER_MSG) as _;
        // SAFETY: msg describes valid buffers.
        let n = unsafe {
            libc::recvmsg(
                self.sock.as_raw_fd(),
                &mut msg,
                // mutants: equivalent (`|` -> `^`) - disjoint flag bits.
                libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if !matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                self.broken = Some("read failed");
            }
            return Err(e);
        }
        // Take the fds first, even on an error below, so none leaks.
        // SAFETY: iterating the cmsgs the kernel wrote into cbuf; each
        // SCM_RIGHTS fd is a fresh descriptor we now own.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                    for i in 0..count {
                        self.in_fds
                            .push_back(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        let fail = if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            // The kernel dropped fds: every later fd would be misassigned.
            Some(io::Error::new(
                io::ErrorKind::InvalidData,
                "Wayland fd ancillary data truncated",
            ))
        } else if n == 0 {
            Some(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compositor closed the connection",
            ))
        } else if self.in_fds.len() > MAX_QUEUED_FDS {
            Some(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many unclaimed Wayland fds",
            ))
        } else {
            None
        };
        if let Some(e) = fail {
            self.broken = Some("read failed");
            self.in_fds.clear();
            return Err(e);
        }
        self.inb.extend_from_slice(&buf[..n as usize]);
        Ok(())
    }

    /// The next complete event from the bytes read so far, if any. An event
    /// that carries an fd takes it off the queue; it is available from
    /// `take_fd` until the next call, then closed. An event for an object
    /// this side never allocated or registered breaks the connection, so a
    /// `new_id` argument must be registered before the next call.
    ///
    /// The required loop: `event` until `None`, then `read`.
    pub fn event(&mut self) -> Result<Option<Event>, Error> {
        // First: a broken connection must not hand out the last event's fd.
        self.cur_fd = None;
        if let Some(why) = self.broken {
            return Err(Error::Broken(why));
        }
        let frame = match frame(&self.inb) {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(None),
            Err(e) => {
                self.broken = Some("bad message size");
                return Err(e);
            }
        };
        let Frame { obj, op, size } = frame;
        // An id we never allocated, or a server id never announced, has no
        // known fds or meaning: the peer and we disagree on the object map.
        let Some(iface) = self.iface_of(obj) else {
            self.broken = Some("event for an unknown object");
            return Err(Error::Broken("event for an unknown object"));
        };
        let nfds = event_fds(iface, op);
        if nfds > 0 {
            if self.in_fds.len() < nfds {
                self.broken = Some("event without its fd");
                return Err(Error::MissingFd);
            }
            // At most one fd per event in the protocols we speak.
            self.cur_fd = self.in_fds.pop_front();
            self.in_fds.drain(..nfds - 1);
        }
        let args = self.inb[8..size].to_vec();
        self.inb.drain(..size);
        Ok(Some(Event::new(obj, iface, op, args)))
    }

    /// The fd of the event `event` returned last, if it carries one.
    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        self.cur_fd.take()
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op;
    use Arg::*;
    use std::os::fd::AsFd;
    use std::time::Duration;

    fn after(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// A connection and the raw "server" end of its socket.
    fn pair() -> (Conn, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: valid 2-element out array; checked.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        // SAFETY: fresh descriptors we own.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        (Conn::from_fd(a), b)
    }

    fn memfd(content: &[u8]) -> OwnedFd {
        // SAFETY: static C string; checked.
        let fd = unsafe { libc::memfd_create(c"test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: valid buffer of the given length.
        let n = unsafe { libc::write(fd.as_raw_fd(), content.as_ptr().cast(), content.len()) };
        assert_eq!(n as usize, content.len());
        fd
    }

    fn content(fd: &OwnedFd) -> Vec<u8> {
        let mut buf = [0u8; 16];
        // SAFETY: valid buffer of the given length.
        let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        assert!(n >= 0);
        buf[..n as usize].to_vec()
    }

    /// Sends `bytes` with `fds` in one `sendmsg`, as a compositor would.
    fn send(server: &OwnedFd, bytes: &[u8], fds: &[RawFd]) {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        // SAFETY: CMSG_SPACE is a pure size computation.
        let space = unsafe { libc::CMSG_SPACE((fds.len() * 4) as u32) } as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        // SAFETY: zeroed msghdr is valid; pointers set below outlive sendmsg.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = space;
            // SAFETY: the control buffer holds CMSG_SPACE(fds.len() * 4) bytes.
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as usize;
                let data = libc::CMSG_DATA(c) as *mut RawFd;
                for (i, f) in fds.iter().enumerate() {
                    *data.add(i) = *f;
                }
            }
        }
        // SAFETY: msg describes valid buffers.
        let n = unsafe { libc::sendmsg(server.as_raw_fd(), &msg, 0) };
        assert_eq!(n as usize, bytes.len());
    }

    /// Everything the client sent so far, and the number of fds with it.
    fn received(server: &OwnedFd) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        let mut nfds = 0;
        loop {
            let mut buf = [0u8; 8192];
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let mut cbuf = [0u64; 64];
            // SAFETY: zeroed msghdr is valid; pointers set below outlive recvmsg.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&cbuf);
            // SAFETY: msg describes valid buffers.
            let n = unsafe {
                libc::recvmsg(
                    server.as_raw_fd(),
                    &mut msg,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if n <= 0 {
                return (out, nfds);
            }
            out.extend_from_slice(&buf[..n as usize]);
            // SAFETY: iterating the cmsgs the kernel wrote; the fds are ours.
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / 4;
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    for i in 0..count {
                        drop(OwnedFd::from_raw_fd(*data.add(i)));
                    }
                    nfds += count;
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
        }
    }

    /// One message: header plus words.
    fn msg(obj: u32, op: u16, args: &[u8]) -> Vec<u8> {
        let size = (8 + args.len()) as u32;
        let mut m = obj.to_ne_bytes().to_vec();
        m.extend(((size << 16) | op as u32).to_ne_bytes());
        m.extend(args);
        m
    }

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|w| w.to_ne_bytes()).collect()
    }

    /// A wire string: length with NUL, bytes, NUL, padding.
    fn wstr(s: &str) -> Vec<u8> {
        let mut v = ((s.len() + 1) as u32).to_ne_bytes().to_vec();
        v.extend(s.as_bytes());
        v.push(0);
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
        v
    }

    fn next(c: &mut Conn) -> Event {
        loop {
            if let Some(e) = c.event().unwrap() {
                return e;
            }
            assert!(c.read(after(1000)).unwrap(), "no event");
        }
    }

    #[test]
    fn socket_path_from_env() {
        let p = |d: Option<&str>, r: Option<&str>| {
            socket_path(d.map(OsStr::new), r.map(OsStr::new)).ok()
        };
        assert_eq!(
            p(Some("wayland-1"), Some("/run/user/1")),
            Some("/run/user/1/wayland-1".into())
        );
        assert_eq!(
            p(None, Some("/run/user/1")),
            Some("/run/user/1/wayland-0".into())
        );
        assert_eq!(p(Some(""), Some("/run")), Some("/run/wayland-0".into()));
        assert_eq!(p(Some("/tmp/w"), None), Some("/tmp/w".into()));
        assert_eq!(p(Some("wayland-1"), None), None);
        assert_eq!(p(Some("wayland-1"), Some("relative")), None);
    }

    #[test]
    fn connect_reaches_the_socket_and_refuses_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("wire-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("wayland-test");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut c = Conn::connect_to(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        let server = OwnedFd::from(server);
        c.req(DISPLAY, op::wl_display::SYNC, &[U(2)]);
        c.flush(after(1000)).unwrap();
        assert_eq!(received(&server).0, msg(1, 0, &words(&[2])));
        assert!(Conn::connect_to(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn request_framing_matches_reference() {
        let (mut c, server) = pair();
        assert_eq!(c.new_id(Iface::Registry), 2);
        assert_eq!(c.new_id(Iface::Compositor), 3);
        c.req(2, 0, &[U(5), S("wl_compositor"), U(4), U(3)]);
        c.req(7, 4, &[I(-1), Null, S("")]);
        let pool = memfd(b"x");
        c.req(8, 0, &[U(9), Fd(pool.as_fd()), I(4)]);
        c.req(9, 6, &[]);
        c.flush(after(1000)).unwrap();

        let mut want = Vec::new();
        // bind: name, "wl_compositor" (14 bytes with NUL, padded to 16),
        // version, new id.
        want.extend(words(&[2, (40 << 16)]));
        want.extend(words(&[5, 14]));
        want.extend(b"wl_compositor\0\0\0");
        want.extend(words(&[4, 3]));
        // -1, null, "" (length 1, NUL padded to 4).
        want.extend(words(&[7, (24 << 16) | 4, u32::MAX, 0, 1, 0]));
        // The fd travels out of band: no word in the body.
        want.extend(words(&[8, (16 << 16), 9, 4]));
        want.extend(words(&[9, (8 << 16) | 6]));
        assert_eq!(received(&server), (want, 1));
    }

    fn header(obj: u32, size: u32, op: u16) -> Vec<u8> {
        let mut h = obj.to_ne_bytes().to_vec();
        h.extend(((size << 16) | op as u32).to_ne_bytes());
        h
    }

    #[test]
    fn frame_reads_a_complete_header() {
        let mut m = header(7, 12, 3);
        m.extend([0; 4]);
        assert_eq!(
            frame(&m),
            Ok(Some(Frame {
                obj: 7,
                op: 3,
                size: 12
            }))
        );
        // Bytes of the next message do not matter.
        m.extend([9; 5]);
        assert_eq!(frame(&m).unwrap().unwrap().size, 12);
        let max = MAX_MESSAGE as u32;
        let mut m = header(1, max, 0);
        m.resize(MAX_MESSAGE, 0);
        assert_eq!(frame(&m).unwrap().unwrap().size, MAX_MESSAGE);
    }

    #[test]
    fn frame_waits_for_a_truncated_message() {
        assert_eq!(frame(&[]), Ok(None));
        assert_eq!(frame(&header(7, 12, 3)[..7]), Ok(None));
        // Header complete, body not.
        assert_eq!(frame(&header(7, 12, 3)), Ok(None));
    }

    #[test]
    fn frame_rejects_a_bad_size() {
        for size in [0u32, 4, 7, 9, 4097, 0xffff] {
            assert_eq!(
                frame(&header(7, size, 0)),
                Err(Error::BadSize(size as usize)),
                "{size}"
            );
        }
    }

    #[test]
    fn event_parsing_round_trip() {
        let (mut c, server) = pair();
        let registry = c.new_id(Iface::Registry);
        let pointer = c.new_id(Iface::Pointer);
        let keyboard = c.new_id(Iface::Keyboard);
        let device = c.new_id(Iface::DataDevice);

        let mut global = words(&[7]);
        global.extend(wstr("wl_seat"));
        global.extend(words(&[9]));
        send(
            &server,
            &msg(registry, op::wl_registry::GLOBAL, &global),
            &[],
        );
        let motion = words(&[1234, (12.5f64 * 256.0) as u32, (-320i32) as u32]);
        send(&server, &msg(pointer, op::wl_pointer::MOTION, &motion), &[]);
        // keyboard.enter: serial, surface, keys array [30, 31].
        let enter = words(&[5, 6, 8, 30, 31]);
        send(&server, &msg(keyboard, op::wl_keyboard::ENTER, &enter), &[]);
        // data_device.data_offer creates a server id; its offer event follows.
        let offer = msg(
            device,
            op::wl_data_device::DATA_OFFER,
            &words(&[SERVER_IDS]),
        );
        // Split mid-header: no event until the rest arrives.
        send(&server, &offer[..6], &[]);

        let mut e = next(&mut c);
        assert_eq!((e.obj, e.iface, e.op), (registry, Iface::Registry, 0));
        assert_eq!(e.u32(), Ok(7));
        assert_eq!(e.string().as_deref(), Ok("wl_seat"));
        assert_eq!(e.u32(), Ok(9));
        assert_eq!(e.u32(), Err(Error::Short));

        let mut e = next(&mut c);
        assert_eq!((e.iface, e.op), (Iface::Pointer, 2));
        assert_eq!(e.u32(), Ok(1234));
        assert_eq!(e.fixed(), Ok(12.5));
        assert_eq!(e.fixed(), Ok(-1.25));

        let mut e = next(&mut c);
        assert_eq!(e.iface, Iface::Keyboard);
        assert_eq!((e.u32(), e.u32()), (Ok(5), Ok(6)));
        assert_eq!(e.bytes(), Ok(&words(&[30, 31])[..]));

        assert!(c.event().unwrap().is_none());
        send(&server, &offer[6..], &[]);
        let mut e = next(&mut c);
        assert_eq!(e.iface, Iface::DataDevice);
        let id = e.u32().unwrap();
        c.register_server_id(id, Iface::DataOffer);
        let mime = wstr("text/uri-list");
        send(&server, &msg(id, op::wl_data_offer::OFFER, &mime), &[]);
        let mut e = next(&mut c);
        assert_eq!((e.obj, e.iface), (id, Iface::DataOffer));
        assert_eq!(e.string().as_deref(), Ok("text/uri-list"));

        // A reused server id takes its new type.
        c.register_server_id(id, Iface::PrimaryOffer);
        send(&server, &msg(id, 0, &[]), &[]);
        assert_eq!(next(&mut c).iface, Iface::PrimaryOffer);
    }

    /// An event for an id we never allocated, or a server id never
    /// announced, is a protocol violation: its fds and size cannot be
    /// trusted to frame the rest of the stream.
    #[test]
    fn event_for_an_unknown_object_breaks() {
        for obj in [0, 99, SERVER_IDS, SERVER_IDS + 7] {
            let (mut c, server) = pair();
            let known = c.new_id(Iface::Pointer);
            send(&server, &msg(known, op::wl_pointer::FRAME, &[]), &[]);
            send(&server, &msg(obj, 0, &[]), &[]);
            assert_eq!(next(&mut c).iface, Iface::Pointer);
            let err = loop {
                match c.event() {
                    Ok(Some(_)) => panic!("unknown object {obj:#x} framed"),
                    Ok(None) => assert!(c.read(after(1000)).unwrap()),
                    Err(e) => break e,
                }
            };
            assert!(matches!(err, Error::Broken(_)), "{obj:#x}: {err:?}");
            assert!(c.event().is_err(), "stays broken");
            assert!(c.flush(after(10)).is_err());
        }
    }

    /// A `new_id` from the server must be in the server's range: one in
    /// ours (an existing object, or 0) would have us send its requests to
    /// our own objects. It breaks the connection and changes nothing.
    #[test]
    fn a_server_id_in_the_client_range_breaks() {
        for id in [0, 1, 2, SERVER_IDS - 1] {
            let (mut c, server) = pair();
            let ours = c.new_id(Iface::Pointer);
            c.register_server_id(id, Iface::DataOffer);
            send(&server, &msg(ours, op::wl_pointer::FRAME, &[]), &[]);
            assert!(matches!(c.event(), Err(Error::Broken(_))), "{id:#x}");
            assert!(c.flush(after(10)).is_err(), "{id:#x}: stays broken");
            assert_eq!(c.iface_of(ours), Some(Iface::Pointer), "{id:#x}");
        }
    }

    /// The input buffer holds at most two messages' worth: a caller that
    /// reads without draining the events gets an error, not unbounded growth.
    #[test]
    fn input_buffer_is_capped() {
        let (mut c, server) = pair();
        let known = c.new_id(Iface::Pointer);
        let big = msg(known, op::wl_pointer::FRAME, &vec![0; MAX_MESSAGE - 8]);
        for _ in 0..3 {
            send(&server, &big, &[]);
        }
        let mut reads = 0;
        let err = loop {
            match c.read(after(1000)) {
                Ok(true) => reads += 1,
                Ok(false) => panic!("deadline"),
                Err(e) => break e,
            }
            assert!(c.inb.len() <= 2 * MAX_MESSAGE, "{}", c.inb.len());
            assert!(reads < 4, "never refused");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(c.inb.len() <= 2 * MAX_MESSAGE);
        // The documented loop never hits the cap.
        let (mut c, server) = pair();
        assert_eq!(c.new_id(Iface::Pointer), known);
        for _ in 0..8 {
            send(&server, &big, &[]);
        }
        let mut got = 0;
        while got < 8 {
            while c.event().unwrap().is_some() {
                got += 1;
            }
            if got < 8 {
                assert!(c.read(after(1000)).unwrap());
            }
        }
        assert_eq!(got, 8);
    }

    /// An fd left behind by the last event is not handed out once the
    /// connection broke.
    #[test]
    fn broken_connection_drops_the_last_fd() {
        let (mut c, server) = pair();
        let keyboard = c.new_id(Iface::Keyboard);
        let a = memfd(b"A");
        let keymap = msg(keyboard, op::wl_keyboard::KEYMAP, &words(&[1, 1]));
        send(&server, &keymap, &[a.as_raw_fd()]);
        assert_eq!(next(&mut c).op, op::wl_keyboard::KEYMAP);
        // Something breaks the connection before the fd is taken.
        c.req(5, 0, &[S("a\0b")]);
        assert!(c.event().is_err());
        assert!(c.take_fd().is_none());
    }

    #[test]
    fn fd_events_take_their_fd() {
        let (mut c, server) = pair();
        let keyboard = c.new_id(Iface::Keyboard);
        let (a, b) = (memfd(b"A"), memfd(b"B"));
        let keymap = msg(keyboard, op::wl_keyboard::KEYMAP, &words(&[1, 1]));
        send(&server, &keymap, &[a.as_raw_fd()]);
        send(&server, &keymap, &[b.as_raw_fd()]);
        let key = msg(keyboard, op::wl_keyboard::KEY, &words(&[1, 2, 3, 4]));
        send(&server, &key, &[]);

        // The first keymap's fd is ignored, yet the second gets its own.
        assert_eq!(next(&mut c).op, op::wl_keyboard::KEYMAP);
        assert_eq!(next(&mut c).op, op::wl_keyboard::KEYMAP);
        assert_eq!(content(&c.take_fd().unwrap()), b"B");
        assert_eq!(next(&mut c).op, op::wl_keyboard::KEY);
        assert!(c.take_fd().is_none());
        assert!(c.in_fds.is_empty());

        // An fd event without its fd is an error, not a later mismatch.
        send(&server, &keymap, &[]);
        assert!(c.read(after(1000)).unwrap());
        assert_eq!(c.event().err(), Some(Error::MissingFd));
    }

    #[test]
    fn ctrunc_is_an_error() {
        let (mut c, server) = pair();
        let fd = memfd(b"x");
        let fds = [fd.as_raw_fd(); FDS_PER_MSG + 1];
        send(&server, &msg(50, 0, &[]), &fds);
        assert!(c.read(after(1000)).is_err());
        assert!(c.read(after(10)).is_err());
        assert!(c.event().is_err());
    }

    #[test]
    fn oversized_message_is_refused() {
        // Incoming: a header claiming more than the protocol allows, or less
        // than a header.
        for size in [MAX_MESSAGE + 4, 4, 10] {
            let (mut c, server) = pair();
            let mut m = words(&[50, (size as u32) << 16]);
            m.resize(MAX_MESSAGE + 8, 0);
            send(&server, &m, &[]);
            assert!(c.read(after(1000)).unwrap());
            assert_eq!(c.event().err(), Some(Error::BadSize(size)));
            assert!(c.event().is_err(), "stays broken");
        }
        // Exactly the limit is fine.
        let (mut c, server) = pair();
        let known = c.new_id(Iface::Pointer);
        send(&server, &msg(known, 0, &vec![0; MAX_MESSAGE - 8]), &[]);
        assert_eq!(next(&mut c).obj, known);

        // Outgoing: nothing reaches the peer, and the connection reports it.
        let (mut c, server) = pair();
        c.req(DISPLAY, 0, &[U(2)]);
        c.req(5, 0, &[S(&"x".repeat(MAX_MESSAGE))]);
        assert!(c.flush(after(1000)).is_err());
        assert_eq!(received(&server).0, Vec::<u8>::new());
        // A NUL inside a string cannot be framed either.
        let (mut c, _server) = pair();
        c.req(5, 0, &[S("a\0b")]);
        assert!(c.flush(after(1000)).is_err());
    }

    #[test]
    fn read_honours_the_deadline() {
        let (mut c, _server) = pair();
        let t = Instant::now();
        assert!(!c.read(t + Duration::from_millis(100)).unwrap());
        let ms = t.elapsed().as_millis();
        assert!((100..=120).contains(&ms), "{ms} ms");
        // A passed deadline returns at once.
        let t = Instant::now();
        assert!(!c.read(t).unwrap());
        assert!(t.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn flush_honours_the_deadline() {
        // A peer that never reads: the socket fills, the flush gives up.
        let (mut c, _server) = pair();
        let t = Instant::now();
        let deadline = t + Duration::from_millis(100);
        let filler = "x".repeat(MAX_MESSAGE - 16);
        let r = loop {
            c.req(5, 0, &[S(&filler)]);
            if let Err(e) = c.flush(deadline) {
                break e;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "never blocked");
        };
        assert_eq!(r.kind(), io::ErrorKind::TimedOut);
        let ms = t.elapsed().as_millis();
        assert!((100..=120).contains(&ms), "{ms} ms");
        // Half a request may be on the wire: the connection is done.
        assert!(c.flush(after(10)).is_err());
    }

    #[test]
    fn closed_peer_is_an_error() {
        let (mut c, server) = pair();
        drop(server);
        assert_eq!(
            c.read(after(1000)).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn too_many_queued_fds_breaks_the_connection() {
        let (mut c, server) = pair();
        let fd = memfd(b"x");
        let fds = [fd.as_raw_fd(); FDS_PER_MSG];
        // fds no event claims: 3 x 28 > 64.
        for _ in 0..3 {
            send(&server, &msg(50, 0, &[]), &fds);
        }
        let mut reads = 0;
        let err = loop {
            match c.read(after(1000)) {
                Ok(true) => reads += 1,
                Ok(false) => panic!("deadline"),
                Err(e) => break e,
            }
            assert!(reads < 3, "never broke");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(c.in_fds.len() <= MAX_QUEUED_FDS);
        assert!(c.event().is_err());
        assert!(c.flush(after(10)).is_err());
    }

    /// Runs `f` in a forked child and returns its wait status. For checks
    /// that need a process-wide change (resource limits, signal
    /// dispositions, fd 0 closed) which must not touch the parallel tests.
    /// `f` returns the child's exit code (0: all held); a panic is code 99
    /// and an alarm bounds a hang.
    fn in_child(f: impl FnOnce() -> i32) -> libc::c_int {
        // SAFETY: fork(2); the child runs `f` and leaves with `_exit`, never
        // returning into the test harness. glibc keeps malloc usable in a
        // forked child.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: plain alarm(2): a hung child dies by SIGALRM.
            unsafe { libc::alarm(20) };
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

    fn lower_nofile(limit: libc::rlim_t) {
        let rl = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        // SAFETY: valid rlimit; only called in a forked child.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rl) }, 0);
    }

    fn fd_flags(fd: RawFd) -> (bool, bool) {
        // SAFETY: fcntl queries on a descriptor the caller holds.
        let (fdf, fl) = unsafe {
            (
                libc::fcntl(fd, libc::F_GETFD),
                libc::fcntl(fd, libc::F_GETFL),
            )
        };
        (fdf & libc::FD_CLOEXEC != 0, fl & libc::O_NONBLOCK != 0)
    }

    #[test]
    fn connect_makes_a_cloexec_nonblocking_socket() {
        let dir = std::env::temp_dir().join(format!("wire-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("w");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let c = Conn::connect_to(&path).unwrap();
        assert_eq!(fd_flags(c.as_raw_fd()), (true, true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `as_raw_fd` is the connection's socket: what a caller polls.
    #[test]
    fn as_raw_fd_is_the_stream_socket() {
        let (c, _server) = pair();
        let mut ty: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&ty) as libc::socklen_t;
        // SAFETY: valid out pointers of the stated size.
        let r = unsafe {
            libc::getsockopt(
                c.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut ty).cast(),
                &mut len,
            )
        };
        assert_eq!((r, ty), (0, libc::SOCK_STREAM));
    }

    /// A socket that cannot be created (descriptor limit) or lands on fd 0
    /// (stdin closed) is judged by the result, not the number.
    #[test]
    fn connect_handles_socket_failure_and_fd_zero() {
        let dir = std::env::temp_dir().join(format!("wire-fd0-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("w");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let st = in_child(|| {
            lower_nofile(0);
            i32::from(Conn::connect_to(&path).is_ok())
        });
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");
        let st = in_child(|| {
            // SAFETY: the lowest free descriptor becomes 0 for the socket.
            unsafe { libc::close(0) };
            match Conn::connect_to(&path) {
                Ok(c) => i32::from(c.as_raw_fd() != 0),
                Err(_) => 1,
            }
        });
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A failing `poll` is reported, not retried until the deadline.
    /// (`poll` with more fds than RLIMIT_NOFILE allows fails with EINVAL.)
    #[test]
    fn a_failing_poll_is_an_error() {
        let st = in_child(|| {
            let (mut c, _server) = pair();
            lower_nofile(0);
            let t = Instant::now();
            let r = c.read(after(5000));
            i32::from(!(r.is_err() && t.elapsed() < Duration::from_secs(2)))
        });
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");
    }

    /// Writing to a gone peer is an error, never SIGPIPE (the library does
    /// not rely on the process ignoring it), and reported at once.
    #[test]
    fn flush_to_a_closed_peer_fails_without_sigpipe() {
        let st = in_child(|| {
            // SAFETY: restore the default disposition, which kills on SIGPIPE.
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            let (mut c, server) = pair();
            drop(server);
            c.req(5, 0, &[U(1)]);
            let t = Instant::now();
            let r = c.flush(after(5000));
            let fast = t.elapsed() < Duration::from_secs(2);
            let kind = r.as_ref().err().map(io::Error::kind);
            i32::from(
                !(fast && kind == Some(io::ErrorKind::BrokenPipe) && c.flush(after(10)).is_err()),
            )
        });
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "{st:#x}");
    }

    /// A receive error other than "try again" breaks the connection.
    #[test]
    fn a_reset_connection_breaks() {
        let (mut c, server) = pair();
        c.req(5, 0, &[U(1)]);
        c.flush(after(1000)).unwrap();
        // The peer closes with our bytes unread: we get ECONNRESET.
        drop(server);
        let e = c.read(after(1000)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(matches!(c.event(), Err(Error::Broken(_))));
        assert!(c.flush(after(10)).is_err());
    }

    /// Up to `FDS_PER_MSG` fds cross in one message each way, intact.
    #[test]
    fn a_full_batch_of_fds_crosses_both_ways() {
        // Client to compositor: the cmsg length covers every fd.
        for n in [2, FDS_PER_MSG] {
            let (mut c, server) = pair();
            let fd = memfd(b"x");
            let args: Vec<Arg> = (0..n).map(|_| Fd(fd.as_fd())).collect();
            c.req(5, 0, &args);
            c.flush(after(1000)).unwrap();
            assert_eq!(received(&server), (msg(5, 0, &[]), n));
        }
        // Compositor to client: all 28 fit the control buffer, none is lost,
        // each is close-on-exec and goes to its own event.
        let (mut c, server) = pair();
        let keyboard = c.new_id(Iface::Keyboard);
        let fd = memfd(b"x");
        let keymap = msg(keyboard, op::wl_keyboard::KEYMAP, &words(&[1, 1]));
        send(
            &server,
            &keymap.repeat(FDS_PER_MSG),
            &[fd.as_raw_fd(); FDS_PER_MSG],
        );
        for _ in 0..FDS_PER_MSG {
            assert_eq!(next(&mut c).op, op::wl_keyboard::KEYMAP);
            let got = c.take_fd().expect("fd");
            assert!(fd_flags(got.as_raw_fd()).0, "close-on-exec");
            assert_eq!(content(&got), b"x");
        }
        assert!(c.in_fds.is_empty());
    }

    /// More fds in one flush than one message carries is refused whole.
    #[test]
    fn too_many_fds_in_one_request_is_refused() {
        let (mut c, server) = pair();
        let fd = memfd(b"x");
        let args: Vec<Arg> = (0..FDS_PER_MSG + 1).map(|_| Fd(fd.as_fd())).collect();
        c.req(5, 0, &args);
        assert!(c.flush(after(1000)).is_err());
        assert_eq!(received(&server), (Vec::new(), 0));
    }

    /// Exactly `MAX_QUEUED_FDS` unclaimed fds are tolerated; one more is not.
    #[test]
    fn the_queued_fd_limit_is_exact() {
        let (mut c, server) = pair();
        let fd = memfd(b"x");
        for n in [FDS_PER_MSG, FDS_PER_MSG, MAX_QUEUED_FDS - 2 * FDS_PER_MSG] {
            send(&server, &msg(50, 0, &[]), &vec![fd.as_raw_fd(); n]);
        }
        while c.in_fds.len() < MAX_QUEUED_FDS {
            assert!(c.read(after(1000)).unwrap());
        }
        assert_eq!(c.in_fds.len(), MAX_QUEUED_FDS);
        send(&server, &msg(50, 0, &[]), &[fd.as_raw_fd()]);
        let e = c.read(after(1000)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(c.event().is_err());
    }

    /// Only SCM_RIGHTS control data is fds: other ancillary data (here
    /// SCM_CREDENTIALS, which a receiver asks for with SO_PASSCRED) is not
    /// turned into descriptors.
    #[test]
    fn other_ancillary_data_is_not_fds() {
        let (mut c, server) = pair();
        let on: libc::c_int = 1;
        // SAFETY: valid option value of the stated size.
        let r = unsafe {
            libc::setsockopt(
                c.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&raw const on).cast(),
                std::mem::size_of_val(&on) as libc::socklen_t,
            )
        };
        assert_eq!(r, 0);
        send(&server, &msg(50, 0, &[]), &[]);
        assert!(c.read(after(1000)).unwrap());
        assert!(c.in_fds.is_empty());
    }

    /// The input buffer really holds two messages: a second full message
    /// arrives whole while the first is still unframed.
    #[test]
    fn input_buffer_holds_two_full_messages() {
        let (mut c, server) = pair();
        let known = c.new_id(Iface::Pointer);
        let big = msg(known, op::wl_pointer::FRAME, &vec![0; MAX_MESSAGE - 8]);
        send(&server, &big, &[]);
        send(&server, &big, &[]);
        assert!(c.read(after(1000)).unwrap());
        assert!(c.read(after(1000)).unwrap());
        assert_eq!(c.inb.len(), 2 * MAX_MESSAGE);
        assert!(c.event().unwrap().is_some());
        assert!(c.event().unwrap().is_some());
        assert!(c.event().unwrap().is_none());
    }
}
