//! Writer for ZIP archives with stored (method 0) entries only, written to
//! a file on disk. The output is fixed and deterministic: version needed
//! and made by 10 (MS-DOS), flags 0, modification time 0x0000 and date
//! 0x0021 (1980-01-01), no extra field, comment, data descriptor or ZIP64.
//! The same entries in the same order give the same bytes.
//!
//! Entry data is streamed in chunks of 64 KiB and never held whole. The
//! local header is written first with zero CRC and sizes, and patched in
//! place once the data is known, so the output must be a file that takes
//! positioned writes.
//!
//! A failed entry (the source errors, or it is larger than the caller's
//! limit, or the archive would pass the format's limits) is rolled back by
//! cutting the file to where the entry began; the writer stays usable. A
//! failed write to the file leaves it in an unknown state, so the writer is
//! poisoned and refuses everything after. The writer never writes its own
//! end record before `finish`. After any `Err` from `add` that is `Io` or
//! `Poisoned`, or from `finish`, the file is in an unknown state and
//! discarding it is the caller's job: entry data is arbitrary bytes, so a
//! leftover prefix may still hold an end record some reader would accept.

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;

use flate::Crc32;

/// Chunk size of the copy: the size of the one buffer the writer holds, and
/// the largest piece of entry data in memory. Same as the reader's chunk.
const CHUNK: usize = 64 * 1024;
/// Local file header, fixed part (APPNOTE 4.3.7).
const LOCAL_LEN: u64 = 30;
/// Central directory file header, fixed part (APPNOTE 4.3.12).
const CENTRAL_LEN: u64 = 46;
/// End of central directory record without comment (APPNOTE 4.3.16).
const EOCD_LEN: u64 = 22;
/// The entry count fields are 16 bits wide; 0xFFFF is the ZIP64 marker
/// (APPNOTE 4.4.1.4) and our own reader rejects it as ZIP64, so the largest
/// count that reads back is one less.
pub const MAX_ENTRIES: usize = 65_534;
/// Every offset, size and the archive itself must fit the 32 bits fields:
/// past them a reader would need ZIP64.
const MAX_ARCHIVE: u64 = u32::MAX as u64;
/// Longest name: a file name component is at most 255 bytes on the
/// filesystems an archive is unpacked to.
const MAX_NAME: usize = 255;
/// DOS date 1980-01-01 (year 0, month 1, day 1): the earliest date, a fixed
/// value so the output does not depend on the clock.
const DOS_DATE: u16 = 0x0021;
/// Version needed to extract and made by (MS-DOS): 1.0, the version of
/// stored entries.
const VERSION: u16 = 10;

#[derive(Debug)]
pub enum WriteError {
    /// Writing to the file failed; the writer is poisoned.
    Io(io::Error),
    /// Reading the entry's source failed; the entry is rolled back.
    Read(io::Error),
    /// The source gave more than the caller's `max` bytes; rolled back.
    EntryTooLarge,
    /// The archive would pass `u32::MAX` bytes; rolled back.
    ArchiveTooLarge,
    TooManyEntries,
    /// Empty, longer than 255 bytes, not ASCII graphic, or `/` or `\` in
    /// it, or the name `.` or `..`.
    BadName,
    DuplicateName,
    /// An earlier write failed.
    Poisoned,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "write failed: {e}"),
            Self::Read(e) => write!(f, "read failed: {e}"),
            Self::EntryTooLarge => f.write_str("an entry is too large"),
            Self::ArchiveTooLarge => f.write_str("the archive would be too large"),
            Self::TooManyEntries => f.write_str("too many entries"),
            Self::BadName => f.write_str("the entry name is not allowed"),
            Self::DuplicateName => f.write_str("two entries have the same name"),
            Self::Poisoned => f.write_str("an earlier write failed"),
        }
    }
}

impl std::error::Error for WriteError {}

struct Record {
    name: String,
    crc: u32,
    size: u32,
    offset: u32,
}

pub struct StoreWriter {
    file: File,
    entries: Vec<Record>,
    names: HashSet<String>,
    /// End of the written entries (where the next local header goes).
    pos: u64,
    /// Bytes the central directory records written so far would take.
    cd_len: u64,
    poisoned: bool,
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'/' && b != b'\\')
}

impl StoreWriter {
    /// Write an archive from the start of `file`, which should be empty and
    /// writable (and readable is not needed). Anything past the archive's
    /// end is cut at `finish`.
    pub fn new(file: File) -> Self {
        Self {
            file,
            entries: Vec::new(),
            names: HashSet::new(),
            pos: 0,
            cd_len: 0,
            poisoned: false,
        }
    }

    /// Test seam: a writer whose next entry starts at `pos`.
    #[cfg(test)]
    fn at(file: File, pos: u64) -> Self {
        let mut w = Self::new(file);
        w.pos = pos;
        w
    }

    /// Entries added so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The finished archive's length if one more entry with a name of
    /// `name_len` bytes and `data_len` bytes of data is added: its local
    /// header and data, the whole central directory with its record and the
    /// end record. `None` when that passes 65 534 entries or `u32::MAX`
    /// bytes.
    pub fn projected_len(&self, name_len: usize, data_len: u64) -> Option<u64> {
        if self.entries.len() >= MAX_ENTRIES {
            return None;
        }
        let name_len = u64::try_from(name_len).ok()?;
        let cd = self
            .cd_len
            .checked_add(CENTRAL_LEN)?
            .checked_add(name_len)?;
        let total = self
            .pos
            .checked_add(LOCAL_LEN)?
            .checked_add(name_len)?
            .checked_add(data_len)?
            .checked_add(cd)?
            .checked_add(EOCD_LEN)?;
        (total <= MAX_ARCHIVE).then_some(total)
    }

    fn io(&mut self, e: io::Error) -> WriteError {
        self.poisoned = true;
        WriteError::Io(e)
    }

    /// Add a stored entry read from `data`, at most `max` bytes of it;
    /// returns its size. On `Read`, `EntryTooLarge` and `ArchiveTooLarge`
    /// the file is cut back to before the entry and the writer is usable
    /// again; on `Io` it is poisoned. Name errors write nothing.
    pub fn add(&mut self, name: &str, data: &mut impl Read, max: u64) -> Result<u64, WriteError> {
        if self.poisoned {
            return Err(WriteError::Poisoned);
        }
        if !name_ok(name) {
            return Err(WriteError::BadName);
        }
        if self.names.contains(name) {
            return Err(WriteError::DuplicateName);
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(WriteError::TooManyEntries);
        }
        if self.projected_len(name.len(), 0).is_none() {
            return Err(WriteError::ArchiveTooLarge);
        }
        let start = self.pos;
        // Fits u32: projected_len(.., 0) above is <= u32::MAX.
        let offset = u32::try_from(start).map_err(|_| WriteError::ArchiveTooLarge)?;

        let mut head = Vec::with_capacity(LOCAL_LEN as usize + name.len());
        head.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        head.extend_from_slice(&VERSION.to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes()); // flags
        head.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        head.extend_from_slice(&0u16.to_le_bytes()); // time
        head.extend_from_slice(&DOS_DATE.to_le_bytes());
        head.extend_from_slice(&[0; 12]); // crc, sizes: patched below
        head.extend_from_slice(&(name.len() as u16).to_le_bytes()); // <= 255
        head.extend_from_slice(&0u16.to_le_bytes()); // extra length
        head.extend_from_slice(name.as_bytes());
        if let Err(e) = self.file.write_all_at(&head, start) {
            return Err(self.io(e));
        }

        // Cannot overflow: start + header is bounded by projected_len above.
        let data_start = start + head.len() as u64;
        let mut crc = Crc32::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; CHUNK];
        // Reading `max + 1` bytes tells "exactly max" from "more than max".
        let cap = max.saturating_add(1);
        let failure = loop {
            let want = usize::try_from(cap - size).map_or(CHUNK, |w| w.min(CHUNK));
            if want == 0 {
                break Some(WriteError::EntryTooLarge);
            }
            let n = match data.read(&mut buf[..want]) {
                Ok(0) => break None,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break Some(WriteError::Read(e)),
            };
            // Cannot overflow: size <= max + 1 by the read cap, and the
            // projected_len check below bounds it by u32::MAX per chunk.
            size += n as u64;
            if size > max {
                break Some(WriteError::EntryTooLarge);
            }
            if self.projected_len(name.len(), size).is_none() {
                break Some(WriteError::ArchiveTooLarge);
            }
            crc.update(&buf[..n]);
            if let Err(e) = self
                .file
                // Bounded by projected_len above (data_start + size <= u32::MAX).
                .write_all_at(&buf[..n], data_start + size - n as u64)
            {
                return Err(self.io(e));
            }
        };
        if let Some(err) = failure {
            if let Err(e) = self.file.set_len(start) {
                return Err(self.io(e));
            }
            return Err(err);
        }

        // size <= u32::MAX: projected_len bounded it above.
        let crc = crc.finish();
        let size32 = size as u32;
        let mut patch = [0u8; 12];
        patch[0..4].copy_from_slice(&crc.to_le_bytes());
        patch[4..8].copy_from_slice(&size32.to_le_bytes());
        patch[8..12].copy_from_slice(&size32.to_le_bytes());
        if let Err(e) = self.file.write_all_at(&patch, start + 14) {
            return Err(self.io(e));
        }

        // Both bounded by the last projected_len check (<= u32::MAX).
        self.pos = data_start + size;
        self.cd_len += CENTRAL_LEN + name.len() as u64;
        self.names.insert(name.to_owned());
        self.entries.push(Record {
            name: name.to_owned(),
            crc,
            size: size32,
            offset,
        });
        Ok(size)
    }

    /// Write the central directory and the end record, cut the file there
    /// and hand it back.
    pub fn finish(self) -> Result<File, WriteError> {
        if self.poisoned {
            return Err(WriteError::Poisoned);
        }
        let cd_start = self.pos;
        let total = cd_start
            .checked_add(self.cd_len)
            .and_then(|t| t.checked_add(EOCD_LEN))
            .filter(|&t| t <= MAX_ARCHIVE)
            .ok_or(WriteError::ArchiveTooLarge)?;
        let mut at = cd_start;
        let mut out: Vec<u8> = Vec::with_capacity(CHUNK + CENTRAL_LEN as usize + MAX_NAME);
        for r in &self.entries {
            out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            out.extend_from_slice(&VERSION.to_le_bytes()); // made by: MS-DOS
            out.extend_from_slice(&VERSION.to_le_bytes()); // needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&DOS_DATE.to_le_bytes());
            out.extend_from_slice(&r.crc.to_le_bytes());
            out.extend_from_slice(&r.size.to_le_bytes());
            out.extend_from_slice(&r.size.to_le_bytes());
            out.extend_from_slice(&(r.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0; 2]); // extra length
            out.extend_from_slice(&[0; 2]); // comment length
            out.extend_from_slice(&[0; 2]); // disk number
            out.extend_from_slice(&[0; 2]); // internal attributes
            out.extend_from_slice(&[0; 4]); // external attributes
            out.extend_from_slice(&r.offset.to_le_bytes());
            out.extend_from_slice(r.name.as_bytes());
            if out.len() >= CHUNK {
                self.file.write_all_at(&out, at).map_err(WriteError::Io)?;
                at += out.len() as u64;
                out.clear();
            }
        }
        let count = self.entries.len() as u16; // <= 65 534
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // disk numbers
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&(self.cd_len as u32).to_le_bytes()); // <= total
        out.extend_from_slice(&(cd_start as u32).to_le_bytes()); // <= total
        out.extend_from_slice(&[0; 2]); // comment length
        self.file.write_all_at(&out, at).map_err(WriteError::Io)?;
        self.file.set_len(total).map_err(WriteError::Io)?;
        Ok(self.file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u32_limit_is_projected() {
        let w = StoreWriter::at(
            // Nothing is written: only projected_len is called.
            File::open("/dev/null").unwrap(),
            MAX_ARCHIVE - (30 + 1 + 10 + 46 + 1 + 22),
        );
        // Exactly fits.
        assert_eq!(w.projected_len(1, 10), Some(MAX_ARCHIVE));
        // One byte more of data, or of name, does not.
        assert_eq!(w.projected_len(1, 11), None);
        assert_eq!(w.projected_len(2, 10), None);
        assert_eq!(w.projected_len(1, u64::MAX), None);
    }

    /// A sparse temp file path, removed on drop (also when a test fails).
    struct Sparse(std::path::PathBuf);

    impl Sparse {
        /// The guard exists before the file does, so no path leaks it.
        fn new() -> Self {
            // One name per call: the tests of this module run in parallel.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            Sparse(std::path::PathBuf::from(format!(
                "{}/zipw-unit-{}-{}",
                std::env::temp_dir().display(),
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )))
        }

        /// The file, `len` bytes long (sparse: no disk is used).
        fn create(&self, len: u64) -> File {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&self.0)
                .unwrap();
            f.set_len(len).unwrap();
            f
        }
    }

    impl Drop for Sparse {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn mid_entry_archive_limit_rolls_back() {
        // 1.5 chunks of room: the first 64 KiB chunk of data is written,
        // the second passes the limit, and the cut must undo the first.
        let start = MAX_ARCHIVE - (CHUNK + CHUNK / 2) as u64;
        let sp = Sparse::new();
        let f = sp.create(start);
        let probe = f.try_clone().unwrap();
        let mut w = StoreWriter::at(f, start);
        let big = vec![1u8; 3 * CHUNK];
        assert!(matches!(
            w.add("a", &mut &big[..], 1 << 30),
            Err(WriteError::ArchiveTooLarge)
        ));
        assert_eq!(
            probe.metadata().unwrap().len(),
            start,
            "cut back to the entry start"
        );
        assert_eq!(w.len(), 0);
        // A small entry still fits and the archive finishes.
        w.add("b", &mut &b"xyz"[..], 100).unwrap();
        let done = w.finish().unwrap();
        assert!(done.metadata().unwrap().len() <= MAX_ARCHIVE);
    }

    /// Serves `parts` one read at a time (an `Err` part is returned as that
    /// error) and records the length of every buffer it was handed.
    struct Script {
        parts: std::collections::VecDeque<io::Result<Vec<u8>>>,
        asked: Vec<usize>,
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.asked.push(buf.len());
            match self.parts.pop_front() {
                None => Ok(0),
                Some(Err(e)) => Err(e),
                Some(Ok(d)) => {
                    let n = d.len().min(buf.len());
                    buf[..n].copy_from_slice(&d[..n]);
                    Ok(n)
                }
            }
        }
    }

    fn script(parts: Vec<io::Result<Vec<u8>>>) -> Script {
        Script {
            parts: parts.into(),
            asked: Vec::new(),
        }
    }

    #[test]
    fn every_error_has_its_own_text() {
        let io_err = || io::Error::other("boom");
        for (e, text) in [
            (WriteError::Io(io_err()), "write failed: boom"),
            (WriteError::Read(io_err()), "read failed: boom"),
            (WriteError::EntryTooLarge, "an entry is too large"),
            (
                WriteError::ArchiveTooLarge,
                "the archive would be too large",
            ),
            (WriteError::TooManyEntries, "too many entries"),
            (WriteError::BadName, "the entry name is not allowed"),
            (WriteError::DuplicateName, "two entries have the same name"),
            (WriteError::Poisoned, "an earlier write failed"),
        ] {
            assert_eq!(e.to_string(), text);
        }
    }

    #[test]
    fn is_empty_follows_the_entries() {
        let sp = Sparse::new();
        let mut w = StoreWriter::new(sp.create(0));
        assert!(w.is_empty());
        w.add("a", &mut &b"x"[..], 10).unwrap();
        assert!(!w.is_empty());
    }

    #[test]
    fn a_read_asks_for_at_most_one_chunk_and_at_most_the_room_left() {
        let sp = Sparse::new();
        let mut w = StoreWriter::new(sp.create(0));
        let mut big = script(vec![Ok(vec![1; CHUNK]), Ok(vec![2; 10])]);
        w.add("a", &mut big, 1 << 30).unwrap();
        assert_eq!(big.asked, [CHUNK, CHUNK, CHUNK], "one buffer of CHUNK");
        assert_eq!(CHUNK, 65_536);
        // max 10: the first read may take 11 (10 and one more, to see the
        // limit passed); after 5 bytes, 6 more.
        let mut small = script(vec![Ok(vec![3; 5]), Ok(vec![4; 5])]);
        w.add("b", &mut small, 10).unwrap();
        assert_eq!(small.asked, [11, 6, 1]);
    }

    #[test]
    fn an_interrupted_read_is_retried() {
        let sp = Sparse::new();
        let mut w = StoreWriter::new(sp.create(0));
        let mut r = script(vec![
            Err(io::ErrorKind::Interrupted.into()),
            Ok(b"abc".to_vec()),
        ]);
        assert_eq!(w.add("a", &mut r, 10).unwrap(), 3);
        // Another error is the entry's failure, and the entry is rolled back.
        let mut r = script(vec![Err(io::Error::other("boom"))]);
        assert!(matches!(
            w.add("b", &mut r, 10),
            Err(WriteError::Read(e)) if e.kind() == io::ErrorKind::Other
        ));
        assert_eq!(w.len(), 1);
    }
}
