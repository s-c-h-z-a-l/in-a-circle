//! `text/uri-list` (RFC 2483) to local paths.
//!
//! Lines end in CRLF or LF; a line starting with `#` is a comment. Only
//! `file://` URIs with an empty or `localhost` host count; `%XX` decodes to
//! bytes. Anything else, a bad escape, or a NUL in the path drops that
//! entry alone.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// Most bytes read from a drop.
pub const MAX_BYTES: usize = 64 * 1024;
/// Most URIs (comments and blank lines aside) looked at in a drop, dropped
/// entries included; later ones are ignored. The path is kept as given:
/// non-UTF-8 bytes and `..` stay, as on the command line.
pub const MAX_URIS: usize = 1024;

/// The local paths of a URI list, in order.
pub fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    data.split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty() && !l.starts_with(b"#"))
        .take(MAX_URIS)
        .filter_map(path_of)
        .collect()
}

fn path_of(uri: &[u8]) -> Option<PathBuf> {
    let rest = uri.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (host, path) = rest.split_at(slash);
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
        // At most MAX_URIS entries.
        let many: String = (0..MAX_URIS + 1)
            .map(|i| format!("file:///f{i}\r\n"))
            .collect();
        let paths = parse_uri_list(many.as_bytes());
        assert_eq!(paths.len(), MAX_URIS);
        assert_eq!(
            paths.last(),
            Some(&PathBuf::from(format!("/f{}", MAX_URIS - 1)))
        );
    }
}
