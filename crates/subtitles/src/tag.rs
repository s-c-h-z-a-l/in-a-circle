//! The SRT reader's tag rule. The SRT reader and the SRT writer both ask it, and
//! the fuzz oracle includes this file, so the rule has one source.

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
