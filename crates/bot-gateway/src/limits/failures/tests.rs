use super::*;
use std::net::{Ipv4Addr, Ipv6Addr};

fn ip(n: u8) -> Option<IpAddr> {
    Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)))
}

fn limiter() -> SourceFailures {
    SourceFailures::new(3, Duration::from_secs(60), Duration::from_secs(30))
}

#[test]
fn a_source_is_blocked_after_max_failures_inside_the_window_and_told_how_long() {
    let limiter = limiter();
    let t0 = Instant::now();
    for step in 0..2 {
        limiter.fail(ip(1), t0 + Duration::from_secs(step));
        assert!(limiter.check(ip(1), t0 + Duration::from_secs(step)).is_ok());
    }
    limiter.fail(ip(1), t0 + Duration::from_secs(2));
    let wait = limiter
        .check(ip(1), t0 + Duration::from_secs(2))
        .unwrap_err();
    assert_eq!(wait, Duration::from_secs(30));
    // 막힌 동안은 시간이 가는 만큼 남은 시간이 준다
    let later = limiter
        .check(ip(1), t0 + Duration::from_secs(12))
        .unwrap_err();
    assert_eq!(later, Duration::from_secs(20));
    // 차단이 끝나면 처음부터 다시 센다
    assert!(limiter.check(ip(1), t0 + Duration::from_secs(33)).is_ok());
    limiter.fail(ip(1), t0 + Duration::from_secs(34));
    assert!(
        limiter.check(ip(1), t0 + Duration::from_secs(34)).is_ok(),
        "한 번만 실패했다"
    );
}

#[test]
fn failures_older_than_the_window_do_not_add_up() {
    let limiter = limiter();
    let t0 = Instant::now();
    limiter.fail(ip(1), t0);
    limiter.fail(ip(1), t0 + Duration::from_secs(20));
    // 첫 실패는 창(60초)을 벗어났다: 세 번째가 와도 창 안에는 둘뿐이다
    limiter.fail(ip(1), t0 + Duration::from_secs(70));
    assert!(limiter.check(ip(1), t0 + Duration::from_secs(70)).is_ok());
    limiter.fail(ip(1), t0 + Duration::from_secs(71));
    assert!(limiter.check(ip(1), t0 + Duration::from_secs(71)).is_err());
}

#[test]
fn one_sources_failures_never_block_another_source() {
    let limiter = limiter();
    let t0 = Instant::now();
    for _ in 0..5 {
        limiter.fail(ip(1), t0);
    }
    assert!(limiter.check(ip(1), t0).is_err());
    assert!(limiter.check(ip(2), t0).is_ok());
    assert!(limiter.check(None, t0).is_ok());
    // 주소를 알 수 없는 경로는 한 칸을 같이 쓴다
    for _ in 0..3 {
        limiter.fail(None, t0);
    }
    assert!(limiter.check(None, t0).is_err());
    assert!(limiter.check(ip(2), t0).is_ok());
}

#[test]
fn a_check_alone_never_counts_and_an_unknown_source_is_never_remembered_by_it() {
    let limiter = limiter();
    let t0 = Instant::now();
    for _ in 0..50 {
        assert!(limiter.check(ip(1), t0).is_ok());
    }
    assert_eq!(limiter.sources(), 0);
}

#[test]
fn remembered_sources_are_bounded_and_a_blocked_one_outlasts_idle_ones() {
    let limiter = SourceFailures::new(1, Duration::from_secs(60), Duration::from_secs(600));
    let t0 = Instant::now();
    // 한 번 실패하면 바로 막히는 설정으로 아주 많은 출처를 채운다
    for n in 0..(MAX_SOURCES as u32 + 100) {
        let addr = IpAddr::V6(Ipv6Addr::new(
            0x2001,
            0xdb8,
            0,
            0,
            0,
            0,
            (n >> 16) as u16,
            n as u16,
        ));
        limiter.fail(Some(addr), t0);
    }
    assert!(limiter.sources() <= MAX_SOURCES, "{}", limiter.sources());
}

#[test]
fn entries_that_no_longer_matter_are_dropped_first_when_the_table_is_full() {
    let limiter = SourceFailures::new(5, Duration::from_secs(10), Duration::from_secs(10));
    let t0 = Instant::now();
    for n in 0..MAX_SOURCES as u32 {
        let addr = IpAddr::V6(Ipv6Addr::new(
            0x2001,
            0xdb8,
            0,
            0,
            0,
            0,
            (n >> 16) as u16,
            n as u16,
        ));
        limiter.fail(Some(addr), t0);
    }
    // 창이 지난 뒤 새 출처가 오면 오래된 것들이 한꺼번에 정리된다
    limiter.fail(ip(9), t0 + Duration::from_secs(60));
    assert!(limiter.sources() < MAX_SOURCES / 2, "{}", limiter.sources());
}

fn nth(n: u32) -> Option<IpAddr> {
    Some(IpAddr::V6(Ipv6Addr::new(
        0x2001,
        0xdb8,
        0,
        0,
        0,
        0,
        (n >> 16) as u16,
        n as u16,
    )))
}

#[test]
fn when_the_table_is_full_a_blocked_source_outlasts_sources_with_only_a_few_recent_failures() {
    // 혼합 상태: 일부는 차단 중이고 나머지는 최근 실패만 있다(창 안이라 «더는 기억할 이유가 없는 것»이 아니다)
    let limiter = SourceFailures::new(2, Duration::from_secs(60), Duration::from_secs(600));
    let t0 = Instant::now();
    let blocked: Vec<_> = (0..64).map(nth).collect();
    for source in &blocked {
        limiter.fail(*source, t0);
        limiter.fail(*source, t0);
    }
    for n in 64..MAX_SOURCES as u32 {
        limiter.fail(nth(n), t0 + Duration::from_secs(1));
    }
    assert_eq!(limiter.sources(), MAX_SOURCES);
    // 새 출처가 계속 들어와도(그때마다 한 칸을 비운다) 차단 중인 출처는 하나도 풀리지 않는다
    for n in 0..1000u32 {
        limiter.fail(nth(100_000 + n), t0 + Duration::from_secs(2));
    }
    assert!(limiter.sources() <= MAX_SOURCES);
    for source in &blocked {
        assert!(
            limiter.check(*source, t0 + Duration::from_secs(3)).is_err(),
            "차단 중인 출처가 밀려났어요"
        );
    }
}

#[test]
fn the_oldest_source_with_only_recent_failures_is_the_one_dropped() {
    let limiter = SourceFailures::new(5, Duration::from_secs(600), Duration::from_secs(600));
    let t0 = Instant::now();
    for n in 0..MAX_SOURCES as u32 {
        // 번호가 클수록 최근에 실패했다
        limiter.fail(nth(n), t0 + Duration::from_millis(u64::from(n)));
    }
    limiter.fail(nth(999_999), t0 + Duration::from_secs(10));
    let by = limiter.by.lock().unwrap();
    assert!(!by.contains_key(&nth(0)), "가장 오래된 것이 밀려나야 해요");
    assert!(by.contains_key(&nth(1)) && by.contains_key(&nth(MAX_SOURCES as u32 - 1)));
    assert!(by.contains_key(&nth(999_999)));
}

#[test]
fn only_when_every_source_is_blocked_the_one_whose_block_ends_first_is_dropped() {
    let limiter = SourceFailures::new(1, Duration::from_secs(60), Duration::from_secs(600));
    let t0 = Instant::now();
    for n in 0..MAX_SOURCES as u32 {
        limiter.fail(nth(n), t0 + Duration::from_millis(u64::from(n)));
    }
    limiter.fail(nth(999_999), t0 + Duration::from_secs(1));
    let by = limiter.by.lock().unwrap();
    assert_eq!(by.len(), MAX_SOURCES);
    assert!(!by.contains_key(&nth(0)), "차단이 가장 먼저 끝나는 것");
    assert!(by.contains_key(&nth(999_999)));
}

#[test]
fn a_blocked_source_survives_a_cleanup_long_after_its_failures_left_the_window() {
    // 집계 창(60초)은 지났지만 차단(600초)은 아직 남은 출처: 가득 찬 표를 정리해도 풀려서는 안 된다(나머지는 한 번씩만 실패해 차단이 아니다)
    let limiter = SourceFailures::new(2, Duration::from_secs(60), Duration::from_secs(600));
    let t0 = Instant::now();
    limiter.fail(nth(0), t0);
    limiter.fail(nth(0), t0);
    assert!(limiter.check(nth(0), t0).is_err());
    let later = t0 + Duration::from_secs(120);
    for n in 1..MAX_SOURCES as u32 {
        limiter.fail(nth(n), later);
    }
    limiter.fail(nth(900_000), later);
    assert!(
        limiter.check(nth(0), later).is_err(),
        "차단이 남은 출처가 정리로 풀렸어요"
    );
}
