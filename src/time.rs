//! UTC timestamps: milliseconds since the Unix epoch, and the RFC 3339 forms written to sessions.

use std::time::{SystemTime, UNIX_EPOCH};

const MS_PER_DAY: i64 = 86_400_000;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    system_time_ms(SystemTime::now())
}

/// Milliseconds since the Unix epoch for `time`; negative before 1970.
pub fn system_time_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

/// RFC 3339 in UTC with milliseconds, e.g. `2026-10-07T14:03:09.512Z`.
pub fn rfc3339_ms(ms: i64) -> String {
    let t = Parts::from_ms(ms);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", t.year, t.month, t.day, t.hour, t.minute, t.second, t.milli)
}

/// Like [`rfc3339_ms`] but safe in file names, e.g. `2026-10-07T14-03-09-512Z`.
pub fn file_stamp(ms: i64) -> String {
    let t = Parts::from_ms(ms);
    format!("{:04}-{:02}-{:02}T{:02}-{:02}-{:02}-{:03}Z", t.year, t.month, t.day, t.hour, t.minute, t.second, t.milli)
}

/// Parse an RFC 3339 timestamp (`Z` or `±hh:mm` offset, optional fraction) to milliseconds since
/// the epoch. Fractions finer than a millisecond are truncated.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let field = |range: std::ops::Range<usize>| s.get(range).and_then(digits);
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    // A leap second (`:60`) is accepted and rolls into the next minute.
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    // Everything before index 19 is now known to be ASCII, so these slices fall on char boundaries.
    let mut rest = &s[19..];
    let mut milli = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return None;
        }
        let kept = &fraction[..len.min(3)];
        milli = digits(kept)? * 10_i64.pow(3 - kept.len() as u32);
        rest = &fraction[len..];
    }

    let offset_minutes = match rest {
        "Z" | "z" => 0,
        _ => {
            let ob = rest.as_bytes();
            if ob.len() != 6 || ob[3] != b':' {
                return None;
            }
            let sign = match ob[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (h, m) = (digits(&rest[1..3])?, digits(&rest[4..6])?);
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 60 + m)
        }
    };

    let minutes = hour * 60 + minute - offset_minutes;
    Some(days_from_civil(year, month, day) * MS_PER_DAY + (minutes * 60 + second) * 1000 + milli)
}

/// A UTC instant broken into calendar fields.
struct Parts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    milli: i64,
}

impl Parts {
    fn from_ms(ms: i64) -> Parts {
        let (year, month, day) = civil_from_days(ms.div_euclid(MS_PER_DAY));
        let ms = ms.rem_euclid(MS_PER_DAY);
        Parts {
            year,
            month,
            day,
            hour: ms / 3_600_000,
            minute: ms / 60_000 % 60,
            second: ms / 1000 % 60,
            milli: ms % 1000,
        }
    }
}

/// An all-ASCII-digit string as a number.
fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Conversions between days since 1970-01-01 and proleptic Gregorian dates, from Howard Hinnant's
// "chrono-Compatible Low-Level Date Algorithms". Eras are 400-year cycles starting March 1 so that
// the leap day falls at the end of the year.

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_791_381_789_512), "2026-10-07T14:03:09.512Z");
        assert_eq!(rfc3339_ms(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(file_stamp(1_791_381_789_512), "2026-10-07T14-03-09-512Z");
    }

    #[test]
    fn parses_offsets_and_fractions() {
        assert_eq!(parse_rfc3339_ms("2026-10-07T14:03:09.512Z"), Some(1_791_381_789_512));
        assert_eq!(parse_rfc3339_ms("2026-10-07T14:03:09Z"), Some(1_791_381_789_000));
        assert_eq!(parse_rfc3339_ms("2026-10-07T14:03:09.5129999Z"), Some(1_791_381_789_512));
        assert_eq!(parse_rfc3339_ms("2026-10-07T14:03:09.5Z"), Some(1_791_381_789_500));
        assert_eq!(parse_rfc3339_ms("2026-10-07T09:03:09.512-05:00"), Some(1_791_381_789_512));
        assert_eq!(parse_rfc3339_ms("2026-10-08T00:33:09.512+10:30"), Some(1_791_381_789_512));
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for bad in [
            "",
            "2026-10-07",
            "2026-10-07T14:03:09",
            "2026-10-07 14:03:09Z",
            "2026-13-07T14:03:09Z",
            "2026-02-29T14:03:09Z",
            "2026-10-07T24:00:00Z",
            "2026-10-07T14:03:09.Z",
            "2026-10-07T14:03:09+0500",
            "2026-10-07T14:03:09Zjunk",
            "２026-10-07T14:03:09Z",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn round_trips_across_eras() {
        // Every day from 1600 to 2400, at an arbitrary time of day.
        let mut ms = days_from_civil(1600, 1, 1) * MS_PER_DAY + 45_296_789;
        let end = days_from_civil(2400, 12, 31) * MS_PER_DAY;
        while ms < end {
            assert_eq!(parse_rfc3339_ms(&rfc3339_ms(ms)), Some(ms));
            ms += MS_PER_DAY;
        }
    }
}
