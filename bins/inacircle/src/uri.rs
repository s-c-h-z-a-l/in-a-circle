//! `text/uri-list` (RFC 2483) to local paths.
//!
//! Lines end in CRLF or LF; a line starting with `#` is a comment. Only
//! `file://` URIs with an empty or `localhost` host count (scheme and host
//! in any case); a raw `?query` or `#fragment` is cut off; `%XX` decodes to
//! bytes, so an encoded `?` or `#` stays in the path. Anything else, a bad
//! escape, or a NUL in the path drops that entry alone.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// Most bytes read from a drop: 1 MiB, the most an action invocation
/// accepts as its path list (`action::INPUT_LIST_BYTES`; a test keeps the
/// two equal; a literal here because the fuzz target includes this file
/// on its own). A list is turned into that invocation, so reading more
/// than it takes would only fail later. Over it the wheel says "too many
/// files" and offers nothing.
pub const MAX_BYTES: usize = 1_048_576;
/// Most URIs (comments and blank lines aside) in a drop, dropped entries
/// included: the invocation's input count (`action::INPUT_COUNT`). More
/// than this and the wheel says "too many files" and offers nothing,
/// rather than silently working on a truncated selection. The path is kept
/// as given: non-UTF-8 bytes and `..` stay, as on the command line.
pub const MAX_URIS: usize = 1024;

/// The non-comment, non-blank lines of a list.
fn entries(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|&b| b == b'\n')
        .filter(|l| is_entry(l))
        .map(strip_cr)
}

/// Whether the list has more than `MAX_URIS` entries.
pub fn too_many(data: &[u8]) -> bool {
    entries(data).nth(MAX_URIS).is_some()
}

/// The local paths of a URI list, in order; at most `MAX_URIS` entries
/// are looked at (the wheel refuses a longer list before this). The wheel
/// uses [`parse_uri_list_owned`]; this one is the reference the tests and
/// the fuzz target check it against.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    entries(data).take(MAX_URIS).filter_map(path_of).collect()
}

/// How many bytes of a consumed list may stay allocated past what is
/// still unparsed before they are given back: 64 KiB, 1/16 of `MAX_BYTES`.
/// The list at its limit is then released in at most 16 steps (on glibc a
/// `realloc` that shrinks is done in place: an `mremap` for a large
/// buffer, a split of the chunk on the heap, no copy), and the list and its
/// paths overlap by at most this much, 1.3% of the 5,000,000 B memory
/// budget. Smaller costs more calls for no measurable gain; larger keeps
/// more of the list beside its paths.
#[cfg(not(fuzzing))]
const RELEASE_STEP: usize = 64 * 1024;

/// The fuzz build's step: 64 B, so that every fuzz input a few hundred
/// bytes long reaches the shrink (libFuzzer's inputs stay far below 64 KiB
/// by default, and the corpus is small). The step moves no logic: it only
/// decides when the same `shrink_to_fit` runs.
#[cfg(fuzzing)]
const RELEASE_STEP: usize = 64;

/// [`parse_uri_list`] on a list it consumes: the same paths in the same
/// order, but the list is decoded from its end and its bytes are given
/// back as they are decoded, so at no point are the whole list and all of
/// its paths held together (at the 1 MiB limit that pair alone would be
/// about 2 MiB, 40% of the wheel's memory budget).
pub fn parse_uri_list_owned(mut data: Vec<u8>) -> Vec<PathBuf> {
    // The same entries as `parse_uri_list`: the list ends after its
    // `MAX_URIS`-th entry.
    let mut seen = 0;
    let mut start = 0;
    for line in data.split(|&b| b == b'\n') {
        let end = start + line.len();
        if is_entry(line) {
            seen += 1;
            if seen == MAX_URIS {
                data.truncate(end);
                break;
            }
        }
        start = end + 1;
    }
    let mut paths = Vec::new();
    while !data.is_empty() {
        let at = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if is_entry(&data[at..])
            && let Some(p) = path_of(strip_cr(&data[at..]))
        {
            paths.push(p);
        }
        // The line and its `\n` are done.
        data.truncate(at.saturating_sub(1));
        if data.capacity() - data.len() >= RELEASE_STEP {
            data.shrink_to_fit();
        }
    }
    paths.reverse();
    paths
}

fn strip_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Whether a line (its `\n` cut off) is a URI: not blank, not a comment.
fn is_entry(line: &[u8]) -> bool {
    let l = strip_cr(line);
    !l.is_empty() && !l.starts_with(b"#")
}

fn path_of(uri: &[u8]) -> Option<PathBuf> {
    const SCHEME: &[u8] = b"file://";
    let (scheme, rest) = uri.split_at_checked(SCHEME.len())?;
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (host, path) = rest.split_at(slash);
    // A raw `?` or `#` starts the query or fragment, not part of the path.
    let path = path.split(|&b| b == b'?' || b == b'#').next()?;
    if !host.is_empty() && !host.eq_ignore_ascii_case(b"localhost") {
        return None;
    }
    let raw = decode(path)?;
    if raw.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(raw)))
}

fn decode(s: &[u8]) -> Option<Vec<u8>> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            let h = hex(*s.get(i + 1)?)?;
            let l = hex(*s.get(i + 2)?)?;
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_edge_cases() {
        let list = b"# from a file manager\r\nfile:///home/u/a%20b.txt\r\n\
            file://localhost/tmp/%C3%A1.md\nhttps://example.com/x\n\
            file://other-host/etc/passwd\nfile:relative\nfile:/one-slash\n\
            file://localhost\nfile:///bad%zz\nfile:///cut%2\n\
            file:///nul%00x\nfile:///last";
        assert_eq!(
            parse_uri_list(list),
            [
                PathBuf::from("/home/u/a b.txt"),
                PathBuf::from("/tmp/á.md"),
                PathBuf::from("/last"),
            ]
        );
        assert!(parse_uri_list(b"").is_empty());
        assert!(parse_uri_list(b"#only\r\n\r\n").is_empty());
        // Not UTF-8: kept as bytes. `..` is kept as given, as on the
        // command line (resolving it lexically is wrong across symlinks).
        assert_eq!(
            parse_uri_list(b"file:///x/%FF\xfe.txt\nfile:///a/../b\n"),
            [
                PathBuf::from(OsString::from_vec(b"/x/\xff\xfe.txt".to_vec())),
                PathBuf::from("/a/../b"),
            ]
        );
        // The scheme and host are case-insensitive; a raw query or
        // fragment is not part of the path (an encoded one is).
        assert_eq!(
            parse_uri_list(
                b"FILE:///up\nFile://LocalHost/mixed\nfile:///q?b=c\n\
                  file:///f#frag\nfile:///both?x#y\nfile:///enc%3Fa%23b\n\
                  http://x/?file:///no\n"
            ),
            [
                PathBuf::from("/up"),
                PathBuf::from("/mixed"),
                PathBuf::from("/q"),
                PathBuf::from("/f"),
                PathBuf::from("/both"),
                PathBuf::from("/enc?a#b"),
            ]
        );
        // At most MAX_URIS entries are parsed; `too_many` tells the rest.
        assert!(!too_many(
            (0..MAX_URIS)
                .map(|i| format!("file:///f{i}\n"))
                .collect::<String>()
                .as_bytes()
        ));
        let many: String = (0..MAX_URIS + 1)
            .map(|i| format!("file:///f{i}\r\n"))
            .collect();
        let paths = parse_uri_list(many.as_bytes());
        assert!(too_many(many.as_bytes()));
        assert_eq!(paths.len(), MAX_URIS);
        assert_eq!(
            paths.last(),
            Some(&PathBuf::from(format!("/f{}", MAX_URIS - 1)))
        );
    }

    /// The consuming parser gives what the borrowing one gives, on every
    /// list above and on one large enough to be released in steps.
    #[test]
    fn the_consuming_parse_matches() {
        let same = |list: &[u8]| {
            assert_eq!(
                parse_uri_list_owned(list.to_vec()),
                parse_uri_list(list),
                "{}",
                String::from_utf8_lossy(list)
            );
        };
        for list in [
            &b""[..],
            b"\n",
            b"\r\n",
            b"\n\n#c\n",
            b"file:///a",
            b"file:///a\n",
            b"\nfile:///a\r\n\r\nfile:///b\r",
            b"file:///a\r\r\nfile:///%zz\n#file:///c\nfile:///d%20e",
            b"# from a file manager\r\nfile:///home/u/a%20b.txt\r\n\
              file://localhost/tmp/%C3%A1.md\nhttps://example.com/x\n\
              file:///nul%00x\nfile:///last",
        ] {
            same(list);
        }
        // Past MAX_URIS (the cut counts comments and blanks out), and a
        // list of ~1 MiB, given back in RELEASE_STEP pieces.
        let many: String = (0..MAX_URIS + 3)
            .map(|i| format!("#c\n\nfile:///f{i}\r\n"))
            .collect();
        same(many.as_bytes());
        let long = "x".repeat(1000);
        let big: String = (0..MAX_URIS)
            .map(|i| format!("file:///{i}{long}\r\n"))
            .collect();
        assert!(big.len() > 8 * RELEASE_STEP && big.len() <= MAX_BYTES);
        same(big.as_bytes());
        assert_eq!(parse_uri_list_owned(big.into_bytes()).len(), MAX_URIS);
    }

    /// The read limits are the invocation's.
    #[test]
    fn limits_match_the_invocation() {
        assert_eq!(MAX_BYTES, action::INPUT_LIST_BYTES);
        assert_eq!(MAX_URIS, action::INPUT_COUNT);
    }
}
