//! The SRT reader's markup rules. The SRT reader, the SRT writer and the fuzz
//! oracle use them (the oracle includes this file), so each rule has one
//! source.

/// Whether a `<` at the start of `tail` opens a tag for the SRT reader: `<`,
/// then an optional `/`, then an ASCII letter. The reader also needs a `>` later
/// in the line, and a `<` without one is text. The writer does not look for the
/// `>` (it writes the line in pieces), so it treats every `<` that opens a tag
/// as one: a look-alike for a `<` that was only text is harmless.
pub(crate) fn opens_tag(tail: &str) -> bool {
    let Some(body) = tail.strip_prefix('<') else {
        return false;
    };
    let body = body.strip_prefix('/').unwrap_or(body);
    body.starts_with(|c: char| c.is_ascii_alphabetic())
}

/// Whether a `{` at the start of `tail` opens an override block for the SRT
/// reader: `{` immediately followed by `\`. As with `<`, the reader also needs
/// a `}` later in the line, and the writer treats every such `{` as one.
pub(crate) fn opens_override(tail: &str) -> bool {
    tail.starts_with("{\\")
}

/// The look-alike for the character at byte `at` of `text`, when that character
/// opens markup for the SRT reader: `<` becomes U+FF1C and `{` becomes U+FF5B,
/// neither of which any reader takes for markup. `None` when it opens none.
pub(crate) fn look_alike_at(text: &str, at: usize) -> Option<char> {
    let tail = &text[at..];
    if opens_tag(tail) {
        Some('\u{ff1c}')
    } else if opens_override(tail) {
        Some('\u{ff5b}')
    } else {
        None
    }
}
