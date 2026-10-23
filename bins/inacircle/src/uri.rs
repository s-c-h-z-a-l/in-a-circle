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
/// Most URIs taken from a drop; later ones are ignored.
pub const MAX_URIS: usize = 1024;

/// The local paths of a URI list, in order.
pub fn parse(list: &[u8]) -> Vec<PathBuf> {
    list.split(|&b| b == b'\n')
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
    fn local_file_uris_decode_to_paths() {
        let list = b"# from a file manager\r\nfile:///home/u/a%20b.txt\r\n\
            file://localhost/tmp/%C3%A1.md\nhttps://example.com/x\n\
            file://other-host/etc/passwd\nfile:relative\nfile:///bad%zz\n\
            file:///nul%00x\nfile:///last";
        assert_eq!(
            parse(list),
            [
                PathBuf::from("/home/u/a b.txt"),
                PathBuf::from("/tmp/á.md"),
                PathBuf::from("/last"),
            ]
        );
        assert!(parse(b"").is_empty());
        assert!(parse(b"#only\r\n\r\n").is_empty());
    }
}
