//! Strict reader for ZIP archives (stored and deflated entries) read from a
//! file on disk, and writer of archives with stored entries (`write`). The
//! archive the reader opens is foreign input and a security boundary:
//!
//! - Everything is read with positioned reads (`read_exact_at`); there is no
//!   way to read outside the file, and every offset or length addition is
//!   checked. The file length is taken once, from `metadata()`, at `open`.
//! - `open` validates the whole directory against the local headers, the
//!   data ranges against the file and each other, the names against each
//!   other, and the declared sizes against the `Limits`, before any entry
//!   data is read. Limits are checked on the declared values and again on
//!   what is actually walked (records iterated) or produced (the inflate
//!   output limit is the declared size).
//! - Entry data is never held whole: `read_to` hands it on in chunks of at
//!   most 64 KiB. The CRC-32 and the sizes are verified at the end of the
//!   stream, so a caller that got chunks must discard them if `read_to`
//!   returns an error.
//! - Not supported (and rejected with a reason): ZIP64, multi-disk archives.
//!   Encrypted entries and methods other than stored and deflated are listed
//!   by `open` and refused by `read`, so one such entry does not hide the
//!   rest of the archive.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;

pub mod write;
pub use write::{MAX_ENTRIES, StoreWriter, WriteError};

use flate::{Crc32, InflateError, inflate};

/// Compression method of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Stored,
    Deflated,
    /// Any other method code (not readable).
    Other(u16),
}

/// One central-directory record.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The raw name bytes (not necessarily UTF-8).
    pub name: Vec<u8>,
    pub method: Method,
    /// Any encryption flag is set (traditional, strong or masked headers).
    pub encrypted: bool,
    pub compressed_size: u64,
    pub size: u64,
    pub crc32: u32,
    /// Start of the compressed data in the file (behind the local header),
    /// validated by `open` to lie inside the file.
    data_start: u64,
}

impl Entry {
    /// A directory entry: the name ends in `/`.
    pub fn is_dir(&self) -> bool {
        self.name.last() == Some(&b'/')
    }
}

/// Bounds the reader enforces. There is no default: the caller states what
/// its own memory and time budget allows.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Most central-directory records (checked on the declared count and on
    /// the records actually walked).
    pub entries: usize,
    /// Largest declared uncompressed size of one entry; it is also the
    /// inflate output limit, so it holds for the bytes actually produced.
    pub entry_bytes: u64,
    /// Largest `size / compressed_size`. A nonzero size with a zero
    /// compressed size is over the limit whatever it is (fail closed).
    pub ratio: u64,
    /// Longest entry name in bytes.
    pub name_bytes: usize,
}

#[derive(Debug)]
pub enum ZipError {
    NotZip,
    Truncated,
    Zip64,
    MultiDisk,
    OutOfBounds,
    Overlap,
    HeaderMismatch(&'static str),
    DuplicateName,
    TooManyEntries,
    NameTooLong,
    EntryTooLarge,
    RatioTooHigh,
    Encrypted,
    UnsupportedMethod(u16),
    SizeMismatch,
    CrcMismatch,
    Inflate(InflateError),
    Io(io::Error),
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotZip => f.write_str("not a ZIP archive"),
            Self::Truncated => f.write_str("the central directory is truncated"),
            Self::Zip64 => f.write_str("ZIP64 archives are not supported"),
            Self::MultiDisk => f.write_str("multi-disk archives are not supported"),
            Self::OutOfBounds => f.write_str("an entry lies outside the file"),
            Self::Overlap => f.write_str("entries overlap each other or the directory"),
            Self::HeaderMismatch(what) => write!(f, "local and central headers disagree: {what}"),
            Self::DuplicateName => f.write_str("two entries have the same name"),
            Self::TooManyEntries => f.write_str("too many entries"),
            Self::NameTooLong => f.write_str("an entry name is too long"),
            Self::EntryTooLarge => f.write_str("an entry is too large"),
            Self::RatioTooHigh => f.write_str("an entry's compression ratio is too high"),
            Self::Encrypted => f.write_str("the entry is encrypted"),
            Self::UnsupportedMethod(m) => write!(f, "unsupported compression method {m}"),
            Self::SizeMismatch => f.write_str("the entry's data does not match its declared size"),
            Self::CrcMismatch => f.write_str("the entry's CRC-32 does not match"),
            Self::Inflate(e) => write!(f, "deflate stream: {e}"),
            Self::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

impl std::error::Error for ZipError {}

impl From<io::Error> for ZipError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// End-of-central-directory record: fixed part, RFC-less but fixed by the
/// PKWARE APPNOTE 4.3.16.
const EOCD_LEN: u64 = 22;
/// The comment length is a 16 bits field, so the end record starts at most
/// this far (plus its own length) from the end of the file; searching
/// further back cannot find a valid one.
const MAX_COMMENT: u64 = 65_535;
/// Central directory file header, fixed part (APPNOTE 4.3.12).
const CENTRAL_LEN: u64 = 46;
/// Local file header, fixed part (APPNOTE 4.3.7).
const LOCAL_LEN: u64 = 30;
/// ZIP64 end-of-central-directory locator length (APPNOTE 4.3.15).
const ZIP64_LOCATOR_LEN: u64 = 20;
/// Largest chunk handed to the sink and the positioned-read size for stored
/// entries: big enough to keep syscalls rare (16 reads per MiB), small
/// enough to stay cache-friendly and a fixed 64 KiB of memory.
const CHUNK: usize = 64 * 1024;
/// General purpose flag bits that mean encryption: bit 0 (traditional),
/// bit 6 (strong), bit 13 (central directory masked).
const ENCRYPTION_FLAGS: u16 = 1 | (1 << 6) | (1 << 13);
/// Bit 3: sizes and CRC follow the data; the local header holds zeros.
const DATA_DESCRIPTOR: u16 = 1 << 3;
/// Flag bits that must agree between the local and central headers: the
/// ones that change how the data is read.
const SEMANTIC_FLAGS: u16 = ENCRYPTION_FLAGS | DATA_DESCRIPTOR;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// An opened archive: the validated entry list and the file to read from.
pub struct Archive<'f> {
    file: &'f File,
    entries: Vec<Entry>,
}

/// Open and validate the archive in `file`.
pub fn open(file: &File, limits: Limits) -> Result<Archive<'_>, ZipError> {
    let file_len = file.metadata()?.len();
    if file_len < EOCD_LEN {
        return Err(ZipError::NotZip);
    }

    // End record: search backwards over at most the longest possible comment.
    let tail_len = file_len.min(EOCD_LEN + MAX_COMMENT);
    let tail_start = file_len - tail_len;
    let mut tail = vec![0u8; tail_len as usize];
    file.read_exact_at(&mut tail, tail_start)?;
    let mut found = None;
    for i in (0..=tail.len() - EOCD_LEN as usize).rev() {
        // A candidate counts only if its comment length ends exactly at the
        // end of the file: a signature inside a comment is not a record.
        if tail[i..i + 4] == *b"PK\x05\x06"
            && i as u64 + EOCD_LEN + u64::from(le16(&tail, i + 20)) == tail_len
        {
            found = Some(i);
            break;
        }
    }
    let i = found.ok_or(ZipError::NotZip)?;
    let eocd_pos = tail_start + i as u64;
    let rec = &tail[i..i + EOCD_LEN as usize];
    let (disk, cd_disk) = (le16(rec, 4), le16(rec, 6));
    let (on_disk, total) = (le16(rec, 8), le16(rec, 10));
    let (cd_size, cd_offset) = (le32(rec, 12), le32(rec, 16));
    drop(tail);

    // ZIP64 is signalled by saturated fields or by its locator in front of
    // the end record; either way the real values are elsewhere.
    let mut locator = [0u8; 4];
    let has_locator = eocd_pos >= ZIP64_LOCATOR_LEN && {
        file.read_exact_at(&mut locator, eocd_pos - ZIP64_LOCATOR_LEN)?;
        locator == *b"PK\x06\x07"
    };
    if has_locator
        || on_disk == 0xFFFF
        || total == 0xFFFF
        || cd_size == 0xFFFF_FFFF
        || cd_offset == 0xFFFF_FFFF
    {
        return Err(ZipError::Zip64);
    }
    if disk != 0 || cd_disk != 0 || on_disk != total {
        return Err(ZipError::MultiDisk);
    }
    if usize::from(total) > limits.entries {
        return Err(ZipError::TooManyEntries);
    }
    let cd_offset = u64::from(cd_offset);
    let cd_end = cd_offset
        .checked_add(u64::from(cd_size))
        .filter(|&end| end <= eocd_pos)
        .ok_or(ZipError::Truncated)?;

    // Central directory: walk the records actually present, not the count
    // the end record claims. Each record costs at least 46 bytes of the
    // directory, so the walk is bounded by `entries` records.
    let mut entries = Vec::new();
    let mut local_offsets = Vec::new();
    let mut pos = cd_offset;
    while pos < cd_end {
        if entries.len() >= limits.entries {
            return Err(ZipError::TooManyEntries);
        }
        if cd_end - pos < CENTRAL_LEN {
            return Err(ZipError::Truncated);
        }
        let mut h = [0u8; CENTRAL_LEN as usize];
        file.read_exact_at(&mut h, pos)?;
        if h[..4] != *b"PK\x01\x02" {
            return Err(ZipError::HeaderMismatch("central directory signature"));
        }
        let flags = le16(&h, 8);
        let method = le16(&h, 10);
        let crc32 = le32(&h, 16);
        let (csize, size) = (le32(&h, 20), le32(&h, 24));
        let name_len = usize::from(le16(&h, 28));
        let variable = name_len as u64 + u64::from(le16(&h, 30)) + u64::from(le16(&h, 32));
        let disk_start = le16(&h, 34);
        let local_offset = le32(&h, 42);
        if cd_end - pos - CENTRAL_LEN < variable {
            return Err(ZipError::Truncated);
        }
        if csize == 0xFFFF_FFFF || size == 0xFFFF_FFFF || local_offset == 0xFFFF_FFFF {
            return Err(ZipError::Zip64);
        }
        if disk_start != 0 {
            return Err(ZipError::MultiDisk);
        }
        // Checked before the name is allocated.
        if name_len > limits.name_bytes {
            return Err(ZipError::NameTooLong);
        }
        let (csize, size) = (u64::from(csize), u64::from(size));
        if size > limits.entry_bytes {
            return Err(ZipError::EntryTooLarge);
        }
        // size > csize * ratio, with saturation: csize == 0 and size > 0 is
        // over the limit for every ratio (an inflate stream is never empty).
        if size > csize.saturating_mul(limits.ratio) {
            return Err(ZipError::RatioTooHigh);
        }
        let mut name = vec![0u8; name_len];
        file.read_exact_at(&mut name, pos + CENTRAL_LEN)?;
        entries.push(Entry {
            name,
            method: match method {
                0 => Method::Stored,
                8 => Method::Deflated,
                m => Method::Other(m),
            },
            encrypted: flags & ENCRYPTION_FLAGS != 0,
            compressed_size: csize,
            size,
            crc32,
            data_start: 0,
        });
        local_offsets.push((u64::from(local_offset), flags));
        // Cannot overflow: bounded by cd_end - pos, checked just above.
        pos += CENTRAL_LEN + variable;
    }
    if entries.len() != usize::from(total) {
        return Err(ZipError::HeaderMismatch("entry count"));
    }

    // Local headers: each must agree with its central record, and fix the
    // data range, which has to lie in the file.
    // ranges: (start, end) of every local header + data, and the directory.
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(entries.len() + 1);
    for (entry, &(offset, central_flags)) in entries.iter_mut().zip(&local_offsets) {
        let header_end = offset
            .checked_add(LOCAL_LEN)
            .filter(|&end| end <= file_len)
            .ok_or(ZipError::OutOfBounds)?;
        let mut h = [0u8; LOCAL_LEN as usize];
        file.read_exact_at(&mut h, offset)?;
        if h[..4] != *b"PK\x03\x04" {
            return Err(ZipError::HeaderMismatch("local signature"));
        }
        let flags = le16(&h, 6);
        if (flags ^ central_flags) & SEMANTIC_FLAGS != 0 {
            return Err(ZipError::HeaderMismatch("flags"));
        }
        let method = le16(&h, 8);
        let want = match entry.method {
            Method::Stored => 0,
            Method::Deflated => 8,
            Method::Other(m) => m,
        };
        if method != want {
            return Err(ZipError::HeaderMismatch("method"));
        }
        if flags & DATA_DESCRIPTOR == 0
            && (u64::from(le32(&h, 14)) != u64::from(entry.crc32)
                || u64::from(le32(&h, 18)) != entry.compressed_size
                || u64::from(le32(&h, 22)) != entry.size)
        {
            return Err(ZipError::HeaderMismatch("sizes or crc"));
        }
        let name_len = u64::from(le16(&h, 26));
        if name_len != entry.name.len() as u64 {
            return Err(ZipError::HeaderMismatch("name"));
        }
        // Same length as the central name, which is within name_bytes.
        let mut name = vec![0u8; entry.name.len()];
        // header_end <= file_len, so this read starts inside the file; a
        // name running past the end is an I/O error (short read).
        file.read_exact_at(&mut name, header_end)?;
        if name != entry.name {
            return Err(ZipError::HeaderMismatch("name"));
        }
        let data_start = header_end
            .checked_add(name_len)
            .and_then(|v| v.checked_add(u64::from(le16(&h, 28))))
            .ok_or(ZipError::OutOfBounds)?;
        let data_end = data_start
            .checked_add(entry.compressed_size)
            .filter(|&end| end <= file_len)
            .ok_or(ZipError::OutOfBounds)?;
        entry.data_start = data_start;
        ranges.push((offset, data_end));
    }
    ranges.push((cd_offset, cd_end));

    // Overlap: after sorting by start, each range must end at or before the
    // next one starts. Every range is non-empty except an empty directory
    // (cd_offset == cd_end), which cannot overlap anything it touches.
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(ZipError::Overlap);
    }

    // Duplicate names: compare byte-wise after sorting the indices.
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_unstable_by(|&a, &b| entries[a].name.cmp(&entries[b].name));
    if order
        .windows(2)
        .any(|w| entries[w[0]].name == entries[w[1]].name)
    {
        return Err(ZipError::DuplicateName);
    }

    Ok(Archive { file, entries })
}

/// A `Read` over the file range `[pos, end)`: the decoder can never read
/// past the entry's compressed data.
struct Bounded<'f> {
    file: &'f File,
    pos: u64,
    end: u64,
}

impl Read for Bounded<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.end - self.pos; // pos <= end by construction
        let n = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        if n == 0 {
            return Ok(0);
        }
        loop {
            match self.file.read_at(&mut buf[..n], self.pos) {
                Ok(got) => {
                    self.pos += got as u64;
                    return Ok(got);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Archive<'_> {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Stream entry `index` to `out` in chunks of at most 64 KiB; returns
    /// the number of bytes delivered. The sizes and the CRC-32 are checked
    /// at the end of the stream: on an error, chunks already delivered must
    /// be discarded. An `index` out of range is an `Io(InvalidInput)` error.
    pub fn read_to(
        &self,
        index: usize,
        out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<u64, ZipError> {
        let entry = self.entries.get(index).ok_or_else(|| {
            ZipError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entry index out of range",
            ))
        })?;
        if entry.encrypted {
            return Err(ZipError::Encrypted);
        }
        let mut crc = Crc32::new();
        let produced = match entry.method {
            Method::Other(m) => return Err(ZipError::UnsupportedMethod(m)),
            Method::Stored => {
                if entry.compressed_size != entry.size {
                    return Err(ZipError::SizeMismatch);
                }
                let mut buf = vec![0u8; CHUNK.min(entry.size as usize)];
                let mut done = 0u64;
                while done < entry.size {
                    let n = buf.len().min((entry.size - done) as usize);
                    // data_start + size <= file_len was checked at open.
                    self.file
                        .read_exact_at(&mut buf[..n], entry.data_start + done)?;
                    crc.update(&buf[..n]);
                    out(&buf[..n])?;
                    done += n as u64;
                }
                done
            }
            Method::Deflated => {
                let input = Bounded {
                    file: self.file,
                    pos: entry.data_start,
                    end: entry.data_start + entry.compressed_size,
                };
                // The output limit is the declared size: nothing past it is
                // ever delivered.
                let done = inflate(input, entry.size, &mut |chunk| {
                    crc.update(chunk);
                    out(chunk)
                })
                .map_err(|e| match e {
                    InflateError::OutputLimit => ZipError::SizeMismatch,
                    InflateError::Io(e) => ZipError::Io(e),
                    e => ZipError::Inflate(e),
                })?;
                if done.in_bytes != entry.compressed_size {
                    return Err(ZipError::SizeMismatch);
                }
                done.out_bytes
            }
        };
        if produced != entry.size {
            return Err(ZipError::SizeMismatch);
        }
        if crc.finish() != entry.crc32 {
            return Err(ZipError::CrcMismatch);
        }
        Ok(produced)
    }

    /// Append entry `index` to `out`. On an error `out` is cut back to its
    /// previous length. Reserves at most the declared size, which `open`
    /// has bounded by `Limits::entry_bytes`.
    pub fn read(&self, index: usize, out: &mut Vec<u8>) -> Result<(), ZipError> {
        let size = self.entries.get(index).map_or(0, |e| e.size);
        let base = out.len();
        out.try_reserve(size as usize).map_err(|_| {
            ZipError::Io(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "cannot reserve the entry size",
            ))
        })?;
        let r = self.read_to(index, &mut |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        });
        if r.is_err() {
            out.truncate(base);
        }
        r.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reader_error_has_its_own_text() {
        for (e, text) in [
            (ZipError::NotZip, "not a ZIP archive"),
            (ZipError::Truncated, "the central directory is truncated"),
            (ZipError::Zip64, "ZIP64 archives are not supported"),
            (ZipError::MultiDisk, "multi-disk archives are not supported"),
            (ZipError::OutOfBounds, "an entry lies outside the file"),
            (
                ZipError::Overlap,
                "entries overlap each other or the directory",
            ),
            (
                ZipError::HeaderMismatch("name"),
                "local and central headers disagree: name",
            ),
            (ZipError::DuplicateName, "two entries have the same name"),
            (ZipError::TooManyEntries, "too many entries"),
            (ZipError::NameTooLong, "an entry name is too long"),
            (ZipError::EntryTooLarge, "an entry is too large"),
            (
                ZipError::RatioTooHigh,
                "an entry's compression ratio is too high",
            ),
            (ZipError::Encrypted, "the entry is encrypted"),
            (
                ZipError::UnsupportedMethod(99),
                "unsupported compression method 99",
            ),
            (
                ZipError::SizeMismatch,
                "the entry's data does not match its declared size",
            ),
            (ZipError::CrcMismatch, "the entry's CRC-32 does not match"),
            (
                ZipError::from(io::Error::other("boom")),
                "read failed: boom",
            ),
        ] {
            assert_eq!(e.to_string(), text);
        }
    }
}
