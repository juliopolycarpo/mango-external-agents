//! A strict RFC 3339 timestamp parser for the `updatedAt` of an ACP `session/list` row.
//!
//! The value is vendor-controlled, so the parser never panics and never overflows: every step of
//! the arithmetic is checked, the input length is capped, and any non-ASCII byte is refused before
//! a byte offset could land inside a character. A value it cannot read is an error here and `None`
//! at the one call site, the way an unreadable `updatedAt` always degraded.
//!
//! The grammar is RFC 3339 section 5.6 `date-time`, with the two liberties RFC 3339 section 5.6
//! itself allows in a note: `T` and `Z` may be lowercase, and a space may stand for `T`. This is
//! the set `chrono::DateTime::parse_from_rfc3339` accepted when this module replaced it, and
//! `tests.rs` pins the equivalence case by case.
//!
//! ```text
//! YYYY-MM-DD ("T" | "t" | " ") HH:MM:SS [ "." 1*DIGIT ] ( "Z" | "z" | ("+" | "-") HH:MM )
//! ```
//!
//! Ranges: year 0000-9999, month 1-12, day by month with the Gregorian leap rule, hour 0-23,
//! minute 0-59, second 0-60 (60 is a leap second and lands on the following second, as it did
//! under chrono), offset hours 0-23 and offset minutes 0-59. A fraction of any length is truncated
//! to nanoseconds. `-00:00` is read as a zero offset.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The longest input read. A valid timestamp is 35 bytes with a nine digit fraction; the rest of
/// the room is for a longer fraction, which RFC 3339 puts no bound on.
const MAX_INPUT_BYTES: usize = 128;

const NANOS_PER_SECOND: u32 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// Why a timestamp was refused. Only the call site's `None` is observable; the variants let the
/// tests assert the reason and not just the refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rfc3339Error {
    /// The input is longer than [`MAX_INPUT_BYTES`].
    TooLong { len: usize },
    /// The input holds a byte outside ASCII.
    NotAscii,
    /// The input does not follow the grammar: the position is the byte offset of the first
    /// mismatch, or the input length when it ended early.
    Malformed { position: usize },
    /// A field is outside its range.
    OutOfRange(Field),
    /// The instant is valid but the platform's `SystemTime` cannot hold it.
    Unrepresentable,
}

/// A calendar or offset field, named by an out-of-range refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Month,
    Day,
    Hour,
    Minute,
    Second,
    OffsetHour,
    OffsetMinute,
}

impl fmt::Display for Rfc3339Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { len } => {
                write!(
                    out,
                    "timestamp is {len} bytes, expected at most {MAX_INPUT_BYTES}"
                )
            }
            Self::NotAscii => write!(out, "timestamp holds a non-ASCII byte, expected ASCII"),
            Self::Malformed { position } => {
                write!(
                    out,
                    "timestamp breaks the RFC 3339 grammar at byte {position}"
                )
            }
            Self::OutOfRange(field) => write!(out, "timestamp {field:?} is out of range"),
            Self::Unrepresentable => write!(out, "timestamp is not representable as a SystemTime"),
        }
    }
}

impl std::error::Error for Rfc3339Error {}

/// Reads one RFC 3339 timestamp as a `SystemTime`, or `None` when it is malformed, out of range or
/// not representable on this platform.
pub(crate) fn parse_system_time(value: &str) -> Option<SystemTime> {
    parse_rfc3339(value).ok()
}

/// [`parse_system_time`] with the reason a timestamp was refused.
pub(crate) fn parse_rfc3339(value: &str) -> Result<SystemTime, Rfc3339Error> {
    let (unix_seconds, nanos) = parse_unix_parts(value)?;
    system_time_from_unix(unix_seconds, nanos).ok_or(Rfc3339Error::Unrepresentable)
}

/// The timestamp as whole seconds from the epoch, rounded down, and the nanoseconds forward from
/// there. Exact on every platform: the conversion to `SystemTime` is where Windows drops below
/// 100 ns and before 1601, so the parser's own results are checked at this layer.
fn parse_unix_parts(value: &str) -> Result<(i64, u32), Rfc3339Error> {
    if value.len() > MAX_INPUT_BYTES {
        return Err(Rfc3339Error::TooLong { len: value.len() });
    }
    if !value.is_ascii() {
        return Err(Rfc3339Error::NotAscii);
    }
    let bytes = value.as_bytes();
    let mut cursor = Cursor { bytes, position: 0 };

    let year = cursor.digits(4)?;
    cursor.expect(|byte| byte == b'-')?;
    let month = cursor.digits(2)?;
    cursor.expect(|byte| byte == b'-')?;
    let day = cursor.digits(2)?;
    cursor.expect(|byte| matches!(byte, b'T' | b't' | b' '))?;
    let hour = cursor.digits(2)?;
    cursor.expect(|byte| byte == b':')?;
    let minute = cursor.digits(2)?;
    cursor.expect(|byte| byte == b':')?;
    let second = cursor.digits(2)?;
    let nanos = cursor.fraction()?;
    let offset_seconds = cursor.offset_seconds()?;
    if cursor.position != bytes.len() {
        return Err(Rfc3339Error::Malformed {
            position: cursor.position,
        });
    }

    if !(1..=12).contains(&month) {
        return Err(Rfc3339Error::OutOfRange(Field::Month));
    }
    if day < 1 || day > days_in_month(year, month) {
        return Err(Rfc3339Error::OutOfRange(Field::Day));
    }
    if hour > 23 {
        return Err(Rfc3339Error::OutOfRange(Field::Hour));
    }
    if minute > 59 {
        return Err(Rfc3339Error::OutOfRange(Field::Minute));
    }
    if second > 60 {
        return Err(Rfc3339Error::OutOfRange(Field::Second));
    }

    let local_seconds = days_from_civil(i64::from(year), i64::from(month), i64::from(day))
        .checked_mul(SECONDS_PER_DAY)
        .and_then(|seconds| seconds.checked_add(i64::from(hour * 3600 + minute * 60 + second)))
        .ok_or(Rfc3339Error::Unrepresentable)?;
    // The offset says how far the local clock is ahead of UTC, so UTC is the local reading less it.
    let unix_seconds = local_seconds
        .checked_sub(offset_seconds)
        .ok_or(Rfc3339Error::Unrepresentable)?;
    Ok((unix_seconds, nanos))
}

/// A left-to-right reader over the ASCII bytes of one timestamp.
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Cursor<'_> {
    fn malformed(&self) -> Rfc3339Error {
        Rfc3339Error::Malformed {
            position: self.position,
        }
    }

    /// Consumes one byte that satisfies `accept`.
    fn expect(&mut self, accept: impl Fn(u8) -> bool) -> Result<u8, Rfc3339Error> {
        match self.bytes.get(self.position) {
            Some(&byte) if accept(byte) => {
                self.position += 1;
                Ok(byte)
            }
            _ => Err(self.malformed()),
        }
    }

    /// Consumes exactly `count` ASCII digits. `str::parse` would also take a sign, so each digit
    /// is checked by hand.
    fn digits(&mut self, count: usize) -> Result<u32, Rfc3339Error> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = self.expect(|byte| byte.is_ascii_digit())?;
            value = value * 10 + u32::from(byte - b'0');
        }
        Ok(value)
    }

    /// Consumes an optional `.` and one or more digits, and returns the nanoseconds. Digits past
    /// the ninth are read and dropped, which truncates rather than rounds.
    fn fraction(&mut self) -> Result<u32, Rfc3339Error> {
        if self.bytes.get(self.position) != Some(&b'.') {
            return Ok(0);
        }
        self.position += 1;
        let mut nanos = 0u32;
        let mut scale = NANOS_PER_SECOND / 10;
        self.expect(|byte| byte.is_ascii_digit()).map(|byte| {
            nanos = scale * u32::from(byte - b'0');
            scale /= 10;
        })?;
        while let Some(&byte) = self.bytes.get(self.position) {
            if !byte.is_ascii_digit() {
                break;
            }
            self.position += 1;
            // `scale` reaches zero after the ninth digit and later digits add nothing.
            nanos += scale * u32::from(byte - b'0');
            scale /= 10;
        }
        Ok(nanos)
    }

    /// Consumes `Z`, `z` or a signed `HH:MM`, and returns the offset in seconds east of UTC.
    fn offset_seconds(&mut self) -> Result<i64, Rfc3339Error> {
        let sign = match self.expect(|byte| matches!(byte, b'Z' | b'z' | b'+' | b'-'))? {
            b'Z' | b'z' => return Ok(0),
            b'+' => 1,
            _ => -1,
        };
        let hours = self.digits(2)?;
        self.expect(|byte| byte == b':')?;
        let minutes = self.digits(2)?;
        if hours > 23 {
            return Err(Rfc3339Error::OutOfRange(Field::OffsetHour));
        }
        if minutes > 59 {
            return Err(Rfc3339Error::OutOfRange(Field::OffsetMinute));
        }
        Ok(sign * i64::from(hours * 3600 + minutes * 60))
    }
}

/// The Gregorian rule: every fourth year, except centuries that are not multiples of 400.
fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// `month` is already known to be 1-12.
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date, negative before it.
///
/// Howard Hinnant's `days_from_civil`, <https://howardhinnant.github.io/date_algorithms.html#days_from_civil>.
/// It counts from a March-based year so the leap day is the last day of the year, splits the
/// years into 400-year eras of 146097 days, and places the month inside the year with a closed
/// formula. No loop over years. Inputs are in range (year 0-9999, month 1-12, day 1-31), so no
/// intermediate can overflow an `i64`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `UNIX_EPOCH` shifted by `seconds` and then forward by `nanos` (below one second), with
/// `checked_add` / `checked_sub` since `SystemTime` is narrower on some platforms (Windows cannot
/// go before 1601). Before the epoch the nanoseconds still point forward, so they are taken off the
/// whole seconds back.
fn system_time_from_unix(seconds: i64, nanos: u32) -> Option<SystemTime> {
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(seconds.unsigned_abs(), nanos))
    } else {
        let back = Duration::new(seconds.unsigned_abs(), 0).checked_sub(Duration::new(0, nanos))?;
        UNIX_EPOCH.checked_sub(back)
    }
}

#[cfg(test)]
mod tests;
