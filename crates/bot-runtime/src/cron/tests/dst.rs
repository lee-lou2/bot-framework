//! DST tests for cron occurrence calculation (spring gaps skipped, fall folds emit both UTC instants).

use super::*;

#[test]
fn spring_gap_in_new_york_skips_the_missing_minutes() {
    // 2026-03-08 02:00 EST -> 03:00 EDT: 02:xx does not exist locally.
    assert_eq!(
        occ(
            "30 2 * * *",
            NEW_YORK,
            "2026-03-07T00:00:00Z",
            "2026-03-10T23:59:00Z",
            10
        ),
        [
            "2026-03-07 02:30 -05:00",
            "2026-03-09 02:30 -04:00",
            "2026-03-10 02:30 -04:00"
        ],
        "2026-03-08 02:30 must be skipped without shifting"
    );
    assert_eq!(
        occ(
            "*/30 1-3 * * *",
            NEW_YORK,
            "2026-03-08T05:00:00Z",
            "2026-03-08T09:00:00Z",
            20
        ),
        [
            "2026-03-08 01:00 -05:00",
            "2026-03-08 01:30 -05:00",
            "2026-03-08 03:00 -04:00",
            "2026-03-08 03:30 -04:00"
        ]
    );
    assert_eq!(
        occ(
            "0 3 * * *",
            NEW_YORK,
            "2026-03-07T00:00:00Z",
            "2026-03-09T23:59:00Z",
            10
        ),
        [
            "2026-03-07 03:00 -05:00",
            "2026-03-08 03:00 -04:00",
            "2026-03-09 03:00 -04:00"
        ]
    );
}

#[test]
fn fall_fold_in_new_york_yields_both_utc_occurrences() {
    // 2026-11-01 02:00 EDT -> 01:00 EST: 01:xx occurs twice.
    assert_eq!(
        occ(
            "30 1 * * *",
            NEW_YORK,
            "2026-10-31T00:00:00Z",
            "2026-11-02T23:59:00Z",
            10
        ),
        [
            "2026-10-31 01:30 -04:00",
            "2026-11-01 01:30 -04:00",
            "2026-11-01 01:30 -05:00",
            "2026-11-02 01:30 -05:00"
        ]
    );
    let both = Cron::parse("30 1 * * *").unwrap().occurrences(
        NEW_YORK,
        at("2026-11-01T00:00:00Z"),
        at("2026-11-01T23:59:00Z"),
        5,
    );
    assert_eq!(
        both,
        [at("2026-11-01T05:30:00Z"), at("2026-11-01T06:30:00Z")],
        "two distinct UTC occurrences"
    );
    let all = Cron::parse("*/30 0-3 * * *").unwrap().occurrences(
        NEW_YORK,
        at("2026-11-01T03:59:59Z"),
        at("2026-11-01T09:00:00Z"),
        50,
    );
    assert_eq!(all.len(), 10);
    assert!(all.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(show(NEW_YORK, all[2]), "2026-11-01 01:00 -04:00");
    assert_eq!(show(NEW_YORK, all[4]), "2026-11-01 01:00 -05:00");
}

#[test]
fn half_hour_zone_lord_howe_folds_and_skips_thirty_minutes() {
    // 2026-04-05 02:00 +11:00 -> 01:30 +10:30: 01:30..01:59 occurs twice.
    assert_eq!(
        occ(
            "45 1 * * *",
            chrono_tz::Australia::Lord_Howe,
            "2026-04-03T00:00:00Z",
            "2026-04-05T20:00:00Z",
            10
        ),
        [
            "2026-04-04 01:45 +11:00",
            "2026-04-05 01:45 +11:00",
            "2026-04-05 01:45 +10:30",
            "2026-04-06 01:45 +10:30"
        ]
    );
    // 2026-10-04 02:00 +10:30 -> 02:30 +11:00: 02:00..02:29 does not exist.
    assert_eq!(
        occ(
            "15 2 * * *",
            chrono_tz::Australia::Lord_Howe,
            "2026-10-01T00:00:00Z",
            "2026-10-05T00:00:00Z",
            10
        ),
        [
            "2026-10-02 02:15 +10:30",
            "2026-10-03 02:15 +10:30",
            "2026-10-05 02:15 +11:00"
        ]
    );
    assert_eq!(
        occ(
            "45 2 * * *",
            chrono_tz::Australia::Lord_Howe,
            "2026-10-03T00:00:00Z",
            "2026-10-04T23:59:00Z",
            10
        ),
        ["2026-10-04 02:45 +11:00", "2026-10-05 02:45 +11:00"],
        "minutes outside the skipped gap still occur"
    );
}

#[test]
fn a_fold_that_crosses_midnight_is_found_on_both_passes() {
    // Beirut: 2026-10-25 00:00 EEST -> 10-24 23:00 EET (10-24 23:xx occurs twice).
    assert_eq!(
        occ(
            "30 23 * * *",
            chrono_tz::Asia::Beirut,
            "2026-10-24T00:00:00Z",
            "2026-10-25T00:00:00Z",
            10
        ),
        ["2026-10-24 23:30 +03:00", "2026-10-24 23:30 +02:00"]
    );
}

#[test]
fn a_fold_at_00_01_interleaves_local_dates_in_utc_order() {
    // Historical St. John's rule (2007-2010): 2010-11-07 00:01 NDT -> 11-06 23:01 NST,
    // interleaving local dates relative to UTC order.
    assert_eq!(
        occ(
            "0,30 23,0 * * *",
            chrono_tz::America::St_Johns,
            "2010-11-07T01:00:00Z",
            "2010-11-07T05:00:00Z",
            20
        ),
        [
            "2010-11-06 23:00 -02:30",
            "2010-11-06 23:30 -02:30",
            "2010-11-07 00:00 -02:30",
            "2010-11-06 23:30 -03:30",
            "2010-11-07 00:00 -03:30",
            "2010-11-07 00:30 -03:30",
        ]
    );
}
