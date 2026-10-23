//! D-Bus marshalling: typed values to and from the wire format.

use crate::Error;

/// Variants nested this deep or deeper are rejected.
const MAX_VARIANT_DEPTH: usize = 64;
/// Maximum container nesting of one signature.
const MAX_SIG_DEPTH: usize = 32;

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
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    le: bool,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], little_endian: bool) -> Reader<'a> {
        Reader {
            buf,
            pos: 0,
            le: little_endian,
        }
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

    fn read_ty(&mut self, ty: &Ty, vdepth: usize) -> Result<Value, Error> {
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
                if vdepth + 1 >= MAX_VARIANT_DEPTH {
                    return Err(Error::Malformed("variants nested too deep"));
                }
                let n = self.take(1)?[0] as usize;
                let sig = self.text(n)?;
                let mut pos = 0;
                let inner = parse_type(&sig, &mut pos, 0)?;
                if pos != sig.len() {
                    return Err(Error::Malformed("variant signature is not one type"));
                }
                Value::Variant(Box::new(self.read_ty(&inner, vdepth + 1)?))
            }
            Ty::Array(es, elem) => {
                let end = self.array_end(elem_align(elem))?;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.read_ty(elem, vdepth)?);
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
                    out.push(self.read_ty(f, vdepth)?);
                }
                Value::Struct(out)
            }
            Ty::Dict(ks, vs, k, v) => {
                let end = self.array_end(8)?;
                let mut entries = Vec::new();
                while self.pos < end {
                    self.align(8)?;
                    let key = self.read_ty(k, vdepth)?;
                    let val = self.read_ty(v, vdepth)?;
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
        err("a{vy}", &[0, 0, 0, 0]); // container dict key
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
        let ok = nested_variants(10);
        assert!(Reader::new(&ok, true).read("v").is_ok());
        for levels in [64, 100, 10_000] {
            err("v", &nested_variants(levels));
        }
    }
}
