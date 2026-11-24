//! 실행 한도와 결과 검사: 절대 기한, 종료 확인, 결과 차단·검사, 서버 오류의 구분.

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
    // 도구가 환경의 프록시 주소(호출 전용 비밀번호 포함)를 결과로 흘리면 막는다
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
    // 잘못된 요청·없는 메서드·잘못된 인자: 서버가 실행 전에 거절했다
    for code in [-32600, -32601, -32602] {
        let r = rig(
            Server::new(&[("delete", schema())]).replying(move || Answer::Error(code)),
            |_| {},
            |_| {},
        );
        let ran = run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await;
        assert!(matches!(ran, Ran::Rejected), "{code}: {ran:?}");
    }
    // 내부 오류·서버 정의 오류: 일부 효과가 났을 수 있다
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
    // 서버가 선언과 달라 호출 전에 멈췄다(미전송). 프로세스 종료가 확인되면 NotSent다.
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
    // 종료가 실패하거나 시간 안에 확인되지 않으면 미전송을 입증할 수 없다
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
    // 이미 받은 결과는 종료가 확인되지 않아도 버리지 않는다
    let r = plain();
    r.sandbox.set_terminate(Terminate::Fails);
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"a"}"#)).await,
        Ran::Done(_)
    ));
    // 프로세스를 띄우지 못했으면 만들어진 프로세스가 없으니 미전송이다
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
    // 서버가 tools/list에 답한 뒤 입력을 읽지 않는다: 큰 인자를 쓰다 막혀 call 타이머에 닿지 못한다
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
        "기한을 넘겨 오래 막혔다"
    );
    assert_eq!(r.sandbox.terminated().len(), 1, "프로세스 그룹은 끝낸다");
    // 호출을 보내기 전에 막히면(핸드셰이크가 멈춤) 미전송이다
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
    // 따옴표·역슬래시·개행이 든 자격은 직렬화한 글에서는 이스케이프돼 바이트가 달라진다
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
    // 키 이름에 숨겨도 막는다
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
