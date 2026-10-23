//! A scripted D-Bus peer for tests: listens on a fresh unix socket, accepts
//! one client and runs a scenario against it on a thread. Shared with other
//! test crates through `#[path]`.
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dbus::{Address, Kind, Message, Value, decode, encode, frame_len};

/// How long the peer waits for the client before it gives up (a test that
/// hangs is worse than one that fails).
const PATIENCE: Duration = Duration::from_secs(5);

pub struct Fake {
    addr: Address,
    dir: Option<PathBuf>,
    thread: Option<JoinHandle<()>>,
}

fn unique() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "inacircle-dbus-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

impl Fake {
    /// Listens on a socket file in a fresh temporary directory.
    pub fn start(scenario: impl FnOnce(&mut Peer) + Send + 'static) -> Fake {
        let dir = std::env::temp_dir().join(unique());
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("bus");
        let listener = UnixListener::bind(&path).unwrap();
        Fake::spawn(listener, Address::Path(path), Some(dir), scenario)
    }

    /// Listens on a fresh abstract socket name.
    pub fn start_abstract(scenario: impl FnOnce(&mut Peer) + Send + 'static) -> Fake {
        let name = unique().into_bytes();
        let sa = SocketAddr::from_abstract_name(&name).unwrap();
        let listener = UnixListener::bind_addr(&sa).unwrap();
        Fake::spawn(listener, Address::Abstract(name), None, scenario)
    }

    fn spawn(
        listener: UnixListener,
        addr: Address,
        dir: Option<PathBuf>,
        scenario: impl FnOnce(&mut Peer) + Send + 'static,
    ) -> Fake {
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            let start = Instant::now();
            let stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < PATIENCE, "no client connected");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(PATIENCE)).unwrap();
            let mut peer = Peer {
                stream,
                buf: Vec::new(),
                serial: 0,
            };
            scenario(&mut peer);
        });
        Fake {
            addr,
            dir,
            thread: Some(thread),
        }
    }

    pub fn address(&self) -> Address {
        self.addr.clone()
    }

    /// Waits for the scenario to finish; fails the test if it panicked.
    pub fn join(mut self) {
        if let Err(e) = self.thread.take().unwrap().join() {
            std::panic::resume_unwind(e);
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The server side of one connection.
pub struct Peer {
    pub stream: UnixStream,
    buf: Vec<u8>,
    serial: u32,
}

impl Peer {
    /// Reads more bytes; false on EOF. A client that closes with unread data
    /// resets the connection: that is a hangup too.
    fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        let n = match self.stream.read(&mut chunk) {
            Err(e) if e.kind() == ErrorKind::ConnectionReset => 0,
            r => r.expect("peer read"),
        };
        self.buf.extend_from_slice(&chunk[..n]);
        n > 0
    }

    /// One line including its CRLF.
    pub fn read_line(&mut self) -> Vec<u8> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                return self.buf.drain(..i + 2).collect();
            }
            assert!(self.fill(), "EOF before a full line");
        }
    }

    pub fn write(&mut self, b: &[u8]) {
        self.stream.write_all(b).unwrap();
    }

    /// The client's auth exchange, answered with OK.
    pub fn auth(&mut self) {
        let line = self.read_line();
        assert!(line.starts_with(b"\0AUTH EXTERNAL "), "{line:?}");
        self.write(b"OK 0123456789abcdef0123456789abcdef\r\n");
        assert_eq!(self.read_line(), b"BEGIN\r\n");
    }

    /// Reads one message; `None` on EOF.
    pub fn try_recv(&mut self) -> Option<Message> {
        loop {
            if let Some(head) = self.buf.first_chunk::<16>() {
                let total = frame_len(head).unwrap();
                if self.buf.len() >= total {
                    let m = decode(&self.buf[..total]).unwrap();
                    self.buf.drain(..total);
                    return Some(m);
                }
            }
            if !self.fill() {
                return None;
            }
        }
    }

    pub fn recv(&mut self) -> Message {
        self.try_recv().expect("EOF before a message")
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    pub fn send(&mut self, mut m: Message) {
        if m.serial == 0 {
            m.serial = self.next_serial();
        }
        let b = encode(&m).unwrap();
        self.write(&b);
    }

    fn base(kind: Kind, body: Vec<Value>) -> Message {
        Message {
            kind,
            serial: 0,
            reply_serial: None,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            destination: None,
            sender: Some("org.freedesktop.DBus".into()),
            signature: String::new(),
            body,
        }
    }

    /// A method return for `call`.
    pub fn reply_msg(call: &Message, body: Vec<Value>) -> Message {
        Message {
            reply_serial: Some(call.serial),
            ..Peer::base(Kind::MethodReturn, body)
        }
    }

    pub fn reply(&mut self, call: &Message, body: Vec<Value>) {
        self.send(Peer::reply_msg(call, body));
    }

    pub fn error(&mut self, call: &Message, name: &str, text: &str) {
        self.send(Message {
            reply_serial: Some(call.serial),
            error_name: Some(name.into()),
            ..Peer::base(Kind::Error, vec![Value::Str(text.into())])
        });
    }

    pub fn signal(&mut self, path: &str, interface: &str, member: &str, body: Vec<Value>) {
        self.send(Message {
            path: Some(path.into()),
            interface: Some(interface.into()),
            member: Some(member.into()),
            ..Peer::base(Kind::Signal, body)
        });
    }

    /// Answers the client's `Hello` with `name`.
    pub fn hello(&mut self, name: &str) {
        let m = self.recv();
        assert_eq!(m.member.as_deref(), Some("Hello"));
        self.reply(&m, vec![Value::Str(name.into())]);
    }

    /// Auth and `Hello`.
    pub fn handshake(&mut self) {
        self.auth();
        self.hello(":1.42");
    }

    /// Writes `batch` over and over until the client hangs up.
    pub fn flood(&mut self, batch: &[u8]) {
        while self.stream.write_all(batch).is_ok() {}
    }

    /// Reads and discards until the client hangs up.
    pub fn drain(&mut self) {
        while self.fill() {
            self.buf.clear();
        }
    }

    /// Sends `bytes` with `fds` attached as `SCM_RIGHTS`.
    pub fn send_with_fds(&mut self, bytes: &[u8], fds: &[i32]) {
        let space = size_of_val(fds);
        // SAFETY: CMSG_SPACE and CMSG_LEN only compute sizes.
        let (ctrl_len, cmsg_len) =
            unsafe { (libc::CMSG_SPACE(space as u32), libc::CMSG_LEN(space as u32)) };
        let mut ctrl = vec![0u64; (ctrl_len as usize).div_ceil(8)];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        // SAFETY: a zeroed msghdr is valid; we point it at live buffers
        // below, and CMSG_FIRSTHDR/CMSG_DATA stay inside `ctrl`, which holds
        // CMSG_SPACE(space) bytes.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = ctrl.as_mut_ptr().cast();
            msg.msg_controllen = ctrl_len as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = cmsg_len as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast(), fds.len());
            let n = libc::sendmsg(self.stream.as_raw_fd(), &msg, 0);
            assert_eq!(n, bytes.len() as isize);
        }
    }
}
