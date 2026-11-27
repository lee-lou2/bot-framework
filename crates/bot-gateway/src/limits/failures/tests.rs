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
    let later = limiter
        .check(ip(1), t0 + Duration::from_secs(12))
        .unwrap_err();
    assert_eq!(later, Duration::from_secs(20));
    assert!(limiter.check(ip(1), t0 + Duration::from_secs(33)).is_ok());
    limiter.fail(ip(1), t0 + Duration::from_secs(34));
    assert!(
        limiter.check(ip(1), t0 + Duration::from_secs(34)).is_ok(),
        "only one failure after block expiry"
    );
}

#[test]
fn failures_older_than_the_window_do_not_add_up() {
    let limiter = limiter();
    let t0 = Instant::now();
    limiter.fail(ip(1), t0);
    limiter.fail(ip(1), t0 + Duration::from_secs(20));
    // First failure fell outside the 60s window, leaving only two inside.
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
    // Unknown peer addresses share a single bucket.
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
    for n in 0..1000u32 {
        limiter.fail(nth(100_000 + n), t0 + Duration::from_secs(2));
    }
    assert!(limiter.sources() <= MAX_SOURCES);
    for source in &blocked {
        assert!(
            limiter.check(*source, t0 + Duration::from_secs(3)).is_err(),
            "blocked source must not be evicted"
        );
    }
}

#[test]
fn the_oldest_source_with_only_recent_failures_is_the_one_dropped() {
    let limiter = SourceFailures::new(5, Duration::from_secs(600), Duration::from_secs(600));
    let t0 = Instant::now();
    for n in 0..MAX_SOURCES as u32 {
        limiter.fail(nth(n), t0 + Duration::from_millis(u64::from(n)));
    }
    limiter.fail(nth(999_999), t0 + Duration::from_secs(10));
    let by = limiter.by.lock().unwrap();
    assert!(!by.contains_key(&nth(0)), "oldest entry should be evicted");
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
    assert!(
        !by.contains_key(&nth(0)),
        "earliest-expiring block should be evicted"
    );
    assert!(by.contains_key(&nth(999_999)));
}

#[test]
fn a_blocked_source_survives_a_cleanup_long_after_its_failures_left_the_window() {
    // Failure window (60s) expired, but the 600s block is still active and must survive eviction.
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
        "actively blocked source must not be cleared by cleanup"
    );
}
