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

    /// Appends a bare value (any string; `=` is escaped on write).
    pub fn bare(mut self, v: &str) -> Line {
        self.items.push(Item::Bare(v.to_owned()));
        self
    }

    /// Appends a `key=value` pair (any strings; `=` in the key is escaped on
    /// write, in the value it may stay raw).
    pub fn pair(mut self, k: &str, v: &str) -> Line {
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

/// Escapes `s`. `eq` also escapes `=` (kinds, keys, bare values; pair values
/// may keep it raw). `lead` escapes a leading `#` so the line is not a comment.
fn escape_into(out: &mut String, s: &str, eq: bool, lead: bool) {
    for (i, c) in s.chars().enumerate() {
        match c {
            '=' if eq => out.push_str("\\="),
            '#' if lead && i == 0 => out.push_str("\\#"),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// Escapes `s` the way a bare item is written by `write_line`, for
/// line-per-value files (`unescape` is the inverse).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(&mut out, s, true, true);
    out
}

/// Reverses the escapes of `write_line` for a single field (for line-per-value files).
pub fn unescape(s: &str) -> Result<String, String> {
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
            Some('=') => out.push('='),
            Some('#') => out.push('#'),
            Some(o) => return Err(format!("unknown escape \\{o}")),
            None => return Err("dangling backslash".to_owned()),
        }
    }
    Ok(out)
}

/// Serializes one line: tab-separated, escaped, terminated by `\n`.
pub fn write_line(line: &Line) -> String {
    let mut out = String::new();
    escape_into(&mut out, &line.kind, true, true);
    for item in &line.items {
        out.push('\t');
        match item {
            Item::Bare(v) => escape_into(&mut out, v, true, false),
            Item::Pair(k, v) => {
                escape_into(&mut out, k, true, false);
                out.push('=');
                escape_into(&mut out, v, false, false);
            }
        }
    }
    out.push('\n');
    out
}

/// Splits at the first `=` not preceded by an escaping backslash.
fn split_pair(f: &str) -> Option<(&str, &str)> {
    let mut esc = false;
    for (i, c) in f.char_indices() {
        match c {
            _ if esc => esc = false,
            '\\' => esc = true,
            '=' => return Some((&f[..i], &f[i + 1..])),
            _ => {}
        }
    }
    None
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
        // Split before unescaping: only an unescaped `=` separates key from value.
        let item = match split_pair(f) {
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
    #[test]
    fn escape_round_trips_through_unescape() {
        for v in [
            "a\tb", "a\nb", "a=b", "#lead", "a\\b", "x -> y", "a#b", "\r",
        ] {
            let e = escape(v);
            assert!(!e.contains(['\t', '\n', '\r']), "{e:?}");
            assert!(!e.starts_with('#'), "{e:?}");
            assert_eq!(unescape(&e).unwrap(), v, "{e:?}");
        }
        assert_eq!(escape("#a=b\tc"), "\\#a\\=b\\tc");
    }

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
    fn eq_and_hash_roundtrip_in_bare_key_kind() {
        let line = Line::new("#k=ind")
            .bare("/tmp/a=b -> c\td")
            .pair("we=ird\\", "v=al")
            .bare("=");
        let text = write_line(&line);
        assert_eq!(parse_line(&text).unwrap(), line);
        let (_, lines) = parse(&format!("m 1\n{text}"), "m").unwrap();
        assert_eq!(lines, vec![line]);
    }

    #[test]
    fn empty_kind_is_error() {
        assert_eq!(parse_line("\ta=1").unwrap_err().line, 1);
        assert_eq!(parse("m 1\n\tx\n", "m").unwrap_err().line, 2);
    }

    #[test]
    fn parse_line_rejects_blank_and_comment() {
        assert!(parse_line("").is_err());
        assert!(parse_line("# c").is_err());
    }
}
