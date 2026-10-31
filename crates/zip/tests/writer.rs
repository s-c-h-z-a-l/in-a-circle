//! The stored-entry writer: bytes of the headers, rollback of failed
//! entries, limits, and a read-back by our reader and by Python's
//! `zipfile` and `unzip` (test-time tools, not dependencies).

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use zip::{Limits, Method, StoreWriter, WriteError, open};

const LIMITS: Limits = Limits {
    entries: 2000,
    entry_bytes: 64 << 20,
    ratio: 1100,
    name_bytes: 255,
};

/// An archive file path under the cargo test temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        Scratch(PathBuf::from(format!(
            "{}/zipw-{}-{}",
            env!("CARGO_TARGET_TMPDIR"),
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn create(&self) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.0)
            .unwrap()
    }
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.0).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A tool on `PATH`; a missing one skips, unless
/// `INACIRCLE_REQUIRE_PDF_TOOLS=1` makes it a failure.
fn have(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false);
    if !found {
        if std::env::var_os("INACIRCLE_REQUIRE_PDF_TOOLS").is_some_and(|v| v == "1") {
            panic!("{tool} is required (INACIRCLE_REQUIRE_PDF_TOOLS=1) but not installed");
        }
        eprintln!("skipping: {tool} is not installed");
    }
    found
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Python's checker: `testzip()` is clean, and every entry is method 0 with
/// the given name and bytes, in order. Prints `name method hex` per entry.
const CHECK_PY: &str = r#"import sys, zipfile
z = zipfile.ZipFile(sys.argv[1])
assert z.testzip() is None
for i in z.infolist():
    print(i.filename, i.compress_type, z.read(i).hex())
"#;

fn independent_check(archive: &Path, expected: &[(&str, &[u8])]) {
    if have("python3") {
        let dir = Scratch::new();
        std::fs::create_dir(&dir.0).unwrap();
        let script = dir.0.join("check.py");
        std::fs::write(&script, CHECK_PY).unwrap();
        let out = Command::new("python3")
            .arg("-I")
            .arg(&script)
            .arg(archive)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "python zipfile: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let want: Vec<String> = expected
            .iter()
            .map(|(n, d)| format!("{n} 0 {}", hex(d)))
            .collect();
        let got: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(got, want);
        std::fs::remove_dir_all(&dir.0).unwrap();
    }
    if have("unzip") {
        let out = Command::new("unzip")
            .arg("-tq")
            .arg(archive)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        // unzip exits 1 ("warning") for an archive with no entries.
        if !expected.is_empty() {
            assert!(
                out.status.success(),
                "unzip -tq: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn build(entries: &[(&str, &[u8])]) -> (Scratch, Vec<u8>) {
    let s = Scratch::new();
    let mut w = StoreWriter::new(s.create());
    for (n, d) in entries {
        let mut r = *d;
        assert_eq!(w.add(n, &mut r, 1 << 30).unwrap(), d.len() as u64);
    }
    w.finish().unwrap();
    let b = s.bytes();
    (s, b)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[test]
fn empty_archive_is_valid() {
    let (s, b) = build(&[]);
    assert_eq!(b.len(), 22);
    assert_eq!(&b[..4], b"PK\x05\x06");
    assert!(b[4..].iter().all(|&x| x == 0));
    independent_check(&s.0, &[]);
    assert_eq!(
        open(&File::open(&s.0).unwrap(), LIMITS)
            .unwrap()
            .entries()
            .len(),
        0
    );
}

#[test]
fn stored_entries_match_bytes_and_crc() {
    let big = pattern(200 * 1024);
    let entries: [(&str, &[u8]); 3] = [("000.png", b""), ("001.jpg", b"x"), ("002.png", &big)];
    let (s, _) = build(&entries);
    let f = File::open(&s.0).unwrap();
    let a = open(&f, LIMITS).unwrap();
    assert_eq!(a.entries().len(), 3);
    for (i, (name, data)) in entries.iter().enumerate() {
        assert_eq!(a.entries()[i].name, name.as_bytes());
        assert_eq!(a.entries()[i].method, Method::Stored);
        let mut out = Vec::new();
        a.read(i, &mut out).unwrap();
        assert_eq!(&out, data);
    }
    independent_check(&s.0, &entries);
}

#[test]
fn header_fields_are_fixed() {
    let (_s, b) = build(&[("a.png", b"hello"), ("b.jpg", b"")]);
    // Local header of the first entry.
    assert_eq!(&b[..4], b"PK\x03\x04");
    assert_eq!(u16_at(&b, 4), 10);
    assert_eq!(u16_at(&b, 6), 0); // flags
    assert_eq!(u16_at(&b, 8), 0); // method
    assert_eq!(u16_at(&b, 10), 0x0000);
    assert_eq!(u16_at(&b, 12), 0x0021);
    assert_eq!(u16_at(&b, 28), 0); // extra
    assert_eq!(u16_at(&b, 26), 5);
    let (lcrc, lcs, lsz) = (u32_at(&b, 14), u32_at(&b, 18), u32_at(&b, 22));
    assert_eq!(lcrc, 0x3610_a686); // CRC-32 of "hello"
    assert_eq!((lcs, lsz), (5, 5));
    // End record -> central directory.
    let eocd = b.len() - 22;
    assert_eq!(&b[eocd..eocd + 4], b"PK\x05\x06");
    assert_eq!(u16_at(&b, eocd + 8), 2);
    assert_eq!(u16_at(&b, eocd + 10), 2);
    assert_eq!(u16_at(&b, eocd + 20), 0); // comment
    let cd = u32_at(&b, eocd + 16) as usize;
    assert_eq!(u32_at(&b, eocd + 12) as usize, eocd - cd);
    let mut at = cd;
    for (i, (name, size)) in [("a.png", 5u32), ("b.jpg", 0)].iter().enumerate() {
        assert_eq!(&b[at..at + 4], b"PK\x01\x02");
        assert_eq!(u16_at(&b, at + 4), 10); // made by, MS-DOS
        assert_eq!(u16_at(&b, at + 6), 10);
        assert_eq!(u16_at(&b, at + 8), 0);
        assert_eq!(u16_at(&b, at + 10), 0);
        assert_eq!(u16_at(&b, at + 12), 0x0000);
        assert_eq!(u16_at(&b, at + 14), 0x0021);
        assert_eq!(u32_at(&b, at + 20), *size);
        assert_eq!(u32_at(&b, at + 24), *size);
        assert_eq!(u16_at(&b, at + 28), name.len() as u16);
        assert_eq!(u16_at(&b, at + 30), 0); // extra
        assert_eq!(u16_at(&b, at + 32), 0); // comment
        assert_eq!(u16_at(&b, at + 34), 0); // disk
        assert_eq!(u16_at(&b, at + 36), 0); // internal
        assert_eq!(u32_at(&b, at + 38), 0); // external
        let off = u32_at(&b, at + 42) as usize;
        assert_eq!(&b[at + 46..at + 46 + name.len()], name.as_bytes());
        // The local header it points at agrees (the write-back happened).
        assert_eq!(&b[off..off + 4], b"PK\x03\x04");
        assert_eq!(u32_at(&b, off + 14), u32_at(&b, at + 16));
        assert_eq!(u32_at(&b, off + 18), *size);
        assert_eq!(u32_at(&b, off + 22), *size);
        if i == 0 {
            assert_eq!(u32_at(&b, at + 16), lcrc);
        }
        at += 46 + name.len();
    }
    assert_eq!(at, eocd);
}

#[test]
fn output_is_deterministic() {
    let data = pattern(70_000);
    let a = build(&[("a.png", &data), ("b.png", b"zz")]).1;
    let b = build(&[("a.png", &data), ("b.png", b"zz")]).1;
    assert_eq!(a, b);
}

#[test]
fn rejects_bad_names() {
    let s = Scratch::new();
    let mut w = StoreWriter::new(s.create());
    w.add("ok.jpg", &mut &b"1"[..], 10).unwrap();
    let before = s.bytes();
    let long = "a".repeat(256);
    for bad in [
        "",
        "a/b",
        "a\\b",
        "..",
        ".",
        "é.jpg",
        "a b",
        "a\n",
        long.as_str(),
    ] {
        assert!(
            matches!(w.add(bad, &mut &b"x"[..], 10), Err(WriteError::BadName)),
            "{bad:?}"
        );
    }
    assert!(matches!(
        w.add("ok.jpg", &mut &b"2"[..], 10),
        Err(WriteError::DuplicateName)
    ));
    assert_eq!(s.bytes(), before);
    assert_eq!(w.len(), 1);
    // 255 bytes is the longest allowed.
    w.add(&"a".repeat(255), &mut &b"x"[..], 10).unwrap();
}

/// Gives `n` bytes, then an error.
struct FailAfter(usize);
impl Read for FailAfter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0 == 0 {
            return Err(io::Error::other("boom"));
        }
        let n = buf.len().min(self.0);
        buf[..n].fill(7);
        self.0 -= n;
        Ok(n)
    }
}

#[test]
fn failed_entry_rolls_back() {
    let s = Scratch::new();
    let mut w = StoreWriter::new(s.create());
    w.add("a.png", &mut &b"first"[..], 100).unwrap();
    let len_before = s.bytes().len();
    assert!(matches!(
        w.add("bad.png", &mut FailAfter(100 * 1024), 1 << 30),
        Err(WriteError::Read(_))
    ));
    assert_eq!(s.bytes().len(), len_before, "file is cut back");
    let big = pattern(1000);
    assert!(matches!(
        w.add("big.png", &mut &big[..], 999),
        Err(WriteError::EntryTooLarge)
    ));
    assert_eq!(s.bytes().len(), len_before);
    // Exactly max is fine, and the names of failed entries are free again.
    w.add("big.png", &mut &big[..], 1000).unwrap();
    w.add("bad.png", &mut &b"again"[..], 100).unwrap();
    assert_eq!(w.len(), 3);
    w.finish().unwrap();
    independent_check(
        &s.0,
        &[
            ("a.png", b"first"),
            ("big.png", &big),
            ("bad.png", b"again"),
        ],
    );
}

#[test]
fn limits_are_projected() {
    let s = Scratch::new();
    let mut w = StoreWriter::new(s.create());
    let first = w.projected_len(5, 3).unwrap();
    assert_eq!(first, 30 + 5 + 3 + 46 + 5 + 22);
    // Each empty entry "NNNNN" takes 30 + 5 and 46 + 5 bytes.
    for i in 0..65_535u32 {
        assert!(w.projected_len(5, 0).is_some(), "{i}");
        w.add(&format!("{i:05}"), &mut &b""[..], 0).unwrap();
    }
    assert_eq!(w.len(), 65_535);
    assert_eq!(w.projected_len(5, 0), None);
    assert!(matches!(
        w.add("extra", &mut &b""[..], 0),
        Err(WriteError::TooManyEntries)
    ));
    let f = w.finish().unwrap();
    assert_eq!(f.metadata().unwrap().len(), 65_535 * (35 + 51) + 22);
    // The u32 limit is covered by a unit test in the module (it needs a
    // writer positioned near 4 GiB without a 4 GiB file).
}

#[test]
fn io_error_poisons() {
    let s = Scratch::new();
    drop(s.create());
    let ro = File::open(&s.0).unwrap();
    let mut w = StoreWriter::new(ro);
    assert!(matches!(
        w.add("a.png", &mut &b"x"[..], 10),
        Err(WriteError::Io(_))
    ));
    assert!(matches!(
        w.add("b.png", &mut &b"x"[..], 10),
        Err(WriteError::Poisoned)
    ));
    assert!(matches!(w.finish(), Err(WriteError::Poisoned)));
}

#[test]
fn reads_back_with_cbz_to_pdf_limits() {
    let s = Scratch::new();
    let mut w = StoreWriter::new(s.create());
    for i in 0..1024 {
        let d = vec![(i % 251) as u8; i % 40];
        w.add(&format!("{:04}.png", i + 1), &mut &d[..], 1 << 20)
            .unwrap();
    }
    w.finish().unwrap();
    // The limits `cbz-to-pdf` reads with (plan 9): 1024 entries, 64 MiB.
    let limits = Limits {
        entries: 1024,
        entry_bytes: 64 << 20,
        ratio: 1100,
        name_bytes: 1024,
    };
    let f = File::open(&s.0).unwrap();
    let a = open(&f, limits).unwrap();
    for i in 0..1024 {
        let mut out = Vec::new();
        a.read(i, &mut out).unwrap();
        assert_eq!(out, vec![(i % 251) as u8; i % 40]);
    }
    // Positioned write-back leaves no stray bytes: pread past the end is 0.
    let mut one = [0u8; 1];
    assert_eq!(f.read_at(&mut one, f.metadata().unwrap().len()).unwrap(), 0);
}
