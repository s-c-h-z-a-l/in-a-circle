//! TTML time expressions and the `tt` parameters that give them a meaning.
//! The rules are TTML2 (https://www.w3.org/TR/ttml2/): the grammar and the value
//! constraints of 12.3.1, the parameters of 7.2.4 to 7.2.11, and the media and
//! SMPTE time bases of Appendix I.2 and I.3.
//!
//! The arithmetic is exact. Every value is a rational until its last step,
//! which rounds half up to the nearest millisecond, so no float is involved.

use super::Unsupported;
use crate::MAX_MS;

/// Frames per second when `ttp:frameRate` is absent: 30 (TTML2 7.2.5).
const DEFAULT_FRAME_RATE: u32 = 30;
/// Sub-frames per frame when `ttp:subFrameRate` is absent: 1 (7.2.9).
const DEFAULT_SUB_FRAME_RATE: u32 = 1;
/// Largest minute in a clock time: minutes are `[0…59]` (12.3.1).
const MAX_MINUTE: u128 = 59;
/// Largest second in a clock time, leap seconds aside (12.3.1).
const MAX_SECOND: u128 = 59;
/// The leap second. Presentation reads it as 59 seconds (12.3.1).
const LEAP_SECOND: u128 = 60;
const SECONDS_PER_MINUTE: u128 = 60;
const SECONDS_PER_HOUR: u128 = 3_600;
const MS_PER_SECOND: u128 = 1_000;
/// Fraction digits kept. Nine digits is a nanosecond, a millionth of the
/// millisecond unit. shortcut: digits past the ninth are dropped; a rounded
/// millisecond can change only for an hour, minute or frame count whose value
/// sits within a nanosecond of a half-millisecond boundary, and a decimal count
/// of seconds cannot, so upgrade if such a file turns up.
const FRACTION_DIGITS: usize = 9;

/// The `tt` parameters that decide how time expressions are read. Built by
/// [`Params::from_tt`], which refuses what the reader cannot convert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Params {
    /// `ttp:frameRate`: frames per second before the multiplier.
    frame_rate: u32,
    /// `ttp:frameRateMultiplier` as numerator and denominator (1:1 by default).
    frame_rate_mul: (u32, u32),
    /// `ttp:subFrameRate`: sub-frames per frame.
    sub_frame_rate: u32,
    /// `ttp:tickRate`. A rational because its default is the effective frame
    /// rate times the sub-frame rate, which is 30000/1001 for NTSC (7.2.10).
    tick_rate: Ratio,
    /// The SMPTE time base (I.3); otherwise the media time base (I.2).
    smpte: bool,
}

/// Why a time expression is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeError {
    /// Not the grammar of 12.3.1.
    Syntax,
    /// The grammar is right, but a field breaks its constraint, or the time is
    /// 100 hours or more ([`MAX_MS`]), or the arithmetic does not fit in `u128`.
    /// The expression is skipped; the document is still read.
    OutOfRange,
    /// A form the reader does not convert. The whole document is refused,
    /// because a time converted from a different time base would be wrong.
    Unsupported(Unsupported),
}

impl Params {
    /// Reads the parameters from the `tt` element's attributes, as (local name,
    /// value) pairs in the TTML parameter namespace. An absent parameter takes
    /// its default. A value that breaks its syntax is refused, and so is a time
    /// base or marker mode whose time this reader cannot convert.
    pub(crate) fn from_tt(attrs: &[(&str, &str)]) -> Result<Params, Unsupported> {
        let frame_rate_given = attribute(attrs, "frameRate");
        let frame_rate = match frame_rate_given {
            Some(v) => positive(v, "invalid ttp:frameRate")?,
            None => DEFAULT_FRAME_RATE,
        };
        let frame_rate_mul = match attribute(attrs, "frameRateMultiplier") {
            Some(v) => multiplier(v)?,
            None => (1, 1),
        };
        let sub_frame_rate = match attribute(attrs, "subFrameRate") {
            Some(v) => positive(v, "invalid ttp:subFrameRate")?,
            None => DEFAULT_SUB_FRAME_RATE,
        };
        let tick_rate = match attribute(attrs, "tickRate") {
            Some(v) => Ratio::int(u128::from(positive(v, "invalid ttp:tickRate")?)),
            // 7.2.10: with a frame rate given, ticks are sub-frames of the
            // effective frame rate (frame rate times multiplier); without one,
            // a tick is a second.
            None if frame_rate_given.is_some() => Ratio::new(
                u128::from(frame_rate) * u128::from(frame_rate_mul.0) * u128::from(sub_frame_rate),
                u128::from(frame_rate_mul.1),
            )
            .map_err(|_| Unsupported("invalid ttp:frameRateMultiplier"))?,
            None => Ratio::int(1),
        };

        let smpte = match attribute(attrs, "timeBase") {
            // 7.2.11: the default is media.
            None | Some("media") => false,
            Some("smpte") => true,
            // 7.2.11: a clock time base is real-world time, not media time, and
            // converting it to media time is not something this reader does.
            Some("clock") => return Err(Unsupported("clock time base")),
            Some(_) => return Err(Unsupported("invalid ttp:timeBase")),
        };
        // 7.2.4: nonDrop is the default. dropNTSC and dropPAL apply to the SMPTE
        // time base only; under the media time base the parameter has no effect.
        match attribute(attrs, "dropMode").unwrap_or("nonDrop") {
            "nonDrop" => {}
            "dropNTSC" | "dropPAL" if smpte => {
                return Err(Unsupported("drop-frame time code"));
            }
            "dropNTSC" | "dropPAL" => {}
            _ => return Err(Unsupported("invalid ttp:dropMode")),
        }
        // 7.2.7: the default is discontinuous, and a discontinuous SMPTE time
        // has no media time: its expressions name markers the document does not
        // carry (I.3). Only `continuous` gives the SMPTE formula below.
        match attribute(attrs, "markerMode").unwrap_or("discontinuous") {
            "continuous" => {}
            "discontinuous" if smpte => {
                return Err(Unsupported("discontinuous SMPTE markers"));
            }
            "discontinuous" => {}
            _ => return Err(Unsupported("invalid ttp:markerMode")),
        }

        Ok(Params {
            frame_rate,
            frame_rate_mul,
            sub_frame_rate,
            tick_rate,
            smpte,
        })
    }
}

/// Reads one time expression to whole milliseconds, rounded half up. The result
/// is the expression's own offset; the caller adds the reference begin of its
/// time container (I.2.2).
pub(crate) fn parse_time(expr: &str, p: &Params) -> Result<u64, TimeError> {
    if expr.starts_with("wallclock(") {
        // 12.3.1: a wall-clock time needs the clock time base, which is refused.
        return Err(TimeError::Unsupported(Unsupported("wall-clock time")));
    }
    let count = if expr.contains(':') {
        clock(expr, p)?
    } else {
        offset(expr)?
    };
    let effective = Ratio::new(
        u128::from(p.frame_rate) * u128::from(p.frame_rate_mul.0),
        u128::from(p.frame_rate_mul.1),
    )?;
    match count {
        Count::Ticks(ticks) => {
            // The tick has no definition under the SMPTE time base (7.2.10 says
            // "if the media time base applies"), so the expression is refused.
            if p.smpte {
                return Err(TimeError::Unsupported(Unsupported(
                    "tick count under the SMPTE time base",
                )));
            }
            // I.2.2: M = ticks / tickRate.
            ticks.over(p.tick_rate)?.ms()
        }
        Count::Time { seconds, frames } => {
            let time = if p.smpte {
                // I.3: S = (countedFrames + subFrames / subFrameRate) / effective
                // rate, countedFrames = seconds * frameRate + frames. The frames
                // count at the nominal rate, so 30 fps NTSC drifts against
                // real time: one minute of such timecode is 60.06 s.
                seconds
                    .times(Ratio::int(u128::from(p.frame_rate)))?
                    .plus(frames)?
                    .over(effective)?
            } else {
                // I.2.2: M = seconds + (frames + subFrames / subFrameRate) / effective rate.
                seconds.plus(frames.over(effective)?)?
            };
            time.ms()
        }
    }
}

/// What an expression counts before the rates apply.
enum Count {
    /// Seconds of the time line, and frames (frames plus sub-frames over the
    /// sub-frame rate, both in frames).
    Time { seconds: Ratio, frames: Ratio },
    /// A count of ticks, the `t` metric.
    Ticks(Ratio),
}

/// `hours ":" minutes ":" seconds ( fraction | ":" frames ( "." sub-frames )? )?`
/// (12.3.1). Hours take two digits or more; minutes, seconds and frames at
/// least two; sub-frames one or more.
fn clock(expr: &str, p: &Params) -> Result<Count, TimeError> {
    let mut parts = expr.split(':');
    let (Some(hours), Some(minutes), Some(seconds)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(TimeError::Syntax);
    };
    let frames_text = parts.next();
    if parts.next().is_some() {
        return Err(TimeError::Syntax);
    }
    let hours = digits(hours, 2, usize::MAX)?;
    let minutes = digits(minutes, 2, 2)?;
    if minutes > MAX_MINUTE {
        return Err(TimeError::OutOfRange);
    }
    // A fraction and a frame count are alternatives in the grammar.
    let (whole, fraction) = match seconds.split_once('.') {
        Some((whole, fraction)) if frames_text.is_none() => (whole, decimal(fraction)?),
        Some(_) => return Err(TimeError::Syntax),
        None => (seconds, Ratio::int(0)),
    };
    let second = match digits(whole, 2, 2)? {
        n @ 0..=MAX_SECOND => n,
        // 12.3.1: 60 is a leap second, and presentation reads it as 59. A
        // fraction past 60 breaks the constraint [0, 60].
        LEAP_SECOND if fraction.is_zero() => MAX_SECOND,
        _ => return Err(TimeError::OutOfRange),
    };

    let frames = match frames_text {
        None => Ratio::int(0),
        Some(text) => {
            let (frame, sub) = match text.split_once('.') {
                Some((frame, sub)) => (frame, Some(sub)),
                None => (text, None),
            };
            let frame = digits(frame, 2, usize::MAX)?;
            // 12.3.1: frames are [0…F-1], F the frame rate.
            if frame >= u128::from(p.frame_rate) {
                return Err(TimeError::OutOfRange);
            }
            let sub = match sub {
                None => Ratio::int(0),
                Some(sub) => {
                    let sub = digits(sub, 1, usize::MAX)?;
                    // 12.3.1: sub-frames are [0…S-1], S the sub-frame rate.
                    if sub >= u128::from(p.sub_frame_rate) {
                        return Err(TimeError::OutOfRange);
                    }
                    Ratio::new(sub, u128::from(p.sub_frame_rate))?
                }
            };
            Ratio::int(frame).plus(sub)?
        }
    };

    let seconds = Ratio::int(hours)
        .times(Ratio::int(SECONDS_PER_HOUR))?
        .plus(Ratio::int(minutes * SECONDS_PER_MINUTE + second))?
        .plus(fraction)?;
    Ok(Count::Time { seconds, frames })
}

/// `time-count fraction? metric`, the metrics `h`, `m`, `s`, `ms`, `f` and `t`
/// (12.3.1).
fn offset(expr: &str) -> Result<Count, TimeError> {
    let end = expr
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(expr.len());
    let (number, metric) = expr.split_at(end);
    let count = match number.split_once('.') {
        None => Ratio::int(digits(number, 1, usize::MAX)?),
        Some((whole, fraction)) => {
            Ratio::int(digits(whole, 1, usize::MAX)?).plus(decimal(fraction)?)?
        }
    };
    let (seconds, frames) = match metric {
        "h" => (count.times(Ratio::int(SECONDS_PER_HOUR))?, Ratio::int(0)),
        "m" => (count.times(Ratio::int(SECONDS_PER_MINUTE))?, Ratio::int(0)),
        "s" => (count, Ratio::int(0)),
        "ms" => (count.over(Ratio::int(MS_PER_SECOND))?, Ratio::int(0)),
        "f" => (Ratio::int(0), count),
        "t" => return Ok(Count::Ticks(count)),
        _ => return Err(TimeError::Syntax),
    };
    Ok(Count::Time { seconds, frames })
}

/// `digit+` with a length in `min..=max`, as a value. Anything that is not a
/// digit is a syntax error; a value past `u128` is out of range.
fn digits(s: &str, min: usize, max: usize) -> Result<u128, TimeError> {
    if !(min..=max).contains(&s.len()) || !is_digits(s) {
        return Err(TimeError::Syntax);
    }
    s.bytes()
        .try_fold(0u128, |n, b| {
            n.checked_mul(10)?.checked_add(u128::from(b - b'0'))
        })
        .ok_or(TimeError::OutOfRange)
}

/// The digits after a decimal point, as a fraction of one unit. Only the first
/// [`FRACTION_DIGITS`] count; the rest is never converted to a value, so a long
/// fraction cannot overflow.
fn decimal(s: &str) -> Result<Ratio, TimeError> {
    if !is_digits(s) {
        return Err(TimeError::Syntax);
    }
    let kept = &s[..s.len().min(FRACTION_DIGITS)];
    let scale = (0..kept.len()).fold(1u128, |n, _| n * 10);
    Ratio::new(digits(kept, 1, usize::MAX)?, scale)
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The value of the attribute `name`, if the element has it.
fn attribute<'a>(attrs: &[(&str, &'a str)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| *n == name).map(|&(_, v)| v)
}

/// A `<digit>+` value above zero (7.2.5, 7.2.9, 7.2.10). Parsing a `u32` alone
/// would take a sign, so the digits are checked first.
fn positive(v: &str, reason: &'static str) -> Result<u32, Unsupported> {
    match v.parse::<u32>() {
        Ok(n) if is_digits(v) && n > 0 => Ok(n),
        _ => Err(Unsupported(reason)),
    }
}

/// `numerator lwsp denominator`, both above zero (7.2.6). `lwsp` is one or
/// more of space, tab, CR and LF (10.3.23, 10.3.41); nothing may come before
/// the numerator or after the denominator.
fn multiplier(v: &str) -> Result<(u32, u32), Unsupported> {
    const REASON: &str = "invalid ttp:frameRateMultiplier";
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let (numerator, rest) = v.split_once(is_space).ok_or(Unsupported(REASON))?;
    Ok((
        positive(numerator, REASON)?,
        positive(rest.trim_start_matches(is_space), REASON)?,
    ))
}

/// The largest common divisor. `gcd(0, d)` is `d`, so zero reduces to `0/1`.
fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn checked(v: Option<u128>) -> Result<u128, TimeError> {
    v.ok_or(TimeError::OutOfRange)
}

/// An exact non-negative rational, `num / den`, kept reduced. Every operation
/// is checked: an intermediate past `u128` is out of range, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ratio {
    num: u128,
    den: u128,
}

impl Ratio {
    fn new(num: u128, den: u128) -> Result<Ratio, TimeError> {
        // Zero is not reachable from validated parameters. It is refused as
        // out of range rather than divided by.
        if den == 0 {
            return Err(TimeError::OutOfRange);
        }
        let g = gcd(num, den);
        Ok(Ratio {
            num: num / g,
            den: den / g,
        })
    }

    fn int(n: u128) -> Ratio {
        Ratio { num: n, den: 1 }
    }

    fn is_zero(self) -> bool {
        self.num == 0
    }

    fn plus(self, other: Ratio) -> Result<Ratio, TimeError> {
        let g = gcd(self.den, other.den);
        let left = checked(self.num.checked_mul(other.den / g))?;
        let right = checked(other.num.checked_mul(self.den / g))?;
        let den = checked((self.den / g).checked_mul(other.den))?;
        Ratio::new(checked(left.checked_add(right))?, den)
    }

    fn times(self, other: Ratio) -> Result<Ratio, TimeError> {
        Ratio::new(
            checked(self.num.checked_mul(other.num))?,
            checked(self.den.checked_mul(other.den))?,
        )
    }

    fn over(self, other: Ratio) -> Result<Ratio, TimeError> {
        if other.is_zero() {
            return Err(TimeError::OutOfRange);
        }
        Ratio::new(
            checked(self.num.checked_mul(other.den))?,
            checked(self.den.checked_mul(other.num))?,
        )
    }

    /// Whole milliseconds, rounded half up: floor(x + 1/2) for x = num * 1000 /
    /// den, computed as (2 * num * 1000 + den) / (2 * den) so no half is lost.
    /// A value at or past [`MAX_MS`] is out of range.
    fn ms(self) -> Result<u64, TimeError> {
        let twice_ms = checked(self.num.checked_mul(2 * MS_PER_SECOND))?;
        let rounded = checked(twice_ms.checked_add(self.den))? / checked(self.den.checked_mul(2))?;
        let ms = u64::try_from(rounded).map_err(|_| TimeError::OutOfRange)?;
        if ms > MAX_MS {
            return Err(TimeError::OutOfRange);
        }
        Ok(ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parameters from `attrs`, which must be valid.
    fn params(attrs: &[(&str, &str)]) -> Params {
        Params::from_tt(attrs).expect("valid parameters")
    }

    /// The milliseconds of `expr` under `attrs`.
    fn read(expr: &str, attrs: &[(&str, &str)]) -> Result<u64, TimeError> {
        parse_time(expr, &params(attrs))
    }

    const NTSC: &[(&str, &str)] = &[("frameRate", "30"), ("frameRateMultiplier", "1000 1001")];
    const SMPTE: &[(&str, &str)] = &[
        ("timeBase", "smpte"),
        ("markerMode", "continuous"),
        ("frameRate", "30"),
        ("frameRateMultiplier", "1000 1001"),
    ];

    /// The clock-time forms (12.3.1): hours of two digits or more, minutes and
    /// seconds of two, a fraction of any length, and frames with optional
    /// sub-frames. Default parameters: 30 fps, one sub-frame, one tick a second.
    #[test]
    fn clock_time_forms_read_seconds_and_fractions() {
        let cases = [
            ("00:00:01", 1_000),
            ("00:00:01.5", 1_500),
            ("00:00:01.500", 1_500),
            ("00:01:02.003", 62_003),
            ("10:00:00", 36_000_000),
            ("000:00:01", 1_000),
            ("099:00:00", 356_400_000),
            ("99:59:59.999", 359_999_999),
            ("00:00:01.123456789999", 1_123),
            ("00:00:00.0000000005", 0),
        ];
        for (expr, ms) in cases {
            assert_eq!(read(expr, &[]), Ok(ms), "{expr}");
        }
    }

    /// The offset-time forms (12.3.1): every metric, a fraction before the
    /// metric, and the defaults that the metric needs.
    #[test]
    fn offset_time_forms_read_their_metric() {
        type Attrs<'a> = &'a [(&'a str, &'a str)];
        let cases: [(&str, Attrs, u64); 9] = [
            ("10.5s", &[], 10_500),
            ("2m", &[], 120_000),
            ("1h", &[], 3_600_000),
            ("500ms", &[], 500),
            ("1.5s", &[], 1_500),
            ("25f", &[("frameRate", "25")], 1_000),
            ("1.5f", &[("frameRate", "25")], 60),
            ("90000t", &[("tickRate", "90000")], 1_000),
            ("1.5t", &[], 1_500),
        ];
        for (expr, attrs, ms) in cases {
            assert_eq!(read(expr, attrs), Ok(ms), "{expr}");
        }
    }

    /// Frames count at the frame rate: `00:00:01:15` is 1.6 s at 25 fps. The
    /// frame code may have three digits, and sub-frames divide one frame.
    #[test]
    fn frame_codes_use_the_frame_rate() {
        let at_25 = [("frameRate", "25")];
        assert_eq!(read("00:00:01:15", &at_25), Ok(1_600));
        assert_eq!(read("00:00:01:015", &at_25), Ok(1_600));
        assert_eq!(read("00:00:00:24", &at_25), Ok(960));
        let at_25_with_ten_sub_frames = [("frameRate", "25"), ("subFrameRate", "10")];
        assert_eq!(read("00:00:01:15.5", &at_25_with_ten_sub_frames), Ok(1_620));
    }

    /// The parameter defaults (7.2.4 to 7.2.11) and the tick rate default on
    /// both branches: with a frame rate, the effective frame rate times the
    /// sub-frame rate; without one, one tick a second.
    #[test]
    fn parameters_take_the_standard_defaults() {
        assert_eq!(
            params(&[]),
            Params {
                frame_rate: 30,
                frame_rate_mul: (1, 1),
                sub_frame_rate: 1,
                tick_rate: Ratio::int(1),
                smpte: false,
            }
        );
        assert_eq!(params(&[("frameRate", "25")]).tick_rate, Ratio::int(25));
        assert_eq!(
            params(&[("frameRate", "25"), ("subFrameRate", "4")]).tick_rate,
            Ratio::int(100)
        );
        assert_eq!(
            params(&[("subFrameRate", "4")]).tick_rate,
            Ratio::int(1),
            "no frame rate: one tick a second"
        );
        // 7.2.6 and 7.2.10: the multiplier is part of the effective frame rate.
        assert_eq!(
            params(NTSC).tick_rate,
            Ratio {
                num: 30_000,
                den: 1_001,
            }
        );
        assert_eq!(
            params(&[("tickRate", "90000")]).tick_rate,
            Ratio::int(90_000)
        );
        // With no frame rate given, 30 fps is the frame count's rate, and a
        // missing frame rate does not change the tick.
        assert_eq!(read("00:00:01:15", &[]), Ok(1_500));
    }

    /// 29.97 fps: `00:00:01:15` is 1 + 15 * 1001 / 30000 = 1.5005 s, which is
    /// 1500.5 ms, a tie that rounds up to 1501 (half up, as everywhere here).
    #[test]
    fn ntsc_frame_rate_rounds_the_tie_up() {
        assert_eq!(read("00:00:01:15", NTSC), Ok(1_501));
        // Two spaces are one lwsp run (10.3.23), so the same rate.
        assert_eq!(
            read(
                "00:00:01:15",
                &[("frameRate", "30"), ("frameRateMultiplier", "1000  1001")]
            ),
            Ok(1_501)
        );
    }

    /// Half a millisecond rounds up, anything less rounds down.
    #[test]
    fn half_millisecond_boundaries_round_half_up() {
        assert_eq!(read("00:00:00.0004", &[]), Ok(0));
        assert_eq!(read("00:00:00.0005", &[]), Ok(1));
        assert_eq!(read("00:00:00.0015", &[]), Ok(2));
        assert_eq!(read("00:00:00.0025", &[]), Ok(3));
        // One frame at 2000 fps is half a millisecond; three frames, 1.5 ms.
        assert_eq!(read("00:00:00:01", &[("frameRate", "2000")]), Ok(1));
        assert_eq!(read("00:00:00:03", &[("frameRate", "2000")]), Ok(2));
        assert_eq!(read("00:00:00:01", &[("frameRate", "4000")]), Ok(0));
        assert_eq!(read("1.5ms", &[]), Ok(2));
    }

    /// 60 seconds is a leap second, read as 59 (12.3.1). A fraction past it,
    /// or a larger second, breaks the constraint [0, 60].
    #[test]
    fn sixty_seconds_is_a_leap_second_read_as_fifty_nine() {
        assert_eq!(read("00:00:60", &[]), Ok(59_000));
        assert_eq!(read("00:00:60.000", &[]), Ok(59_000));
        assert_eq!(read("00:00:60.5", &[]), Err(TimeError::OutOfRange));
        assert_eq!(read("00:00:61", &[]), Err(TimeError::OutOfRange));
    }

    /// The constraints of 12.3.1: minutes [0, 59], frames [0, F-1], sub-frames
    /// [0, S-1]. A value past its constraint is out of range, not wrapped.
    #[test]
    fn field_ranges_are_checked() {
        assert_eq!(read("00:60:00", &[]), Err(TimeError::OutOfRange));
        assert_eq!(
            read("00:00:00:25", &[("frameRate", "25")]),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            read("00:00:00:24", &[("frameRate", "25")]),
            Ok(960),
            "the last frame is in range"
        );
        let four_sub_frames = [("subFrameRate", "4")];
        assert_eq!(
            read("00:00:00:00.4", &four_sub_frames),
            Err(TimeError::OutOfRange)
        );
        // Three of four sub-frames at 30 fps: 0.75 / 30 s.
        assert_eq!(read("00:00:00:00.3", &four_sub_frames), Ok(25));
    }

    /// The SMPTE time base counts frames at the nominal rate (I.3). One minute
    /// of 29.97 fps timecode is 1800 frames at 30000/1001 per second, which is
    /// 60.06 s; the media time base gives 60 s.
    #[test]
    fn smpte_time_counts_frames_at_the_nominal_rate() {
        assert_eq!(read("00:01:00:00", SMPTE), Ok(60_060));
        assert_eq!(read("00:01:00:00", NTSC), Ok(60_000));
        assert_eq!(read("00:00:01:00", SMPTE), Ok(1_001));
        assert_eq!(read("1s", SMPTE), Ok(1_001));
    }

    /// Syntax the grammar of 12.3.1 does not allow: a short field, a missing
    /// field, a stray character, a space, an empty value, a sign.
    #[test]
    fn syntax_errors_are_refused_as_syntax() {
        let cases = [
            "1:2:3",
            "10",
            "10x",
            " 10s",
            "10 s",
            "5 ms",
            "5\tms",
            "",
            "10.5",
            "1:00:00",
            "00:0:01",
            "00:00:1",
            "00:00:01:1",
            "00:00:01.",
            "00:00:01.5.5",
            "00:00:01.5:15",
            "00:00:01:15.5.5",
            "10:00",
            "00:00:01:15:5",
            "1:2:3:4:5",
            ".5s",
            "5.s",
            "5..5s",
            "+5s",
            "5S",
            "5mss",
        ];
        for expr in cases {
            assert_eq!(read(expr, &[]), Err(TimeError::Syntax), "{expr:?}");
        }
    }

    /// A value that is well formed but out of range: a time from 100 hours on,
    /// including values past `u128`, which must not panic.
    #[test]
    fn values_from_one_hundred_hours_are_out_of_range() {
        let forty_nines = "9".repeat(40);
        let cases = [
            "100:00:00".to_string(),
            "100h".to_string(),
            "99:59:59.9995".to_string(),
            "99999999999999999999h".to_string(),
            "999999999999999999999h".to_string(),
            format!("{forty_nines}h"),
            format!("{forty_nines}:00:00"),
        ];
        for expr in cases {
            assert_eq!(read(&expr, &[]), Err(TimeError::OutOfRange), "{expr}");
        }
    }

    /// A wall-clock time has no media time, so it refuses the document (12.3.1).
    #[test]
    fn wall_clock_time_refuses_the_document() {
        assert_eq!(
            read("wallclock(2024-01-01T10:00:00)", &[]),
            Err(TimeError::Unsupported(Unsupported("wall-clock time")))
        );
    }

    /// The bases and drop modes this reader does not convert refuse the whole
    /// document, as do the SMPTE tick and a discontinuous SMPTE time (7.2.7).
    #[test]
    fn unconverted_time_bases_refuse_the_document() {
        assert_eq!(
            Params::from_tt(&[("timeBase", "clock")]),
            Err(Unsupported("clock time base"))
        );
        assert_eq!(
            Params::from_tt(&[("timeBase", "smpte")]),
            Err(Unsupported("discontinuous SMPTE markers")),
            "the marker mode defaults to discontinuous"
        );
        let smpte_drop = |drop: &'static str| {
            Params::from_tt(&[
                ("timeBase", "smpte"),
                ("markerMode", "continuous"),
                ("dropMode", drop),
            ])
        };
        assert_eq!(
            smpte_drop("dropNTSC"),
            Err(Unsupported("drop-frame time code"))
        );
        assert_eq!(
            smpte_drop("dropPAL"),
            Err(Unsupported("drop-frame time code"))
        );
        assert!(smpte_drop("nonDrop").is_ok());
        assert_eq!(
            read("5t", SMPTE),
            Err(TimeError::Unsupported(Unsupported(
                "tick count under the SMPTE time base"
            )))
        );
        // Under the media time base the drop mode has no effect (7.2.4).
        assert!(Params::from_tt(&[("dropMode", "dropNTSC")]).is_ok());
    }

    /// A parameter value that breaks its syntax refuses the document: zero,
    /// a sign, a fraction, an overflow of `u32`, a missing or extra part of the
    /// multiplier, an unknown time base. The multiplier needs whitespace only
    /// between its two numbers.
    #[test]
    fn invalid_parameter_values_refuse_the_document() {
        for value in ["0", "-5", "1.5", "+5", "", "99999999999999999999"] {
            assert_eq!(
                Params::from_tt(&[("frameRate", value)]),
                Err(Unsupported("invalid ttp:frameRate")),
                "{value:?}"
            );
        }
        for value in [
            "1000",
            "1000 0",
            "0 1001",
            "1000 1001 ",
            " 1000 1001",
            "1000x1001",
            "+1000 1001",
        ] {
            assert_eq!(
                Params::from_tt(&[("frameRateMultiplier", value)]),
                Err(Unsupported("invalid ttp:frameRateMultiplier")),
                "{value:?}"
            );
        }
        assert_eq!(
            Params::from_tt(&[("subFrameRate", "0")]),
            Err(Unsupported("invalid ttp:subFrameRate"))
        );
        assert_eq!(
            Params::from_tt(&[("tickRate", "0")]),
            Err(Unsupported("invalid ttp:tickRate"))
        );
        assert_eq!(
            Params::from_tt(&[("timeBase", "local")]),
            Err(Unsupported("invalid ttp:timeBase"))
        );
        assert_eq!(
            Params::from_tt(&[("dropMode", "dropX")]),
            Err(Unsupported("invalid ttp:dropMode"))
        );
    }
}
