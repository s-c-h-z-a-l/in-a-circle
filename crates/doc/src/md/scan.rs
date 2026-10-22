//! Byte scanners shared by the block and inline parsers: HTML tags and the
//! parts of links (label, destination, title). All of them split only at
//! ASCII bytes, so slices of valid UTF-8 stay valid UTF-8.

pub(super) fn peek(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

pub(super) fn is_space_or_tab(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// Spaces, tabs and up to one line ending (whitespace inside a tag).
fn skip_tag_space(s: &[u8], mut i: usize) -> usize {
    while is_space_or_tab(peek(s, i)) {
        i += 1;
    }
    if peek(s, i) == b'\r' {
        i += 1;
        if peek(s, i) == b'\n' {
            i += 1;
        }
    } else if peek(s, i) == b'\n' {
        i += 1;
    }
    while is_space_or_tab(peek(s, i)) {
        i += 1;
    }
    i
}

fn tag_name_end(s: &[u8], i: usize) -> Option<usize> {
    if !peek(s, i).is_ascii_alphabetic() {
        return None;
    }
    let n = s[i..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-')
        .count();
    Some(i + n)
}

/// A complete HTML open tag at the start of `s` (which begins with `<`):
/// its length. Shared with raw inline HTML.
pub(super) fn open_tag(s: &[u8]) -> Option<usize> {
    let mut i = tag_name_end(s, 1)?;
    loop {
        let j = skip_tag_space(s, i);
        let c = peek(s, j);
        if j == i || !(c.is_ascii_alphabetic() || c == b'_' || c == b':') {
            i = j;
            break;
        }
        let k = j + s[j..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(c))
            .count();
        let v = skip_tag_space(s, k);
        if peek(s, v) != b'=' {
            i = k;
            continue;
        }
        let w = skip_tag_space(s, v + 1);
        i = match peek(s, w) {
            q @ (b'"' | b'\'') => {
                let close = s[w + 1..].iter().position(|&c| c == q)?;
                w + 1 + close + 1
            }
            _ => {
                let n = s[w..]
                    .iter()
                    .take_while(|c| !b" \t\n\r\"'=<>`".contains(c))
                    .count();
                if n == 0 {
                    return None;
                }
                w + n
            }
        };
    }
    if peek(s, i) == b'/' {
        i += 1;
    }
    (peek(s, i) == b'>').then_some(i + 1)
}

/// A complete HTML closing tag at the start of `s`: its length.
pub(super) fn close_tag(s: &[u8]) -> Option<usize> {
    if !s.starts_with(b"</") {
        return None;
    }
    let i = skip_tag_space(s, tag_name_end(s, 2)?);
    (peek(s, i) == b'>').then_some(i + 1)
}

pub(super) fn skip_spaces(s: &[u8], mut i: usize) -> usize {
    while is_space_or_tab(peek(s, i)) {
        i += 1;
    }
    i
}

/// Spaces and tabs with at most one line ending among them.
pub(super) fn skip_spnl(s: &[u8], i: usize) -> usize {
    let mut i = skip_spaces(s, i);
    if peek(s, i) == b'\n' {
        i = skip_spaces(s, i + 1);
    }
    i
}

/// Link label `[...]` at `i`: (end after `]`, the text inside). At most 999
/// characters, no unescaped brackets. Shared with the inline parser.
pub(super) fn link_label(s: &[u8], i: usize) -> Option<(usize, &[u8])> {
    if peek(s, i) != b'[' {
        return None;
    }
    let mut j = i + 1;
    let mut chars = 0;
    loop {
        let c = *s.get(j)?;
        match c {
            b']' => return Some((j + 1, &s[i + 1..j])),
            b'[' => return None,
            b'\\' if s.get(j + 1).is_some_and(u8::is_ascii_punctuation) => {
                j += 2;
                chars += 2;
            }
            _ => {
                j += 1;
                if c & 0xC0 != 0x80 {
                    chars += 1;
                }
            }
        }
        if chars > 999 {
            return None;
        }
    }
}

/// Link destination at `i`: (end, raw text without `<>`). `<...>` may be
/// empty; a bare destination is non-empty, has no spaces or controls and
/// balanced parentheses. Shared with the inline parser.
pub(super) fn link_destination(s: &[u8], i: usize) -> Option<(usize, &[u8])> {
    if peek(s, i) == b'<' {
        let mut j = i + 1;
        loop {
            match *s.get(j)? {
                b'>' => return Some((j + 1, &s[i + 1..j])),
                b'\\' if s.get(j + 1).is_some_and(u8::is_ascii_punctuation) => j += 2,
                b'\n' | b'\r' | b'<' => return None,
                _ => j += 1,
            }
        }
    }
    let mut j = i;
    let mut depth = 0u32;
    while let Some(&c) = s.get(j) {
        match c {
            b'\\' if s.get(j + 1).is_some_and(u8::is_ascii_punctuation) => j += 2,
            b'(' => {
                depth += 1;
                if depth > 32 {
                    return None;
                }
                j += 1;
            }
            b')' if depth == 0 => break,
            b')' => {
                depth -= 1;
                j += 1;
            }
            c if c <= b' ' || c == 0x7f => break,
            _ => j += 1,
        }
    }
    (j > i && depth == 0).then(|| (j, &s[i..j]))
}

/// Link title (`"..."`, `'...'` or `(...)`) at `i`: (end, raw text inside).
/// Shared with the inline parser.
pub(super) fn link_title(s: &[u8], i: usize) -> Option<(usize, &[u8])> {
    let close = match peek(s, i) {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let mut j = i + 1;
    loop {
        match *s.get(j)? {
            b'\\' if s.get(j + 1).is_some_and(u8::is_ascii_punctuation) => j += 2,
            c if c == close => return Some((j + 1, &s[i + 1..j])),
            b'(' if close == b')' => return None,
            _ => j += 1,
        }
    }
}

pub(super) fn utf8(b: &[u8]) -> &str {
    // Every scanner splits only at ASCII bytes, so slices of valid UTF-8
    // stay valid.
    std::str::from_utf8(b).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(open_tag(b"<a href='x' b=c d>"), Some(18));
        assert_eq!(open_tag(b"<a\n b=\"1\"/>z"), Some(11));
        assert_eq!(open_tag(b"<a b=>"), None);
        assert_eq!(open_tag(b"<a\n\nb>"), None);
        assert_eq!(open_tag(b"<1>"), None);
        assert_eq!(open_tag(b"<a b='x>"), None);
        assert_eq!(close_tag(b"</a-1 >"), Some(7));
        assert_eq!(close_tag(b"</a b>"), None);
    }
}
