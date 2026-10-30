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
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty() && !l.starts_with(b"#"))
}

/// Whether the list has more than `MAX_URIS` entries.
pub fn too_many(data: &[u8]) -> bool {
    entries(data).nth(MAX_URIS).is_some()
}

/// The local paths of a URI list, in order; at most `MAX_URIS` entries
/// are looked at (the wheel refuses a longer list before this).
pub fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    entries(data).take(MAX_URIS).filter_map(path_of).collect()
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

    /// The read limits are the invocation's.
    #[test]
    fn limits_match_the_invocation() {
        assert_eq!(MAX_BYTES, action::INPUT_LIST_BYTES);
        assert_eq!(MAX_URIS, action::INPUT_COUNT);
    }
}
