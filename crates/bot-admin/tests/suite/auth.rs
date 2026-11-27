//! Auth gate: Host validation, per-peer rate limiting, credential check, CSRF, and response leak scanning.

use crate::support::*;
use bot_admin::fake::Fake;

#[tokio::test]
async fn unknown_hosts_and_missing_or_wrong_credentials_are_refused() {
    let s = start(Fake::demo()).await;
    let ok = [("Authorization", "Bearer operator-secret-1")];
    for host in ["evil.example", "localhost", "127.0.0.1:1", "localhost:80"] {
        let r = raw(s.addr, "GET", "/v1/admin/me", Some(host), &ok, None).await;
        assert_eq!(r.status, 421, "{host}");
    }
    assert_eq!(
        raw(s.addr, "GET", "/v1/admin/me", Some(""), &ok, None)
            .await
            .status,
        421
    );
    let none = raw(s.addr, "GET", "/v1/admin/me", None, &[], None).await;
    assert_eq!(none.status, 401);
    assert!(
        none.header("www-authenticate")
            .is_some_and(|v| v.starts_with("Basic"))
    );
    // Stay under the 5-failure lockout threshold (missing credentials do not count).
    for bad in ["Bearer nope", "Basic !!!", "Token operator-secret-1"] {
        let r = raw(
            s.addr,
            "GET",
            "/v1/admin/me",
            None,
            &[("Authorization", bad)],
            None,
        )
        .await;
        assert_eq!(r.status, 401, "{bad}");
    }
    assert_eq!(get(&s, OPERATOR, "/v1/admin/me").await.status, 200);
}

#[tokio::test]
async fn basic_credentials_ignore_the_user_name() {
    let s = start(Fake::demo()).await;
    // base64("anyone:operator-secret-1")
    let h = [("Authorization", "Basic YW55b25lOm9wZXJhdG9yLXNlY3JldC0x")];
    let r = raw(s.addr, "GET", "/v1/admin/me", None, &h, None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["principal"], "local:owner");
}

#[tokio::test]
async fn repeated_failures_block_even_the_right_secret_for_a_while() {
    let s = start(Fake::demo()).await;
    for _ in 0..5 {
        let r = raw(
            s.addr,
            "GET",
            "/v1/admin/me",
            None,
            &[("Authorization", "Bearer wrong")],
            None,
        )
        .await;
        assert_eq!(r.status, 401);
    }
    let blocked = get(&s, OPERATOR, "/v1/admin/me").await;
    assert_eq!(blocked.status, 429);
    assert!(blocked.header("retry-after").is_some());
    assert_eq!(blocked.json()["error"], "rate_limited");
}

#[tokio::test]
async fn unknown_routes_and_methods_are_closed_after_authentication() {
    let s = start(Fake::demo()).await;
    assert_eq!(
        raw(s.addr, "GET", "/v1/admin/nothing", None, &[], None)
            .await
            .status,
        401
    );
    let r = get(&s, OPERATOR, "/v1/admin/nothing").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (404, Some("not_found"))
    );
    let d = raw(
        s.addr,
        "DELETE",
        "/v1/admin/runs",
        None,
        &[("Authorization", "Bearer operator-secret-1")],
        None,
    )
    .await;
    assert!(d.status == 403 || d.status == 405, "{}", d.status);
}

#[tokio::test]
async fn responses_are_hardened_and_json() {
    let s = start(Fake::demo()).await;
    for path in ["/v1/admin/me", "/v1/admin/runs", "/v1/admin/nothing"] {
        let r = get(&s, OPERATOR, path).await;
        assert_eq!(r.header("cache-control"), Some("no-store"), "{path}");
        assert_eq!(
            r.header("x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"), "{path}");
        assert!(
            r.header("content-type")
                .is_some_and(|c| c.starts_with("application/json")),
            "{path}"
        );
        assert!(
            r.header("access-control-allow-origin").is_none(),
            "{path}: CORS must stay disabled"
        );
    }
}

#[tokio::test]
async fn closed_without_credentials_means_no_routes() {
    use bot_admin::{AdminConfig, AdminDeps, StaticTokens, router};
    use std::sync::Arc;
    let fake = Arc::new(Fake::new());
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake,
        auth: Arc::new(StaticTokens::new().with("", bot_admin::auth::viewer("local:owner", true))),
        profiles: Arc::new(bot_admin::fake::demo_catalog()),
    };
    assert!(router(AdminConfig::new("127.0.0.1:1".parse().expect("addr")), deps).is_none());
}

#[tokio::test]
async fn one_sources_failures_never_lock_out_an_operator_on_another_source() {
    let s = start(Fake::demo()).await;
    let attacker = [
        ("Authorization", "Bearer wrong"),
        ("X-Test-Peer", "203.0.113.7"),
    ];
    for _ in 0..5 {
        assert_eq!(
            raw(s.addr, "GET", "/v1/admin/me", None, &attacker, None)
                .await
                .status,
            401
        );
    }
    let same = [
        ("Authorization", "Bearer operator-secret-1"),
        ("X-Test-Peer", "203.0.113.7"),
    ];
    assert_eq!(
        raw(s.addr, "GET", "/v1/admin/me", None, &same, None)
            .await
            .status,
        429
    );
    let op = [
        ("Authorization", "Bearer operator-secret-1"),
        ("X-Test-Peer", "198.51.100.9"),
    ];
    assert_eq!(
        raw(s.addr, "GET", "/v1/admin/me", None, &op, None)
            .await
            .status,
        200
    );
    let csrf = csrf(&s, OPERATOR).await;
    let h = [
        ("Authorization", "Bearer operator-secret-1"),
        ("X-Test-Peer", "198.51.100.9"),
        ("Content-Type", "application/json"),
        ("X-Bot-Csrf", csrf.as_str()),
    ];
    let seen = bot_admin::json::hex(&bot_admin::fake::digest("c-1:agenda.delete"));
    let body = format!(r#"{{"allow":true,"seen":"{seen}"}}"#);
    let r = raw(
        s.addr,
        "POST",
        "/v1/admin/approvals/ap-1/decide",
        None,
        &h,
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(get(&s, OPERATOR, "/v1/admin/me").await.status, 200);
}

/// Starts a server with a synthetic secret injected into an approval card.
async fn leaky(
    secret: &str,
    put: impl FnOnce(&mut bot_admin::fake::Data, &str),
) -> (std::net::SocketAddr, String) {
    use bot_admin::{AdminConfig, AdminDeps, StaticTokens, router};
    use std::sync::Arc;
    let fake = Arc::new(Fake::demo());
    fake.edit(|d| put(d, secret));
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("addr"), false)
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake,
        auth: Arc::new(
            StaticTokens::new().with(OPERATOR, bot_admin::auth::viewer("local:owner", true)),
        ),
        profiles: Arc::new(bot_admin::fake::demo_catalog()),
    };
    let app = router(
        AdminConfig::new(addr).leak_values([secret.to_owned()]),
        deps,
    )
    .expect("router");
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    (addr, secret.to_owned())
}

#[tokio::test]
async fn configured_secret_values_never_appear_in_responses_even_with_json_escapes() {
    let h = [("Authorization", "Bearer operator-secret-1")];
    // Synthetic secrets requiring JSON escaping (quotes, backslashes, control chars, UTF-8).
    for secret in [
        "plain-synthetic-value",
        "a\"b\\c-synthetic",
        "line1\nline2-synthetic",
        "tab\there-synthetic",
        "€★\"val\"-synthetic",
    ] {
        let (addr, secret) = leaky(secret, |d, s| {
            d.approvals[0]
                .render
                .push(("Note".into(), format!("before {s} after")))
        })
        .await;
        let r = raw(addr, "GET", "/v1/admin/approvals", None, &h, None).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (500, Some("leak")),
            "{secret:?}"
        );
        assert!(!r.body.contains("synthetic"), "{secret:?}");
        let (addr, _) = leaky(&secret, |d, s| {
            d.approvals[0].render.push((s.to_owned(), "value".into()))
        })
        .await;
        let r = raw(addr, "GET", "/v1/admin/approvals", None, &h, None).await;
        assert_eq!(r.status, 500, "label position: {secret:?}");
        assert_eq!(
            raw(addr, "GET", "/v1/admin/runs/personal/r-1", None, &h, None)
                .await
                .status,
            200
        );
    }
}

/// Key rotation: secrets added to the live redactor after startup are checked on subsequent responses.
#[tokio::test]
async fn a_value_added_to_the_live_redactor_after_start_is_checked_from_the_next_response() {
    use bot_admin::{AdminConfig, AdminDeps, StaticTokens, router, trace::Redactor};
    use std::sync::Arc;
    const OLD: &str = "provider-key-OLD-71c3d9";
    const NEW: &str = "provider-key-NEW-5be802";
    let h = [("Authorization", "Bearer operator-secret-1")];
    let fake = Arc::new(Fake::demo());
    fake.edit(|d| {
        d.approvals[0]
            .render
            .push(("Note".into(), format!("before {NEW} after")))
    });
    let listener = bot_admin::bind("127.0.0.1:0".parse().expect("addr"), false)
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let live = Redactor::new([OLD.to_owned()]);
    let deps = AdminDeps {
        read: fake.clone(),
        act: fake.clone(),
        broker: fake,
        auth: Arc::new(
            StaticTokens::new().with(OPERATOR, bot_admin::auth::viewer("local:owner", true)),
        ),
        profiles: Arc::new(bot_admin::fake::demo_catalog()),
    };
    let app = router(AdminConfig::new(addr).leak_redactor(live.clone()), deps).expect("router");
    tokio::spawn(async move {
        let _ = bot_admin::serve(listener, app, std::future::pending()).await;
    });
    let before = raw(addr, "GET", "/v1/admin/approvals", None, &h, None).await;
    assert_eq!(before.status, 200, "{}", before.body);
    live.add(NEW);
    let after = raw(addr, "GET", "/v1/admin/approvals", None, &h, None).await;
    assert_eq!(
        (after.status, after.json()["error"].as_str()),
        (500, Some("leak"))
    );
    assert!(!after.body.contains("NEW"));
}

#[tokio::test]
async fn requests_without_any_credential_are_not_guesses_and_never_lock_anyone_out() {
    let s = start(Fake::demo()).await;
    // Unauthenticated browser login prompts (401) must never trigger rate limiting.
    for _ in 0..30 {
        let r = raw(s.addr, "GET", "/ui", None, &[], None).await;
        assert_eq!(r.status, 401);
        assert!(r.header("www-authenticate").is_some());
    }
    assert_eq!(get(&s, OPERATOR, "/v1/admin/me").await.status, 200);
}

#[tokio::test]
async fn unreadable_or_wrong_credentials_still_count_as_guesses() {
    let s = start(Fake::demo()).await;
    for bad in [
        "Basic !!!!",
        "Token x",
        "Bearer wrong",
        "Basic YW55b25lOndyb25n",
        "Bearer ",
    ] {
        let r = raw(
            s.addr,
            "GET",
            "/v1/admin/me",
            None,
            &[("Authorization", bad)],
            None,
        )
        .await;
        assert_eq!(r.status, 401, "{bad}");
    }
    let blocked = get(&s, OPERATOR, "/v1/admin/me").await;
    assert_eq!(
        (blocked.status, blocked.json()["error"].as_str()),
        (429, Some("rate_limited"))
    );
}
