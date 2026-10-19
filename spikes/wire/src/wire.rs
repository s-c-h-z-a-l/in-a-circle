//! Minimal Wayland wire protocol client: Unix socket, message framing,
//! fd passing (SCM_RIGHTS). No codegen: callers use opcodes directly.
//!
//! Wire format: header = object id (u32), then (size << 16 | opcode) (u32);
//! args are 32-bit words; strings/arrays are length-prefixed and padded to 4;
//! fds travel out of band in ancillary data, in order.

// Shared by two binaries; each uses a subset.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Iface {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    ShmPool,
    Buffer,
    Seat,
    Pointer,
    Keyboard,
    Subcompositor,
    Subsurface,
    DataDeviceManager,
    DataDevice,
    DataOffer,
    Viewporter,
    Viewport,
    Presentation,
    PresentationFeedback,
    PrimaryManager,
    PrimaryDevice,
    PrimaryOffer,
    LayerShell,
    LayerSurface,
}

pub enum Arg<'a> {
    U(u32),
    I(i32),
    S(&'a str),
    /// Null string or null object: a zero word.
    Null,
    Fd(BorrowedFd<'a>),
}

pub struct Event {
    pub obj: u32,
    pub iface: Option<Iface>,
    pub op: u16,
    args: Vec<u8>,
    pos: usize,
}

impl Event {
    pub fn u32(&mut self) -> u32 {
        let v = u32::from_ne_bytes(self.args[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }
    pub fn i32(&mut self) -> i32 {
        self.u32() as i32
    }
    /// 24.8 fixed point.
    pub fn fixed(&mut self) -> f64 {
        self.i32() as f64 / 256.0
    }
    pub fn bytes(&mut self) -> &[u8] {
        let len = self.u32() as usize;
        let start = self.pos;
        self.pos += len.div_ceil(4) * 4;
        &self.args[start..start + len]
    }
    pub fn string(&mut self) -> String {
        let b = self.bytes();
        let b = b.strip_suffix(&[0]).unwrap_or(b);
        String::from_utf8_lossy(b).into_owned()
    }
}

pub struct Wl {
    sock: OwnedFd,
    out: Vec<u8>,
    out_fds: Vec<OwnedFd>,
    inb: Vec<u8>,
    in_fds: VecDeque<OwnedFd>,
    next_id: u32,
    /// Interface of client-allocated ids, indexed by id.
    ids: Vec<Option<Iface>>,
    /// Server-allocated ids (>= 0xff00_0000), e.g. data offers.
    server_ids: Vec<(u32, Iface)>,
}

pub const DISPLAY: u32 = 1;

impl Wl {
    pub fn connect() -> io::Result<Wl> {
        let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
        let path = if name.starts_with('/') {
            name
        } else {
            format!(
                "{}/{name}",
                std::env::var("XDG_RUNTIME_DIR").map_err(io::Error::other)?
            )
        };
        // SAFETY: plain socket(2); result checked.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh fd we own.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: zeroed sockaddr_un is valid; path length checked below.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        let p = path.as_bytes();
        if p.len() >= addr.sun_path.len() {
            return Err(io::Error::other("socket path too long"));
        }
        for (d, s) in addr.sun_path.iter_mut().zip(p) {
            *d = *s as _;
        }
        // SAFETY: addr is a valid sockaddr_un of the given size.
        let r = unsafe {
            libc::connect(
                sock.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as u32,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Wl {
            sock,
            out: Vec::with_capacity(4096),
            out_fds: Vec::new(),
            inb: Vec::with_capacity(4096),
            in_fds: VecDeque::new(),
            next_id: 2,
            ids: vec![None, Some(Iface::Display)],
            server_ids: Vec::new(),
        })
    }

    pub fn new_id(&mut self, iface: Iface) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.ids.push(Some(iface));
        id
    }

    pub fn register_server_id(&mut self, id: u32, iface: Iface) {
        self.server_ids.push((id, iface));
    }

    fn iface_of(&self, id: u32) -> Option<Iface> {
        if id >= 0xff00_0000 {
            self.server_ids
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, f)| *f)
        } else {
            self.ids.get(id as usize).copied().flatten()
        }
    }

    pub fn req(&mut self, obj: u32, op: u16, args: &[Arg]) {
        let start = self.out.len();
        self.out.extend([0u8; 8]);
        for a in args {
            match a {
                Arg::U(v) => self.out.extend(v.to_ne_bytes()),
                Arg::Null => self.out.extend(0u32.to_ne_bytes()),
                Arg::I(v) => self.out.extend(v.to_ne_bytes()),
                Arg::S(s) => {
                    let len = s.len() + 1;
                    self.out.extend((len as u32).to_ne_bytes());
                    self.out.extend(s.as_bytes());
                    self.out.push(0);
                    self.out
                        .resize(self.out.len() + (len.div_ceil(4) * 4 - len), 0);
                }
                Arg::Fd(fd) => self.out_fds.push(fd.try_clone_to_owned().unwrap()),
            }
        }
        let size = (self.out.len() - start) as u32;
        self.out[start..start + 4].copy_from_slice(&obj.to_ne_bytes());
        self.out[start + 4..start + 8].copy_from_slice(&((size << 16) | op as u32).to_ne_bytes());
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let mut iov = libc::iovec {
            iov_base: self.out.as_mut_ptr() as *mut _,
            iov_len: self.out.len(),
        };
        let nfds = self.out_fds.len();
        let space = if nfds > 0 {
            // SAFETY: CMSG_SPACE is a pure size computation.
            unsafe { libc::CMSG_SPACE((nfds * 4) as u32) as usize }
        } else {
            0
        };
        let mut cbuf = vec![0u8; space];
        // SAFETY: zeroed msghdr is valid; pointers set below outlive sendmsg.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if nfds > 0 {
            msg.msg_control = cbuf.as_mut_ptr() as *mut _;
            msg.msg_controllen = space;
            // SAFETY: control buffer is CMSG_SPACE(nfds*4) bytes.
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN((nfds * 4) as u32) as usize;
                let data = libc::CMSG_DATA(c) as *mut RawFd;
                for (i, f) in self.out_fds.iter().enumerate() {
                    *data.add(i) = f.as_raw_fd();
                }
            }
        }
        // SAFETY: msg describes valid buffers.
        let n = unsafe { libc::sendmsg(self.sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(n as usize, self.out.len(), "short write");
        self.out.clear();
        self.out_fds.clear(); // closes our dups; the compositor has its own
        Ok(())
    }

    /// Block (up to `timeout_ms`, -1 = forever) for data, then read it.
    pub fn read(&mut self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.sock.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } <= 0 {
            return Ok(false);
        }
        let mut buf = [0u8; 4096];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: buf.len(),
        };
        let mut cbuf = [0u8; 256]; // room for 28+ fds
        // SAFETY: zeroed msghdr is valid; pointers set below outlive recvmsg.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cbuf.len();
        // SAFETY: msg describes valid buffers.
        let n = unsafe { libc::recvmsg(self.sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compositor closed",
            ));
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            // fds were dropped by the kernel; every later fd would be misassigned.
            return Err(io::Error::other("fd ancillary data truncated"));
        }
        self.inb.extend_from_slice(&buf[..n as usize]);
        // SAFETY: iterating cmsgs the kernel wrote into cbuf.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / 4;
                    for i in 0..count {
                        self.in_fds.push_back(OwnedFd::from_raw_fd(*data.add(i)));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        Ok(true)
    }

    /// Next complete event from the input buffer, if any.
    pub fn event(&mut self) -> Option<Event> {
        if self.inb.len() < 8 {
            return None;
        }
        let obj = u32::from_ne_bytes(self.inb[0..4].try_into().unwrap());
        let w = u32::from_ne_bytes(self.inb[4..8].try_into().unwrap());
        let size = (w >> 16) as usize;
        if self.inb.len() < size {
            return None;
        }
        let args = self.inb[8..size].to_vec();
        self.inb.drain(..size);
        Some(Event {
            obj,
            iface: self.iface_of(obj),
            op: (w & 0xffff) as u16,
            args,
            pos: 0,
        })
    }

    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        self.in_fds.pop_front()
    }
}
