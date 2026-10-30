//! The strict ZIP reader against archives made by `testzip`, and against
//! damaged variants made by patching bytes.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use testzip::{Store, zip};
use zip::{Limits, Method, ZipError, open};

const BIG: Limits = Limits {
    entries: 1000,
    entry_bytes: 64 << 20,
    ratio: 1100,
    name_bytes: 1024,
};

/// Write `bytes` to a fresh file under the cargo test temp dir and reopen it
/// read-only.
fn file(bytes: &[u8]) -> File {
    static N: AtomicU32 = AtomicU32::new(0);
    let path = format!(
        "{}/zip-{}-{}",
        env!("CARGO_TARGET_TMPDIR"),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    File::create(&path).unwrap().write_all(bytes).unwrap();
    let f = File::open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    f
}

fn open_err(bytes: &[u8], limits: Limits) -> ZipError {
    match open(&file(bytes), limits) {
        Ok(_) => panic!("open accepted a damaged archive"),
        Err(e) => e,
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Offset of the end-of-central-directory record (no comment).
fn eocd(b: &[u8]) -> usize {
    b.len() - 22
}
/// Offset of central record `k` (records are `46 + name` long, no extra).
fn central(b: &[u8], k: usize) -> usize {
    let mut at = u32_at(b, eocd(b) + 16) as usize;
    for _ in 0..k {
        at += 46 + u16_at(b, at + 28) as usize;
    }
    at
}
/// Offset of the local header of entry `k`.
fn local(b: &[u8], k: usize) -> usize {
    u32_at(b, central(b, k) + 42) as usize
}

fn pseudo(len: usize) -> Vec<u8> {
    let mut x = 0x1234_5678u32;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

fn two() -> Vec<u8> {
    zip(&[
        (b"a.txt", b"hello hello hello hello", Store::Stored),
        (b"dir/b.bin", &[7u8; 5000], Store::Deflated),
    ])
}

fn read_all(f: &File, i: usize) -> Result<Vec<u8>, ZipError> {
    let a = open(f, BIG)?;
    let mut v = Vec::new();
    a.read(i, &mut v)?;
    Ok(v)
}

#[test]
fn reads_stored_and_deflated_entries() {
    let f = file(&two());
    let a = open(&f, BIG).unwrap();
    assert_eq!(a.entries().len(), 2);
    assert_eq!(a.entries()[0].name, b"a.txt");
    assert_eq!(a.entries()[0].method, Method::Stored);
    assert_eq!(a.entries()[1].method, Method::Deflated);
    assert!(!a.entries()[1].is_dir());
    let mut v = Vec::new();
    a.read(0, &mut v).unwrap();
    assert_eq!(v, b"hello hello hello hello");
    let mut v = Vec::new();
    a.read(1, &mut v).unwrap();
    assert_eq!(v, vec![7u8; 5000]);
}

#[test]
fn directory_names_end_in_a_slash() {
    let f = file(&zip(&[(b"d/", b"", Store::Stored)]));
    let a = open(&f, BIG).unwrap();
    assert!(a.entries()[0].is_dir());
}

#[test]
fn data_descriptor_entry_uses_central_sizes() {
    let mut b = two();
    // Bit 3 in both headers; the local crc and sizes are zero (the real
    // values follow the data in a descriptor, which the reader ignores).
    for at in [local(&b, 1), central(&b, 1)] {
        let flags = if at == local(&b, 1) { at + 6 } else { at + 8 };
        put16(&mut b, flags, 8);
    }
    let l = local(&b, 1);
    put32(&mut b, l + 14, 0);
    put32(&mut b, l + 18, 0);
    put32(&mut b, l + 22, 0);
    assert_eq!(read_all(&file(&b), 1).unwrap(), vec![7u8; 5000]);
    // The central crc is still enforced.
    let c = central(&b, 1);
    let crc = u32_at(&b, c + 16);
    put32(&mut b, c + 16, crc ^ 1);
    assert!(matches!(read_all(&file(&b), 1), Err(ZipError::CrcMismatch)));
}

#[test]
fn rejects_central_local_name_mismatch() {
    let mut b = two();
    let l = local(&b, 0);
    b[l + 30] = b'X';
    assert!(matches!(
        open_err(&b, BIG),
        ZipError::HeaderMismatch("name")
    ));
}

#[test]
fn rejects_central_local_size_and_method_mismatch() {
    let mut b = two();
    let l = local(&b, 0);
    put32(&mut b, l + 22, 99);
    assert!(matches!(open_err(&b, BIG), ZipError::HeaderMismatch(_)));
    let mut b = two();
    let l = local(&b, 0);
    put16(&mut b, l + 8, 8);
    assert!(matches!(
        open_err(&b, BIG),
        ZipError::HeaderMismatch("method")
    ));
    let mut b = two();
    let l = local(&b, 0);
    b[l] = b'Q';
    assert!(matches!(open_err(&b, BIG), ZipError::HeaderMismatch(_)));
}

#[test]
fn rejects_offset_past_end() {
    let mut b = two();
    let c = central(&b, 1);
    let len = b.len() as u32;
    put32(&mut b, c + 42, len);
    assert!(matches!(open_err(&b, BIG), ZipError::OutOfBounds));
    // A data range that runs past the end of the file.
    let mut b = two();
    let c = central(&b, 1);
    let l = local(&b, 1);
    put32(&mut b, c + 20, 0x00FF_FFFF);
    put32(&mut b, l + 18, 0x00FF_FFFF);
    assert!(matches!(open_err(&b, BIG), ZipError::OutOfBounds));
    // Near u32::MAX the additions must not wrap.
    let mut b = two();
    let c = central(&b, 1);
    put32(&mut b, c + 42, 0xFFFF_FFF0);
    assert!(matches!(open_err(&b, BIG), ZipError::OutOfBounds));
}

#[test]
fn rejects_overlapping_entries() {
    let mut b = two();
    // Entry 0 claims enough stored bytes to cover entry 1's local header.
    let span = (local(&b, 1) - local(&b, 0) - 30 - 5 + 10) as u32;
    let (c, l) = (central(&b, 0), local(&b, 0));
    for at in [c + 20, c + 24, l + 18, l + 22] {
        put32(&mut b, at, span);
    }
    assert!(matches!(open_err(&b, BIG), ZipError::Overlap));
}

#[test]
fn rejects_entry_overlapping_the_central_directory() {
    let mut b = two();
    let span = (u32_at(&b, eocd(&b) + 16) as usize - local(&b, 1) - 30 - 9 + 4) as u32;
    let (c, l) = (central(&b, 1), local(&b, 1));
    put16(&mut b, c + 10, 0);
    put16(&mut b, l + 8, 0);
    for at in [c + 20, c + 24, l + 18, l + 22] {
        put32(&mut b, at, span);
    }
    assert!(matches!(open_err(&b, BIG), ZipError::Overlap));
}

#[test]
fn rejects_duplicate_name() {
    let b = zip(&[
        (b"same", b"one", Store::Stored),
        (b"other", b"x", Store::Stored),
        (b"same", b"two", Store::Deflated),
    ]);
    assert!(matches!(open_err(&b, BIG), ZipError::DuplicateName));
}

#[test]
fn rejects_truncated_central_directory() {
    // The directory size claims less than the records need.
    let mut b = two();
    let e = eocd(&b);
    let size = u32_at(&b, e + 12);
    put32(&mut b, e + 12, size - 10);
    assert!(matches!(open_err(&b, BIG), ZipError::Truncated));
    // The directory runs into the end record.
    let mut b = two();
    let e = eocd(&b);
    put32(&mut b, e + 12, size + 10);
    assert!(matches!(open_err(&b, BIG), ZipError::Truncated));
    // The end record sits right behind the first record's fixed part.
    let b = two();
    let cut = central(&b, 0) + 46;
    let mut t = b[..cut].to_vec();
    t.extend_from_slice(&b[eocd(&b)..]);
    let e = eocd(&t);
    put32(&mut t, e + 12, 46);
    assert!(matches!(open_err(&t, BIG), ZipError::Truncated));
}

#[test]
fn rejects_zip64_marker() {
    let mut b = two();
    let e = eocd(&b);
    put16(&mut b, e + 10, 0xFFFF);
    assert!(matches!(open_err(&b, BIG), ZipError::Zip64));
    let mut b = two();
    let e = eocd(&b);
    put32(&mut b, e + 16, 0xFFFF_FFFF);
    assert!(matches!(open_err(&b, BIG), ZipError::Zip64));
    let mut b = two();
    let c = central(&b, 0);
    put32(&mut b, c + 24, 0xFFFF_FFFF);
    assert!(matches!(open_err(&b, BIG), ZipError::Zip64));
    // A zip64 end-of-central-directory locator in front of the end record.
    let b = two();
    let e = eocd(&b);
    let mut z = b[..e].to_vec();
    z.extend_from_slice(b"PK\x06\x07");
    z.extend_from_slice(&[0u8; 16]);
    z.extend_from_slice(&b[e..]);
    assert!(matches!(open_err(&z, BIG), ZipError::Zip64));
}

#[test]
fn rejects_multi_disk() {
    let mut b = two();
    let e = eocd(&b);
    put16(&mut b, e + 4, 1);
    assert!(matches!(open_err(&b, BIG), ZipError::MultiDisk));
    let mut b = two();
    let e = eocd(&b);
    put16(&mut b, e + 6, 1);
    assert!(matches!(open_err(&b, BIG), ZipError::MultiDisk));
    let mut b = two();
    let c = central(&b, 0);
    put16(&mut b, c + 34, 1);
    assert!(matches!(open_err(&b, BIG), ZipError::MultiDisk));
}

#[test]
fn encrypted_entry_reads_as_encrypted() {
    let mut b = two();
    for at in [central(&b, 0) + 8, local(&b, 0) + 6] {
        put16(&mut b, at, 1);
    }
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    assert!(a.entries()[0].encrypted);
    assert!(!a.entries()[1].encrypted);
    assert!(matches!(
        a.read(0, &mut Vec::new()),
        Err(ZipError::Encrypted)
    ));
    a.read(1, &mut Vec::new()).unwrap();
}

#[test]
fn other_method_reads_as_unsupported() {
    let mut b = two();
    for at in [central(&b, 0) + 10, local(&b, 0) + 8] {
        put16(&mut b, at, 12);
    }
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    assert_eq!(a.entries()[0].method, Method::Other(12));
    assert!(matches!(
        a.read(0, &mut Vec::new()),
        Err(ZipError::UnsupportedMethod(12))
    ));
}

/// Set the declared (uncompressed) size of entry `k` in both headers.
fn declare_size(b: &mut [u8], k: usize, size: u32) {
    let (c, l) = (central(b, k), local(b, k));
    put32(b, c + 24, size);
    put32(b, l + 22, size);
}

#[test]
fn rejects_more_data_than_declared() {
    let mut b = two();
    declare_size(&mut b, 1, 100); // really 5000
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    let mut got = 0usize;
    let r = a.read_to(1, &mut |c| {
        got += c.len();
        Ok(())
    });
    assert!(matches!(r, Err(ZipError::SizeMismatch)));
    assert!(
        got <= 100,
        "delivered {got} bytes for a declared size of 100"
    );
    let mut v = Vec::new();
    assert!(matches!(a.read(1, &mut v), Err(ZipError::SizeMismatch)));
    assert!(v.is_empty(), "read must not keep bytes after an error");
}

#[test]
fn rejects_less_data_than_declared() {
    let mut b = two();
    declare_size(&mut b, 1, 9000); // really 5000
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    assert!(matches!(
        a.read(1, &mut Vec::new()),
        Err(ZipError::SizeMismatch)
    ));
    // Stored: the two sizes must agree.
    let mut b = two();
    let c = central(&b, 0);
    let l = local(&b, 0);
    put32(&mut b, c + 24, 3);
    put32(&mut b, l + 22, 3);
    assert!(matches!(
        read_all(&file(&b), 0),
        Err(ZipError::SizeMismatch)
    ));
}

#[test]
fn rejects_bad_crc() {
    for k in [0, 1] {
        let mut b = two();
        let (c, l) = (central(&b, k), local(&b, k));
        let crc = u32_at(&b, c + 16);
        put32(&mut b, c + 16, crc ^ 0x8000_0000);
        put32(&mut b, l + 14, crc ^ 0x8000_0000);
        assert!(matches!(read_all(&file(&b), k), Err(ZipError::CrcMismatch)));
    }
}

#[test]
fn rejects_deflate_range_that_cuts_the_stream() {
    // The declared compressed size is one byte short of the real stream:
    // the decoder hits the end of its bounded range and must fail, not read
    // on into the next record.
    let mut b = two();
    let (c, l) = (central(&b, 1), local(&b, 1));
    let cs = u32_at(&b, c + 20);
    put32(&mut b, c + 20, cs - 1);
    put32(&mut b, l + 18, cs - 1);
    assert!(read_all(&file(&b), 1).is_err());
}

#[test]
fn read_to_streams_in_bounded_chunks() {
    let big = pseudo(1 << 20);
    let mut zeros_and_noise = pseudo(1 << 19);
    zeros_and_noise.extend(std::iter::repeat_n(0u8, 1 << 19));
    let b = zip(&[
        (b"stored", &big, Store::Stored),
        (b"deflated", &zeros_and_noise, Store::Deflated),
    ]);
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    for (i, want) in [&big, &zeros_and_noise].into_iter().enumerate() {
        let mut all = Vec::new();
        let mut chunks = 0;
        let n = a
            .read_to(i, &mut |c| {
                assert!(c.len() <= 64 * 1024, "chunk of {} bytes", c.len());
                all.extend_from_slice(c);
                chunks += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(n, want.len() as u64);
        assert_eq!(&all, want);
        assert!(chunks >= 16);
    }
    // The size and CRC checks fire at the end of the stream as well.
    let mut bad = b.clone();
    let c = central(&bad, 0);
    let l = local(&bad, 0);
    put32(&mut bad, c + 16, 0);
    put32(&mut bad, l + 14, 0);
    let f = file(&bad);
    let a = open(&f, BIG).unwrap();
    let mut got = 0;
    let r = a.read_to(0, &mut |c| {
        got += c.len();
        Ok(())
    });
    assert!(matches!(r, Err(ZipError::CrcMismatch)));
    assert_eq!(got, 1 << 20);
    // A sink error stops the read and surfaces as Io.
    let r = a.read_to(1, &mut |_| Err(std::io::Error::other("sink")));
    assert!(matches!(r, Err(ZipError::Io(_))));
}

#[test]
fn limits_entries_names_size_ratio() {
    let names: Vec<Vec<u8>> = (0..4).map(|i| format!("n{i}").into_bytes()).collect();
    let es: Vec<(&[u8], &[u8], Store)> = names
        .iter()
        .map(|n| (n.as_slice(), &b"x"[..], Store::Stored))
        .collect();
    let b = zip(&es);
    let lim = Limits { entries: 3, ..BIG };
    assert!(matches!(open_err(&b, lim), ZipError::TooManyEntries));
    assert!(open(&file(&b), Limits { entries: 4, ..BIG }).is_ok());
    // A lying end record: it says 1 entry, the directory holds 4.
    let mut lie = b.clone();
    let e = eocd(&lie);
    put16(&mut lie, e + 8, 1);
    put16(&mut lie, e + 10, 1);
    assert!(matches!(
        open_err(&lie, Limits { entries: 3, ..BIG }),
        ZipError::TooManyEntries
    ));
    // ... and below the limit the disagreement itself is rejected.
    assert!(matches!(open_err(&lie, BIG), ZipError::HeaderMismatch(_)));

    let long = zip(&[(&[b'a'; 40], b"x", Store::Stored)]);
    assert!(matches!(
        open_err(
            &long,
            Limits {
                name_bytes: 39,
                ..BIG
            }
        ),
        ZipError::NameTooLong
    ));
    assert!(
        open(
            &file(&long),
            Limits {
                name_bytes: 40,
                ..BIG
            }
        )
        .is_ok()
    );

    let big = zip(&[(b"f", &[1u8; 1000], Store::Stored)]);
    assert!(matches!(
        open_err(
            &big,
            Limits {
                entry_bytes: 999,
                ..BIG
            }
        ),
        ZipError::EntryTooLarge
    ));
    assert!(
        open(
            &file(&big),
            Limits {
                entry_bytes: 1000,
                ..BIG
            }
        )
        .is_ok()
    );

    let zeros = zip(&[(b"z", &vec![0u8; 100_000], Store::Deflated)]);
    let f = file(&zeros);
    let cs = open(&f, BIG).unwrap().entries()[0].compressed_size;
    assert!(cs > 0 && cs < 200);
    let exact = 100_000 / cs;
    assert!(matches!(
        open_err(
            &zeros,
            Limits {
                ratio: exact - 1,
                ..BIG
            }
        ),
        ZipError::RatioTooHigh
    ));
    assert!(
        open(
            &file(&zeros),
            Limits {
                ratio: exact + 1,
                ..BIG
            }
        )
        .is_ok()
    );
}

#[test]
fn ratio_with_zero_compressed_size_fails_closed() {
    // Declared size > 0 with no compressed bytes at all.
    let mut b = zip(&[(b"e", b"abc", Store::Deflated)]);
    let (c, l) = (central(&b, 0), local(&b, 0));
    put32(&mut b, c + 20, 0);
    put32(&mut b, l + 18, 0);
    assert!(matches!(open_err(&b, BIG), ZipError::RatioTooHigh));
    // An empty entry is fine.
    let b = zip(&[(b"e", b"", Store::Stored)]);
    assert!(open(&file(&b), BIG).is_ok());
}

#[test]
fn eocd_found_behind_a_comment() {
    let plain = two();
    // No comment: the record is the last 22 bytes.
    assert!(open(&file(&plain), BIG).is_ok());
    // The longest comment (16-bit length), full of fake end signatures.
    let mut b = plain.clone();
    let e = eocd(&b);
    put16(&mut b, e + 20, 65535);
    let mut comment = Vec::new();
    while comment.len() < 65535 {
        comment.extend_from_slice(b"PK\x05\x06junk");
    }
    comment.truncate(65535);
    b.extend_from_slice(&comment);
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    assert_eq!(a.entries().len(), 2);
    // A comment length that does not reach the end of the file is not an
    // end record.
    let mut bad = plain.clone();
    let e = eocd(&bad);
    put16(&mut bad, e + 20, 5);
    assert!(matches!(open_err(&bad, BIG), ZipError::NotZip));
}

#[test]
fn not_a_zip() {
    assert!(matches!(open_err(b"", BIG), ZipError::NotZip));
    assert!(matches!(open_err(b"PK\x05\x06", BIG), ZipError::NotZip));
    assert!(matches!(open_err(&[0u8; 5000], BIG), ZipError::NotZip));
    assert!(matches!(
        open_err(b"just some text, definitely not an archive", BIG),
        ZipError::NotZip
    ));
}

#[test]
fn empty_archive_is_valid() {
    let b = zip(&[]);
    let f = file(&b);
    let a = open(&f, BIG).unwrap();
    assert!(a.entries().is_empty());
}

#[test]
fn bad_index_is_an_error_not_a_panic() {
    let f = file(&two());
    let a = open(&f, BIG).unwrap();
    assert!(matches!(a.read(2, &mut Vec::new()), Err(ZipError::Io(_))));
}

#[test]
fn rejects_bytes_after_the_deflate_stream() {
    // Four junk bytes inside the declared compressed range, behind the end
    // of the deflate stream: the stream must consume the range exactly.
    let mut b = zip(&[(b"f", &[9u8; 3000], Store::Deflated)]);
    let cd = u32_at(&b, eocd(&b) + 16) as usize;
    b.splice(cd..cd, [1, 2, 3, 4]);
    let e = eocd(&b);
    put32(&mut b, e + 16, cd as u32 + 4);
    let (c, l) = (central(&b, 0), local(&b, 0));
    let cs = u32_at(&b, c + 20) + 4;
    put32(&mut b, c + 20, cs);
    put32(&mut b, l + 18, cs);
    assert!(matches!(
        read_all(&file(&b), 0),
        Err(ZipError::SizeMismatch)
    ));
}
