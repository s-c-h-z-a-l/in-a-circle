//! Line-based, tab-separated format shared by action manifests, the manifest
//! cache, history and action event lines.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Bare(String),
    Pair(String, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub msg: String,
}

impl Line {
    pub fn new(kind: &str) -> Line {
        Line {
            kind: kind.to_owned(),
            items: Vec::new(),
        }
    }

    /// Appends a bare value. It must not contain `=`, or it would read back
    /// as a pair; that is a programming error (debug assertion).
    pub fn bare(mut self, v: &str) -> Line {
        debug_assert!(!v.contains('='), "bare value must not contain '='");
        self.items.push(Item::Bare(v.to_owned()));
        self
    }

    /// Appends a `key=value` pair. The key must not contain `=`: the split
    /// happens at the first unescaped `=` and keys have no escape for it, so
    /// such a key is a programming error (debug assertion).
    pub fn pair(mut self, k: &str, v: &str) -> Line {
        debug_assert!(!k.contains('='), "key must not contain '='");
        self.items.push(Item::Pair(k.to_owned(), v.to_owned()));
        self
    }

    /// Value of the first pair with this key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.items.iter().find_map(|i| match i {
            Item::Pair(k, v) if k == key => Some(v.as_str()),
            _ => None,
        })
    }

    /// The `i`-th bare value, counting bare values only.
    pub fn bare_at(&self, i: usize) -> Option<&str> {
        self.items
            .iter()
            .filter_map(|i| match i {
                Item::Bare(v) => Some(v.as_str()),
                _ => None,
            })
            .nth(i)
    }
}

pub fn header(magic: &str, version: u32) -> String {
    format!("{magic} {version}\n")
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(o) => return Err(format!("unknown escape \\{o}")),
            None => return Err("dangling backslash".to_owned()),
        }
    }
    Ok(out)
}

/// Serializes one line: tab-separated, escaped, terminated by `\n`.
pub fn write_line(line: &Line) -> String {
    let mut out = String::new();
    escape_into(&mut out, &line.kind);
    for item in &line.items {
        out.push('\t');
        match item {
            Item::Bare(v) => escape_into(&mut out, v),
            Item::Pair(k, v) => {
                escape_into(&mut out, k);
                out.push('=');
                escape_into(&mut out, v);
            }
        }
    }
    out.push('\n');
    out
}

fn parse_at(s: &str, no: usize) -> Result<Line, Error> {
    let err = |msg: String| Error { line: no, msg };
    let mut fields = s.split('\t');
    let kind = unescape(fields.next().unwrap_or("")).map_err(err)?;
    if kind.is_empty() {
        return Err(err("empty kind".to_owned()));
    }
    let mut line = Line {
        kind,
        items: Vec::new(),
    };
    for f in fields {
        // Split before unescaping: only a raw `=` separates key from value.
        let item = match f.split_once('=') {
            Some((k, v)) => Item::Pair(unescape(k).map_err(err)?, unescape(v).map_err(err)?),
            None => Item::Bare(unescape(f).map_err(err)?),
        };
        line.items.push(item);
    }
    Ok(line)
}

/// Parses a header line plus body. Blank and `#` lines are skipped; line
/// numbers in errors are 1-based and count skipped lines.
pub fn parse(src: &str, magic: &str) -> Result<(u32, Vec<Line>), Error> {
    let mut lines = src.lines();
    let err = |msg: String| Error { line: 1, msg };
    let head = lines
        .next()
        .ok_or_else(|| err("missing header".to_owned()))?;
    let (m, v) = head
        .split_once(' ')
        .ok_or_else(|| err("malformed header".to_owned()))?;
    if m != magic {
        return Err(err(format!("bad magic {m:?}, expected {magic:?}")));
    }
    let version = v.parse().map_err(|_| err(format!("bad version {v:?}")))?;
    let mut out = Vec::new();
    for (i, l) in lines.enumerate() {
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        out.push(parse_at(l, i + 2)?);
    }
    Ok((version, out))
}

/// Parses a single headerless line (e.g. an event). Blank and comment lines
/// are errors here. A trailing newline is tolerated.
pub fn parse_line(s: &str) -> Result<Line, Error> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    if s.is_empty() || s.starts_with('#') {
        return Err(Error {
            line: 1,
            msg: "blank or comment line".to_owned(),
        });
    }
    parse_at(s, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_escapes() {
        let line = Line::new("action")
            .pair("cmd", "a\tb\nc\\d\re=f x -> y")
            .bare("plain\\")
            .pair("k", "");
        let text = write_line(&line);
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 1);
        let src = format!("{}{}", header("m", 3), text);
        let (v, lines) = parse(&src, "m").unwrap();
        assert_eq!(v, 3);
        assert_eq!(lines, vec![line.clone()]);
        assert_eq!(parse_line(text.trim_end_matches('\n')).unwrap(), line);
    }

    #[test]
    fn header_version() {
        assert_eq!(header("inacircle-manifest", 0), "inacircle-manifest 0\n");
        let (v, l) = parse("inacircle-manifest 0\n", "inacircle-manifest").unwrap();
        assert_eq!((v, l.len()), (0, 0));
        let e = parse("other 0\n", "inacircle-manifest").unwrap_err();
        assert_eq!(e.line, 1);
        assert_eq!(parse("", "m").unwrap_err().line, 1);
        assert_eq!(parse("m x\n", "m").unwrap_err().line, 1);
    }

    #[test]
    fn pair_splits_at_first_eq() {
        let l = parse_line("kind\ta=b=c\tbare").unwrap();
        assert_eq!(l.kind, "kind");
        assert_eq!(l.get("a"), Some("b=c"));
        assert_eq!(l.get("zz"), None);
        assert_eq!(l.bare_at(0), Some("bare"));
        assert_eq!(l.bare_at(1), None);
    }

    #[test]
    fn comments_and_blanks_skipped() {
        let src = "m 1\n\n# note\nx\ta=1\n\ny\n";
        let (_, lines) = parse(src, "m").unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].kind, "y");
    }

    #[test]
    fn unknown_escape_is_error() {
        let e = parse("m 1\nx\ta=\\q\n", "m").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(parse_line("x\tdangling\\").is_err());
    }

    #[test]
    fn parse_line_rejects_blank_and_comment() {
        assert!(parse_line("").is_err());
        assert!(parse_line("# c").is_err());
    }
}
