//! A fake compositor for process-level wheel tests: it listens on a Unix
//! socket, speaks just enough Wayland for the real `inacircle` to map its
//! overlay, and plays a drag source that drops a `text/uri-list`.
//!
//! Requests are parsed generically by their header; the few whose
//! arguments matter (registry, binds, surfaces, receive) are decoded.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use wire::op;

pub const OUTPUT: (u32, u32) = (1920, 1080);
/// The first offer the fake source makes; `offer(n)` is the nth.
const OFFER: u32 = 0xff00_0000;
const URI_LIST: &str = "text/uri-list";
const DND_COPY: u32 = 1;

const GLOBALS: [(&str, u32); 7] = [
    ("wl_compositor", 4),
    ("wl_shm", 1),
    ("wl_seat", 7),
    ("wl_subcompositor", 1),
    ("wp_viewporter", 1),
    ("zwlr_layer_shell_v1", 4),
    ("wl_data_device_manager", 3),
];

/// One client request: object, opcode, argument bytes, and its fd if the
/// request carries one.
pub struct Req {
    pub obj: u32,
    pub op: u16,
    args: Vec<u8>,
    pub fd: Option<OwnedFd>,
}

impl Req {
    fn word(&self, i: usize) -> u32 {
        u32::from_ne_bytes(self.args[4 * i..4 * i + 4].try_into().unwrap())
    }

    /// The string starting at word `i`, and the word after it.
    fn string(&self, i: usize) -> (String, usize) {
        let len = self.word(i) as usize;
        let start = 4 * (i + 1);
        let s = String::from_utf8_lossy(&self.args[start..start + len - 1]).into_owned();
        (s, i + 1 + len.div_ceil(4))
    }
}

pub struct Compositor {
    sock: UnixStream,
    inb: Vec<u8>,
    fds: VecDeque<OwnedFd>,
    /// Bound globals by interface name.
    bound: HashMap<String, u32>,
    registry: u32,
    surfaces: Vec<u32>,
    layer: u32,
    data_device: u32,
    /// Every request to a data offer so far, in order: (offer, opcode).
    offer_log: Vec<(u32, u16)>,
    /// Commits-with-buffer of the wheel subsurface so far.
    attaches: usize,
    /// The wheel buffer on screen.
    shown: Option<u32>,
    /// Buffers replaced on screen, not yet released to the client.
    held: Vec<u32>,
}

impl Compositor {
    /// Listens on `<runtime>/wayland-0` and accepts the client `spawn`
    /// starts, within `limit`.
    pub fn serve(runtime: &Path, limit: Duration, spawn: impl FnOnce()) -> Compositor {
        let listener = UnixListener::bind(runtime.join("wayland-0")).unwrap();
        listener.set_nonblocking(true).unwrap();
        spawn();
        let until = Instant::now() + limit;
        let sock = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("no client: {e}"),
            }
        };
        sock.set_nonblocking(true).unwrap();
        Compositor {
            sock,
            inb: Vec::new(),
            fds: VecDeque::new(),
            bound: HashMap::new(),
            registry: 0,
            surfaces: Vec::new(),
            layer: 0,
            data_device: 0,
            offer_log: Vec::new(),
            attaches: 0,
            shown: None,
            held: Vec::new(),
        }
    }

    fn send(&mut self, obj: u32, op: u16, args: &[u8]) {
        let size = (8 + args.len()) as u32;
        let mut m = obj.to_ne_bytes().to_vec();
        m.extend(((size << 16) | u32::from(op)).to_ne_bytes());
        m.extend(args);
        let until = Instant::now() + Duration::from_secs(2);
        let mut rest = &m[..];
        while !rest.is_empty() {
            match self.sock.write(rest) {
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("send: {e}"),
            }
        }
    }

    /// Reads what the client sent; false once it closed the connection.
    fn fill(&mut self) -> bool {
        loop {
            let mut buf = [0u8; 8192];
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let mut cbuf = [0u64; 32];
            // SAFETY: a zeroed msghdr is valid; its pointers outlive recvmsg.
            let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
            mh.msg_iov = &mut iov;
            mh.msg_iovlen = 1;
            mh.msg_control = cbuf.as_mut_ptr().cast();
            mh.msg_controllen = std::mem::size_of_val(&cbuf);
            // SAFETY: mh describes valid buffers.
            let n = unsafe {
                libc::recvmsg(
                    self.sock.as_raw_fd(),
                    &mut mh,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if n == 0 {
                return false;
            }
            if n < 0 {
                return true;
            }
            self.inb.extend_from_slice(&buf[..n as usize]);
            // SAFETY: walking the cmsgs the kernel wrote; their fds are ours.
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&mh);
                while !c.is_null() {
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / 4;
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    for i in 0..count {
                        self.fds.push_back(OwnedFd::from_raw_fd(*data.add(i)));
                    }
                    c = libc::CMSG_NXTHDR(&mh, c);
                }
            }
        }
    }

    /// The next whole request, if one has arrived.
    fn next(&mut self) -> Option<Req> {
        if self.inb.len() < 8 {
            return None;
        }
        let word = |i: usize| u32::from_ne_bytes(self.inb[i..i + 4].try_into().unwrap());
        let (obj, head) = (word(0), word(4));
        let size = (head >> 16) as usize;
        if self.inb.len() < size {
            return None;
        }
        let op = (head & 0xffff) as u16;
        let args = self.inb[8..size].to_vec();
        self.inb.drain(..size);
        let carries_fd = (Some(&obj) == self.bound.get("wl_shm") && op == op::wl_shm::CREATE_POOL)
            || (is_offer(obj) && op == op::wl_data_offer::RECEIVE);
        let fd = carries_fd.then(|| self.fds.pop_front().expect("fd with the request"));
        Some(Req { obj, op, args, fd })
    }

    /// Handles requests until `want` returns something or `limit` passes.
    /// Setup requests are answered on the way.
    pub fn until<T>(
        &mut self,
        limit: Duration,
        mut want: impl FnMut(&Compositor, &mut Req) -> Option<T>,
    ) -> T {
        let until = Instant::now() + limit;
        loop {
            let open = self.fill();
            while let Some(mut r) = self.next() {
                self.note(&r);
                self.answer(&r);
                if let Some(t) = want(self, &mut r) {
                    return t;
                }
            }
            assert!(open, "the client hung up");
            assert!(Instant::now() < until, "timed out waiting for the client");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Plays the compositor's side of setup.
    fn answer(&mut self, r: &Req) {
        match (r.obj, r.op) {
            (1, op::wl_display::GET_REGISTRY) => self.registry = r.word(0),
            (1, op::wl_display::SYNC) => {
                let cb = r.word(0);
                for (i, (name, ver)) in GLOBALS.iter().enumerate() {
                    let mut a = (i as u32 + 1).to_ne_bytes().to_vec();
                    a.extend(wstr(name));
                    a.extend(ver.to_ne_bytes());
                    self.send(self.registry, op::wl_registry::GLOBAL, &a);
                }
                self.send(cb, op::wl_callback::DONE, &0u32.to_ne_bytes());
            }
            (obj, op::wl_registry::BIND) if obj == self.registry => {
                let (iface, next) = r.string(1);
                let id = r.word(next + 1);
                self.bound.insert(iface, id);
            }
            (obj, op::wl_compositor::CREATE_SURFACE)
                if Some(&obj) == self.bound.get("wl_compositor") =>
            {
                self.surfaces.push(r.word(0));
            }
            (obj, op::zwlr_layer_shell_v1::GET_LAYER_SURFACE)
                if Some(&obj) == self.bound.get("zwlr_layer_shell_v1") =>
            {
                self.layer = r.word(0);
                let (w, h) = OUTPUT;
                let a = words(&[1, w, h]);
                self.send(self.layer, op::zwlr_layer_surface_v1::CONFIGURE, &a);
            }
            (obj, op::wl_data_device_manager::GET_DATA_DEVICE)
                if Some(&obj) == self.bound.get("wl_data_device_manager") =>
            {
                self.data_device = r.word(0);
            }
            _ => {}
        }
    }

    /// Waits for the overlay to be mapped (its 1x1 buffer committed).
    pub fn mapped(&mut self, limit: Duration) {
        self.until(limit, |c, r| {
            (c.surfaces.first() == Some(&r.obj) && r.op == op::wl_surface::ATTACH).then_some(())
        });
        assert!(self.data_device != 0, "no data device");
    }

    /// A drag offering a URI list (the nth offer, `offer(n)`) enters at the
    /// output's center, copy negotiated; returns the pipe the client wants
    /// the list on (its early read), or `None` if the client did not ask
    /// within `limit`.
    pub fn drag_enters(&mut self, n: u32, limit: Duration) -> Option<OwnedFd> {
        let (dd, overlay, o) = (self.data_device, self.surfaces[0], offer(n));
        self.send(dd, op::wl_data_device::DATA_OFFER, &words(&[o]));
        self.send(o, op::wl_data_offer::OFFER, &wstr(URI_LIST));
        let (cx, cy) = (OUTPUT.0 / 2 * 256, OUTPUT.1 / 2 * 256);
        self.send(
            dd,
            op::wl_data_device::ENTER,
            &words(&[n + 1, overlay, cx, cy, o]),
        );
        self.send(o, op::wl_data_offer::ACTION, &words(&[DND_COPY]));
        self.until_or_timeout(limit, |_, r| {
            (r.obj == o && r.op == op::wl_data_offer::RECEIVE)
                .then(|| r.fd.take().expect("receive carries the pipe"))
        })
    }

    /// The next `receive` on offer `n`, e.g. the drop's own read.
    pub fn next_receive(&mut self, n: u32, limit: Duration) -> Option<OwnedFd> {
        let o = offer(n);
        self.until_or_timeout(limit, |_, r| {
            (r.obj == o && r.op == op::wl_data_offer::RECEIVE)
                .then(|| r.fd.take().expect("receive carries the pipe"))
        })
    }

    /// The drag moves to `(x, y)` (output coordinates).
    pub fn drag_motion(&mut self, x: u32, y: u32) {
        let dd = self.data_device;
        self.send(
            dd,
            op::wl_data_device::MOTION,
            &words(&[0, x * 256, y * 256]),
        );
    }

    /// The drag drops.
    pub fn drop_it(&mut self) {
        let dd = self.data_device;
        self.send(dd, op::wl_data_device::DROP, &[]);
    }

    /// Waits until the client has ended offer `n` (`destroy`), so its
    /// request log is complete.
    pub fn offer_destroyed(&mut self, n: u32, limit: Duration) {
        let o = offer(n);
        self.until(limit, |c, _| {
            c.offer_log
                .contains(&(o, op::wl_data_offer::DESTROY))
                .then_some(())
        });
    }

    /// The requests the client made to offer `n`, as opcodes, in order.
    pub fn offer_requests(&self, n: u32) -> Vec<u16> {
        let o = offer(n);
        self.offer_log
            .iter()
            .filter(|r| r.0 == o)
            .map(|r| r.1)
            .collect()
    }

    /// Wheel commits with a buffer so far.
    pub fn attaches(&self) -> usize {
        self.attaches
    }

    /// A frame passed: releases the buffers a newer one replaced.
    pub fn release_held(&mut self) {
        for b in std::mem::take(&mut self.held) {
            self.send(b, op::wl_buffer::RELEASE, &[]);
        }
    }

    /// The drag leaves without a drop.
    pub fn drag_leaves(&mut self) {
        let dd = self.data_device;
        self.send(dd, op::wl_data_device::LEAVE, &[]);
    }

    /// Handles the client's requests for `d`.
    pub fn pump(&mut self, d: Duration) {
        let _ = self.until_or_timeout(d, |_, _| None::<()>);
    }

    /// Waits until the wheel has had `n` buffers attached in all.
    pub fn attached(&mut self, n: usize, limit: Duration) {
        self.until(limit, |c, _| (c.attaches >= n).then_some(()));
    }

    /// Like `until`, but `None` at the limit instead of a panic.
    fn until_or_timeout<T>(
        &mut self,
        limit: Duration,
        mut want: impl FnMut(&Compositor, &mut Req) -> Option<T>,
    ) -> Option<T> {
        let until = Instant::now() + limit;
        loop {
            let open = self.fill();
            while let Some(mut r) = self.next() {
                self.note(&r);
                self.answer(&r);
                if let Some(t) = want(self, &mut r) {
                    return Some(t);
                }
            }
            assert!(open, "the client hung up");
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Logs a request for the tests.
    fn note(&mut self, r: &Req) {
        if is_offer(r.obj) {
            self.offer_log.push((r.obj, r.op));
        }
        if self.surfaces.get(1) == Some(&r.obj) && r.op == op::wl_surface::ATTACH {
            self.attaches += 1;
            // Like a compositor: the buffer shown before is released only
            // later, when the test says a frame passed (`release_held`).
            let buf = r.word(0);
            if buf != 0
                && let Some(prev) = self.shown.replace(buf)
            {
                self.held.push(prev);
            }
        }
    }

    /// The compositor closes the overlay.
    pub fn close(&mut self) {
        let layer = self.layer;
        self.send(layer, op::zwlr_layer_surface_v1::CLOSED, &[]);
    }
}

/// The nth offer's object id (server-created ids start at 0xff000000).
fn offer(n: u32) -> u32 {
    OFFER + n
}

fn is_offer(obj: u32) -> bool {
    (OFFER..OFFER + 0x100).contains(&obj)
}

/// Writes `data` into the drop pipe and closes it.
pub fn source_writes(w: OwnedFd, data: &[u8]) {
    let mut f = std::fs::File::from(w);
    f.write_all(data).unwrap();
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
