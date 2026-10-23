//! One received event and its argument readers. A reader that runs past the
//! end of the arguments returns an error; nothing here panics on bad input.

use crate::Error;
use crate::proto::Iface;

pub struct Event {
    pub obj: u32,
    pub iface: Iface,
    pub op: u16,
    args: Vec<u8>,
    pos: usize,
}

impl Event {
    pub(crate) fn new(obj: u32, iface: Iface, op: u16, args: Vec<u8>) -> Event {
        Event {
            obj,
            iface,
            op,
            args,
            pos: 0,
        }
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        let w = self.args.get(self.pos..self.pos + 4).ok_or(Error::Short)?;
        let v = u32::from_ne_bytes([w[0], w[1], w[2], w[3]]);
        self.pos += 4;
        Ok(v)
    }

    pub fn i32(&mut self) -> Result<i32, Error> {
        self.u32().map(|v| v as i32)
    }

    /// 24.8 signed fixed point.
    pub fn fixed(&mut self) -> Result<f64, Error> {
        self.i32().map(|v| v as f64 / 256.0)
    }

    /// An array (or the raw bytes of a string, NUL included).
    pub fn bytes(&mut self) -> Result<&[u8], Error> {
        let start = self.pos;
        let len = self.u32()? as usize;
        // The length word was read; the body must fit, padding included.
        let end = len
            .checked_next_multiple_of(4)
            .and_then(|l| (start + 4).checked_add(l));
        match end {
            Some(end) if end <= self.args.len() => {
                self.pos = end;
                Ok(&self.args[start + 4..start + 4 + len])
            }
            _ => {
                self.pos = start;
                Err(Error::Short)
            }
        }
    }

    /// A string, NUL-terminated UTF-8 on the wire. A null string reads as
    /// empty.
    pub fn string(&mut self) -> Result<String, Error> {
        let b = self.bytes()?;
        if b.is_empty() {
            return Ok(String::new());
        }
        let s = b.strip_suffix(&[0]).ok_or(Error::BadString)?;
        if s.contains(&0) {
            return Err(Error::BadString);
        }
        std::str::from_utf8(s)
            .map(str::to_owned)
            .map_err(|_| Error::BadString)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(words: &[u32], tail: &[u8]) -> Event {
        let mut args: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        args.extend(tail);
        Event::new(1, Iface::Display, 0, args)
    }

    #[test]
    fn short_event_args_are_errors_not_panics() {
        let mut e = ev(&[], &[]);
        assert_eq!(e.u32(), Err(Error::Short));
        assert_eq!(e.i32(), Err(Error::Short));
        assert_eq!(e.fixed(), Err(Error::Short));
        assert_eq!(e.bytes(), Err(Error::Short));
        assert_eq!(e.string(), Err(Error::Short));
        // A partial word.
        assert_eq!(ev(&[], &[1, 2, 3]).u32(), Err(Error::Short));
        assert_eq!(ev(&[], &[1, 2, 3]).fixed(), Err(Error::Short));
        // Array length past the end, unpadded tail, and a length that would
        // overflow the position.
        assert_eq!(ev(&[8, 0], &[]).bytes(), Err(Error::Short));
        assert_eq!(ev(&[3], &[1, 2, 3]).bytes(), Err(Error::Short));
        assert_eq!(ev(&[u32::MAX], &[]).bytes(), Err(Error::Short));
        assert_eq!(ev(&[u32::MAX - 2], &[0; 8]).string(), Err(Error::Short));
        // Strings: no NUL, NUL inside, not UTF-8.
        assert_eq!(ev(&[4], b"abcd").string(), Err(Error::BadString));
        assert_eq!(ev(&[4], b"a\0b\0").string(), Err(Error::BadString));
        assert_eq!(ev(&[3], b"\xff\xfe\0\0").string(), Err(Error::BadString));
        // A failed read leaves the position alone.
        let mut e = ev(&[7], &[1, 2]);
        assert_eq!(e.bytes(), Err(Error::Short));
        assert_eq!(e.u32(), Ok(7));
    }

    #[test]
    fn strings_and_arrays_read_in_order() {
        let mut e = ev(&[0, 6], b"hello\0\0\0");
        let mut e2 = ev(&[2, 0xffff_fe00], b"ab\0\0");
        assert_eq!(e.string().as_deref(), Ok(""), "null string");
        assert_eq!(e.string().as_deref(), Ok("hello"));
        assert_eq!(e.u32(), Err(Error::Short));
        assert_eq!(e2.u32(), Ok(2));
        assert_eq!(e2.fixed(), Ok(-2.0));
        assert_eq!(e2.bytes(), Err(Error::Short));
        assert_eq!(ev(&[2], b"ab\0\0").bytes(), Ok(&b"ab"[..]));
        assert_eq!(ev(&[(-5i32) as u32], &[]).i32(), Ok(-5));
    }
}
