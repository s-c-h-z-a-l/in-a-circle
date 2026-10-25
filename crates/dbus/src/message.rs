//! D-Bus messages: fixed header, header fields and body.

use crate::Error;
use crate::marshal::{Reader, Value, Writer, validate, validate_str};

/// Largest message (header + body) we accept. Notification traffic is tiny.
const MAX_MESSAGE: u64 = 64 << 10;

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
///
/// Fails with `Malformed` instead of producing wire data the peer would
/// reject: interior NULs, signatures over 255 bytes, array or dict items that
/// do not match the declared element signature.
pub fn encode(m: &Message) -> Result<Vec<u8>, Error> {
    for s in [
        &m.path,
        &m.interface,
        &m.member,
        &m.error_name,
        &m.destination,
        &m.sender,
    ]
    .into_iter()
    .flatten()
    {
        validate_str(s)?;
    }
    let mut body = Writer::new(true);
    let mut sig = String::new();
    for v in &m.body {
        validate(v)?;
        body.value(v);
        sig.push_str(&v.signature());
    }
    if sig.len() > 255 {
        return Err(Error::Malformed("body signature too long"));
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
    Ok(w.into_bytes())
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
/// prefix. At most 64 KiB.
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
    let left = r.budget();
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
        match (code, v) {
            (1, Value::Path(s)) => m.path = Some(s),
            (2, Value::Str(s)) => m.interface = Some(s),
            (3, Value::Str(s)) => m.member = Some(s),
            (4, Value::Str(s)) => m.error_name = Some(s),
            (5, Value::U32(n)) => m.reply_serial = Some(n),
            (6, Value::Str(s)) => m.destination = Some(s),
            (7, Value::Str(s)) => m.sender = Some(s),
            (8, Value::Sig(s)) => m.signature = s,
            (1..=8, _) => return Err(Error::Malformed("header field has the wrong type")),
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

    let mut br = Reader::new(&buf[total - body_len..], le).with_budget(left);
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
        let bytes = encode(&hello()).unwrap();
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
        let bytes = encode(&m).unwrap();
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
        assert_eq!(decode(&encode(&ret).unwrap()).unwrap(), ret);
        let err = Message {
            kind: Kind::Error,
            error_name: Some("org.freedesktop.DBus.Error.UnknownMethod".into()),
            signature: "s".into(),
            body: vec![Value::Str("nope".into())],
            ..ret.clone()
        };
        assert_eq!(decode(&encode(&err).unwrap()).unwrap(), err);
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
        assert_eq!(decode(&encode(&sig).unwrap()).unwrap(), sig);
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
        let bytes = encode(&hello()).unwrap();
        let head: [u8; 16] = bytes[..16].try_into().unwrap();
        assert_eq!(frame_len(&head).unwrap(), bytes.len());
    }

    #[test]
    fn oversized_frame_is_too_large() {
        let mut head = [0u8; 16];
        head[0] = b'l';
        head[4..8].copy_from_slice(&(1u32 << 20).to_le_bytes()); // body alone + 16 > 64 KiB
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
        assert!(decode(&encode(&m).unwrap()).is_err());
        // Body longer than the signature says.
        let mut m = hello();
        m.body = vec![Value::U32(1)];
        let mut b = encode(&m).unwrap();
        let n = b.len();
        b[4] = 8; // claim 8 body bytes
        b.extend([0; 4]);
        assert_eq!(b.len(), n + 4);
        assert!(decode(&b).is_err());
    }

    #[test]
    fn frame_cap_is_64_kib() {
        let mut head = [0u8; 16];
        head[0] = b'l';
        head[4..8].copy_from_slice(&(64u32 * 1024 - 16).to_le_bytes());
        assert_eq!(frame_len(&head).unwrap(), 64 * 1024);
        head[4..8].copy_from_slice(&(64u32 * 1024 - 15).to_le_bytes());
        assert!(matches!(frame_len(&head), Err(Error::TooLarge)));
    }

    fn ay_message(n: usize) -> Vec<u8> {
        let m = Message::call(
            1,
            "a.b",
            "/o",
            "a.B",
            "M",
            vec![Value::Array("y".into(), vec![Value::Byte(0); n])],
        );
        encode(&m).unwrap()
    }

    #[test]
    fn value_budget_covers_the_whole_message() {
        // Header fields and body share one budget of 4096 values.
        assert!(decode(&ay_message(4000)).is_ok());
        assert!(decode(&ay_message(4060)).is_ok());
        assert!(matches!(
            decode(&ay_message(4080)),
            Err(Error::Malformed("too many values"))
        ));
        assert!(matches!(
            decode(&ay_message(60_000)),
            Err(Error::Malformed("too many values"))
        ));
    }

    #[test]
    fn header_fields_must_have_their_exact_type() {
        // Re-type one header field of an encoded call: (code, wrong value).
        fn with_field(code: u8, v: Value) -> Vec<u8> {
            let mut fields: Vec<Value> = vec![
                field(1, Value::Path("/o".into())),
                field(3, Value::Str("M".into())),
            ];
            fields.retain(|f| !matches!(f, Value::Struct(p) if p[0] == Value::Byte(code)));
            fields.push(field(code, v));
            let mut w = Writer::new(true);
            for x in [
                Value::Byte(b'l'),
                Value::Byte(1),
                Value::Byte(0),
                Value::Byte(1),
                Value::U32(0),
                Value::U32(1),
                Value::Array("(yv)".into(), fields),
            ] {
                w.value(&x);
            }
            w.align(8);
            w.into_bytes()
        }
        let s = || Value::Str("x".into());
        let o = || Value::Path("/x".into());
        let g = || Value::Sig("y".into());
        // Sanity: the right types decode.
        assert!(decode(&with_field(2, s())).is_ok());
        assert!(decode(&with_field(4, s())).is_ok());
        assert!(decode(&with_field(5, Value::U32(1))).is_ok());
        assert!(decode(&with_field(6, s())).is_ok());
        assert!(decode(&with_field(7, s())).is_ok());
        for code in [2, 3, 4, 6, 7] {
            for wrong in [o(), g(), Value::U32(1)] {
                assert!(
                    decode(&with_field(code, wrong.clone())).is_err(),
                    "{code} {wrong:?}"
                );
            }
        }
        assert!(decode(&with_field(1, s())).is_err());
        assert!(decode(&with_field(1, g())).is_err());
        assert!(decode(&with_field(5, s())).is_err());
        assert!(decode(&with_field(8, s())).is_err());
    }

    #[test]
    fn encode_rejects_values_that_would_be_invalid_on_the_wire() {
        let call = |body: Vec<Value>| Message::call(1, "a.b", "/o", "a.B", "M", body);
        let bad = |body: Vec<Value>| matches!(encode(&call(body)), Err(Error::Malformed(_)));
        assert!(!bad(vec![Value::Str("fine".into())]));
        // Interior NUL in strings, paths and signatures.
        assert!(bad(vec![Value::Str("a\0b".into())]));
        assert!(bad(vec![Value::Path("/a\0".into())]));
        // Signature longer than 255 bytes: a signature value, and a variant
        // holding a wide struct.
        assert!(bad(vec![Value::Sig("y".repeat(256))]));
        assert!(bad(vec![Value::Variant(Box::new(Value::Struct(vec![
            Value::Byte(0);
            300
        ])))]));
        // More than 32 nested containers in one signature, at the top level
        // and inside a variant.
        let mut deep = Value::Byte(0);
        for _ in 0..33 {
            deep = Value::Array(deep.signature(), vec![]);
        }
        assert!(bad(vec![deep.clone()]));
        assert!(bad(vec![Value::Variant(Box::new(deep))]));
        // Empty structs have no valid signature.
        assert!(bad(vec![Value::Struct(vec![])]));
        assert!(bad(vec![Value::Variant(Box::new(Value::Struct(vec![])))]));
        // Items that do not match the declared element signature.
        assert!(bad(vec![Value::Array("s".into(), vec![Value::U32(1)])]));
        assert!(bad(vec![Value::Array("zz".into(), vec![])]));
        assert!(bad(vec![Value::Dict(
            "s".into(),
            "v".into(),
            vec![(Value::U32(1), Value::Variant(Box::new(Value::Byte(0))))]
        )]));
        assert!(bad(vec![Value::Dict(
            "s".into(),
            "v".into(),
            vec![(Value::Str("k".into()), Value::Byte(0))]
        )]));
        // A body signature over 255 bytes.
        assert!(bad(vec![Value::Byte(0); 256]));
        // Header strings are checked too.
        let mut m = call(vec![]);
        m.member = Some("M\0".into());
        assert!(matches!(encode(&m), Err(Error::Malformed(_))));
    }

    fn call_with(body: Vec<Value>) -> Message {
        Message::call(1, "a.b", "/o", "a.B", "M", body)
    }

    /// `n` layers of `wrap` around a byte.
    fn nest(n: usize, wrap: fn(Value) -> Value) -> Value {
        (0..n).fold(Value::Byte(0), |v, _| wrap(v))
    }

    #[test]
    fn encode_and_decode_agree_on_the_nesting_limit() {
        fn variant(v: Value) -> Value {
            Value::Variant(Box::new(v))
        }
        fn in_struct(v: Value) -> Value {
            variant(Value::Struct(vec![v]))
        }
        fn in_array(v: Value) -> Value {
            variant(Value::Array(v.signature(), vec![v]))
        }
        fn in_dict(v: Value) -> Value {
            variant(Value::Dict(
                "s".into(),
                v.signature(),
                vec![(Value::Str("k".into()), v)],
            ))
        }
        // (wrapper, layers that fit): a variant is one level, the others two.
        for (what, wrap, fits) in [
            ("variant", variant as fn(Value) -> Value, 64),
            ("struct", in_struct, 32),
            ("array", in_array, 32),
            ("dict", in_dict, 32),
        ] {
            let ok = call_with(vec![nest(fits, wrap)]);
            assert_eq!(decode(&encode(&ok).unwrap()).unwrap(), ok, "{what}");
            assert!(
                matches!(
                    encode(&call_with(vec![nest(fits + 1, wrap)])),
                    Err(Error::Malformed("containers nested too deep"))
                ),
                "{what}"
            );
        }
    }

    #[test]
    fn nesting_is_checked_before_a_deep_dict_key() {
        // A dict that sits exactly at the limit: its key is one level too
        // deep, so the depth error comes before the NUL in the key.
        let dict = Value::Dict(
            "s".into(),
            "y".into(),
            vec![(Value::Str("a\0".into()), Value::Byte(0))],
        );
        let mut v = dict;
        for _ in 0..64 {
            v = Value::Variant(Box::new(v));
        }
        assert!(matches!(
            encode(&call_with(vec![v])),
            Err(Error::Malformed("containers nested too deep"))
        ));
    }

    #[test]
    fn strings_inside_structs_are_validated() {
        let bad = Value::Struct(vec![Value::Byte(1), Value::Str("a\0b".into())]);
        assert!(matches!(
            encode(&call_with(vec![bad])),
            Err(Error::Malformed("NUL inside string"))
        ));
    }

    #[test]
    fn dict_keys_must_be_basic_types() {
        for key in ["ay", "v", "(y)", "a{sy}"] {
            let d = Value::Dict(key.into(), "y".into(), vec![]);
            assert!(
                matches!(
                    encode(&call_with(vec![d])),
                    Err(Error::Malformed("dict key is not a basic type"))
                ),
                "{key}"
            );
        }
    }

    #[test]
    fn signatures_of_exactly_255_bytes_are_allowed() {
        // A signature value, a variant holding a wide struct, and the body
        // signature itself: 255 fit (one length byte), 256 do not.
        let sig = Value::Sig("y".repeat(255));
        let wide = Value::Variant(Box::new(Value::Struct(vec![Value::Byte(7); 253])));
        let body = vec![Value::Byte(0); 255];
        for body in [vec![sig], vec![wide], body] {
            let m = call_with(body);
            let back = decode(&encode(&m).unwrap()).unwrap();
            assert_eq!(back, m);
        }
        let too_wide = Value::Variant(Box::new(Value::Struct(vec![Value::Byte(7); 254])));
        assert!(matches!(
            encode(&call_with(vec![too_wide])),
            Err(Error::Malformed("signature too long"))
        ));
    }

    /// A message of `kind` with exactly the header fields in `have`
    /// (bit 0 PATH, 1 INTERFACE, 2 MEMBER, 3 ERROR_NAME, 4 REPLY_SERIAL).
    fn with_fields(kind: Kind, have: u8) -> Message {
        let on = |bit: u8| have & (1 << bit) != 0;
        let text = |bit: u8, s: &str| on(bit).then(|| s.to_owned());
        Message {
            kind,
            serial: 1,
            reply_serial: on(4).then_some(1),
            path: text(0, "/o"),
            interface: text(1, "a.B"),
            member: text(2, "M"),
            error_name: text(3, "a.B.Err"),
            destination: None,
            sender: None,
            signature: String::new(),
            body: vec![],
        }
    }

    #[test]
    fn each_message_type_needs_exactly_its_required_fields() {
        // Required fields per message type, from the specification:
        // METHOD_CALL PATH+MEMBER; METHOD_RETURN REPLY_SERIAL;
        // ERROR ERROR_NAME+REPLY_SERIAL; SIGNAL PATH+INTERFACE+MEMBER.
        let (path, iface, member, error, reply) = (1u8, 2, 4, 8, 16);
        for (kind, required) in [
            (Kind::MethodCall, path | member),
            (Kind::MethodReturn, reply),
            (Kind::Error, error | reply),
            (Kind::Signal, path | iface | member),
        ] {
            for have in 0..32u8 {
                let m = with_fields(kind, have);
                let r = decode(&encode(&m).unwrap());
                if have & required == required {
                    assert_eq!(r.unwrap(), m, "{kind:?} {have:05b}");
                } else {
                    assert!(
                        matches!(r, Err(Error::Malformed("required header field missing"))),
                        "{kind:?} {have:05b}"
                    );
                }
            }
        }
    }

    /// A little-endian message laid out by hand from the D-Bus specification:
    /// the fixed header, the header-field array (each field struct aligned to
    /// 8 from the array start, the array length excluding the final padding),
    /// padding to 8, then the body. `fields` are the field structs without
    /// padding; none of this goes through our encoder.
    fn hand_frame(kind: u8, fields: &[Vec<u8>], body: &[u8]) -> Vec<u8> {
        let mut arr = Vec::new();
        for f in fields {
            while arr.len() % 8 != 0 {
                arr.push(0);
            }
            arr.extend(f);
        }
        let mut b = vec![b'l', kind, 0, 1];
        b.extend((body.len() as u32).to_le_bytes());
        b.extend(1u32.to_le_bytes()); // serial
        b.extend((arr.len() as u32).to_le_bytes());
        b.extend(arr);
        while b.len() % 8 != 0 {
            b.push(0);
        }
        b.extend(body);
        b
    }

    /// A header field whose variant holds a string-like value of type `t`
    /// ('o', 's'): code, signature "t", u32 length, bytes, NUL.
    fn str_field(code: u8, t: u8, v: &str) -> Vec<u8> {
        let mut f = vec![code, 1, t, 0];
        f.extend((v.len() as u32).to_le_bytes());
        f.extend(v.as_bytes());
        f.push(0);
        f
    }

    /// SIGNATURE field (code 8, type 'g').
    fn sig_field(sig: &[u8]) -> Vec<u8> {
        let mut f = vec![8, 1, b'g', 0, sig.len() as u8];
        f.extend(sig);
        f.push(0);
        f
    }

    fn call_fields(sig: &[u8]) -> Vec<Vec<u8>> {
        vec![
            str_field(1, b'o', "/o"),
            str_field(3, b's', "M"),
            sig_field(sig),
        ]
    }

    #[test]
    fn hand_written_frames_decode_when_valid() {
        // y=7 as the body of a call: the helper's layout is itself right.
        let m = decode(&hand_frame(1, &call_fields(b"y"), &[7])).unwrap();
        assert_eq!(m.body, vec![Value::Byte(7)]);
        assert_eq!(m.member.as_deref(), Some("M"));
    }

    #[test]
    fn a_65th_nested_variant_is_refused() {
        let chain = |n: usize| {
            let mut body = Vec::new();
            for _ in 0..n {
                body.extend([1, b'v', 0]); // variant signature "v"
            }
            body.extend([1, b'y', 0, 9]); // innermost: variant "y" = 9
            body
        };
        // `chain(n)` is n + 1 variants inside each other (n of signature "v",
        // then one holding a byte). 64 variants fit; the 65th does not.
        assert!(decode(&hand_frame(1, &call_fields(b"v"), &chain(62))).is_ok());
        assert!(decode(&hand_frame(1, &call_fields(b"v"), &chain(63))).is_ok());
        for n in [64, 65] {
            assert!(
                matches!(
                    decode(&hand_frame(1, &call_fields(b"v"), &chain(n))),
                    Err(Error::Malformed("containers nested too deep"))
                ),
                "{n}"
            );
        }
    }

    #[test]
    fn over_long_and_over_deep_signatures_are_refused() {
        // A signature field is limited to 255 bytes by its one-byte length;
        // a 256-byte signature cannot even be written. 255 bytes are fine as
        // long as the type is, and 33 nested arrays are not.
        let deep = [vec![b'a'; 33], b"y".to_vec()].concat();
        assert!(matches!(
            decode(&hand_frame(1, &call_fields(&deep), &[])),
            Err(Error::Malformed("signature too deep"))
        ));
        // A length byte of 255 with fewer bytes behind it.
        let mut f = hand_frame(1, &call_fields(b"y"), &[0]);
        let at = f.iter().position(|&b| b == b'g').unwrap() + 2;
        f[at] = 255;
        assert!(decode(&f).is_err());
        // 255 one-byte types are a valid signature: 255 bytes of body.
        let body = vec![5u8; 255];
        let m = decode(&hand_frame(1, &call_fields(&[b'y'; 255]), &body)).unwrap();
        assert_eq!(m.body.len(), 255);
    }

    #[test]
    fn a_signal_without_an_interface_is_refused() {
        let fields = vec![str_field(1, b'o', "/o"), str_field(3, b's', "M")];
        assert!(matches!(
            decode(&hand_frame(4, &fields, &[])),
            Err(Error::Malformed("required header field missing"))
        ));
        let mut with = fields;
        with.push(str_field(2, b's', "a.B"));
        assert!(decode(&hand_frame(4, &with, &[])).is_ok());
    }

    #[test]
    fn a_dict_with_a_non_basic_key_is_refused() {
        // Body signature a{vy}: an array of length 0, padded to its 8-aligned
        // elements. Valid in every other respect.
        for sig in [&b"a{vy}"[..], b"a{ayy}", b"a{(y)y}"] {
            assert!(
                matches!(
                    decode(&hand_frame(1, &call_fields(sig), &[0; 8])),
                    Err(Error::Malformed("dict key is not a basic type"))
                ),
                "{}",
                String::from_utf8_lossy(sig)
            );
        }
        assert!(decode(&hand_frame(1, &call_fields(b"a{sy}"), &[0; 8])).is_ok());
    }
}
