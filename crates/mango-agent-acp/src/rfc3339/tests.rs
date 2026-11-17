use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    Field, MAX_INPUT_BYTES, Rfc3339Error, days_from_civil, is_leap_year, parse_rfc3339,
    parse_system_time, system_time_from_unix,
};

/// A `SystemTime` as whole seconds from the epoch, rounded down, and the nanoseconds forward from
/// there. One shape for both sides of the epoch, so a literal reads the same either way.
fn unix_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => (
            i64::try_from(after.as_secs()).expect("expected seconds that fit an i64"),
            after.subsec_nanos(),
        ),
        Err(before_epoch) => {
            let before = before_epoch.duration();
            let seconds =
                i64::try_from(before.as_secs()).expect("expected seconds that fit an i64");
            match before.subsec_nanos() {
                0 => (-seconds, 0),
                nanos => (-seconds - 1, 1_000_000_000 - nanos),
            }
        }
    }
}

/// What `chrono::DateTime::parse_from_rfc3339(input).ok().map(SystemTime::from)` returned for each
/// input on chrono 0.4.45, captured on the commit before chrono was removed and pasted here as
/// literals. `Some((seconds, nanos))` is `unix_parts` of the result.
#[rustfmt::skip]
const CHRONO_EQUIVALENCE_TABLE: &[(&str, Option<(i64, u32)>)] = &[
    // The separator and the zone designator: chrono takes `T`, `t` and a space, `Z` and `z`.
    ("1970-01-01T00:00:00Z", Some((0, 0))),
    ("1970-01-01T00:00:00z", Some((0, 0))),
    ("1970-01-01t00:00:00Z", Some((0, 0))),
    ("1970-01-01 00:00:00Z", Some((0, 0))),
    ("1970-01-01 00:00:00z", Some((0, 0))),
    ("1970-01-01t00:00:00+01:00", Some((-3600, 0))),
    ("1970-01-01 00:00:00.5-01:30", Some((5400, 500_000_000))),
    ("1970-01-01_00:00:00Z", None),
    // Offsets: the shape and the two extremes, `-00:00` read as zero.
    ("1970-01-01T00:00:00+00:00", Some((0, 0))),
    ("1970-01-01T00:00:00-00:00", Some((0, 0))),
    ("1970-01-01T00:00:00+0000", None),
    ("1970-01-01T00:00:00+00", None),
    ("1970-01-01T00:00:00+00:00:00", None),
    ("1970-01-01T00:00:00 Z", None),
    ("1970-01-01T00:00:00 +00:00", None),
    ("1970-01-01T00:00:00+00:00 ", None),
    ("1970-01-01T00:00:00+0:00", None),
    ("1970-01-01T00:00:00+00:0", None),
    ("1970-01-01T00:00:00+ab:cd", None),
    ("1970-01-01T00:00:00+00:00Z", None),
    ("1970-01-01T00:00Z", None),
    ("1970-01-01T00:00:00", None),
    ("1970-01-01T00:00:00.5", None),
    ("2024-02-03T04:05:06Z", Some((1_706_933_106, 0))),
    ("2024-02-03T04:05:06+05:30", Some((1_706_913_306, 0))),
    ("2024-02-03T04:05:06-08:00", Some((1_706_961_906, 0))),
    ("2024-02-03T04:05:06+23:59", Some((1_706_846_766, 0))),
    ("2024-02-03T04:05:06-23:59", Some((1_707_019_446, 0))),
    ("2024-02-03T04:05:06+24:00", None),
    ("2024-02-03T04:05:06+00:60", None),
    ("2024-02-03T04:05:06+00:59", Some((1_706_929_566, 0))),
    ("2024-02-03T04:05:06+99:00", None),
    ("2024-02-03T04:05:06+1:00", None),
    ("2024-02-03T04:05:06+01:0", None),
    ("2024-02-03T04:05:06++01:00", None),
    ("1960-01-01T00:00:00+14:00", Some((-315_669_600, 0))),
    // Years 0000 and 9999, with the offsets pushing each edge furthest.
    ("0000-01-01T00:00:00Z", Some((-62_167_219_200, 0))),
    ("0000-01-01T00:00:00+23:59", Some((-62_167_305_540, 0))),
    ("0000-01-01T00:00:00-23:59", Some((-62_167_132_860, 0))),
    ("0000-01-01T00:00:00-00:01", Some((-62_167_219_140, 0))),
    ("0000-01-01T00:00:60+23:59", Some((-62_167_305_480, 0))),
    ("0000-12-31T23:59:59Z", Some((-62_135_596_801, 0))),
    ("0001-01-01T00:00:00Z", Some((-62_135_596_800, 0))),
    ("9999-12-31T23:59:59Z", Some((253_402_300_799, 0))),
    ("9999-12-31T23:59:59-23:59", Some((253_402_387_139, 0))),
    ("9999-12-31T23:59:59+23:59", Some((253_402_214_459, 0))),
    ("9999-12-31T23:59:60Z", Some((253_402_300_800, 0))),
    ("9999-12-31T23:59:60-23:59", Some((253_402_387_140, 0))),
    ("10000-01-01T00:00:00Z", None),
    ("+2024-01-01T00:00:00Z", None),
    ("-0001-01-01T00:00:00Z", None),
    ("024-01-01T00:00:00Z", None),
    ("20240-01-01T00:00:00Z", None),
    ("a970-01-01T00:00:00Z", None),
    // Before the epoch, including the second just before it and the 1601 Windows floor.
    ("1600-12-31T23:59:59Z", Some((-11_644_473_601, 0))),
    ("1601-01-01T00:00:00Z", Some((-11_644_473_600, 0))),
    ("1900-03-01T00:00:00Z", Some((-2_203_891_200, 0))),
    ("1969-07-20T20:17:40Z", Some((-14_182_940, 0))),
    ("1969-12-31T23:59:59Z", Some((-1, 0))),
    ("1969-12-31T23:59:59.5Z", Some((-1, 500_000_000))),
    ("1969-12-31T23:59:59.999999999Z", Some((-1, 999_999_999))),
    ("1677-09-21T00:12:43Z", Some((-9_223_372_037, 0))),
    // After 2^31 and 2^32 seconds, and past the i64 nanosecond range.
    ("2038-01-19T03:14:07Z", Some((2_147_483_647, 0))),
    ("2038-01-19T03:14:08Z", Some((2_147_483_648, 0))),
    ("2106-02-07T06:28:16Z", Some((4_294_967_296, 0))),
    ("2262-04-11T23:47:16Z", Some((9_223_372_036, 0))),
    // Month and day ranges, a month at a time.
    ("2024-00-01T00:00:00Z", None),
    ("2024-13-01T00:00:00Z", None),
    ("2024-1-01T00:00:00Z", None),
    ("2024-0a-01T00:00:00Z", None),
    ("2024-01-00T00:00:00Z", None),
    ("2024-01-32T00:00:00Z", None),
    ("2024-01-1T00:00:00Z", None),
    ("2024-01-0aT00:00:00Z", None),
    ("2024-01-31T00:00:00Z", Some((1_706_659_200, 0))),
    ("2024-04-30T00:00:00Z", Some((1_714_435_200, 0))),
    ("2024-04-31T00:00:00Z", None),
    ("2024-06-31T00:00:00Z", None),
    ("2024-09-31T00:00:00Z", None),
    ("2024-11-31T00:00:00Z", None),
    ("2024-12-31T23:59:59Z", Some((1_735_689_599, 0))),
    // Leap days under the Gregorian rule: valid, and invalid.
    ("2024-02-29T00:00:00Z", Some((1_709_164_800, 0))),
    ("2023-02-29T00:00:00Z", None),
    ("2024-02-30T00:00:00Z", None),
    ("2000-02-29T00:00:00Z", Some((951_782_400, 0))),
    ("1900-02-29T00:00:00Z", None),
    ("2100-02-29T00:00:00Z", None),
    ("1996-02-29T12:00:00Z", Some((825_595_200, 0))),
    ("0000-02-29T00:00:00Z", Some((-62_162_121_600, 0))),
    ("0004-02-29T00:00:00Z", Some((-62_035_891_200, 0))),
    ("0100-02-29T00:00:00Z", None),
    ("0400-02-29T00:00:00Z", Some((-49_539_340_800, 0))),
    ("2400-02-29T00:00:00Z", Some((13_574_563_200, 0))),
    ("4000-02-29T00:00:00Z", Some((64_065_686_400, 0))),
    ("9996-02-29T00:00:00Z", Some((253_281_168_000, 0))),
    // Hour, minute and second ranges. Second 60 is a leap second and lands on the next second at
    // any time of day, not only 23:59:60.
    ("2024-01-01T24:00:00Z", None),
    ("2024-01-01T23:00:00Z", Some((1_704_150_000, 0))),
    ("2024-01-01T0:00:00Z", None),
    ("2024-01-01T0a:00:00Z", None),
    ("2024-01-01T00:60:00Z", None),
    ("2024-01-01T00:59:00Z", Some((1_704_070_740, 0))),
    ("2024-01-01T00:0:00Z", None),
    ("2024-01-01T00:00:59Z", Some((1_704_067_259, 0))),
    ("2024-01-01T00:00:60Z", Some((1_704_067_260, 0))),
    ("2024-01-01T00:00:61Z", None),
    ("2024-01-01T00:00:0Z", None),
    ("2024-01-01T23:59:60Z", Some((1_704_153_600, 0))),
    ("2024-01-01T12:00:60.5Z", Some((1_704_110_460, 500_000_000))),
    ("2024-01-01T12:00:60.999999999Z", Some((1_704_110_460, 999_999_999))),
    ("2024-01-01T12:00:60+01:00", Some((1_704_106_860, 0))),
    // Fractions of 0, 1, 2, 3, 6, 9, 10, 12, 24 and 50 digits, truncated to nanoseconds.
    ("2024-01-01T00:00:00.Z", None),
    ("2024-01-01T00:00:00.1Z", Some((1_704_067_200, 100_000_000))),
    ("2024-01-01T00:00:00.12Z", Some((1_704_067_200, 120_000_000))),
    ("2024-01-01T00:00:00.123Z", Some((1_704_067_200, 123_000_000))),
    ("2024-01-01T00:00:00.123456Z", Some((1_704_067_200, 123_456_000))),
    ("2024-01-01T00:00:00.123456789Z", Some((1_704_067_200, 123_456_789))),
    ("2024-01-01T00:00:00.1234567891Z", Some((1_704_067_200, 123_456_789))),
    ("2024-01-01T00:00:00.999999999999Z", Some((1_704_067_200, 999_999_999))),
    ("2024-01-01T00:00:00.333333333333333333333333Z", Some((1_704_067_200, 333_333_333))),
    ("2024-01-01T00:00:00.000000000999Z", Some((1_704_067_200, 0))),
    ("2024-01-01T00:00:00.12345678901234567890123456789012345678901234567890Z", Some((1_704_067_200, 123_456_789))),
    ("2024-01-01T00:00:00.123+01:00", Some((1_704_063_600, 123_000_000))),
    ("1999-12-31T23:59:59.999Z", Some((946_684_799, 999_000_000))),
    ("2024-07-04T12:34:56.78+02:00", Some((1_720_089_296, 780_000_000))),
    ("2024-07-04T12:34:56.78-02:00", Some((1_720_103_696, 780_000_000))),
    ("2024-01-01T00:00:00,123Z", None),
    ("2024-01-01T00:00:00.1a3Z", None),
    ("2024-01-01T00:00:00.-1Z", None),
    ("2024-01-01T00:00:00.123 Z", None),
    ("2024-01-01T00:00:00.5.5Z", None),
    ("2024-01-01T00:00:00..5Z", None),
    // Garbage, empty and the wrong shape.
    ("", None),
    (" ", None),
    ("Z", None),
    ("garbage", None),
    ("not a timestamp at all", None),
    ("2024-01-01", None),
    ("2024-01-01T", None),
    ("T00:00:00Z", None),
    ("2024/01/01T00:00:00Z", None),
    ("2024-01-01T00-00-00Z", None),
    ("20240101T000000Z", None),
    // Trailing and leading data.
    ("1970-01-01T00:00:00zZ", None),
    ("2024-01-01T00:00:00ZZ", None),
    ("2024-01-01T00:00:00Z ", None),
    (" 2024-01-01T00:00:00Z", None),
    ("2024-01-01T00:00:00Z\n", None),
    ("2024-01-01T00:00:00+00:00x", None),
    ("2024-01-01T00:00:00Zjunk", None),
    ("2024-01-01T00:00:00\0Z", None),
    ("2024-01-01T00:00:00Z\0", None),
    // Multi-byte characters in every field.
    ("2024-01-01T00:00:00é", None),
    ("2024-01-01T00:00:00+0é:00", None),
    ("2024-01-01T00:00:00+01:0é", None),
    ("2024-01-0é T00:00:00Z", None),
    ("é024-01-01T00:00:00Z", None),
    ("2024-01-01é00:00:00Z", None),
    ("2024-01-01T00:00:00.é23Z", None),
    ("２０２４-01-01T00:00:00Z", None),
    ("2024-01-01T００:00:00Z", None),
    ("2024-01-01T00:00:00+01：00", None),
    ("2024-01-01T00:00:00Z😀", None),
    ("2024-01-01T00:00:00+00:00\u{2212}", None),
    ("😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀", None),
];

#[test]
fn the_parser_matches_what_chrono_returned_on_every_table_case() {
    assert!(
        CHRONO_EQUIVALENCE_TABLE.len() >= 40,
        "expected at least 40 equivalence cases, received {}",
        CHRONO_EQUIVALENCE_TABLE.len()
    );
    let mismatches: Vec<String> = CHRONO_EQUIVALENCE_TABLE
        .iter()
        .filter_map(|&(input, expected)| {
            let received = parse_system_time(input).map(unix_parts);
            (received != expected)
                .then(|| format!("{input:?}: expected {expected:?}, received {received:?}"))
        })
        .collect();
    assert!(
        mismatches.is_empty(),
        "(rfc3339) mismatches against chrono:\n{}",
        mismatches.join("\n")
    );
}

/// The one input chrono took that this parser refuses on purpose: U+2212 MINUS SIGN as the offset
/// sign. RFC 3339 section 5.6 writes `time-numoffset` with an ASCII `+` or `-`, and the parser
/// refuses all non-ASCII input so it never has to slice inside a character. chrono 0.4.45 returned
/// `Some((1_704_070_800, 0))` for it. No ACP agent is known to write it.
#[test]
fn a_unicode_minus_sign_offset_is_refused_where_chrono_took_it() {
    assert_eq!(
        parse_rfc3339("2024-01-01T00:00:00\u{2212}01:00"),
        Err(Rfc3339Error::NotAscii),
        "(rfc3339): expected a U+2212 offset sign to be refused as non-ASCII"
    );
}

/// chrono refused every proper prefix of a valid timestamp, and so must this parser, at every byte.
#[test]
fn every_proper_prefix_of_a_valid_timestamp_is_refused() {
    for valid in [
        "2024-02-03T04:05:06Z",
        "2024-02-03T04:05:06.789+05:30",
        "2024-02-03 04:05:06.1z",
    ] {
        assert!(
            parse_system_time(valid).is_some(),
            "expected {valid:?} to parse"
        );
        for end in 0..valid.len() {
            let prefix = &valid[..end];
            assert_eq!(
                parse_system_time(prefix),
                None,
                "(rfc3339): expected the {end}-byte prefix {prefix:?} of {valid:?} to be refused"
            );
        }
    }
}

/// chrono refused one extra character after a valid timestamp, whatever it was.
#[test]
fn trailing_data_after_a_valid_timestamp_is_refused() {
    for trailing in ["x", " ", "Z", "0", ":", "\n", "\0", "é", "😀"] {
        let input = format!("2024-02-03T04:05:06Z{trailing}");
        assert_eq!(
            parse_system_time(&input),
            None,
            "(rfc3339): expected trailing {trailing:?} to be refused in {input:?}"
        );
    }
}

#[test]
fn a_refusal_names_the_reason() {
    let cases: &[(&str, Rfc3339Error)] = &[
        (
            "2024-13-01T00:00:00Z",
            Rfc3339Error::OutOfRange(Field::Month),
        ),
        ("2024-02-30T00:00:00Z", Rfc3339Error::OutOfRange(Field::Day)),
        (
            "2024-01-01T24:00:00Z",
            Rfc3339Error::OutOfRange(Field::Hour),
        ),
        (
            "2024-01-01T00:60:00Z",
            Rfc3339Error::OutOfRange(Field::Minute),
        ),
        (
            "2024-01-01T00:00:61Z",
            Rfc3339Error::OutOfRange(Field::Second),
        ),
        (
            "2024-01-01T00:00:00+24:00",
            Rfc3339Error::OutOfRange(Field::OffsetHour),
        ),
        (
            "2024-01-01T00:00:00+00:60",
            Rfc3339Error::OutOfRange(Field::OffsetMinute),
        ),
        ("", Rfc3339Error::Malformed { position: 0 }),
        (
            "2024-01-01T00:00:00",
            Rfc3339Error::Malformed { position: 19 },
        ),
        (
            "2024-01-01T00:00:00Zx",
            Rfc3339Error::Malformed { position: 20 },
        ),
        (
            "2024-01-01T00:00:00.Z",
            Rfc3339Error::Malformed { position: 20 },
        ),
        ("2024-01-01T00:00:00é", Rfc3339Error::NotAscii),
    ];
    for &(input, expected) in cases {
        assert_eq!(
            parse_rfc3339(input),
            Err(expected),
            "(rfc3339): input {input:?}"
        );
    }
}

/// A day that exists only in a 400-year cycle that the rule must get right, where a month check
/// against a 28-day February would refuse it.
#[test]
fn the_leap_rule_is_the_gregorian_one() {
    let leap = [0, 4, 400, 1996, 2000, 2024, 2400, 9996];
    let common = [1, 100, 200, 300, 1900, 2023, 2100, 9999];
    for year in leap {
        assert!(is_leap_year(year), "expected {year} to be a leap year");
    }
    for year in common {
        assert!(!is_leap_year(year), "expected {year} to be a common year");
    }
}

/// The cap refuses on length before anything else is read, and a fraction up to it still parses.
#[test]
fn input_past_the_length_cap_is_refused_and_input_at_it_is_read() {
    let prefix = "2024-01-01T00:00:00.";
    let suffix = "Z";
    let fraction_digits = MAX_INPUT_BYTES - prefix.len() - suffix.len();
    let at_cap = format!("{prefix}{}{suffix}", "7".repeat(fraction_digits));
    assert_eq!(at_cap.len(), MAX_INPUT_BYTES);
    assert_eq!(
        parse_system_time(&at_cap).map(unix_parts),
        Some((1_704_067_200, 777_777_777)),
        "(rfc3339): expected the {MAX_INPUT_BYTES}-byte input to be read"
    );

    let past_cap = format!("{prefix}{}{suffix}", "7".repeat(fraction_digits + 1));
    assert_eq!(
        parse_rfc3339(&past_cap),
        Err(Rfc3339Error::TooLong {
            len: MAX_INPUT_BYTES + 1
        }),
        "(rfc3339): expected a {}-byte input to be refused for its length",
        past_cap.len()
    );
    // A megabyte of non-timestamp is refused for its length, not scanned.
    assert_eq!(
        parse_rfc3339(&"x".repeat(1 << 20)),
        Err(Rfc3339Error::TooLong { len: 1 << 20 }),
        "(rfc3339): expected a 1 MiB input to be refused for its length"
    );
}

/// What `SystemTime` holds is the platform's: Linux takes any `i64` second, Windows stops at 1601.
/// So the extremes assert only that the conversion answers without panicking, and the values it
/// does answer keep the nanoseconds pointing forward on both sides of the epoch.
#[test]
fn the_unix_conversion_answers_at_the_extremes_without_panicking() {
    let _ = system_time_from_unix(i64::MIN, 0);
    let _ = system_time_from_unix(i64::MIN, 999_999_999);
    let _ = system_time_from_unix(i64::MAX, 999_999_999);
    assert_eq!(
        system_time_from_unix(-1, 1).map(unix_parts),
        Some((-1, 1)),
        "expected one nanosecond after the second before the epoch to round-trip"
    );
    assert_eq!(system_time_from_unix(0, 0), Some(UNIX_EPOCH));
}

/// xorshift64*: a few lines, seeded, and the same sequence on every platform.
struct SeededGenerator(u64);

impl SeededGenerator {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `low..=high`.
    fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low + 1)
    }
}

/// Seconds since the epoch of midnight UTC on a date, counted the slow way: a year at a time and
/// a month at a time. Shares nothing with `days_from_civil`, which is what makes it a check.
fn midnight_by_counting(year: u32, month: u32, day: u32) -> i64 {
    let year_days = |year: u32| {
        if is_leap_by_definition(year) {
            366
        } else {
            365
        }
    };
    let mut days: i64 = 0;
    if year >= 1970 {
        for earlier in 1970..year {
            days += year_days(earlier);
        }
    } else {
        for earlier in year..1970 {
            days -= year_days(earlier);
        }
    }
    for earlier in 1..month {
        days += i64::from(days_in_month_by_definition(year, earlier));
    }
    days += i64::from(day) - 1;
    days * 86_400
}

/// Leap by the calendar's definition, written as three cases and not as the production boolean.
fn is_leap_by_definition(year: u32) -> bool {
    if year.is_multiple_of(400) {
        true
    } else if year.is_multiple_of(100) {
        false
    } else {
        year.is_multiple_of(4)
    }
}

fn days_in_month_by_definition(year: u32, month: u32) -> u32 {
    [
        31,
        if is_leap_by_definition(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][(month - 1) as usize]
}

#[test]
fn random_valid_timestamps_match_an_independent_count() {
    let mut generator = SeededGenerator(0x9E37_79B9_7F4A_7C15);
    for round in 0..3_000 {
        let year = generator.between(0, 9999) as u32;
        let month = generator.between(1, 12) as u32;
        let day = generator.between(1, u64::from(days_in_month_by_definition(year, month))) as u32;
        let (hour, minute, second) = (
            generator.between(0, 23) as u32,
            generator.between(0, 59) as u32,
            generator.between(0, 60) as u32,
        );
        let separator = ["T", "t", " "][generator.between(0, 2) as usize];
        let fraction_digits = generator.between(0, 14) as usize;
        let digits: String = (0..fraction_digits)
            .map(|_| char::from(b'0' + generator.between(0, 9) as u8))
            .collect();
        let (fraction_text, nanos) = if fraction_digits == 0 {
            (String::new(), 0u32)
        } else {
            let nine = format!("{digits:0<9}");
            (
                format!(".{digits}"),
                nine[..9].parse::<u32>().expect("expected nine digits"),
            )
        };
        let (zone_text, offset_seconds) = match generator.between(0, 3) {
            0 => (
                ["Z", "z"][generator.between(0, 1) as usize].to_owned(),
                0i64,
            ),
            _ => {
                let (hours, minutes) = (
                    generator.between(0, 23) as i64,
                    generator.between(0, 59) as i64,
                );
                let negative = generator.between(0, 1) == 1;
                let sign = if negative { '-' } else { '+' };
                let seconds = hours * 3600 + minutes * 60;
                (
                    format!("{sign}{hours:02}:{minutes:02}"),
                    if negative { -seconds } else { seconds },
                )
            }
        };
        let input = format!(
            "{year:04}-{month:02}-{day:02}{separator}{hour:02}:{minute:02}:{second:02}{fraction_text}{zone_text}"
        );
        let expected_seconds = midnight_by_counting(year, month, day)
            + i64::from(hour * 3600 + minute * 60 + second)
            - offset_seconds;
        let received = parse_system_time(&input).map(unix_parts);
        assert_eq!(
            received,
            Some((expected_seconds, nanos)),
            "(rfc3339): round {round}, input {input:?}"
        );
    }
}

#[test]
fn random_out_of_range_fields_are_refused() {
    let mut generator = SeededGenerator(0xD1B5_4A32_D192_ED03);
    for round in 0..2_000 {
        let year = generator.between(0, 9999) as u32;
        let month = generator.between(1, 12) as u32;
        let last_day = days_in_month_by_definition(year, month);
        // One field is pushed out of range, the rest stay valid.
        let (mut m, mut d, mut h, mut mi, mut s, mut oh, mut om) =
            (month, 1u32, 0u32, 0u32, 0u32, 0u32, 0u32);
        match generator.between(0, 6) {
            0 => m = generator.between(13, 99) as u32,
            1 => d = generator.between(u64::from(last_day) + 1, 99) as u32,
            2 => h = generator.between(24, 99) as u32,
            3 => mi = generator.between(60, 99) as u32,
            4 => s = generator.between(61, 99) as u32,
            5 => oh = generator.between(24, 99) as u32,
            _ => om = generator.between(60, 99) as u32,
        }
        let input = format!("{year:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}+{oh:02}:{om:02}");
        assert_eq!(
            parse_system_time(&input),
            None,
            "(rfc3339): round {round}, expected a refusal for {input:?}"
        );
    }
    // Day 0 is out of range in every month.
    assert_eq!(parse_system_time("2024-05-00T00:00:00Z"), None);
}

/// The closed-form date count against the one-year-at-a-time count, for every day of every year
/// at the 400-year cycle's edges and the epoch's.
#[test]
fn days_from_civil_agrees_with_counting_across_the_calendar() {
    for year in (0..=9999u32)
        .step_by(7)
        .chain([0, 1, 99, 100, 399, 400, 1969, 1970, 1971, 9999])
    {
        for month in 1..=12 {
            for day in [1, days_in_month_by_definition(year, month)] {
                assert_eq!(
                    days_from_civil(i64::from(year), i64::from(month), i64::from(day)) * 86_400,
                    midnight_by_counting(year, month, day),
                    "(rfc3339): date {year:04}-{month:02}-{day:02}"
                );
            }
        }
    }
}

/// A stand-in for the fuzz target the repository does not have: mutated and random bytes must
/// come back as a value, never a panic.
#[test]
fn mutated_and_random_input_never_panics() {
    let alphabet: Vec<char> = "0123456789-:.TtZz +é😀\0\u{2212}".chars().collect();
    let mut generator = SeededGenerator(0x0123_4567_89AB_CDEF);
    let seed = "2024-02-29T23:59:60.123456789+23:59";
    for _ in 0..20_000 {
        let mut text: Vec<char> = seed.chars().collect();
        for _ in 0..generator.between(1, 4) {
            let at = generator.between(0, text.len() as u64 - 1) as usize;
            let replacement = alphabet[generator.between(0, alphabet.len() as u64 - 1) as usize];
            match generator.between(0, 2) {
                0 => text[at] = replacement,
                1 => text.insert(at, replacement),
                _ => {
                    text.remove(at);
                    if text.is_empty() {
                        text.push(replacement);
                    }
                }
            }
        }
        let mutated: String = text.into_iter().collect();
        let _ = parse_system_time(&mutated);
    }
}
