#![no_main]
//! `text.epub-to-md` on arbitrary bytes as a book: the shipped conversion
//! (`epub_md::convert`, with the heading index, the link and image
//! rewrite, the image checks and the GFM writer), included from the
//! action's own sources, with only the image files replaced by memory
//! (`Store`) and the md by a buffer. Asserts what the action promises on
//! every input: at most `MAX_IMAGES` images, named once each, each within
//! the image limit; the image entries charged (read or tried) within twice
//! the book file, and the images kept within what was charged; the md
//! within the text output budget and valid UTF-8; the `warn` lines within
//! the supervisor's cap. Every fifth input's store refuses every name as
//! too long, so the alt-text path of a long stem runs too.
//!
//! The CRC-32 of every stored entry is fixed before the bytes are written
//! (`fix_crcs`, as in the `epub_reader` target), so mutations reach the
//! XML and the images rather than only the ZIP reader's CRC check.

use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;

#[allow(dead_code)]
#[path = "../../bins/inacircle-text/src/base.rs"]
mod base;
#[allow(dead_code)]
#[path = "../../bins/inacircle-text/src/epub_md.rs"]
mod epub_md;
#[allow(dead_code)]
#[path = "../../bins/inacircle-text/src/epub_read.rs"]
mod epub_read;

// What the included modules name from the crate root, as in the action.
use base::{ConvertError, IMAGE_BYTES, LIMITS, MAX_PIXELS, Outcome, output_cap, too_large};

use epub::read::open_with;
use epub_md::{Images, MAX_IMAGES, Put, Store, convert, warnings};
use epub_read::MAX_WARN;

/// The `epub_reader` target's scaled-down reader limits, for the same
/// reason (the fuzzer reaches each at speed; the shipped boundaries are
/// pinned by crates/epub/tests/read.rs): 256 KiB chapters, 1 MiB of
/// chapters per book, 256 KiB and 4,096 events per skeleton file.
const READ_LIMITS: epub::read::Limits = epub::read::Limits {
    chapter_bytes: 256 << 10,
    book_bytes: 1 << 20,
    package_bytes: 256 << 10,
    package_nodes: 4096,
};

/// The images a conversion stored, in memory.
struct Memory {
    names: HashSet<String>,
    bytes: u64,
    /// Every name is too long for the folder.
    too_long: bool,
}

impl Store for Memory {
    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<(), Put> {
        if self.too_long {
            return Err(Put::NameTooLong);
        }
        assert!(self.names.insert(name.to_string()), "{name} stored twice");
        assert!(bytes.len() as u64 <= IMAGE_BYTES, "{} B image", bytes.len());
        self.bytes += bytes.len() as u64;
        Ok(())
    }
}

thread_local! {
    /// One scratch file per process in the fuzz crate's own `target/`
    /// (gitignored, never HOME or the real temp dir), unlinked at once.
    static SCRATCH: RefCell<Option<File>> = const { RefCell::new(None) };
}

fn scratch() -> File {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/epub_md_scratch");
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

/// The `epub_reader` target's CRC fix: each stored entry's CRC-32
/// recomputed into both its headers (APPNOTE 4.3.7, 4.3.12, 4.3.16).
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

fuzz_target!(|data: &[u8]| {
    let mut data = data.to_vec();
    let _ = fix_crcs(&mut data);
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        let f = s.get_or_insert_with(scratch);
        f.set_len(0).unwrap();
        f.write_all_at(&data, 0).unwrap();
        let Ok(book) = open_with(f, READ_LIMITS) else {
            return;
        };
        let book_len = data.len() as u64;
        let mut store = Memory {
            names: HashSet::new(),
            bytes: 0,
            too_long: data.len() % 5 == 0,
        };
        let mut md = Vec::new();
        let mut images = Images::new(&mut store, "b", book_len);
        let r = convert(&book, &mut md, &mut images);
        let (count, left) = (images.fuzz_count(), images.fuzz_budget_left());
        drop(images);
        // Charged: the budget's start less what is left (it never goes
        // below zero: the subtraction would panic).
        let charged = 2 * book_len - left;
        assert!(count <= MAX_IMAGES, "{count} images");
        assert_eq!(count, store.names.len());
        assert!(
            store.bytes <= charged,
            "{} B stored, {charged} B charged",
            store.bytes
        );
        assert!(md.len() <= LIMITS.output_bytes, "{} B of md", md.len());
        assert!(std::str::from_utf8(&md).is_ok(), "md is not UTF-8");
        if let Ok(done) = r {
            let lines = warnings(&done);
            assert!(lines.len() <= MAX_WARN, "{} warn lines", lines.len());
            assert!(done.replaced == 0 || lines.iter().any(|l| l.ends_with("replaced by alt text")));
        }
    });
});
