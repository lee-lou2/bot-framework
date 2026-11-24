//! cron 해석과 발생 계산 시험. 독립 기준(UTC 분을 하나씩 현지 시각으로 바꿔 맞는지 보는 전수 탐색)과도 무작위로 대조한다.

use super::*;
use std::time::Duration;

const SEOUL: Tz = chrono_tz::Asia::Seoul;
const NEW_YORK: Tz = chrono_tz::America::New_York;

fn at(rfc3339: &str) -> SystemTime {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().into()
}

/// 현지 시각과 오프셋: `2026-11-01 01:30 -04:00`
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
        .map_or_else(|| "없음".into(), |t| show(tz, t))
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
    // after는 제외, until은 포함: 정확히 00:00Z(= 09:00 KST)인 경계
    let edge = |after: &str, until: &str| occ("0 9 * * *", SEOUL, after, until, 10).len();
    assert_eq!(
        edge("2026-11-03T00:00:00Z", "2026-11-04T00:00:00Z"),
        1,
        "앞 경계 제외·뒤 경계 포함"
    );
    assert_eq!(edge("2026-11-02T23:59:59.999Z", "2026-11-03T00:00:00Z"), 1);
    assert_eq!(edge("2026-11-03T00:00:00Z", "2026-11-03T23:59:59Z"), 0);
    assert_eq!(
        next("0 9 * * *", SEOUL, "2026-11-03T00:00:00Z"),
        "2026-11-04 09:00 +09:00",
        "딱 그 시각이면 다음 날"
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
    // 네팔(+05:45)처럼 15분 단위 오프셋도 현지 벽시계 그대로
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
        "9,13,17시"
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
        "10부터 끝까지 20씩: 10,30,50"
    );
    assert_eq!(
        next("0 0,12 * * *", utc, "2026-11-03T12:00:00Z"),
        "2026-11-04 00:00 +00:00"
    );
}

#[test]
fn weekdays_and_names() {
    let utc = chrono_tz::UTC;
    // 2026-11-06은 금요일, 11-09는 월요일
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
        "7도 일요일"
    );
    assert_eq!(
        next("0 10 * * 5-7", utc, "2026-11-07T11:00:00Z"),
        "2026-11-08 10:00 +00:00",
        "금~일"
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
        "돌아가는 범위는 받지 않아요"
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
        "11월은 31일이 없다"
    );
    assert_eq!(
        next("0 0 31 * *", utc, "2026-12-31T00:00:00Z"),
        "2027-01-31 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 29 2 *", utc, "2026-03-01T00:00:00Z"),
        "2028-02-29 00:00 +00:00",
        "윤일은 다음 윤년에"
    );
    assert_eq!(
        next("59 23 31 12 *", utc, "2026-12-31T23:59:00Z"),
        "2027-12-31 23:59 +00:00"
    );
    assert_eq!(
        next("@yearly", utc, "2026-11-03T00:00:00Z"),
        "2027-01-01 00:00 +00:00"
    );
    // 31일이 있는 달만: 1월 31일 다음은 3월 31일
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
    // 2100년은 윤년이 아니다: 2096-02-29 다음 윤일은 2104-02-29
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
    assert_eq!(leap.len(), 18, "2028..2096 사이 18번(2100은 평년)");
    assert_eq!(
        (leap[0].as_str(), leap[17].as_str()),
        ("2028-02-29 00:00 +00:00", "2096-02-29 00:00 +00:00")
    );
}

#[test]
fn day_of_month_and_weekday_are_or_unless_one_starts_with_a_star() {
    let utc = chrono_tz::UTC;
    // 13일이거나 금요일 (둘 다 정했으니 OR) — 11-06이 금요일
    assert_eq!(
        next("0 0 13 * 5", utc, "2026-11-03T00:00:00Z"),
        "2026-11-06 00:00 +00:00"
    );
    assert_eq!(
        next("0 0 13 * 5", utc, "2026-11-06T00:00:00Z"),
        "2026-11-13 00:00 +00:00"
    );
    // `*/2`는 `*`로 시작하므로 AND: 홀수 날이면서 월요일 → 11-09
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
        "? 는 * 와 같다"
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
        "공백은 하나로 정리한다"
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
        assert!(Cron::parse(bad).is_err(), "받으면 안 돼요: '{bad}'");
    }
    let e = Cron::parse("0 0 9 * * *").unwrap_err();
    assert!(e.contains("다섯 칸") && e.contains("초 칸"), "{e}");
    assert!(
        Cron::parse("60 * * * *").unwrap_err().contains("분 칸"),
        "어느 칸이 틀렸는지 알려 준다"
    );
}

#[test]
fn dates_that_never_happen_have_no_occurrence() {
    let utc = chrono_tz::UTC;
    assert_eq!(
        next("0 0 31 2 *", utc, "2026-01-01T00:00:00Z"),
        "없음",
        "2월 31일"
    );
    assert_eq!(next("0 0 30 2 *", utc, "2026-01-01T00:00:00Z"), "없음");
    assert_eq!(
        next("0 0 31 4,6,9,11 *", utc, "2026-01-01T00:00:00Z"),
        "없음",
        "30일까지인 달만 고르면 없다"
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
        "after는 제외"
    );
    assert_eq!(
        every_minute.occurrences(chrono_tz::UTC, a, u, 1000).len(),
        120,
        "until은 포함"
    );
    assert!(every_minute.occurrences(chrono_tz::UTC, a, u, 0).is_empty());
    assert!(
        every_minute
            .occurrences(chrono_tz::UTC, u, a, 10)
            .is_empty(),
        "거꾸로 된 창"
    );
    assert!(
        every_minute
            .occurrences(chrono_tz::UTC, a, a, 10)
            .is_empty(),
        "빈 창"
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
    // 150일 치(216,000분)가 밀렸다: 오래된 순으로 세는 방식은 한도(10만)에서 멈춰 최신보다 한참 이른 것을 고른다
    let (after, until) = (at("2026-01-01T00:00:00Z"), at("2026-05-31T12:34:30Z"));
    let last = every_minute.last_in(chrono_tz::UTC, after, until).unwrap();
    assert_eq!(show(chrono_tz::UTC, last), "2026-05-31 12:34 +00:00");
    // 구간 끝 시각 자체가 발생이면 포함한다(앞은 제외)
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
    // 한 달에 한 번뿐인 표현식: 뒤쪽 창이 비어도 앞으로 거슬러 찾는다
    let monthly = Cron::parse("0 9 15 * *").unwrap();
    let last = monthly
        .last_in(
            SEOUL,
            at("2026-01-01T00:00:00Z"),
            at("2026-11-30T00:00:00Z"),
        )
        .unwrap();
    assert_eq!(show(SEOUL, last), "2026-11-15 09:00 +09:00");
    // 이 구간에는 발생이 없다
    assert_eq!(
        monthly.last_in(
            SEOUL,
            at("2026-11-16T00:00:00Z"),
            at("2026-12-14T00:00:00Z")
        ),
        None
    );
    // 가을 되돌림: 두 번 오는 01:30 중 늦은 것(UTC 오름차순의 마지막)
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
