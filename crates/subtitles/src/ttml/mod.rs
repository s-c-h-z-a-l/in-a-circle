//! The TTML reader (TTML2, https://www.w3.org/TR/ttml2/). This module holds the
//! time expressions and the `tt` parameters so far; the document reader is
//! built on them.

// Nothing calls these outside the tests yet; the document reader does.
#![cfg_attr(not(test), allow(dead_code))]

mod time;

/// A construct the reader does not convert. The whole document is refused with
/// it: converting around it would give a wrong result, and a refusal says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unsupported(pub(crate) &'static str);
