use std::io::{self, Write};

const MS_PER_SECOND: u64 = 1_000;
const MS_PER_MINUTE: u64 = 60 * MS_PER_SECOND;
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;

/// Writes `ms` as `HH:MM:SS<sep>mmm`, `sep` being `,` (SRT) or `.` (WebVTT).
/// Hours get at least two digits. The readers never pass a value past
/// `MAX_MS`, so the hour field stays two digits; a larger value still prints
/// in full rather than panicking.
pub(crate) fn write_time(out: &mut impl Write, ms: u64, sep: char) -> io::Result<()> {
    let hours = ms / MS_PER_HOUR;
    let rest = ms % MS_PER_HOUR;
    let minutes = rest / MS_PER_MINUTE;
    let seconds = rest % MS_PER_MINUTE / MS_PER_SECOND;
    let millis = rest % MS_PER_SECOND;
    write!(out, "{hours:02}:{minutes:02}:{seconds:02}{sep}{millis:03}")
}
