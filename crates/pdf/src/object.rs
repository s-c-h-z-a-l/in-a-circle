//! The PDF file structure: ids, objects, streams, xref and trailer.

use std::io::{self, Write};

/// An xref entry holds the offset in exactly ten decimal digits (ISO 32000
/// 7.5.4), so the file must not grow past 10^10 - 1 bytes (~9.3 GiB). A
/// write that would start an object past it fails instead of producing a
/// table readers cannot parse.
const MAX_OFFSET: u64 = 9_999_999_999;

pub(crate) struct Objects<W: Write> {
    out: W,
    /// Bytes written so far: the offset of the next byte.
    pos: u64,
    /// `offsets[id - 1]` is where object `id` starts; 0 until written
    /// (offset 0 is the header, so it never names an object).
    offsets: Vec<u64>,
}

impl<W: Write> Objects<W> {
    pub(crate) fn new(out: W) -> io::Result<Self> {
        let mut o = Objects {
            out,
            pos: 0,
            offsets: Vec::new(),
        };
        // The comment line of four bytes >= 0x80 tells transfer programs
        // the file is binary (ISO 32000 7.5.2).
        o.put(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")?;
        Ok(o)
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.out.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    /// Reserve the next object id.
    pub(crate) fn alloc(&mut self) -> u32 {
        self.offsets.push(0);
        self.offsets.len() as u32
    }

    fn begin(&mut self, id: u32) -> io::Result<()> {
        let slot = self
            .offsets
            .get_mut(id as usize - 1)
            .filter(|o| **o == 0)
            .ok_or_else(|| io::Error::other("object id not reserved or written twice"))?;
        if self.pos > MAX_OFFSET {
            return Err(io::Error::other("PDF too large for the xref table"));
        }
        *slot = self.pos;
        self.put(format!("{id} 0 obj\n").as_bytes())
    }

    /// `<< dict >>` object; `dict` is the entries without the brackets.
    pub(crate) fn write_object(&mut self, id: u32, dict: &str) -> io::Result<()> {
        self.begin(id)?;
        self.put(format!("<<{dict}>>\nendobj\n").as_bytes())
    }

    /// Stream object whose data is the concatenation of `parts`. `/Length`
    /// is the sum of the part sizes and is checked against the bytes the
    /// output actually took, so it cannot disagree with the file.
    pub(crate) fn write_stream<'a, I>(&mut self, id: u32, dict: &str, parts: I) -> io::Result<()>
    where
        I: IntoIterator<Item = &'a [u8]> + Clone,
    {
        let len: u64 = parts.clone().into_iter().map(|p| p.len() as u64).sum();
        self.begin(id)?;
        self.put(format!("<<{dict} /Length {len} >>\nstream\n").as_bytes())?;
        let start = self.pos;
        for p in parts {
            self.put(p)?;
        }
        if self.pos - start != len {
            return Err(io::Error::other("stream length mismatch"));
        }
        self.put(b"\nendstream\nendobj\n")
    }

    /// Cross-reference table and trailer; returns the output.
    pub(crate) fn finish(mut self, root: u32) -> io::Result<W> {
        let xref_at = self.pos;
        let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", self.offsets.len() + 1);
        for (i, off) in self.offsets.iter().enumerate() {
            if *off == 0 {
                return Err(io::Error::other(format!(
                    "object {} was never written",
                    i + 1
                )));
            }
            table.push_str(&format!("{off:010} 00000 n \n"));
        }
        table.push_str(&format!(
            "trailer\n<</Size {} /Root {root} 0 R>>\nstartxref\n{xref_at}\n%%EOF\n",
            self.offsets.len() + 1
        ));
        self.put(table.as_bytes())?;
        self.out.flush()?;
        Ok(self.out)
    }
}
