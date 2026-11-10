//! The SRT reader's markup rules. The SRT reader, the SRT writer and the fuzz
//! oracle use them (the oracle includes this file), so each rule has one
//! source.

/// Whether a `<` at the start of `tail` opens a tag for the SRT reader: `<`, an
/// optional `/`, then an ASCII letter, and the tag name (up to the first space
/// or `>`) holds only ASCII letters, digits, `_` and `/`. Any other `<` is
/// text: a URL, an address or a dotted name stays (ffmpeg agrees on these).
/// [`takes_tag`] adds the rest of the reader's rule.
pub(crate) fn opens_tag(tail: &str) -> bool {
    let Some(body) = tail.strip_prefix('<') else {
        return false;
    };
    let body = body.strip_prefix('/').unwrap_or(body);
    body.starts_with(|c: char| c.is_ascii_alphabetic())
        && body
            .bytes()
            .take_while(|&b| b != b' ' && b != b'>')
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
}

/// Whether a `{` at the start of `tail` opens an override block for the SRT
/// reader: `{` immediately followed by `\`. [`takes_override`] adds the `}`.
pub(crate) fn opens_override(tail: &str) -> bool {
    tail.starts_with("{\\")
}

/// Whether the `<` at the start of `tail` is a tag for the SRT reader: it opens
/// one ([`opens_tag`]) and the first `<` or `>` after it, `next` (`None` when
/// the line has neither), is a `>`. A `<` before that `>` makes this one text.
pub(crate) fn takes_tag(tail: &str, next: Option<u8>) -> bool {
    next == Some(b'>') && opens_tag(tail)
}

/// Whether the `{` at the start of `tail` is an override block for the SRT
/// reader: it opens one ([`opens_override`]) and a `}` follows it in the line,
/// which is `closed`.
pub(crate) fn takes_override(tail: &str, closed: bool) -> bool {
    closed && opens_override(tail)
}

/// The look-alikes the SRT reader would need changed in a line's plain text
/// (the speaker's `Name: ` prefix included), as byte offsets in order: U+FF1C
/// for each `<` that [`takes_tag`] and U+FF5B for each `{` that
/// [`takes_override`].
///
/// A `<` is judged by the first `<` or `>` after it, and a look-alike is not a
/// `<`. So the line is judged from its end back: each `<` sees the brackets as
/// they will stand after the rewrites to its right, and a rewritten `<` no
/// longer stops the search. The writer's own `<i>` and `</i>` tags are not in
/// the plain text, and at a style boundary they add a `<` that stops a search
/// the plain text does not have: a `<` or `{\` the reader would keep as text
/// may be rewritten there. Accepted: deciding on the rendered line would need a
/// map back to the runs. The guarantee is the other half: nothing the reader
/// would take as markup is left unrewritten.
pub(crate) fn look_alikes(plain: &str) -> Vec<(usize, char)> {
    let last_close = plain.rfind('}');
    // The first `<` or `>` after the position, as it will stand in the line.
    let mut next = None;
    let mut looks = Vec::new();
    for (at, c) in plain.char_indices().rev() {
        match c {
            '>' => next = Some(b'>'),
            '<' if takes_tag(&plain[at..], next) => looks.push((at, '\u{ff1c}')),
            '<' => next = Some(b'<'),
            '{' if takes_override(&plain[at..], last_close.is_some_and(|close| close > at)) => {
                looks.push((at, '\u{ff5b}'));
            }
            _ => {}
        }
    }
    looks.reverse();
    looks
}
