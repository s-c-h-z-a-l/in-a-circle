//! D-Bus messages: fixed header, header fields and body.

use crate::Error;
use crate::marshal::{Reader, Value, Writer};

/// Largest message (header + body) we accept.
const MAX_MESSAGE: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: Kind,
    pub serial: u32,
    pub reply_serial: Option<u32>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    /// Derived from `body` by `encode`; filled in by `decode`.
    pub signature: String,
    pub body: Vec<Value>,
}

impl Message {
    /// A method call with no optional header fields besides the target.
    pub fn call(
        serial: u32,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: Vec<Value>,
    ) -> Message {
        Message {
            kind: Kind::MethodCall,
            serial,
            reply_serial: None,
            path: Some(path.into()),
            interface: Some(interface.into()),
            member: Some(member.into()),
            error_name: None,
            destination: Some(destination.into()),
            sender: None,
            signature: body.iter().map(Value::signature).collect(),
            body,
        }
    }
}

const PROTOCOL_VERSION: u8 = 1;

fn field(code: u8, v: Value) -> Value {
    Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(v))])
}

/// Serializes `m` in little-endian byte order.
pub fn encode(m: &Message) -> Vec<u8> {
    let mut body = Writer::new(true);
    let mut sig = String::new();
    for v in &m.body {
        body.value(v);
        sig.push_str(&v.signature());
    }
    let body = body.into_bytes();

    let mut fields = Vec::new();
    if let Some(p) = &m.path {
        fields.push(field(1, Value::Path(p.clone())));
    }
    if let Some(s) = &m.interface {
        fields.push(field(2, Value::Str(s.clone())));
    }
    if let Some(s) = &m.member {
        fields.push(field(3, Value::Str(s.clone())));
    }
    if let Some(s) = &m.error_name {
        fields.push(field(4, Value::Str(s.clone())));
    }
    if let Some(n) = m.reply_serial {
        fields.push(field(5, Value::U32(n)));
    }
    if let Some(s) = &m.destination {
        fields.push(field(6, Value::Str(s.clone())));
    }
    if let Some(s) = &m.sender {
        fields.push(field(7, Value::Str(s.clone())));
    }
    if !sig.is_empty() {
        fields.push(field(8, Value::Sig(sig)));
    }

    let kind = match m.kind {
        Kind::MethodCall => 1,
        Kind::MethodReturn => 2,
        Kind::Error => 3,
        Kind::Signal => 4,
    };
    let mut w = Writer::new(true);
    for v in [
        Value::Byte(b'l'),
        Value::Byte(kind),
        Value::Byte(0),
        Value::Byte(PROTOCOL_VERSION),
        Value::U32(body.len() as u32),
        Value::U32(m.serial),
        Value::Array("(yv)".into(), fields),
    ] {
        w.value(&v);
    }
    w.align(8);
    w.bytes(&body);
    w.into_bytes()
}

fn u32_at(head: &[u8; 16], at: usize, le: bool) -> u32 {
    let b: [u8; 4] = head[at..at + 4].try_into().unwrap();
    if le {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    }
}

fn endian(marker: u8) -> Result<bool, Error> {
    match marker {
        b'l' => Ok(true),
        b'B' => Ok(false),
        _ => Err(Error::Malformed("bad endianness marker")),
    }
}

/// Total message length (header, padding, body) from the fixed 16-byte
/// prefix. At most 1 MiB.
pub fn frame_len(head: &[u8; 16]) -> Result<usize, Error> {
    let le = endian(head[0])?;
    let body = u64::from(u32_at(head, 4, le));
    let fields = u64::from(u32_at(head, 12, le));
    let total = 16 + fields.next_multiple_of(8) + body;
    if total > MAX_MESSAGE {
        return Err(Error::TooLarge);
    }
    Ok(total as usize)
}

fn take_str(v: Value) -> Result<String, Error> {
    match v {
        Value::Str(s) | Value::Path(s) | Value::Sig(s) => Ok(s),
        _ => Err(Error::Malformed("header field has the wrong type")),
    }
}

fn take_u32(v: Value) -> Result<u32, Error> {
    match v {
        Value::U32(n) => Ok(n),
        _ => Err(Error::Malformed("header field has the wrong type")),
    }
}

/// Parses exactly one complete message.
pub fn decode(buf: &[u8]) -> Result<Message, Error> {
    let head: &[u8; 16] = buf
        .get(..16)
        .and_then(|h| h.try_into().ok())
        .ok_or(Error::Malformed("truncated"))?;
    let total = frame_len(head)?;
    if buf.len() != total {
        return Err(Error::Malformed("message length mismatch"));
    }
    let le = endian(buf[0])?;
    if buf[3] != PROTOCOL_VERSION {
        return Err(Error::Malformed("unsupported protocol version"));
    }
    let body_len = u32_at(head, 4, le) as usize;

    let mut r = Reader::new(&buf[..total - body_len], le);
    let mut hdr = r.read("yyyyuua(yv)")?.into_iter();
    let (Some(Value::Byte(_)), Some(Value::Byte(kind)), Some(Value::Byte(_)), Some(Value::Byte(_))) =
        (hdr.next(), hdr.next(), hdr.next(), hdr.next())
    else {
        unreachable!("signature fixes the types")
    };
    let _ = hdr.next();
    let Some(Value::U32(serial)) = hdr.next() else {
        unreachable!()
    };
    let Some(Value::Array(_, fields)) = hdr.next() else {
        unreachable!()
    };
    if serial == 0 {
        return Err(Error::Malformed("zero serial"));
    }

    let mut m = Message {
        kind: match kind {
            1 => Kind::MethodCall,
            2 => Kind::MethodReturn,
            3 => Kind::Error,
            4 => Kind::Signal,
            _ => return Err(Error::Malformed("unknown message type")),
        },
        serial,
        reply_serial: None,
        path: None,
        interface: None,
        member: None,
        error_name: None,
        destination: None,
        sender: None,
        signature: String::new(),
        body: Vec::new(),
    };
    for f in fields {
        let Value::Struct(mut parts) = f else {
            unreachable!()
        };
        let (Some(Value::Variant(v)), Some(Value::Byte(code))) = (parts.pop(), parts.pop()) else {
            unreachable!()
        };
        let v = *v;
        match code {
            1 if matches!(v, Value::Path(_)) => m.path = Some(take_str(v)?),
            1 => return Err(Error::Malformed("header field has the wrong type")),
            2 => m.interface = Some(take_str(v)?),
            3 => m.member = Some(take_str(v)?),
            4 => m.error_name = Some(take_str(v)?),
            5 => m.reply_serial = Some(take_u32(v)?),
            6 => m.destination = Some(take_str(v)?),
            7 => m.sender = Some(take_str(v)?),
            8 if matches!(v, Value::Sig(_)) => m.signature = take_str(v)?,
            8 => return Err(Error::Malformed("header field has the wrong type")),
            _ => {} // unknown or unused (unix fds): ignored
        }
    }

    let ok = match m.kind {
        Kind::MethodCall => m.path.is_some() && m.member.is_some(),
        Kind::MethodReturn => m.reply_serial.is_some(),
        Kind::Error => m.error_name.is_some() && m.reply_serial.is_some(),
        Kind::Signal => m.path.is_some() && m.interface.is_some() && m.member.is_some(),
    };
    if !ok {
        return Err(Error::Malformed("required header field missing"));
    }

    let mut br = Reader::new(&buf[total - body_len..], le);
    m.body = br.read(&m.signature)?;
    if !br.finished() {
        return Err(Error::Malformed("trailing bytes in body"));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Message {
        Message::call(
            1,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            vec![],
        )
    }

    /// Reference bytes assembled by hand from the D-Bus specification.
    #[rustfmt::skip]
    fn hello_bytes() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend([b'l', 1, 0, 1]);               // little endian, method call, no flags, v1
        b.extend([0, 0, 0, 0]);                  // body length 0
        b.extend([1, 0, 0, 0]);                  // serial 1
        b.extend([109, 0, 0, 0]);                // header fields array: 109 bytes
        // PATH (offset 16): code 1, sig "o", u32 21, string, NUL; pad to 8
        b.extend([1, 1, b'o', 0, 21, 0, 0, 0]);
        b.extend(b"/org/freedesktop/DBus\0");
        b.extend([0, 0]);
        // INTERFACE (offset 48): code 2, sig "s", u32 20, string, NUL; pad to 8
        b.extend([2, 1, b's', 0, 20, 0, 0, 0]);
        b.extend(b"org.freedesktop.DBus\0");
        b.extend([0, 0, 0]);
        // MEMBER (offset 80): code 3, sig "s", u32 5, string, NUL; pad to 8
        b.extend([3, 1, b's', 0, 5, 0, 0, 0]);
        b.extend(b"Hello\0");
        b.extend([0, 0]);
        // DESTINATION (offset 96): code 6, sig "s", u32 20, string, NUL
        b.extend([6, 1, b's', 0, 20, 0, 0, 0]);
        b.extend(b"org.freedesktop.DBus\0");
        b.extend([0, 0, 0]);                     // pad header to 8 (offset 128)
        b
    }

    #[test]
    fn hello_call_matches_reference_bytes() {
        let bytes = encode(&hello());
        assert_eq!(bytes, hello_bytes());
        assert_eq!(decode(&bytes).unwrap(), hello());
    }

    #[test]
    fn notify_message_roundtrips() {
        let hints = Value::Dict(
            "s".into(),
            "v".into(),
            vec![
                (
                    Value::Str("value".into()),
                    Value::Variant(Box::new(Value::I32(50))),
                ),
                (
                    Value::Str("urgency".into()),
                    Value::Variant(Box::new(Value::Byte(1))),
                ),
            ],
        );
        let m = Message::call(
            7,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            vec![
                Value::Str("In.A.Circle".into()),
                Value::U32(0),
                Value::Str(String::new()),
                Value::Str("Markdown".into()),
                Value::Str("1 / 3 files".into()),
                Value::Array("s".into(), vec![Value::Str("cancel".into())]),
                hints,
                Value::I32(-1),
            ],
        );
        assert_eq!(m.signature, "susssasa{sv}i");
        let bytes = encode(&m);
        assert_eq!(decode(&bytes).unwrap(), m);
    }

    #[test]
    fn replies_and_signals_roundtrip() {
        let ret = Message {
            kind: Kind::MethodReturn,
            serial: 3,
            reply_serial: Some(1),
            sender: Some("org.freedesktop.DBus".into()),
            destination: Some(":1.5".into()),
            signature: "s".into(),
            body: vec![Value::Str(":1.5".into())],
            ..hello()
        };
        let ret = Message {
            path: None,
            interface: None,
            member: None,
            ..ret
        };
        assert_eq!(decode(&encode(&ret)).unwrap(), ret);
        let err = Message {
            kind: Kind::Error,
            error_name: Some("org.freedesktop.DBus.Error.UnknownMethod".into()),
            signature: "s".into(),
            body: vec![Value::Str("nope".into())],
            ..ret.clone()
        };
        assert_eq!(decode(&encode(&err)).unwrap(), err);
        let sig = Message {
            kind: Kind::Signal,
            path: Some("/o".into()),
            interface: Some("a.B".into()),
            member: Some("ActionInvoked".into()),
            reply_serial: None,
            signature: "us".into(),
            body: vec![Value::U32(4), Value::Str("undo".into())],
            ..ret
        };
        assert_eq!(decode(&encode(&sig)).unwrap(), sig);
    }

    #[test]
    #[rustfmt::skip]
    fn big_endian_is_read() {
        let mut b = Vec::new();
        b.extend([b'B', 1, 0, 1]);               // big endian, method call
        b.extend([0, 0, 0, 31]);                 // body length 31
        b.extend([0, 0, 0, 5]);                  // serial 5
        b.extend([0, 0, 0, 42]);                 // fields array: 42 bytes
        b.extend([1, 1, b'o', 0, 0, 0, 0, 2]);   // PATH "/a" (offset 16)
        b.extend(b"/a\0");
        b.extend([0; 5]);
        b.extend([3, 1, b's', 0, 0, 0, 0, 1]);   // MEMBER "M" (offset 32)
        b.extend(b"M\0");
        b.extend([0; 6]);
        b.extend([8, 1, b'g', 0, 4]);              // SIGNATURE "usas" (offset 48)
        b.extend(b"usas\0");
        b.extend([0; 6]);                        // pad header to 8 (offset 64)
        b.extend([0, 0, 0, 7]);                  // u32 7
        b.extend([0, 0, 0, 2]);                  // "hi"
        b.extend(b"hi\0");
        b.push(0);
        b.extend([0, 0, 0, 15]);                 // as: 15 bytes
        b.extend([0, 0, 0, 1]);
        b.extend(b"a\0");
        b.extend([0, 0]);
        b.extend([0, 0, 0, 2]);
        b.extend(b"bc\0");
        let m = decode(&b).unwrap();
        assert_eq!(m.serial, 5);
        assert_eq!(m.path.as_deref(), Some("/a"));
        assert_eq!(m.member.as_deref(), Some("M"));
        assert_eq!(
            m.body,
            vec![
                Value::U32(7),
                Value::Str("hi".into()),
                Value::Array(
                    "s".into(),
                    vec![Value::Str("a".into()), Value::Str("bc".into())]
                ),
            ]
        );
    }

    #[test]
    fn frame_len_reads_the_fixed_prefix() {
        let bytes = encode(&hello());
        let head: [u8; 16] = bytes[..16].try_into().unwrap();
        assert_eq!(frame_len(&head).unwrap(), bytes.len());
    }

    #[test]
    fn oversized_frame_is_too_large() {
        let mut head = [0u8; 16];
        head[0] = b'l';
        head[4..8].copy_from_slice(&(1u32 << 20).to_le_bytes()); // body alone + 16 > 1 MiB
        assert!(matches!(frame_len(&head), Err(Error::TooLarge)));
        head[4..8].copy_from_slice(&0u32.to_le_bytes());
        head[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(frame_len(&head), Err(Error::TooLarge)));
        head[12..16].copy_from_slice(&0u32.to_le_bytes());
        head[0] = b'x';
        assert!(matches!(frame_len(&head), Err(Error::Malformed(_))));
    }

    #[test]
    fn malformed_messages_are_errors() {
        let good = hello_bytes();
        // Every truncation fails cleanly.
        for n in 0..good.len() {
            assert!(decode(&good[..n]).is_err(), "truncated to {n}");
        }
        // Trailing garbage.
        let mut long = good.clone();
        long.push(0);
        assert!(decode(&long).is_err());
        // Bad version, zero serial, unknown type.
        for (at, val) in [(3, 2u8), (8, 0), (1, 9)] {
            let mut b = good.clone();
            b[at] = val;
            assert!(decode(&b).is_err(), "byte {at}");
        }
        // Header field with a wrong type: PATH carried as a string.
        let mut b = good.clone();
        b[18] = b's';
        assert!(decode(&b).is_err());
        // Required field missing: a call without MEMBER.
        let mut m = hello();
        m.member = None;
        assert!(decode(&encode(&m)).is_err());
        // Body longer than the signature says.
        let mut m = hello();
        m.body = vec![Value::U32(1)];
        let mut b = encode(&m);
        let n = b.len();
        b[4] = 8; // claim 8 body bytes
        b.extend([0; 4]);
        assert_eq!(b.len(), n + 4);
        assert!(decode(&b).is_err());
    }
}
