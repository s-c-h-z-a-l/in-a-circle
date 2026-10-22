//! Content classifier: what is this file, as far as the engine can tell.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub mime: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// A regular file (after following links) and what it is.
    File(Kind),
    /// Directory, device, FIFO, socket or broken link: never opened.
    NotRegular,
}

/// How much of the file decides the type.
const HEAD: usize = 4096;

/// Binary signatures: (prefix, mime the engine knows, label).
const MAGIC: &[(&[u8], Option<&str>, &str)] = &[
    (b"%PDF-", None, "PDF"),
    (b"\x89PNG\r\n\x1a\n", None, "PNG"),
    (b"\xff\xd8\xff", None, "JPEG"),
    (b"GIF87a", None, "GIF"),
    (b"GIF89a", None, "GIF"),
    (b"PK\x03\x04", None, "ZIP"),
    (b"\x1f\x8b", None, "gzip"),
    (
        b"INACIRCLE-TEST\n",
        Some("application/x-inacircle-test"),
        "test",
    ),
];

/// Classify by content (first 4096 bytes); the extension only refines text.
/// Follows symlinks; anything that is not a regular file is not read.
pub fn classify(path: &Path) -> io::Result<Classified> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Ok(Classified::NotRegular),
        Err(e) if e.kind() == io::ErrorKind::NotFound && path.is_symlink() => {
            return Ok(Classified::NotRegular);
        }
        Err(e) => return Err(e),
    }
    let mut head = Vec::with_capacity(HEAD);
    File::open(path)?.take(HEAD as u64).read_to_end(&mut head)?;

    for (prefix, mime, label) in MAGIC {
        if head.starts_with(prefix) {
            return Ok(Classified::File(Kind {
                mime: mime.map(str::to_owned),
                label: (*label).to_owned(),
            }));
        }
    }

    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
    let kind = if is_text(&head) {
        match ext.as_deref() {
            None | Some("txt" | "text" | "log" | "1st" | "me" | "readme" | "diz" | "nfo") => Kind {
                mime: Some("text/plain".into()),
                label: "Plain text".into(),
            },
            Some("md" | "markdown" | "mdown" | "mkd") => Kind {
                mime: Some("text/markdown".into()),
                label: "Markdown".into(),
            },
            Some(e) => Kind {
                mime: None,
                label: format!(".{e} text"),
            },
        }
    } else {
        Kind {
            mime: None,
            label: match ext {
                Some(e) => format!(".{e} file"),
                None => "file".into(),
            },
        }
    };
    Ok(Classified::File(kind))
}

/// No NUL and valid UTF-8. A multi-byte sequence cut by the end of a full
/// buffer is allowed; at the end of a shorter (whole) file it is not.
fn is_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() == HEAD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let p = std::env::temp_dir().join(format!("engine-cls-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn kind(p: &Path) -> (Option<String>, String) {
        match classify(p).unwrap() {
            Classified::File(k) => (k.mime, k.label),
            Classified::NotRegular => panic!("not regular: {}", p.display()),
        }
    }
    fn is(mime: Option<&str>, label: &str) -> (Option<String>, String) {
        (mime.map(str::to_owned), label.to_owned())
    }

    #[test]
    fn plain_aliases_and_no_extension() {
        let d = Dir::new("plain");
        for n in [
            "a.TXT", "README", "x.nfo", "b.text", "c.log", "d.1st", "e.me", "f.readme", "g.diz",
        ] {
            let p = d.file(n, b"hello\n");
            assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"), "{n}");
        }
    }

    #[test]
    fn markdown_aliases() {
        let d = Dir::new("md");
        for n in ["a.md", "b.markdown", "c.MDOWN", "d.mkd"] {
            let p = d.file(n, b"# hi\n");
            assert_eq!(kind(&p), is(Some("text/markdown"), "Markdown"), "{n}");
        }
    }

    #[test]
    fn utf8_cut_at_4096_is_text() {
        let d = Dir::new("cut");
        let mut b = vec![b'a'; 4095];
        b.extend_from_slice("€".as_bytes()); // bytes 4095..4098, cut after the first
        let p = d.file("x.txt", &b);
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    #[test]
    fn nul_is_not_text() {
        let d = Dir::new("nul");
        let p = d.file("x.txt", b"ab\0cd");
        assert_eq!(kind(&p), is(None, ".txt file"));
        let p = d.file("noext", b"ab\0cd");
        assert_eq!(kind(&p), is(None, "file"));
    }

    #[test]
    fn invalid_utf8_is_not_text() {
        let d = Dir::new("bad");
        let p = d.file("x.txt", b"ab\xffcd");
        assert_eq!(kind(&p), is(None, ".txt file"));
        // A bad sequence in the middle is not forgiven just because the
        // buffer also ends in a cut one.
        let mut b = b"\xc3\x28".to_vec();
        b.extend(std::iter::repeat_n(b'a', 4094));
        let p = d.file("y.txt", &b);
        assert_eq!(kind(&p), is(None, ".txt file"));
    }

    #[test]
    fn magic_beats_extension() {
        let d = Dir::new("magic");
        let p = d.file("kép.txt", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        assert_eq!(kind(&p), is(None, "PNG"));
        let cases: [(&str, &[u8], &str); 5] = [
            ("a.bin", b"%PDF-1.7\n", "PDF"),
            ("b.bin", b"\xff\xd8\xff\xe0", "JPEG"),
            ("c.bin", b"GIF89a..", "GIF"),
            ("d.bin", b"PK\x03\x04..", "ZIP"),
            ("e.bin", b"\x1f\x8b\x08", "gzip"),
        ];
        for (n, b, l) in cases {
            let p = d.file(n, b);
            assert_eq!(kind(&p), is(None, l), "{n}");
        }
        let p = d.file("f.bin", b"GIF87a");
        assert_eq!(kind(&p), is(None, "GIF"));
    }

    #[test]
    fn test_magic() {
        let d = Dir::new("tm");
        let p = d.file("t.dat", b"INACIRCLE-TEST\nbody");
        assert_eq!(kind(&p), is(Some("application/x-inacircle-test"), "test"));
    }

    #[test]
    fn other_text_extension() {
        let d = Dir::new("rs");
        let p = d.file("x.rs", b"fn main() {}\n");
        assert_eq!(kind(&p), is(None, ".rs text"));
    }

    #[test]
    fn unknown_binary_and_empty() {
        let d = Dir::new("bin");
        let p = d.file("x.bin", b"\x00\x01\x02");
        assert_eq!(kind(&p), is(None, ".bin file"));
        // An empty file has no NUL and is valid UTF-8: text by extension.
        let p = d.file("e.txt", b"");
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    #[test]
    fn directory_and_fifo_and_broken_link_are_not_regular() {
        let d = Dir::new("nr");
        let sub = d.0.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(matches!(classify(&sub).unwrap(), Classified::NotRegular));

        let fifo = d.0.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0);
        // Must return without blocking (no open/read on a FIFO).
        assert!(matches!(classify(&fifo).unwrap(), Classified::NotRegular));

        let link = d.0.join("broken");
        std::os::unix::fs::symlink(d.0.join("missing"), &link).unwrap();
        assert!(matches!(classify(&link).unwrap(), Classified::NotRegular));
    }

    #[test]
    fn missing_file_is_an_error() {
        let d = Dir::new("miss");
        assert!(classify(&d.0.join("nope")).is_err());
    }

    #[test]
    fn symlink_to_file_follows() {
        let d = Dir::new("link");
        let t = d.file("real.md", b"# x");
        let l = d.0.join("alias.bin");
        std::os::unix::fs::symlink(&t, &l).unwrap();
        // Content from the target; extension from the link name.
        assert_eq!(kind(&l), is(None, ".bin text"));
        let l2 = d.0.join("alias.md");
        std::os::unix::fs::symlink(&t, &l2).unwrap();
        assert_eq!(kind(&l2), is(Some("text/markdown"), "Markdown"));
    }
}
