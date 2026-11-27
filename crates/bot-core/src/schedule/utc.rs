//! Dependency-free strict RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS[.frac]Z`) parser and formatter.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap(year) => 29,
        _ => 28,
    }
}

fn digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes
        .iter()
        .try_fold(0i64, |sum, b| Some(sum * 10 + i64::from(b - b'0')))
}

/// Accepts only 1970..=9999 UTC with optional 1..=9 fractional digits; rejects offsets, lowercase, and leap seconds.
pub fn parse_utc(text: &str) -> Option<SystemTime> {
    let b = text.as_bytes();
    if b.len() < 20 || b[b.len() - 1] != b'Z' {
        return None;
    }
    let shape = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if shape.iter().any(|&(at, mark)| b[at] != mark) {
        return None;
    }
    let (year, month, day) = (digits(&b[0..4])?, digits(&b[5..7])?, digits(&b[8..10])?);
    let (hour, minute, second) = (
        digits(&b[11..13])?,
        digits(&b[14..16])?,
        digits(&b[17..19])?,
    );
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let nanos = match &b[19..b.len() - 1] {
        [] => 0,
        [b'.', fraction @ ..] if (1..=9).contains(&fraction.len()) => {
            let value = digits(fraction)?;
            value * 10i64.pow(9 - u32::try_from(fraction.len()).ok()?)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()?;
    Some(UNIX_EPOCH + Duration::new(seconds, u32::try_from(nanos).ok()?))
}

/// Formats as `YYYY-MM-DDTHH:MM:SS[.fff]Z` (millisecond precision when non-zero); clamps pre-1970 to epoch.
pub fn format_utc(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let total = i64::try_from(since.as_secs()).unwrap_or(i64::MAX / 2);
    let (year, month, day) = civil_from_days(total.div_euclid(86_400));
    let rest = total.rem_euclid(86_400);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let millis = since.subsec_millis();
    if millis == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
    }
}

/// Formats UTC millisecond timestamps in 1970..=9999; rejects out-of-range instants to prevent aliasing.
pub(super) fn schedule_instant(at: SystemTime) -> Option<String> {
    const LAST_SECOND: u64 = 253_402_300_799; // 9999-12-31T23:59:59Z
    (at.duration_since(UNIX_EPOCH).ok()?.as_secs() <= LAST_SECOND).then(|| format_utc(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(text: &str) -> u64 {
        parse_utc(text)
            .expect(text)
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn known_instants_match_the_reference_values() {
        for (text, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("2000-03-01T00:00:00Z", 951_868_800),
            ("2024-02-29T23:59:59Z", 1_709_251_199),
            ("2026-11-24T06:30:15Z", 1_795_501_815),
            ("2030-01-01T12:00:00Z", 1_893_499_200),
            ("2100-03-01T00:00:00Z", 4_107_542_400),
            ("9999-12-31T23:59:59Z", 253_402_300_799),
        ] {
            assert_eq!(secs(text), expected, "{text}");
            assert_eq!(format_utc(parse_utc(text).unwrap()), text);
        }
    }

    #[test]
    fn fractions_are_kept_to_the_millisecond_when_written() {
        let at = parse_utc("2030-01-01T12:00:00.250Z").unwrap();
        assert_eq!(
            at.duration_since(UNIX_EPOCH).unwrap().subsec_nanos(),
            250_000_000
        );
        assert_eq!(format_utc(at), "2030-01-01T12:00:00.250Z");
        let tiny = parse_utc("2030-01-01T12:00:00.000000001Z").unwrap();
        assert_eq!(tiny.duration_since(UNIX_EPOCH).unwrap().subsec_nanos(), 1);
        assert_eq!(
            parse_utc("2030-01-01T12:00:00.5Z"),
            parse_utc("2030-01-01T12:00:00.500Z")
        );
    }

    #[test]
    fn only_the_strict_utc_form_is_accepted() {
        for bad in [
            "",
            "2030-01-01",
            "2030-01-01T12:00:00",
            "2030-01-01 12:00:00Z",
            "2030-01-01t12:00:00Z",
            "2030-01-01T12:00:00z",
            "2030-01-01T12:00:00+00:00",
            "2030-01-01T12:00:00+09:00",
            "2030-01-01T12:00:00.Z",
            "2030-01-01T12:00:00.1234567890Z",
            "2030-01-01T12:00:60Z", // leap second
            "2030-01-01T24:00:00Z",
            "2030-01-01T12:60:00Z",
            "2030-13-01T12:00:00Z",
            "2030-00-10T12:00:00Z",
            "2030-02-29T12:00:00Z", // not a leap year
            "2023-04-31T12:00:00Z",
            "1969-12-31T23:59:59Z",
            "0000-01-01T00:00:00Z",
            "2030-01-01T12:00:0xZ",
            "２０３０-01-01T12:00:00Z",
            " 2030-01-01T12:00:00Z",
            "2030-01-01T12:00:00Z ",
        ] {
            assert!(parse_utc(bad).is_none(), "{bad:?}");
        }
        assert!(parse_utc("2028-02-29T12:00:00Z").is_some());
        assert!(parse_utc("2000-02-29T12:00:00Z").is_some());
        assert!(parse_utc("1900-02-29T12:00:00Z").is_none());
    }

    #[test]
    fn every_day_of_a_few_years_round_trips() {
        let mut at = parse_utc("2023-12-31T00:00:00Z").unwrap();
        for _ in 0..1500 {
            at += Duration::from_secs(86_400);
            let text = format_utc(at);
            assert_eq!(parse_utc(&text), Some(at), "{text}");
        }
    }
}
