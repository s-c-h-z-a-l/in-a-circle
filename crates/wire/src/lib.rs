//! Minimal Wayland wire protocol client: Unix socket, message framing,
//! fd passing (SCM_RIGHTS), deadlines on every wait. No codegen: callers
//! use the opcodes in [`op`], which a test checks against the protocol XMLs.
//! `libc` only.
//!
//! Wire format: header = object id (u32), then (size << 16 | opcode) (u32);
//! args are 32-bit words; strings/arrays are length-prefixed and padded to 4;
//! fds travel out of band in ancillary data, in order.

use std::fmt;
use std::io;

pub mod conn;
pub mod event;
pub mod proto;

pub use conn::{Arg, Conn, DISPLAY};
pub use event::Event;
pub use proto::{Iface, event_fds, op};

/// Largest message the protocol allows, header included.
pub const MAX_MESSAGE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An event's arguments end before the one being read.
    Short,
    /// A string argument without its NUL terminator or not UTF-8.
    BadString,
    /// A message header with a size below 8, above [`MAX_MESSAGE`] or not a
    /// multiple of 4: the stream is out of sync.
    BadSize(usize),
    /// An event that carries an fd arrived without one.
    MissingFd,
    /// The connection hit an error earlier and is unusable.
    Broken(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Short => f.write_str("Wayland event arguments too short"),
            Error::BadString => f.write_str("malformed Wayland string"),
            Error::BadSize(n) => write!(f, "bad Wayland message size {n}"),
            Error::MissingFd => f.write_str("Wayland event without its fd"),
            Error::Broken(why) => write!(f, "Wayland connection broken: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}
