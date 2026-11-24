use super::*;

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (k, v) in pairs {
        m.insert(
            header::HeaderName::from_bytes(k.as_bytes()).expect("이름"),
            HeaderValue::from_str(v).expect("값"),
        );
    }
    m
}

#[test]
fn base64_decoding_is_strict() {
    for (i, o) in [
        ("", None),
        ("Zg==", Some("f")),
        ("Zm8=", Some("fo")),
        ("Zm9v", Some("foo")),
        ("Zm9vYg==", Some("foob")),
        ("Zm9vYmE=", Some("fooba")),
        ("Zm9vYmFy", Some("foobar")),
    ] {
        let got = b64_decode(i).map(|b| String::from_utf8(b).expect("utf8"));
        assert_eq!(got.as_deref(), o, "{i}");
    }
    for bad in [
        "Zg=", "Zg===", "Z===", "Zm9v!A==", "Zg==Zm9v", "====", "Zm 9v",
    ] {
        assert!(b64_decode(bad).is_none(), "{bad}");
    }
}

#[test]
fn credentials_come_from_bearer_or_basic_and_ignore_the_user_name() {
    let h = |v: &str| headers(&[("authorization", v)]);
    assert_eq!(
        credential(&h("Bearer s3cret")).as_deref(),
        Some(b"s3cret".as_slice())
    );
    // base64("anyone:s3cret") / base64(":s3cret") / base64("a:b:c")
    assert_eq!(
        credential(&h("Basic YW55b25lOnMzY3JldA==")).as_deref(),
        Some(b"s3cret".as_slice())
    );
    assert_eq!(
        credential(&h("Basic OnMzY3JldA==")).as_deref(),
        Some(b"s3cret".as_slice())
    );
    assert_eq!(
        credential(&h("Basic YTpiOmM=")).as_deref(),
        Some(b"b:c".as_slice())
    );
    for bad in ["Basic", "Basic !!!!", "Basic Zm9v", "Token x", "bearer x"] {
        assert!(credential(&h(bad)).is_none(), "{bad}");
    }
    assert!(credential(&HeaderMap::new()).is_none());
}

#[test]
fn static_tokens_match_only_the_exact_secret_and_skip_empty_ones() {
    let t = StaticTokens::new()
        .with("alpha-secret", viewer("local:owner", true))
        .with("beta-secret", viewer("local:guest", false))
        .with("", viewer("local:nobody", true));
    assert_eq!(t.entries.len(), 2);
    assert_eq!(
        t.identify(b"alpha-secret"),
        Some(viewer("local:owner", true))
    );
    assert_eq!(
        t.identify(b"beta-secret"),
        Some(viewer("local:guest", false))
    );
    for other in [
        &b"alpha-secre"[..],
        b"alpha-secret ",
        b"ALPHA-SECRET",
        b"",
        b"beta",
    ] {
        assert_eq!(t.identify(other), None);
    }
    assert!(StaticTokens::new().with("", viewer("x", true)).is_empty());
}

#[test]
fn limiter_blocks_after_the_failure_budget_and_recovers() {
    let t = Instant::now();
    let mut l = Limiter::new(3, Duration::from_secs(60), Duration::from_secs(30));
    assert!(l.check(t).is_ok());
    l.fail(t);
    l.fail(t + Duration::from_secs(1));
    assert!(l.check(t + Duration::from_secs(2)).is_ok());
    l.fail(t + Duration::from_secs(2));
    assert!(
        l.check(t + Duration::from_secs(3)).is_err(),
        "세 번째 실패에서 막힌다"
    );
    assert!(
        l.check(t + Duration::from_secs(33)).is_ok(),
        "차단 시간이 지나면 풀린다"
    );
    // 집계 창 밖의 실패는 세지 않는다
    let mut l = Limiter::new(2, Duration::from_secs(10), Duration::from_secs(30));
    l.fail(t);
    l.fail(t + Duration::from_secs(20));
    assert!(l.check(t + Duration::from_secs(21)).is_ok());
}

#[test]
fn each_source_has_its_own_failure_budget_and_memory_is_bounded() {
    let t = Instant::now();
    let mut l = Limiters::new(2, Duration::from_secs(60), Duration::from_secs(30));
    let (a, b) = (
        Some("203.0.113.7".parse().expect("주소")),
        Some("198.51.100.9".parse().expect("주소")),
    );
    l.fail(a, t);
    l.fail(a, t);
    assert!(l.check(a, t).is_err(), "실패한 출처는 막힌다");
    assert!(l.check(b, t).is_ok(), "다른 출처는 영향이 없다");
    assert!(l.check(None, t).is_ok());
    l.fail(b, t);
    assert!(l.check(b, t).is_ok(), "예산은 출처마다 센다");
    // 기억하는 출처 수는 한정된다.
    for i in 0..(MAX_SOURCES as u32 + 50) {
        l.fail(
            Some(IpAddr::from(i.to_be_bytes())),
            t + Duration::from_secs(100),
        );
    }
    assert!(l.by.len() <= MAX_SOURCES);
}

fn guard_for_tests() -> Arc<Guard> {
    let hosts = hosts_for(
        "127.0.0.1:8080".parse().expect("주소"),
        &["Admin.Example:8443".into()],
    );
    Guard::new(
        Arc::new(StaticTokens::new()),
        &hosts,
        3,
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
}

#[test]
fn hosts_cover_loopback_names_and_operator_extras_in_lowercase() {
    let g = guard_for_tests();
    for ok in [
        "127.0.0.1:8080",
        "localhost:8080",
        "[::1]:8080",
        "admin.example:8443",
        "LOCALHOST:8080",
    ] {
        assert!(g.host_ok(&headers(&[("host", ok)])), "{ok}");
    }
    for bad in [
        "localhost",
        "localhost:9",
        "evil.example:8080",
        "127.0.0.1:8080.evil.example",
    ] {
        assert!(!g.host_ok(&headers(&[("host", bad)])), "{bad}");
    }
    assert!(!g.host_ok(&HeaderMap::new()));
}

#[test]
fn csrf_token_binds_the_identity_and_post_needs_every_guard() {
    let g = guard_for_tests();
    let (me, other) = (viewer("local:owner", true), viewer("local:guest", false));
    assert_eq!(g.csrf(&me), g.csrf(&me));
    assert_ne!(g.csrf(&me), g.csrf(&other));
    assert_ne!(g.csrf(&me), g.csrf(&viewer("local:owner", false)));
    let tok = g.csrf(&me);
    let base = [
        ("content-type", "application/json; charset=utf-8"),
        ("x-bot-csrf", tok.as_str()),
    ];
    assert!(g.csrf_ok(&headers(&base), &me));
    assert!(!g.csrf_ok(&headers(&base), &other), "다른 신원의 토큰");
    assert!(
        !g.csrf_ok(&headers(&[("content-type", "application/json")]), &me),
        "토큰 없음"
    );
    assert!(!g.csrf_ok(
        &headers(&[("content-type", "text/plain"), ("x-bot-csrf", tok.as_str())]),
        &me
    ));
    let with = |extra: (&'static str, &'static str)| {
        let mut v = base.to_vec();
        v.push(extra);
        headers(&v)
    };
    assert!(g.csrf_ok(&with(("origin", "http://localhost:8080")), &me));
    assert!(g.csrf_ok(&with(("sec-fetch-site", "same-origin")), &me));
    assert!(
        !g.csrf_ok(&with(("origin", "http://evil.example")), &me),
        "교차 출처"
    );
    assert!(
        !g.csrf_ok(&with(("origin", "http://localhost:9999")), &me),
        "다른 포트"
    );
    assert!(!g.csrf_ok(&with(("sec-fetch-site", "cross-site")), &me));
    assert!(!g.csrf_ok(&with(("sec-fetch-site", "same-site")), &me));
}

#[test]
fn new_tokens_are_long_random_hex_and_never_repeat() {
    let (a, b) = (new_token(), new_token());
    assert_eq!((a.len(), b.len()), (64, 64));
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()) && a != b);
    assert!(a.bytes().any(|c| c != b'0'), "영이 아닌 값");
}
