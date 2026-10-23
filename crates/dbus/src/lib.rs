//! Minimal D-Bus client: wire format, messages and a session bus
//! connection with deadlines. `libc` only.

use std::fmt;
use std::io;

pub mod conn;
pub mod marshal;
pub mod message;

pub use conn::{Address, Conn, parse_address, session_address};
pub use marshal::{Reader, Value, Writer};
pub use message::{Kind, Message, decode, encode, frame_len};

#[derive(Debug)]
pub enum Error {
    Malformed(&'static str),
    TooLarge,
    Io(io::Error),
    Timeout,
    Auth,
    /// A D-Bus error reply: error name and message.
    Remote(String, String),
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed(why) => write!(f, "malformed D-Bus data: {why}"),
            Error::TooLarge => f.write_str("D-Bus message too large"),
            Error::Io(e) => write!(f, "D-Bus I/O error: {e}"),
            Error::Timeout => f.write_str("D-Bus timeout"),
            Error::Auth => f.write_str("D-Bus authentication failed"),
            Error::Remote(name, msg) => write!(f, "D-Bus error {name}: {msg}"),
            Error::Closed => f.write_str("D-Bus connection closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}
