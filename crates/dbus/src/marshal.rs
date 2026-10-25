//! D-Bus marshalling: typed values to and from the wire format.

use crate::Error;

/// Maximum container nesting while reading, counted across arrays, structs,
/// dict entries and variants together (the inner signature of a variant
/// continues at the depth of the variant).
const MAX_DEPTH: usize = 64;
/// Maximum container nesting of one signature string. Arrays and structs are
/// deliberately counted together: stricter than the spec (32 of each), and
/// the depth is shared with the variants in `MAX_DEPTH`, so do not "fix" it
/// into separate counters.
const MAX_SIG_DEPTH: usize = 32;
/// Values (every scalar, string, container and variant) one `Reader` may
/// produce; a peer must not turn a few KiB on the wire into megabytes of heap.
const MAX_VALUES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Str(String),
    Path(String),
    Sig(String),
    /// Element signature and elements.
    Array(String, Vec<Value>),
    Struct(Vec<Value>),
    /// Key signature, value signature and entries (`a{kv}`).
    Dict(String, String, Vec<(Value, Value)>),
    Variant(Box<Value>),
}

impl Value {
    pub fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".into(),
            Value::Bool(_) => "b".into(),
            Value::I16(_) => "n".into(),
            Value::U16(_) => "q".into(),
            Value::I32(_) => "i".into(),
            Value::U32(_) => "u".into(),
            Value::I64(_) => "x".into(),
            Value::U64(_) => "t".into(),
            Value::Str(_) => "s".into(),
            Value::Path(_) => "o".into(),
            Value::Sig(_) => "g".into(),
            Value::Array(elem, _) => format!("a{elem}"),
            Value::Struct(fields) => {
                let mut s = String::from("(");
                for f in fields {
                    s.push_str(&f.signature());
                }
                s.push(')');
                s
            }
            Value::Dict(k, v, _) => format!("a{{{k}{v}}}"),
            Value::Variant(_) => "v".into(),
        }
    }
}

fn align_of(sig: &str) -> usize {
    match sig.as_bytes().first() {
        Some(b'n' | b'q') => 2,
        Some(b'b' | b'i' | b'u' | b's' | b'o' | b'a') => 4,
        Some(b'x' | b't' | b'(' | b'{') => 8,
        _ => 1,
    }
}

pub(crate) fn validate_str(s: &str) -> Result<(), Error> {
    if s.contains('\0') {
        return Err(Error::Malformed("NUL inside string"));
    }
    Ok(())
}

/// Checks that `v` can be written as valid wire data: no interior NULs,
/// signatures within 255 bytes, items matching their declared signatures,
/// and every complete type passing the same check `decode` applies (nesting,
/// no empty structs).
pub(crate) fn validate(v: &Value) -> Result<(), Error> {
    one_type(&v.signature())?;
    validate_at(v, 0)
}

fn one_type(sig: &str) -> Result<Ty, Error> {
    let mut pos = 0;
    let t = parse_type(sig, &mut pos, 0)?;
    if pos != sig.len() {
        return Err(Error::Malformed("signature is not one complete type"));
    }
    Ok(t)
}

fn validate_at(v: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Malformed("containers nested too deep"));
    }
    match v {
        Value::Str(s) | Value::Path(s) => validate_str(s),
        Value::Sig(s) => {
            validate_str(s)?;
            if s.len() > 255 {
                return Err(Error::Malformed("signature too long"));
            }
            Ok(())
        }
        Value::Array(elem, items) => {
            one_type(elem)?;
            for it in items {
                if it.signature() != *elem {
                    return Err(Error::Malformed("array item does not match its signature"));
                }
                validate_at(it, depth + 1)?;
            }
            Ok(())
        }
        Value::Struct(fields) => fields.iter().try_for_each(|f| validate_at(f, depth + 1)),
        Value::Dict(ks, vs, entries) => {
            if !one_type(ks)?.is_basic() {
                return Err(Error::Malformed("dict key is not a basic type"));
            }
            one_type(vs)?;
            for (k, val) in entries {
                if k.signature() != *ks || val.signature() != *vs {
                    return Err(Error::Malformed("dict entry does not match its signature"));
                }
                validate_at(k, depth + 1)?;
                validate_at(val, depth + 1)?;
            }
            Ok(())
        }
        Value::Variant(inner) => {
            let sig = inner.signature();
            if sig.len() > 255 {
                return Err(Error::Malformed("signature too long"));
            }
            one_type(&sig)?;
            validate_at(inner, depth + 1)
        }
        _ => Ok(()),
    }
}

/// Serializes values; offsets are counted from the start of the buffer.
pub struct Writer {
    buf: Vec<u8>,
    le: bool,
}

impl Writer {
    pub fn new(little_endian: bool) -> Writer {
        Writer {
            buf: Vec::new(),
            le: little_endian,
        }
    }

    /// Pads with zeros to a multiple of `n`.
    pub fn align(&mut self, n: usize) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(0);
        }
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn put_u32(&mut self, v: u32) {
        self.align(4);
        let b = if self.le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        };
        self.buf.extend_from_slice(&b);
    }

    fn put_sig(&mut self, s: &str) {
        debug_assert!(s.len() <= 255);
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn put_str(&mut self, s: &str) {
        self.put_u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    /// Writes the array length slot, pads to the element alignment and
    /// returns where the length and the data start.
    fn array_start(&mut self, elem_align: usize) -> (usize, usize) {
        self.put_u32(0);
        let len_at = self.buf.len() - 4;
        self.align(elem_align);
        (len_at, self.buf.len())
    }

    fn array_end(&mut self, len_at: usize, start: usize) {
        let len = (self.buf.len() - start) as u32;
        let b = if self.le {
            len.to_le_bytes()
        } else {
            len.to_be_bytes()
        };
        self.buf[len_at..len_at + 4].copy_from_slice(&b);
    }

    pub fn value(&mut self, v: &Value) {
        macro_rules! num {
            ($x:expr, $n:expr) => {{
                self.align($n);
                let b = if self.le {
                    $x.to_le_bytes()
                } else {
                    $x.to_be_bytes()
                };
                self.buf.extend_from_slice(&b);
            }};
        }
        match v {
            Value::Byte(x) => self.buf.push(*x),
            Value::Bool(x) => self.put_u32(*x as u32),
            Value::I16(x) => num!(x, 2),
            Value::U16(x) => num!(x, 2),
            Value::I32(x) => num!(x, 4),
            Value::U32(x) => self.put_u32(*x),
            Value::I64(x) => num!(x, 8),
            Value::U64(x) => num!(x, 8),
            Value::Str(s) | Value::Path(s) => self.put_str(s),
            Value::Sig(s) => self.put_sig(s),
            Value::Array(elem, items) => {
                let (len_at, start) = self.array_start(align_of(elem));
                for it in items {
                    self.value(it);
                }
                self.array_end(len_at, start);
            }
            Value::Struct(fields) => {
                self.align(8);
                for f in fields {
                    self.value(f);
                }
            }
            Value::Dict(_, _, entries) => {
                let (len_at, start) = self.array_start(8);
                for (k, val) in entries {
                    self.align(8);
                    self.value(k);
                    self.value(val);
                }
                self.array_end(len_at, start);
            }
            Value::Variant(inner) => {
                self.put_sig(&inner.signature());
                self.value(inner);
            }
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// A parsed complete type.
enum Ty {
    Byte,
    Bool,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Str,
    Path,
    Sig,
    Variant,
    Array(String, Box<Ty>),
    Struct(Vec<Ty>),
    Dict(String, String, Box<Ty>, Box<Ty>),
}

impl Ty {
    fn is_basic(&self) -> bool {
        !matches!(
            self,
            Ty::Variant | Ty::Array(..) | Ty::Struct(_) | Ty::Dict(..)
        )
    }
}

/// Parses one complete type starting at `*pos`.
fn parse_type(sig: &str, pos: &mut usize, depth: usize) -> Result<Ty, Error> {
    let b = sig.as_bytes();
    let c = *b
        .get(*pos)
        .ok_or(Error::Malformed("incomplete signature"))?;
    *pos += 1;
    Ok(match c {
        b'y' => Ty::Byte,
        b'b' => Ty::Bool,
        b'n' => Ty::I16,
        b'q' => Ty::U16,
        b'i' => Ty::I32,
        b'u' => Ty::U32,
        b'x' => Ty::I64,
        b't' => Ty::U64,
        b's' => Ty::Str,
        b'o' => Ty::Path,
        b'g' => Ty::Sig,
        b'v' => Ty::Variant,
        b'a' => {
            if depth + 1 > MAX_SIG_DEPTH {
                return Err(Error::Malformed("signature too deep"));
            }
            if b.get(*pos) == Some(&b'{') {
                *pos += 1;
                let ks = *pos;
                let k = parse_type(sig, pos, depth + 2)?;
                if !k.is_basic() {
                    return Err(Error::Malformed("dict key is not a basic type"));
                }
                let vs = *pos;
                let v = parse_type(sig, pos, depth + 2)?;
                let ve = *pos;
                if b.get(*pos) != Some(&b'}') {
                    return Err(Error::Malformed("unterminated dict entry"));
                }
                *pos += 1;
                Ty::Dict(
                    sig[ks..vs].to_owned(),
                    sig[vs..ve].to_owned(),
                    Box::new(k),
                    Box::new(v),
                )
            } else {
                let es = *pos;
                let e = parse_type(sig, pos, depth + 1)?;
                Ty::Array(sig[es..*pos].to_owned(), Box::new(e))
            }
        }
        b'(' => {
            if depth + 1 > MAX_SIG_DEPTH {
                return Err(Error::Malformed("signature too deep"));
            }
            let mut fields = Vec::new();
            while b.get(*pos) != Some(&b')') {
                if *pos >= b.len() {
                    return Err(Error::Malformed("unterminated struct"));
                }
                fields.push(parse_type(sig, pos, depth + 1)?);
            }
            *pos += 1;
            if fields.is_empty() {
                return Err(Error::Malformed("empty struct"));
            }
            Ty::Struct(fields)
        }
        _ => return Err(Error::Malformed("unknown type code")),
    })
}

fn parse_all(sig: &str) -> Result<Vec<Ty>, Error> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < sig.len() {
        out.push(parse_type(sig, &mut pos, 0)?);
    }
    Ok(out)
}

/// Deserializes values; offsets are counted from the start of `buf`.
/// Padding bytes are not checked (the spec says zero): being lenient on
/// input is harmless for a client.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    le: bool,
    /// Values we may still produce.
    budget: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], little_endian: bool) -> Reader<'a> {
        Reader {
            buf,
            pos: 0,
            le: little_endian,
            budget: MAX_VALUES,
        }
    }

    /// Values this reader may still produce.
    pub(crate) fn budget(&self) -> usize {
        self.budget
    }

    /// Continues another reader's value budget (one budget per message).
    pub(crate) fn with_budget(mut self, budget: usize) -> Reader<'a> {
        self.budget = budget;
        self
    }

    /// True when every byte has been consumed.
    pub fn finished(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// Reads one value per complete type in `sig`.
    pub fn read(&mut self, sig: &str) -> Result<Vec<Value>, Error> {
        let tys = parse_all(sig)?;
        tys.iter().map(|t| self.read_ty(t, 0)).collect()
    }

    fn align(&mut self, n: usize) -> Result<(), Error> {
        self.pos = self.pos.next_multiple_of(n);
        if self.pos > self.buf.len() {
            return Err(Error::Malformed("truncated"));
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(Error::Malformed("truncated"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.align(4)?;
        let b: [u8; 4] = self.take(4)?.try_into().unwrap();
        Ok(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn text(&mut self, len: usize) -> Result<String, Error> {
        let bytes = self.take(len)?;
        if self.take(1)?[0] != 0 {
            return Err(Error::Malformed("string not NUL-terminated"));
        }
        let s = std::str::from_utf8(bytes).map_err(|_| Error::Malformed("string is not UTF-8"))?;
        if s.contains('\0') {
            return Err(Error::Malformed("NUL inside string"));
        }
        Ok(s.to_owned())
    }

    /// Reads one value; `depth` is the number of containers around it.
    fn read_ty(&mut self, ty: &Ty, depth: usize) -> Result<Value, Error> {
        self.budget = self
            .budget
            .checked_sub(1)
            .ok_or(Error::Malformed("too many values"))?;
        if depth >= MAX_DEPTH
            && matches!(
                ty,
                Ty::Variant | Ty::Array(..) | Ty::Struct(_) | Ty::Dict(..)
            )
        {
            return Err(Error::Malformed("containers nested too deep"));
        }
        macro_rules! num {
            ($t:ty, $n:expr, $v:path) => {{
                self.align($n)?;
                let b: [u8; $n] = self.take($n)?.try_into().unwrap();
                $v(if self.le {
                    <$t>::from_le_bytes(b)
                } else {
                    <$t>::from_be_bytes(b)
                })
            }};
        }
        Ok(match ty {
            Ty::Byte => Value::Byte(self.take(1)?[0]),
            Ty::Bool => match self.u32()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(Error::Malformed("invalid boolean")),
            },
            Ty::I16 => num!(i16, 2, Value::I16),
            Ty::U16 => num!(u16, 2, Value::U16),
            Ty::I32 => num!(i32, 4, Value::I32),
            Ty::U32 => Value::U32(self.u32()?),
            Ty::I64 => num!(i64, 8, Value::I64),
            Ty::U64 => num!(u64, 8, Value::U64),
            Ty::Str => {
                let n = self.u32()? as usize;
                Value::Str(self.text(n)?)
            }
            Ty::Path => {
                let n = self.u32()? as usize;
                Value::Path(self.text(n)?)
            }
            Ty::Sig => {
                let n = self.take(1)?[0] as usize;
                Value::Sig(self.text(n)?)
            }
            Ty::Variant => {
                let n = self.take(1)?[0] as usize;
                let sig = self.text(n)?;
                let mut pos = 0;
                let inner = parse_type(&sig, &mut pos, 0)?;
                if pos != sig.len() {
                    return Err(Error::Malformed("variant signature is not one type"));
                }
                Value::Variant(Box::new(self.read_ty(&inner, depth + 1)?))
            }
            Ty::Array(es, elem) => {
                let end = self.array_end(elem_align(elem))?;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.read_ty(elem, depth + 1)?);
                }
                if self.pos != end {
                    return Err(Error::Malformed("array length mismatch"));
                }
                Value::Array(es.clone(), items)
            }
            Ty::Struct(fields) => {
                self.align(8)?;
                let mut out = Vec::with_capacity(fields.len());
                for f in fields {
                    out.push(self.read_ty(f, depth + 1)?);
                }
                Value::Struct(out)
            }
            Ty::Dict(ks, vs, k, v) => {
                let end = self.array_end(8)?;
                let mut entries = Vec::new();
                while self.pos < end {
                    self.align(8)?;
                    // mutants: equivalent (`depth + 1` -> `depth * 1`): the key is
                    // a basic type (parse_type refuses any other), and `depth` is
                    // only ever compared for containers or passed on to them.
                    let key = self.read_ty(k, depth + 1)?;
                    let val = self.read_ty(v, depth + 1)?;
                    entries.push((key, val));
                }
                if self.pos != end {
                    return Err(Error::Malformed("array length mismatch"));
                }
                Value::Dict(ks.clone(), vs.clone(), entries)
            }
        })
    }

    /// Reads an array length and the padding before its data; returns where
    /// the data ends. The length must fit in the buffer.
    fn array_end(&mut self, elem_align: usize) -> Result<usize, Error> {
        let len = self.u32()? as usize;
        self.align(elem_align)?;
        self.pos
            .checked_add(len)
            .filter(|&e| e <= self.buf.len())
            .ok_or(Error::Malformed("array exceeds message"))
    }
}

fn elem_align(t: &Ty) -> usize {
    match t {
        Ty::I16 | Ty::U16 => 2,
        Ty::Bool | Ty::I32 | Ty::U32 | Ty::Str | Ty::Path | Ty::Array(..) | Ty::Dict(..) => 4,
        Ty::I64 | Ty::U64 | Ty::Struct(_) => 8,
        Ty::Byte | Ty::Sig | Ty::Variant => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(v: &[Value]) -> Vec<u8> {
        let mut w = Writer::new(true);
        for x in v {
            w.value(x);
        }
        w.into_bytes()
    }

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    #[test]
    fn alignment_after_odd_fields() {
        // y, 3 pad, u32 len 2, "ab\0", 1 pad, u32 7
        let b = write(&[Value::Byte(1), s("ab"), Value::U32(7)]);
        assert_eq!(
            b,
            [
                1, 0, 0, 0, // y + padding to 4
                2, 0, 0, 0, // string length
                b'a', b'b', 0, // string + NUL
                0, // padding to 4
                7, 0, 0, 0, // u32
            ]
        );
        // y, 7 pad, u64 5
        let b = write(&[Value::Byte(1), Value::U64(5)]);
        assert_eq!(b, [1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn notify_signature_roundtrips() {
        let hints = Value::Dict(
            "s".into(),
            "v".into(),
            vec![
                (s("value"), Value::Variant(Box::new(Value::I32(50)))),
                (s("urgency"), Value::Variant(Box::new(Value::Byte(1)))),
            ],
        );
        let body = vec![
            s("In.A.Circle"),
            Value::U32(0),
            s(""),
            s("Markdown"),
            s("1 / 3 files"),
            Value::Array("s".into(), vec![s("cancel"), s("Cancel")]),
            hints,
            Value::I32(-1),
        ];
        let sig: String = body.iter().map(Value::signature).collect();
        assert_eq!(sig, "susssasa{sv}i");
        for le in [true, false] {
            let mut w = Writer::new(le);
            for v in &body {
                w.value(v);
            }
            let bytes = w.into_bytes();
            let mut r = Reader::new(&bytes, le);
            assert_eq!(r.read(&sig).unwrap(), body);
            assert!(r.finished());
        }
    }

    #[test]
    fn big_endian_reads_scalars_and_arrays() {
        let bytes = [
            0, 0, 0, 7, // u32 7
            0, 0, 0, 2, b'h', b'i', 0, // "hi"
            0, // pad
            0, 0, 0, 15, // array length
            0, 0, 0, 1, b'a', 0, // "a"
            0, 0, // pad
            0, 0, 0, 2, b'b', b'c', 0, // "bc"
        ];
        let mut r = Reader::new(&bytes, false);
        assert_eq!(
            r.read("usas").unwrap(),
            vec![
                Value::U32(7),
                s("hi"),
                Value::Array("s".into(), vec![s("a"), s("bc")])
            ]
        );
        assert!(r.finished());
    }

    fn err(sig: &str, bytes: &[u8]) {
        assert!(
            matches!(Reader::new(bytes, true).read(sig), Err(Error::Malformed(_))),
            "{sig} {bytes:?}"
        );
    }

    #[test]
    fn malformed_values_are_errors() {
        err("u", &[1, 0]); // truncated
        err("s", &[2, 0, 0, 0, 0xff, 0xfe, 0]); // not UTF-8
        err("s", &[1, 0, 0, 0, b'a', b'b']); // no NUL
        err("s", &[1, 0, 0, 0, b'a']); // NUL missing at end of buffer
        err("s", &[3, 0, 0, 0, b'a', 0, b'b', 0]); // NUL inside
        err("ay", &[9, 0, 0, 0, 1, 2]); // array longer than buffer
        err("ay", &[0xff, 0xff, 0xff, 0xff]); // huge length
        err("b", &[2, 0, 0, 0]); // invalid bool
        err("z", &[]); // unknown type code
        err("a{vy}", &[0; 8]); // container dict key (a body that would read fine)
        err("a{ayy}", &[0; 8]);
        err("()", &[]); // empty struct
        err("(y", &[0]); // unterminated struct
        err("a", &[0, 0, 0, 0]); // incomplete signature
        err("v", &[2, b'y', b'y', 0, 1]); // variant sig is two types
        err("a(uu)", &[4, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]); // element overruns array
    }

    #[test]
    fn signature_depth_is_limited() {
        let ok = format!("{}y", "a".repeat(32));
        assert!(parse_all(&ok).is_ok());
        let deep = format!("{}y", "a".repeat(33));
        assert!(matches!(parse_all(&deep), Err(Error::Malformed(_))));
        let nested = format!("{}y{}", "(".repeat(33), ")".repeat(33));
        assert!(matches!(parse_all(&nested), Err(Error::Malformed(_))));
    }

    fn nested_variants(levels: usize) -> Vec<u8> {
        let mut b = Vec::new();
        for _ in 0..levels {
            b.extend_from_slice(&[1, b'v', 0]);
        }
        b.extend_from_slice(&[1, b'y', 0, 9]);
        b
    }

    #[test]
    fn variant_nesting_is_limited() {
        for levels in [10, 63] {
            let ok = nested_variants(levels);
            assert!(Reader::new(&ok, true).read("v").is_ok(), "{levels}");
        }
        for levels in [64, 100, 10_000] {
            err("v", &nested_variants(levels));
        }
    }

    #[test]
    fn big_endian_reads_every_integer_width() {
        let bytes = [
            0xff, 0xfe, // n = -2
            0x01, 0x02, // q = 0x0102
            0, 0, 0, 1, // b = true
            0xff, 0xff, 0xff, 0xfb, // i = -5
            0, 0, 0, 0, // padding to 8
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfd, // x = -3
            1, 2, 3, 4, 5, 6, 7, 8, // t
        ];
        let mut r = Reader::new(&bytes, false);
        assert_eq!(
            r.read("nqbixt").unwrap(),
            vec![
                Value::I16(-2),
                Value::U16(0x0102),
                Value::Bool(true),
                Value::I32(-5),
                Value::I64(-3),
                Value::U64(0x0102030405060708),
            ]
        );
        assert!(r.finished());
    }

    #[test]
    fn nonzero_padding_is_tolerated() {
        let bytes = [1, 9, 9, 9, 7, 0, 0, 0];
        let v = Reader::new(&bytes, true).read("yu").unwrap();
        assert_eq!(v, vec![Value::Byte(1), Value::U32(7)]);
    }

    fn ay(n: usize) -> Vec<u8> {
        let mut b = (n as u32).to_le_bytes().to_vec();
        b.resize(4 + n, 0);
        b
    }

    #[test]
    fn value_budget_stops_a_byte_array_flood() {
        // A 64 KiB array of zeros would otherwise become 64 Ki Values.
        let flood = ay(64 * 1024 - 4);
        assert!(matches!(
            Reader::new(&flood, true).read("ay"),
            Err(Error::Malformed("too many values"))
        ));
        // The array itself is one value, each byte one more: 4095 fit.
        assert_eq!(
            Reader::new(&ay(4095), true).read("ay").unwrap()[0],
            Value::Array("y".into(), vec![Value::Byte(0); 4095])
        );
        assert!(matches!(
            Reader::new(&ay(4096), true).read("ay"),
            Err(Error::Malformed("too many values"))
        ));
    }

    #[test]
    fn value_budget_counts_containers_and_variants() {
        // 2049 empty arrays: 2049 + 1 values; 4096 of them exceed the budget.
        let mut b = ((2049 * 4) as u32).to_le_bytes().to_vec();
        b.resize(4 + 2049 * 4, 0);
        assert!(Reader::new(&b, true).read("aay").is_ok());
        let mut b = ((4096 * 4) as u32).to_le_bytes().to_vec();
        b.resize(4 + 4096 * 4, 0);
        assert!(matches!(
            Reader::new(&b, true).read("aay"),
            Err(Error::Malformed("too many values"))
        ));
        // Each av element costs two values (variant + payload).
        let mut b = ((2048 * 4) as u32).to_le_bytes().to_vec();
        for _ in 0..2048 {
            b.extend([1, b'y', 0, 0]);
        }
        assert!(matches!(
            Reader::new(&b, true).read("av"),
            Err(Error::Malformed("too many values"))
        ));
    }

    /// `n` containers nested in each other through variants only.
    fn variant_chain(n: usize) -> Value {
        let mut v = Value::Byte(0);
        for _ in 0..n {
            v = Value::Variant(Box::new(v));
        }
        v
    }

    /// `layers` of a variant holding a one-element array of the next layer.
    fn variant_array_chain(layers: usize) -> Value {
        let mut v = Value::Byte(0);
        for _ in 0..layers {
            v = Value::Variant(Box::new(Value::Array(v.signature(), vec![v])));
        }
        v
    }

    fn read_value(v: &Value) -> Result<Vec<Value>, Error> {
        let mut w = Writer::new(true);
        w.value(v);
        let bytes = w.into_bytes();
        Reader::new(&bytes, true).read(&v.signature())
    }

    #[test]
    fn total_container_depth_is_limited_to_64() {
        // Depth counts arrays, structs, dict entries and variants together.
        assert!(read_value(&variant_chain(64)).is_ok());
        assert!(matches!(
            read_value(&variant_chain(65)),
            Err(Error::Malformed(_))
        ));
        assert!(read_value(&variant_array_chain(32)).is_ok());
        assert!(matches!(
            read_value(&variant_array_chain(33)),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn variants_wrapping_deep_signatures_do_not_overflow_the_stack() {
        // 63 nested variants, each wrapping 31 arrays: about 2000 levels in
        // total. Must be an error, not a stack overflow, on a default
        // test-thread stack in a debug build.
        fn layer(inner: Value, arrays: usize) -> Value {
            let mut v = inner;
            for _ in 0..arrays {
                let sig = v.signature();
                v = Value::Array(sig, vec![v]);
            }
            Value::Variant(Box::new(v))
        }
        let mut v = Value::Byte(0);
        for _ in 0..63 {
            v = layer(v, 31);
        }
        let mut w = Writer::new(true);
        w.value(&v);
        let bytes = w.into_bytes();
        assert!(matches!(
            Reader::new(&bytes, true).read("v"),
            Err(Error::Malformed(_))
        ));
    }
    #[test]
    fn false_reads_as_false() {
        let v = Reader::new(&[0, 0, 0, 0, 1, 0, 0, 0], true)
            .read("bb")
            .unwrap();
        assert_eq!(v, vec![Value::Bool(false), Value::Bool(true)]);
    }

    #[test]
    fn alignment_of_each_type_code_follows_the_spec() {
        // The D-Bus specification's alignment table, by first type code.
        for (sig, want) in [
            ("y", 1),
            ("g", 1),
            ("v", 1),
            ("n", 2),
            ("q", 2),
            ("b", 4),
            ("i", 4),
            ("u", 4),
            ("s", 4),
            ("o", 4),
            ("ay", 4),
            ("x", 8),
            ("t", 8),
            ("(yy)", 8),
            ("{sv}", 8),
        ] {
            assert_eq!(align_of(sig), want, "{sig}");
        }
    }

    /// The arrays of 8-aligned elements, as the specification lays them out:
    /// the length, zero padding up to the element alignment (also when the
    /// array is empty), then the elements.
    #[rustfmt::skip]
    fn eight_aligned_arrays() -> Vec<(Value, Vec<u8>)> {
        vec![
            (Value::Array("t".into(), vec![Value::U64(5)]),
             vec![8, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0]),
            (Value::Array("x".into(), vec![Value::I64(-1)]),
             vec![8, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255]),
            (Value::Array("t".into(), vec![]),
             vec![0, 0, 0, 0, 0, 0, 0, 0]),
            (Value::Array("(yy)".into(), vec![Value::Struct(vec![Value::Byte(1), Value::Byte(2)])]),
             vec![2, 0, 0, 0, 0, 0, 0, 0, 1, 2]),
        ]
    }

    #[test]
    fn arrays_of_8_aligned_elements_are_padded_on_write() {
        for (v, want) in eight_aligned_arrays() {
            assert_eq!(write(std::slice::from_ref(&v)), want, "{v:?}");
        }
    }

    #[test]
    fn arrays_of_8_aligned_elements_are_padded_on_read() {
        for (v, bytes) in eight_aligned_arrays() {
            let mut r = Reader::new(&bytes, true);
            assert_eq!(r.read(&v.signature()).unwrap(), vec![v.clone()], "{v:?}");
            assert!(r.finished(), "{v:?}: the padding of an empty array too");
        }
    }

    /// `n` layers of a variant holding a struct that holds the next layer.
    fn variant_struct_chain(n: usize) -> Value {
        let mut v = Value::Byte(0);
        for _ in 0..n {
            v = Value::Variant(Box::new(Value::Struct(vec![v])));
        }
        v
    }

    /// `n` layers of a variant holding a dict whose single value is the next
    /// layer.
    fn variant_dict_chain(n: usize) -> Value {
        let mut v = Value::Byte(0);
        for _ in 0..n {
            let vs = v.signature();
            v = Value::Variant(Box::new(Value::Dict("s".into(), vs, vec![(s("k"), v)])));
        }
        v
    }

    #[test]
    fn structs_and_dict_entries_count_towards_the_depth_limit() {
        // Two levels per layer (variant + container): 32 layers reach the
        // limit of 64, 33 pass it.
        for chain in [variant_struct_chain, variant_dict_chain] {
            assert!(read_value(&chain(32)).is_ok());
            assert!(matches!(
                read_value(&chain(33)),
                Err(Error::Malformed("containers nested too deep"))
            ));
        }
    }

    #[test]
    fn signature_nesting_limit_is_32_for_structs_and_arrays() {
        let structs = |n: usize| format!("{}y{}", "(".repeat(n), ")".repeat(n));
        assert!(parse_all(&structs(32)).is_ok());
        assert!(matches!(
            parse_all(&structs(33)),
            Err(Error::Malformed("signature too deep"))
        ));
        // A dict entry counts for two levels: its array and its braces.
        // Value side: 30 arrays fit, 31 do not.
        let value = |n: usize| format!("a{{s{}y}}", "a".repeat(n));
        assert!(parse_all(&value(30)).is_ok());
        assert!(matches!(
            parse_all(&value(31)),
            Err(Error::Malformed("signature too deep"))
        ));
        // Key side: a key nested past the limit is reported as too deep,
        // before it could be reported as a non-basic key.
        let key = |n: usize| format!("a{{{}y{}y}}", "(".repeat(n), ")".repeat(n));
        assert!(matches!(
            parse_all(&key(30)),
            Err(Error::Malformed("dict key is not a basic type"))
        ));
        assert!(matches!(
            parse_all(&key(31)),
            Err(Error::Malformed("signature too deep"))
        ));
    }
}
