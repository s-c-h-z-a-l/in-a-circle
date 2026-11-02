#![no_main]
//! `epub::read::open_with` and `Book::chapter` of every spine item, on
//! arbitrary bytes as a file: whatever an EPUB on the user's disk holds.
//! Asserts what the reader promises: at most `MAX_SPINE` items numbered
//! from 1, each naming a kept item; each manifest item kept once, its path
//! and media type within their caps, the work to resolve them within the
//! package's length; no chapter over its limit, all chapters read (each as
//! often as the spine has it) within the book limit; an empty buffer on
//! every error.
//!
//! Before the bytes are written, the CRC-32 of every stored entry the
//! directory locates is recomputed into both its headers (`fix_crcs`):
//! otherwise a mutation of a stored XML file only ever reaches the ZIP
//! reader's CRC check, never the reader's XML handling. The CRC check
//! itself is the `zip_reader` target's to fuzz.

use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;
use std::fs::File;
use std::os::unix::fs::FileExt;

use epub::read::{Limits, MAX_MEDIA_TYPE, MAX_PATH, MAX_SPINE, open_with};

/// Small limits so the fuzzer reaches each of them at speed, as the
/// `zip_reader` target does: chapters and skeleton files up to 256 KiB
/// (8 x the 32 KiB inflate window, so a deflated entry still exercises
/// the window and the size checks), 1 MiB of chapters per book (the book
/// limit is reachable with four spine items) and 4,096 XML events per
/// skeleton file (a 64 KiB input holds ~16,000 empty elements). The
/// shipped boundaries (8 MiB, 64 MiB, 2^19) are pinned by
/// crates/epub/tests/read.rs.
const LIMITS: Limits = Limits {
    chapter_bytes: 256 << 10,
    book_bytes: 1 << 20,
    package_bytes: 256 << 10,
    package_nodes: 4096,
};

thread_local! {
    /// One scratch file per process in the fuzz crate's own `target/`
    /// (gitignored, never HOME or the real temp dir), unlinked at once:
    /// the open descriptor keeps it alive and nothing is left behind.
    static SCRATCH: RefCell<Option<File>> = const { RefCell::new(None) };
}

fn scratch() -> File {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/epub_reader_scratch");
    std::fs::create_dir_all(dir).unwrap();
    let path = format!("{dir}/{}", std::process::id());
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    f
}

fn le16(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn le32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// Rewrite the CRC-32 of each stored entry (APPNOTE 4.3.7, 4.3.12, 4.3.16:
/// central CRC at +16, local CRC at +14, data behind the local header's
/// name and extra field) whose headers and data lie inside `b`; anything
/// else is left as it is. Bounded by the directory's record count and the
/// input length.
fn fix_crcs(b: &mut [u8]) -> Option<()> {
    let end = b.windows(4).rposition(|w| w == b"PK\x05\x06")?;
    let (count, mut pos) = (le16(b, end + 10)?, le32(b, end + 16)?);
    for _ in 0..count {
        if b.get(pos..pos + 4)? != b"PK\x01\x02" {
            return None;
        }
        let (method, csize, local) = (le16(b, pos + 10)?, le32(b, pos + 20)?, le32(b, pos + 42)?);
        let next = pos + 46 + le16(b, pos + 28)? + le16(b, pos + 30)? + le16(b, pos + 32)?;
        if method == 0 && b.get(local..local + 4) == Some(&b"PK\x03\x04"[..]) {
            let start = local + 30 + le16(b, local + 26)? + le16(b, local + 28)?;
            if let Some(data) = b.get(start..start.checked_add(csize)?) {
                let mut crc = flate::Crc32::new();
                crc.update(data);
                let crc = crc.finish().to_le_bytes();
                b[pos + 16..pos + 20].copy_from_slice(&crc);
                b[local + 14..local + 18].copy_from_slice(&crc);
            }
        }
        pos = next;
    }
    Some(())
}

/// The largest uncompressed size (APPNOTE 4.3.12, +24) any central
/// directory record in `b` declares, at every `PK\x01\x02` signature
/// whatever end record locates it: every record the ZIP reader accepts is
/// among them, the package document's too. One pass over the input.
fn max_declared(b: &[u8]) -> u64 {
    b.windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"PK\x01\x02")
        .filter_map(|(at, _)| le32(b, at + 24))
        .max()
        .unwrap_or(0) as u64
}

fuzz_target!(|data: &[u8]| {
    let mut data = data.to_vec();
    let _ = fix_crcs(&mut data);
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        let f = s.get_or_insert_with(scratch);
        f.set_len(0).unwrap();
        f.write_all_at(&data, 0).unwrap();
        let Ok(book) = open_with(f, LIMITS) else {
            return;
        };
        assert!(!book.chapters.is_empty() && book.chapters.len() <= MAX_SPINE);
        // Held: one item per manifest item the spine names, each capped
        // (`...` is appended to a cut string).
        assert!(book.items.len() <= book.chapters.len());
        for it in &book.items {
            assert!(it.path.len() <= MAX_PATH + 3, "{} B path", it.path.len());
            assert!(it.media_type.len() <= MAX_MEDIA_TYPE + 3);
        }
        // Work: each item's href and media type once. They are parts of the
        // package document as it was read: at most its declared size (and
        // `package_bytes`), 1.5 x that once a UTF-16 document is decoded to
        // UTF-8. Resolving per itemref instead exceeds it as soon as a
        // long href is named a few times.
        let package = max_declared(&data).min(LIMITS.package_bytes);
        assert!(
            book.steps() <= package * 3 / 2,
            "{} steps, package at most {package} B",
            book.steps()
        );
        let mut out = Vec::new();
        let mut produced = 0u64;
        for (k, c) in book.chapters.iter().enumerate() {
            assert_eq!(c.index, k + 1);
            assert!(c.item.is_none_or(|i| i < book.items.len()));
            match book.chapter(c.index, &mut out) {
                Ok(()) => {
                    assert!(out.len() as u64 <= LIMITS.chapter_bytes);
                    produced += out.len() as u64;
                }
                Err(_) => assert!(out.is_empty()),
            }
        }
        assert!(produced <= LIMITS.book_bytes, "{produced} B produced");
    });
});
