#![no_main]
//! `zip::open` and `Archive::read` of every entry, on arbitrary bytes as a
//! file: whatever a CBZ on the user's disk holds.

use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;
use std::fs::File;
use std::os::unix::fs::FileExt;

/// Small limits so the fuzzer reaches every one of them at speed: at most
/// 8 entries, 256 KiB each (2 MiB of output at the very most; the earlier
/// 8 x 4 MiB ran ~25 exec/s with flat coverage). 256 KiB is 8 x the 32 KiB
/// inflate window, so an entry still exercises window wrap, long matches
/// and the declared-size/CRC checks. The limit boundaries themselves are
/// pinned by `limits_entries_names_size_ratio` and
/// `rejects_more_data_than_declared` in crates/zip/tests/reader.rs.
/// Ratio 1100 (a deflate stream cannot exceed 1032:1, so the ratio limit
/// never hides the entry-size limit), names up to 256 bytes.
const LIMITS: zip::Limits = zip::Limits {
    entries: 8,
    entry_bytes: 256 << 10,
    ratio: 1100,
    name_bytes: 256,
};

thread_local! {
    /// One scratch file per process, created in the fuzz crate's own
    /// `target/` (gitignored, never HOME or the real temp dir) and unlinked
    /// at once: the open descriptor keeps it alive and nothing is left
    /// behind if the process dies.
    static SCRATCH: RefCell<Option<File>> = const { RefCell::new(None) };
}

fn scratch() -> File {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/zip_reader_scratch");
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

fuzz_target!(|data: &[u8]| {
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        let f = s.get_or_insert_with(scratch);
        f.set_len(0).unwrap();
        f.write_all_at(data, 0).unwrap();
        if let Ok(a) = zip::open(f, LIMITS) {
            for i in 0..a.entries().len() {
                let mut v = Vec::new();
                let _ = a.read(i, &mut v);
            }
        }
    });
});
