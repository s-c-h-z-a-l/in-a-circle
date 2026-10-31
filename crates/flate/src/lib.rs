//! Streaming inflate (RFC 1951), zlib (RFC 1950) with Adler-32, and CRC-32.
//!
//! Reads foreign binary input, so every length is checked against what the
//! stream actually produced: the output limit applies to the bytes decoded,
//! never to a size the stream declares, and nothing is allocated from a
//! declared size. Memory is fixed: a 32 KiB window and a 16 KiB read buffer.

mod crc;
mod inflate;
mod zlib;

pub use crc::Crc32;
pub use inflate::{InflateError, Inflated, inflate};
pub use zlib::{adler32, zlib_inflate};
