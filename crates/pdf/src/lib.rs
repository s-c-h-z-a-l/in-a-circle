//! Streaming PDF writer: one image per page, JPEG and PNG data passed
//! through untouched (no pixel is decoded or re-encoded).
//!
//! Two layers. `object` is the PDF file structure: object ids, objects and
//! streams written straight to the output, the cross-reference table and
//! the trailer. `image` is the page layer built only on it. Memory is the
//! Kids list (4 bytes a page) and the offsets (8 bytes an object); the
//! image data goes to the output as it is.

mod image;
mod object;

use object::Objects;
use std::io::{self, Write};

/// Object id of the Pages node, allocated first so every page can name its
/// parent while the Kids list is still growing.
const PAGES_ID: u32 = 1;

pub struct Writer<W: Write> {
    objs: Objects<W>,
    kids: Vec<u32>,
}

impl<W: Write> Writer<W> {
    /// Start a document: writes the header to `out`.
    pub fn new(out: W) -> io::Result<Self> {
        let mut objs = Objects::new(out)?;
        let pages = objs.alloc();
        debug_assert_eq!(pages, PAGES_ID);
        Ok(Writer {
            objs,
            kids: Vec::new(),
        })
    }

    /// Append a page showing `image`, whose file is `bytes` (the slice the
    /// image was probed from; the ranges in a PNG's info index it). The page
    /// is `width x height` points, one pixel per point.
    pub fn add_page(&mut self, bytes: &[u8], image: &probe::Image) -> io::Result<()> {
        let page = image::add_page(&mut self.objs, PAGES_ID, bytes, image)?;
        self.kids.push(page);
        Ok(())
    }

    /// Write the Pages node, the Catalog, the cross-reference table and the
    /// trailer, and return the output. A document without pages is an
    /// error: readers reject an empty page tree.
    pub fn finish(mut self) -> io::Result<W> {
        if self.kids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a PDF needs at least one page",
            ));
        }
        let mut kids = String::new();
        for k in &self.kids {
            kids.push_str(&format!("{k} 0 R "));
        }
        self.objs.write_object(
            PAGES_ID,
            &format!(
                "/Type /Pages /Kids [{}] /Count {}",
                kids.trim_end(),
                self.kids.len()
            ),
        )?;
        let catalog = self.objs.alloc();
        self.objs
            .write_object(catalog, &format!("/Type /Catalog /Pages {PAGES_ID} 0 R"))?;
        self.objs.finish(catalog)
    }
}
