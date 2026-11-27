//! Tests for runner deadlines, termination checks, result filtering, and server error classification.

use super::*;

#[tokio::test]
async fn results_that_leak_a_credential_or_have_an_odd_shape_are_blocked() {
    let token = format!("xoxb-{}", "1234567890abcdef");
    let leaked: &'static str = Box::leak(format!("here: {token}").into_boxed_str());
    let cases: Vec<(&str, Server, &str)> = vec![
        ("secret value", server_saying("value-personal-1"), "result_credential"),
        ("short password", server_saying("the password is pw1"), "result_credential"),
        ("token shape", server_saying(leaked), "result_credential"),
        ("image", Server::new(&[("delete", schema())]).replying(|| Answer::Result(json!({"content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]}))), "result_shape"),
        ("resource link", Server::new(&[("delete", schema())]).replying(|| Answer::Result(json!({"content": [{"type": "resource_link", "uri": "file:///etc/passwd"}]}))), "result_shape"),
        ("no content", Server::new(&[("delete", schema())]).replying(|| Answer::Result(json!({"isError": false}))), "result_shape"),
        ("isError not bool", Server::new(&[("delete", schema())]).replying(|| Answer::Result(json!({"content": [], "isError": "yes"}))), "result_shape"),
        ("structured leak", Server::new(&[("delete", schema())]).replying(|| Answer::Result(json!({"content": [], "structuredContent": {"k": "value-personal-1"}}))), "result_credential"),
        ("unsafe number", Server::new(&[("delete", schema())]).replying(|| Answer::Result(json!({"content": [], "structuredContent": {"n": 1e300}}))), "result_shape"),
    ];
    for (name, server, code) in cases {
        let r = rig(server, |_| {}, |_| {});
        let ran = run(
            &r,
            job(
                decl(&[("TOKEN", "reader"), ("PASSWORD", "password")], &[], false),
                r#"{"event":"a"}"#,
            ),
        )
        .await;
        assert_eq!(blocked(&ran), code, "{name}");
        assert_eq!(r.sandbox.terminated().len(), 1, "{name}");
    }
}

#[tokio::test]
async fn a_pass_marker_with_other_meta_reaches_the_broker_flagged_and_without_the_other_meta() {
    let reply = |meta: Value| {
        let server = Server::new(&[("delete", schema())]).replying(move || {
            Answer::Result(json!({"content": [], "isError": false, "_meta": meta.clone()}))
        });
        rig(server, |_| {}, |_| {})
    };
    let ran = run(
        &reply(json!({"bot/pass": true, "other": {"note": "ignored entry"}})),
        job(decl(&[], &[], false), r#"{"event":"a"}"#),
    )
    .await;
    let text = done_text(&ran);
    assert_eq!(
        text,
        r#"{"_meta":{"bot/extra":true,"bot/pass":true},"content":[],"isError":false}"#
    );
    assert!(
        !text.contains("ignored"),
        "other meta values must not pass through"
    );
    let ran = run(
        &reply(json!({"bot/pass": true})),
        job(decl(&[], &[], false), r#"{"event":"a"}"#),
    )
    .await;
    assert_eq!(
        done_text(&ran),
        r#"{"_meta":{"bot/pass":true},"content":[],"isError":false}"#
    );
}

#[tokio::test]
async fn an_oversized_result_is_blocked_and_a_tool_that_leaks_the_proxy_url_is_caught() {
    let r = rig(
        server_saying("0123456789abcdef"),
        |c| c.max_result_bytes = 10,
        |_| {},
    );
    assert_eq!(
        blocked(&run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await),
        "result_size"
    );
    let leaky = Server::new(&[("delete", schema())]).answering(|ctx| {
        let url = ctx
            .env
            .iter()
            .find(|(k, _)| k == "HTTPS_PROXY")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        Box::pin(async move { Answer::Result(json!({"content": [{"type": "text", "text": url}]})) })
    });
    let r = rig(leaky, |_| {}, |_| {});
    let ran = run(
        &r,
        job(decl(&[], &["api.example:443"], false), r#"{"event":"a"}"#),
    )
    .await;
    assert_eq!(blocked(&ran), "result_credential");
}

#[tokio::test]
async fn a_result_lost_after_sending_is_unknown_and_the_process_is_still_ended() {
    let hang = rig(
        Server::new(&[("delete", schema())]).replying(|| Answer::Hang),
        |c| c.call_timeout = Duration::from_millis(100),
        |_| {},
    );
    assert!(matches!(
        run(&hang, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::Unknown("timeout")
    ));
    assert_eq!(hang.sandbox.terminated().len(), 1);
    let closed = rig(
        Server::new(&[("delete", schema())]).replying(|| Answer::Close),
        |_| {},
        |_| {},
    );
    assert!(matches!(
        run(&closed, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::Unknown("closed")
    ));
    assert_eq!(closed.sandbox.terminated().len(), 1);
}

#[tokio::test]
async fn a_protocol_level_refusal_is_definite_and_any_other_server_error_has_an_unknown_effect() {
    for code in [-32600, -32601, -32602] {
        let r = rig(
            Server::new(&[("delete", schema())]).replying(move || Answer::Error(code)),
            |_| {},
            |_| {},
        );
        let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
        assert!(matches!(ran, Ran::Rejected), "{code}: {ran:?}");
    }
    for code in [-32603, -32000, -32099, 7] {
        let r = rig(
            Server::new(&[("delete", schema())]).replying(move || Answer::Error(code)),
            |_| {},
            |_| {},
        );
        let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
        assert!(
            matches!(ran, Ran::Unknown("server_error")),
            "{code}: {ran:?}"
        );
    }
}

#[tokio::test]
async fn nothing_was_sent_is_only_claimed_when_the_started_process_is_confirmed_gone() {
    use crate::testing::Terminate;
    let wrong = || Server::new(&[("other", schema())]);
    let r = rig(
        wrong(),
        |c| c.terminate_timeout = Duration::from_millis(100),
        |_| {},
    );
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::NotSent
    ));
    for mode in [Terminate::Fails, Terminate::Hangs] {
        let r = rig(
            wrong(),
            |c| c.terminate_timeout = Duration::from_millis(100),
            |_| {},
        );
        r.sandbox.set_terminate(mode);
        let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
        assert!(
            matches!(ran, Ran::Unknown("terminate_unconfirmed")),
            "{mode:?}: {ran:?}"
        );
        assert!(!r.sandbox.methods().contains(&"tools/call".to_string()));
    }
    // Keep an already-received result even if teardown fails.
    let r = plain();
    r.sandbox.set_terminate(Terminate::Fails);
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::Done(_)
    ));
    let r = plain();
    r.sandbox.fail_spawn(true);
    r.sandbox.set_terminate(Terminate::Fails);
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::NotSent
    ));
}

#[tokio::test]
async fn the_wall_clock_limit_covers_a_write_that_blocks_and_settles_by_whether_the_call_was_sent()
{
    let r = rig(
        Server::new(&[("delete", schema())]).stalling_after_list(),
        |c| {
            c.wall = Duration::from_millis(400);
            c.call_timeout = Duration::from_secs(60);
        },
        |_| {},
    );
    let big = format!("{{\"event\":\"{}\"}}", "a".repeat(300_000));
    let started = std::time::Instant::now();
    let ran = run(&r, job(decl(&[], &[], false), &big)).await;
    assert!(matches!(ran, Ran::Unknown("wall_timeout")), "{ran:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "blocked past wall deadline"
    );
    assert_eq!(r.sandbox.terminated().len(), 1, "process group terminated");
    let r = rig(
        Server::new(&[("delete", schema())]).stalling_before_initialize(),
        |c| {
            c.wall = Duration::from_millis(300);
            c.handshake_timeout = Duration::from_secs(60);
        },
        |_| {},
    );
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::NotSent
    ));
    assert_eq!(r.sandbox.terminated().len(), 1);
}

#[tokio::test]
async fn structured_results_are_checked_after_decoding_and_credentials_with_special_characters_are_caught()
 {
    // Escaped quotes, backslashes, and newlines differ in serialized bytes from decoded values.
    let secret = "pa\"ss\\word\n-9";
    let r = rig(
        Server::new(&[("delete", schema())]).replying(|| {
            Answer::Result(json!({"content": [], "structuredContent": {"nested": [{"v": "pa\"ss\\word\n-9"}]}}))
        }),
        |_| {},
        |_| {},
    );
    let r = {
        let mut r = r;
        r.secrets = Arc::new(FakeSecrets::new(&[("personal", "reader", secret)]));
        r.runner = McpRunner::new(
            Deps {
                sandbox: r.sandbox.clone(),
                secrets: r.secrets.clone(),
                effects: r.journal.clone(),
                resolver: Arc::new(FakeResolver::new(&[])),
                connector: r.connector.clone(),
                entropy: Arc::new(CountingEntropy::default()),
            },
            HostRegistry {
                servers: [("demo-agenda".to_owned(), "img1".to_owned())].into(),
                ..HostRegistry::default()
            },
            RunnerConfig::default(),
        );
        r
    };
    let ran = run(
        &r,
        job(decl(&[("TOKEN", "reader")], &[], false), r#"{"event":"a"}"#),
    )
    .await;
    assert_eq!(blocked(&ran), "result_credential", "{ran:?}");
    let r2 = rig(
        Server::new(&[("delete", schema())]).replying(|| {
            Answer::Result(json!({"content": [], "structuredContent": {"value-personal-1": 1}}))
        }),
        |_| {},
        |_| {},
    );
    let ran = run(
        &r2,
        job(decl(&[("TOKEN", "reader")], &[], false), r#"{"event":"a"}"#),
    )
    .await;
    assert_eq!(blocked(&ran), "result_credential");
}
