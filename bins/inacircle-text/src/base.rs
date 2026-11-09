//! The items the text family's modules share through the crate root
//! (`crate::LIMITS`...), in a file of their own: the EPUB to Markdown fuzz
//! target includes it with the modules that use them, so they are the
//! shipped items, not copies.

use std::io;

use doc::{LimitError, Limits, ReadError};

/// The structure budgets of one conversion (`document_*` budgets, see
/// [`Limits::DEFAULT`] for the values and their reasons). The input is read
/// at most `input_bytes` (+1 to detect more); past any budget the input
/// fails (`fail`, nothing staged) and the batch goes on. The sandbox's
/// RLIMIT_DATA (>= 1 GiB) is the second net.
pub(crate) const LIMITS: Limits = Limits::DEFAULT;

/// Why a conversion stopped.
pub(crate) enum ConvertError {
    NotText,
    Limit(LimitError),
    /// Any other failure, as the `fail` reason.
    Other(String),
}

impl From<epub::BookError> for ConvertError {
    fn from(e: epub::BookError) -> Self {
        match e {
            epub::BookError::Limit(l) => ConvertError::Limit(l),
            other => ConvertError::Other(other.to_string()),
        }
    }
}

impl From<ReadError> for ConvertError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::NotText => ConvertError::NotText,
            ReadError::Limit(l) => ConvertError::Limit(l),
        }
    }
}

impl From<LimitError> for ConvertError {
    fn from(e: LimitError) -> Self {
        ConvertError::Limit(e)
    }
}

/// Largest image: 64 MiB, the same as the CBZ and PDF image limit, so an
/// image one action takes, the others take too. Past it that image fails.
pub(crate) const IMAGE_BYTES: u64 = 64 << 20;

/// Most pixels of one image: 180 million, the PDF join's limit
/// (`docs/resource-limits.md`; Pillow rejects above 179 MP). The header
/// check refuses more before anything else is read.
pub(crate) const MAX_PIXELS: u64 = 180_000_000;

/// The soft `RLIMIT_FSIZE` of this process, the largest file it may write
/// (the engine sets it to 16 x the input, 256 MiB to 16 GiB). `None` is
/// unlimited. A failing `getrlimit` is an error, so the input fails closed
/// instead of writing without a known ceiling. `getrlimit` on itself is
/// allowed by the sandbox's seccomp policy.
pub(crate) fn output_cap() -> Result<Option<u64>, ConvertError> {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid, writable `rlimit` for the whole call, and
    // RLIMIT_FSIZE is a valid resource; the call only writes into `rl`.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut rl) };
    if rc != 0 {
        return Err(ConvertError::Other(format!(
            "cannot read the output size limit: {}",
            io::Error::last_os_error()
        )));
    }
    Ok((rl.rlim_cur != libc::RLIM_INFINITY).then_some(rl.rlim_cur))
}

/// The copy for an output past the ceiling (N rounded down, so it never
/// overstates the limit; in KiB below 1 MiB, so it never reads "0 MiB").
/// If the ceiling cannot be read now, the generic copy.
pub(crate) fn too_large(what: &str) -> ConvertError {
    match output_cap() {
        Ok(Some(c)) if c >= 1 << 20 => {
            ConvertError::Other(format!("{what} would be larger than {} MiB", c >> 20))
        }
        Ok(Some(c)) => ConvertError::Other(format!("{what} would be larger than {} KiB", c >> 10)),
        _ => ConvertError::Other(format!("{what} would be too large")),
    }
}

pub(crate) enum Outcome {
    Done,
    Unsupported,
    Failed,
}
