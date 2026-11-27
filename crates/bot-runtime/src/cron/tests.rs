//! Cron parsing and occurrence calculation tests, including randomized comparison against a brute-force reference.

use super::*;
use std::time::Duration;

const SEOUL: Tz = chrono_tz::Asia::Seoul;
const NEW_YORK: Tz = chrono_tz::America::New_York;

fn at(rfc3339: &str) -> SystemTime {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().into()
}

fn show(tz: Tz, t: SystemTime) -> String {
    DateTime::<Utc>::from(t)
        .with_timezone(&tz)
        .format("%Y-%m-%d %H:%M %:z")
        .to_string()
}

fn occ(expr: &str, tz: Tz, after: &str, until: &str, limit: usize) -> Vec<String> {
    let cron = Cron::parse(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
    cron.occurrences(tz, at(after), at(until), limit)
        .into_iter()
        .map(|t| show(tz, t))
        .collect()
}

fn next(expr: &str, tz: Tz, after: &str) -> String {
    let cron = Cron::parse(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
    cron.next_after(tz, at(after))
        .map_or_else(|| "none".into(), |t| show(tz, t))
}

#[test]
fn daily_in_seoul_runs_at_midnight_utc_and_respects_the_window_edges() {
    let got = occ(
        "0 9 * * *",
        SEOUL,
        "2026-11-02T23:59:59Z",
        "2026-11-05T00:00:00Z",
        10,
    );
    assert_eq!(
        got,
        [
            "2026-11-03 09:00 +09:00",
            "2026-11-04 09:00 +09:00",
            "2026-11-05 09:00 +09:00"
        ]
    );
    // Window is half-open: (after, until].
    let edge = |after: &str, until: &str| occ("0 9 * * *", SEOUL, after, until, 10).len();
    assert_eq!(
        edge("2026-11-03T00:00:00Z", "2026-11-04T00:00:00Z"),
        1,
        "start edge excluded, end edge included"
    );
    assert_eq!(edge("2026-11-02T23:59:59.999Z", "2026-11-03T00:00:00Z"), 1);
    assert_eq!(edge("2026-11-03T00:00:00Z", "2026-11-03T23:59:59Z"), 0);
    assert_eq!(
        next("0 9 * * *", SEOUL, "2026-11-03T00:00:00Z"),
        "2026-11-04 09:00 +09:00",
        "exact match advances to the next day"
    );
    assert_eq!(
        next("0 9 * * *", SEOUL, "2026-11-03T00:00:30Z"),
        "2026-11-04 09:00 +09:00"
    );
}

#[test]
fn utc_profile_and_other_whole_zones_follow_their_own_wall_clock() {
    assert_eq!(
        occ(
            "30 18 * * *",
            chrono_tz::UTC,
            "2026-11-03T23:59:00Z",
            "2026-11-04T23:59:00Z",
            5
        ),
        ["2026-11-04 18:30 +00:00"]
    );
    assert_eq!(
        occ(
            "0 9 * * *",
            chrono_tz::Asia::Kathmandu,
            "2026-11-03T00:00:00Z",
            "2026-11-04T23:59:00Z",
            5
        ),
        ["2026-11-03 09:00 +05:45", "2026-11-04 09:00 +05:45"]
    );
}

#[test]
fn steps_ranges_and_lists() {
    let utc = chrono_tz::UTC;
    assert_eq!(
        next("*/15 * * * *", utc, "2026-11-03T10:07:00Z"),
        "2026-11-03 10:15 +00:00"
    );
    assert_eq!(
        next("*/15 * * * *", utc, "2026-11-03T10:45:00Z"),
        "2026-11-03 11:00 +00:00"
    );
    assert_eq!(
        next("0 9-17/4 * * *", utc, "2026-11-03T10:00:00Z"),
        "2026-11-03 13:00 +00:00",
        "hours 9, 13, 17"
    );
    assert_eq!(
        next("0 9-17/4 * * *", utc, "2026-11-03T17:00:00Z"),
        "2026-11-04 09:00 +00:00"
    );
    assert_eq!(
        next("5,35 * * * *", utc, "2026-11-03T10:06:00Z"),
        "2026-11-03 10:35 +00:00"
    );
    assert_eq!(
        next("10/20 * * * *", utc, "2026-11-03T10:31:00Z"),
        "2026-11-03 10:50 +00:00",
        "10..=59 by 20: 10, 30, 50"
    );
    assert_eq!(
        next("0 0,12 * * *", utc, "2026-11-03T12:00:00Z"),
        "2026-11-04 00:00 +00:00"
    );
}

#[test]
fn weekdays_and_names() {
    let utc = chrono_tz::UTC;
    // 2026-11-06 is Friday; 2026-11-09 is Monday.
    assert_eq!(
        next("30 18 * * 1-5", utc, "2026-11-06T19:00:00Z"),
        "2026-11-09 18:30 +00:00"
    );
    assert_eq!(
        next("30 18 * * MON-FRI", utc, "2026-11-06T19:00:00Z"),
        "2026-11-09 18:30 +00:00"
    );
    assert_eq!(
        next("0 10 * * mon", utc, "2026-11-03T00:00:00Z"),
        "2026-11-09 10:00 +00:00"
    );
    assert_eq!(
        next("0 10 * * 0", utc, "2026-11-03T00:00:00Z"),
        "2026-11-08 10:00 +00:00"
    );
    assert_eq!(
        next("0 10 * * 7", utc, "2026-11-03T00:00:00Z"),
        "2026-11-08 10:00 +00:00",
        "7 also means Sunday"
    );
    assert_eq!(
        next("0 10 * * 5-7", utc, "2026-11-07T11:00:00Z"),
        "2026-11-08 10:00 +00:00",
        "Fri..Sun"
    );
    assert_eq!(
        next("0 9 1 JAN *", utc, "2026-11-03T00:00:00Z"),
        "2027-01-01 09:00 +00:00"
    );
    assert_eq!(
        next("0 9 * Dec,jan,FEB *", utc, "2026-11-03T00:00:00Z"),
        "2026-12-01 09:00 +00:00"
    );
    assert!(
        Cron::parse("0 9 * DEC-FEB *").is_err(),
        "wraparound ranges must be rejected"
    );
}

#[test]
fn month_ends_leap_days_and_year_rollover() {
    let utc = chrono_tz::UTC;
    assert_eq!(
        next("0 0 1 * *", utc, "2026-12-15T10:00:00Z"),
        "2027-01-01 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 31 * *", utc, "2026-11-03T00:00:00Z"),
        "2026-12-31 00:00 +00:00",
        "November has no 31st"
    );
    assert_eq!(
        next("0 0 31 * *", utc, "2026-12-31T00:00:00Z"),
        "2027-01-31 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 29 2 *", utc, "2026-03-01T00:00:00Z"),
        "2028-02-29 00:00 +00:00",
        "leap day advances to next leap year"
    );
    assert_eq!(
        next("59 23 31 12 *", utc, "2026-12-31T23:59:00Z"),
        "2027-12-31 23:59 +00:00"
    );
    assert_eq!(
        next("@yearly", utc, "2026-11-03T00:00:00Z"),
        "2027-01-01 00:00 +00:00"
    );
    let months = occ(
        "0 12 31 * *",
        utc,
        "2026-01-01T00:00:00Z",
        "2026-12-31T23:59:59Z",
        20,
    );
    assert_eq!(months.len(), 7, "{months:?}");
    assert_eq!(
        (months[0].as_str(), months[1].as_str()),
        ("2026-01-31 12:00 +00:00", "2026-03-31 12:00 +00:00")
    );
    // 2100 is not a leap year.
    assert_eq!(
        next("0 0 29 2 *", utc, "2096-03-01T00:00:00Z"),
        "2104-02-29 00:00 +00:00"
    );
    let leap = occ(
        "0 0 29 2 *",
        utc,
        "2026-01-01T00:00:00Z",
        "2100-12-31T00:00:00Z",
        100,
    );
    assert_eq!(
        leap.len(),
        18,
        "18 leap days in 2028..2096 (2100 is common)"
    );
    assert_eq!(
        (leap[0].as_str(), leap[17].as_str()),
        ("2028-02-29 00:00 +00:00", "2096-02-29 00:00 +00:00")
    );
}

#[test]
fn day_of_month_and_weekday_are_or_unless_one_starts_with_a_star() {
    let utc = chrono_tz::UTC;
    // Both constrained -> OR (13th or Friday; 2026-11-06 is Friday).
    assert_eq!(
        next("0 0 13 * 5", utc, "2026-11-03T00:00:00Z"),
        "2026-11-06 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 13 * 5", utc, "2026-11-06T00:00:00Z"),
        "2026-11-13 00:00 +00:00"
    );
    // `*/2` starts with `*` -> AND (odd day and Monday -> 2026-11-09).
    assert_eq!(
        next("0 0 */2 * 1", utc, "2026-11-03T00:00:00Z"),
        "2026-11-09 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 * * 1", utc, "2026-11-03T00:00:00Z"),
        "2026-11-09 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 15 * *", utc, "2026-11-03T00:00:00Z"),
        "2026-11-15 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 ? * 1", utc, "2026-11-03T00:00:00Z"),
        "2026-11-09 00:00 +00:00",
        "? acts like *"
    );
}

#[test]
fn macros_expand_to_five_fields() {
    for (m, expr) in [
        ("@hourly", "0 * * * *"),
        ("@daily", "0 0 * * *"),
        ("@midnight", "0 0 * * *"),
        ("@weekly", "0 0 * * 0"),
        ("@monthly", "0 0 1 * *"),
        ("@yearly", "0 0 1 1 *"),
        ("@annually", "0 0 1 1 *"),
    ] {
        assert_eq!(Cron::parse(m).unwrap().to_string(), expr, "{m}");
    }
    assert_eq!(
        Cron::parse("  0   9\t* * *  ").unwrap().as_str(),
        "0 9 * * *",
        "whitespace is collapsed"
    );
    assert_eq!(
        next("@hourly", chrono_tz::UTC, "2026-11-03T10:00:00Z"),
        "2026-11-03 11:00 +00:00"
    );
}

#[test]
fn invalid_expressions_say_what_is_wrong() {
    for bad in [
        "",
        "* * * *",
        "0 0 9 * * *",
        "60 * * * *",
        "* 24 * * *",
        "* * 0 * *",
        "* * 32 * *",
        "* * * 0 *",
        "* * * 13 *",
        "* * * * 8",
        "5-1 * * * *",
        "*/0 * * * *",
        "*/ * * * *",
        "a * * * *",
        "1,,2 * * * *",
        "1-2-3 * * * *",
        "+5 * * * *",
        "-1 * * * *",
        "1 - 2 * * *",
        "@reboot",
        "@every",
        "0 0 L * *",
        "0 0 * * 1#2",
        "0 0 * * MON-XYZ",
        "0 0 * JANUARY *",
        "99999999999 * * * *",
    ] {
        assert!(Cron::parse(bad).is_err(), "should be rejected: '{bad}'");
    }
    let e = Cron::parse("0 0 9 * * *").unwrap_err();
    assert!(e.contains("5 fields") && e.contains("no seconds"), "{e}");
    assert!(
        Cron::parse("60 * * * *")
            .unwrap_err()
            .contains("minute field"),
        "error should name the invalid field"
    );
}

#[test]
fn dates_that_never_happen_have_no_occurrence() {
    let utc = chrono_tz::UTC;
    assert_eq!(
        next("0 0 31 2 *", utc, "2026-01-01T00:00:00Z"),
        "none",
        "Feb 31"
    );
    assert_eq!(next("0 0 30 2 *", utc, "2026-01-01T00:00:00Z"), "none");
    assert_eq!(
        next("0 0 31 4,6,9,11 *", utc, "2026-01-01T00:00:00Z"),
        "none",
        "30-day months only"
    );
    let never = Cron::parse("0 0 31 2 *").unwrap();
    assert!(
        never
            .occurrences(
                utc,
                at("2026-01-01T00:00:00Z"),
                at("2200-01-01T00:00:00Z"),
                5
            )
            .is_empty()
    );
}

#[test]
fn limit_order_and_empty_windows() {
    let every_minute = Cron::parse("* * * * *").unwrap();
    let (a, u) = (at("2026-11-03T10:00:00Z"), at("2026-11-03T12:00:00Z"));
    let ten = every_minute.occurrences(chrono_tz::UTC, a, u, 10);
    assert_eq!(ten.len(), 10);
    assert!(
        ten.windows(2)
            .all(|w| w[0] < w[1] && w[1].duration_since(w[0]).unwrap() == Duration::from_secs(60))
    );
    assert_eq!(
        show(chrono_tz::UTC, ten[0]),
        "2026-11-03 10:01 +00:00",
        "after is excluded"
    );
    assert_eq!(
        every_minute.occurrences(chrono_tz::UTC, a, u, 1000).len(),
        120,
        "until is included"
    );
    assert!(every_minute.occurrences(chrono_tz::UTC, a, u, 0).is_empty());
    assert!(
        every_minute
            .occurrences(chrono_tz::UTC, u, a, 10)
            .is_empty(),
        "reversed window"
    );
    assert!(
        every_minute
            .occurrences(chrono_tz::UTC, a, a, 10)
            .is_empty(),
        "empty window"
    );
}

#[test]
fn extreme_inputs_do_not_panic() {
    let cron = Cron::parse("* * * * *").unwrap();
    let utc = chrono_tz::UTC;
    let far_future = UNIX_EPOCH + Duration::from_secs(u64::MAX / 4);
    assert_eq!(cron.occurrences(utc, UNIX_EPOCH, far_future, 3).len(), 3);
    assert!(
        cron.occurrences(utc, far_future, far_future + Duration::from_secs(60), 3)
            .len()
            <= 3
    );
    let far_past = UNIX_EPOCH - Duration::from_secs(u64::MAX / 4);
    assert_eq!(cron.occurrences(utc, far_past, UNIX_EPOCH, 2).len(), 2);
    assert!(
        cron.next_after(utc, far_future).is_none() || cron.next_after(utc, far_future).is_some()
    );
    assert!(
        Cron::parse("0 0 29 2 *")
            .unwrap()
            .occurrences(utc, UNIX_EPOCH, far_future, 4)
            .len()
            == 4
    );
}

#[cfg(test)]
mod dst;

#[cfg(test)]
mod reference;

#[test]
fn last_in_finds_the_latest_occurrence_even_after_an_enormous_backlog() {
    let every_minute = Cron::parse("* * * * *").unwrap();
    // 150 days (216,000 minutes) exceeds MAX_LIMIT (100,000), so forward enumeration would truncate early.
    let (after, until) = (at("2026-01-01T00:00:00Z"), at("2026-05-31T12:34:30Z"));
    let last = every_minute.last_in(chrono_tz::UTC, after, until).unwrap();
    assert_eq!(show(chrono_tz::UTC, last), "2026-05-31 12:34 +00:00");
    let exact = every_minute
        .last_in(chrono_tz::UTC, after, at("2026-05-31T12:34:00Z"))
        .unwrap();
    assert_eq!(show(chrono_tz::UTC, exact), "2026-05-31 12:34 +00:00");
    assert_eq!(
        every_minute.last_in(
            chrono_tz::UTC,
            at("2026-05-31T12:34:00Z"),
            at("2026-05-31T12:34:00Z")
        ),
        None
    );
}

#[test]
fn last_in_walks_back_across_empty_stretches_and_dst() {
    let monthly = Cron::parse("0 9 15 * *").unwrap();
    let last = monthly
        .last_in(
            SEOUL,
            at("2026-01-01T00:00:00Z"),
            at("2026-11-30T00:00:00Z"),
        )
        .unwrap();
    assert_eq!(show(SEOUL, last), "2026-11-15 09:00 +09:00");
    assert_eq!(
        monthly.last_in(
            SEOUL,
            at("2026-11-16T00:00:00Z"),
            at("2026-12-14T00:00:00Z")
        ),
        None
    );
    // Fall-back DST fold: pick the later of the two 01:30 occurrences (last in UTC order).
    let fold = Cron::parse("30 1 * * *").unwrap();
    let last = fold
        .last_in(
            NEW_YORK,
            at("2026-10-31T00:00:00Z"),
            at("2026-11-01T12:00:00Z"),
        )
        .unwrap();
    assert_eq!(show(NEW_YORK, last), "2026-11-01 01:30 -05:00");
}
